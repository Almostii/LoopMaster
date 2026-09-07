import { useCallback, useEffect, useRef, useState } from "react";

import {
  controlMessage,
  parseMeterFrame,
  type AckMessage,
  type MonitorStatus,
  type RemoteState,
} from "./protocol";

/** 连接状态（方案 3 §6 网络状态机的浏览器端投影）。 */
export type ConnectionStatus = "connecting" | "connected" | "reconnecting" | "disconnected";

/** 心跳判定：超过该时长未收到任何消息即认为连接失效并强制重连。 */
const HEARTBEAT_TIMEOUT_MS = 6000;
const HEARTBEAT_CHECK_INTERVAL_MS = 2000;
const RECONNECT_BASE_MS = 1000;
const RECONNECT_MAX_MS = 10000;

/** 信令请求超时：含 WebRTC 协商（服务端等 ICE gathering，上限 3s）。 */
const SIGNAL_TIMEOUT_MS = 15000;

/** 监听 stats 轮询间隔（S3-6 degraded 判定）。 */
const MONITOR_STATS_INTERVAL_MS = 1000;
/** concealed samples 连续增长 N 个采样周期 → degraded。 */
const DEGRADED_STREAK = 3;

export interface MonitorApi {
  status: MonitorStatus;
  /** 当前可监听的输出通道（initial_state.monitor_available）。 */
  availableBuses: string[];
  /** 最近一次失败原因（用户可读）。 */
  error: string | null;
  /** 手机侧收到的 RTP 包数（诊断用，随 stats 刷新）。 */
  packets: number;
  /** 媒体元素被浏览器暂停（自动播放被拦截）时为 true。 */
  paused: boolean;
  /** 用户手势入口：开始监听指定通道（缺省取第一个可用）。 */
  start: (busId?: string) => Promise<void>;
  /** 停止监听并回收资源。 */
  stop: () => Promise<void>;
  /** 手势内恢复播放（自动播放被拦时点按）。 */
  resume: () => Promise<void>;
}

export interface RemoteConsole {
  state: RemoteState | null;
  status: ConnectionStatus;
  /** send_id → [peak_db, rms_db]（最近一帧二进制 meter 的读数）。 */
  meters: Record<string, [number, number]>;
  setSendGain: (sendId: string, gainDb: number) => void;
  setSendMuted: (sendId: string, muted: boolean) => void;
  /** 6.3 网页端无线监听。 */
  monitor: MonitorApi;
}

interface PendingRequest {
  resolve: (ack: AckMessage) => void;
  reject: (err: { code: string; message: string }) => void;
  timer: number;
}

/** 远程控制台连接状态机 + 指令发送 + 无线监听（Phase 6.3）。 */
export function useRemoteConsole(): RemoteConsole {
  const [state, setState] = useState<RemoteState | null>(null);
  const [status, setStatus] = useState<ConnectionStatus>("connecting");
  const [meters, setMeters] = useState<Record<string, [number, number]>>({});
  const [monitorStatus, setMonitorStatus] = useState<MonitorStatus>("idle");
  const [monitorError, setMonitorError] = useState<string | null>(null);
  const [availableBuses, setAvailableBuses] = useState<string[]>([]);
  const [monitorPaused, setMonitorPaused] = useState(false);
  const [phonePackets, setPhonePackets] = useState(0);

  const wsRef = useRef<WebSocket | null>(null);
  const seqRef = useRef(0);
  const lastMessageAtRef = useRef(performance.now());
  const aliveRef = useRef(true);

  // ------------------------------------------------------------------
  // 监听（Phase 6.3）内部状态
  // ------------------------------------------------------------------
  const pendingRef = useRef(new Map<number, PendingRequest>());
  const pcRef = useRef<RTCPeerConnection | null>(null);
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const peerIdRef = useRef<number | null>(null);
  const busRef = useRef<string | null>(null);
  const monitorActiveRef = useRef(false);
  const remoteSetRef = useRef(false);
  const pendingAnswerRef = useRef<string | null>(null);
  const statsTimerRef = useRef<number | undefined>(undefined);
  const concealedRef = useRef<number | null>(null);
  const degradedStreakRef = useRef(0);
  const wakeLockRef = useRef<{ release: () => Promise<void> } | null>(null);

  const setMonStatus = useCallback((next: MonitorStatus) => {
    setMonitorStatus(next);
    if (next === "playing" || next === "idle" || next === "degraded") {
      setMonitorError(null);
    }
  }, []);

  /** 发送并等待 ack（监听信令专用；超时/错误 reject 结构化错误）。 */
  const signal = useCallback((action: string, data: unknown): Promise<AckMessage> => {
    const ws = wsRef.current;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      return Promise.reject({ code: "disconnected", message: "控制连接未就绪" });
    }
    const seq = ++seqRef.current;
    return new Promise<AckMessage>((resolve, reject) => {
      const timer = window.setTimeout(() => {
        pendingRef.current.delete(seq);
        reject({ code: "timeout", message: `信令 ${action} 超时` });
      }, SIGNAL_TIMEOUT_MS);
      pendingRef.current.set(seq, { resolve, reject, timer });
      ws.send(controlMessage(seq, action, data));
    });
  }, []);

  /** 发送但不等待应答（ICE 候选 / monitor_stop 等尽力而为消息）。 */
  const sendFire = useCallback((action: string, data: unknown) => {
    const ws = wsRef.current;
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    ws.send(controlMessage(++seqRef.current, action, data));
  }, []);

  const sendRaw = useCallback((text: string) => {
    const ws = wsRef.current;
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    ws.send(text);
  }, []);

  // ------------------------------------------------------------------
  // 监听：媒体元素 / Wake Lock / stats 巡检
  // ------------------------------------------------------------------
  const ensureAudio = useCallback(() => {
    if (!audioRef.current) {
      const audio = new Audio();
      audio.autoplay = true;
      audio.setAttribute("playsinline", "");
      audioRef.current = audio;
    }
    return audioRef.current;
  }, []);

  const acquireWakeLock = useCallback(async () => {
    try {
      const nav = navigator as Navigator & {
        wakeLock?: { request: (type: "screen") => Promise<{ release: () => Promise<void> }> };
      };
      // S3-5：API 不可用或被回收时仅静默降级，不承诺绕过锁屏限制。
      wakeLockRef.current = nav.wakeLock ? await nav.wakeLock.request("screen") : null;
    } catch {
      wakeLockRef.current = null;
    }
  }, []);

  const releaseWakeLock = useCallback(() => {
    const lock = wakeLockRef.current;
    wakeLockRef.current = null;
    if (lock) {
      lock.release().catch(() => undefined);
    }
  }, []);

  /** 应用 answer（含 answer 先于 ack 到达的乱序缓存）。 */
  const applyAnswer = useCallback(async (sdp: string) => {
    const pc = pcRef.current;
    if (!pc) {
      pendingAnswerRef.current = sdp;
      return;
    }
    if (remoteSetRef.current) return;
    remoteSetRef.current = true;
    await pc.setRemoteDescription({ type: "answer", sdp });
    pendingAnswerRef.current = null;
    // iOS：手势链可能已断，需再次尝试 play()（失败静默，stats 巡检兜底）。
    const audio = ensureAudio();
    audio.play().catch(() => undefined);
  }, [ensureAudio]);

  const startStats = useCallback(() => {
    if (statsTimerRef.current !== undefined) return;
    concealedRef.current = null;
    degradedStreakRef.current = 0;
    statsTimerRef.current = window.setInterval(async () => {
      const pc = pcRef.current;
      if (!pc) return;
      try {
        const report = await pc.getStats();
        let concealed: number | null = null;
        let packets = 0;
        report.forEach((entry) => {
          if (entry.type === "inbound-rtp" && (entry.kind === "audio" || entry.mediaType === "audio")) {
            concealed = entry.concealedSamples ?? 0;
            packets = entry.packetsReceived ?? 0;
          }
        });
        setPhonePackets(packets);
        setMonitorPaused(audioRef.current?.paused ?? false);
        if (concealed != null) {
          if (concealedRef.current != null && concealed > concealedRef.current) {
            degradedStreakRef.current += 1;
          } else {
            degradedStreakRef.current = 0;
          }
          concealedRef.current = concealed;
          // S3-6：连续超阈值标记 degraded，不自动切诊断通道。
          setMonStatus(degradedStreakRef.current >= DEGRADED_STREAK ? "degraded" : "playing");
        }
      } catch {
        // 连接可能已关闭，忽略本次采样。
      }
    }, MONITOR_STATS_INTERVAL_MS);
  }, [setMonStatus]);

  const stopStats = useCallback(() => {
    if (statsTimerRef.current !== undefined) {
      window.clearInterval(statsTimerRef.current);
      statsTimerRef.current = undefined;
    }
  }, []);

  /** 关闭本地 PeerConnection 与媒体元素（保留 bus 选择以便自动恢复）。 */
  const teardownPeer = useCallback(() => {
    stopStats();
    remoteSetRef.current = false;
    pendingAnswerRef.current = null;
    peerIdRef.current = null;
    if (pcRef.current) {
      try {
        pcRef.current.close();
      } catch {
        // 忽略关闭异常
      }
      pcRef.current = null;
    }
    if (audioRef.current) {
      audioRef.current.srcObject = null;
    }
    releaseWakeLock();
  }, [releaseWakeLock, stopStats]);

  /** 建立监听会话（monitor_start → offer → answer）。 */
  const establish = useCallback(
    async (busId: string) => {
      setMonStatus("connecting");
      try {
        const ack = await signal("monitor_start", {
          bus_id: busId,
          client_request_id: `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
        });
        const peerId = ack.peer_id;
        if (peerId == null) {
          throw { code: "monitor_internal", message: "monitor_start 响应缺少 peer_id" };
        }
        peerIdRef.current = peerId;

        const pc = new RTCPeerConnection({ iceServers: [] }); // 仅 host candidates
        pcRef.current = pc;
        remoteSetRef.current = false;
        pc.addTransceiver("audio", { direction: "recvonly" });
        pc.ontrack = ({ streams }) => {
          const audio = ensureAudio();
          audio.srcObject = streams[0];
          audio.play().catch(() => undefined);
          // 只有媒体轨真正到达才报"监听中"（协商成功 ≠ 链路连通）。
          setMonStatus("playing");
        };
        pc.onconnectionstatechange = () => {
          if (pc.connectionState === "failed") {
            setMonitorError("音频链路连接失败（检查防火墙对 UDP 的放行）");
            setMonStatus("failed");
          }
        };
        pc.onicecandidate = ({ candidate }) => {
          if (candidate && peerIdRef.current != null) {
            sendRaw(
              controlMessage(++seqRef.current, "webrtc_ice_candidate", {
                peer_id: peerIdRef.current,
                candidate: candidate.toJSON(),
              }),
            );
          }
        };

        const offer = await pc.createOffer();
        await pc.setLocalDescription(offer);
        // answer 事件可能在 ack 之前到达（服务端写端双通道），
        // applyAnswer 会把 SDP 缓存到 pendingAnswerRef。
        if (pendingAnswerRef.current) {
          const cached = pendingAnswerRef.current;
          pendingAnswerRef.current = null;
          remoteSetRef.current = true;
          await pc.setRemoteDescription({ type: "answer", sdp: cached });
        }
        const localSdp = pc.localDescription?.sdp;
        if (!localSdp) {
          throw { code: "monitor_internal", message: "本地 SDP 缺失" };
        }
        await signal("webrtc_offer", { peer_id: peerId, sdp: localSdp });
        if (!remoteSetRef.current && pendingAnswerRef.current) {
          const cached = pendingAnswerRef.current;
          pendingAnswerRef.current = null;
          remoteSetRef.current = true;
          await pc.setRemoteDescription({ type: "answer", sdp: cached });
        }
        monitorActiveRef.current = true;
        startStats();
        await acquireWakeLock();
      } catch (err) {
        const { code, message } = err as { code?: string; message?: string };
        teardownPeer();
        if (code === "permission_required") {
          setMonStatus("permission_required");
        } else {
          setMonitorError(message ?? "监听失败");
          setMonStatus("failed");
        }
      }
    },
    [acquireWakeLock, ensureAudio, sendRaw, setMonStatus, signal, startStats, teardownPeer],
  );

  /** 停止监听：通知服务端并回收本地资源。 */
  const stopMonitorInternal = useCallback(async () => {
    const peerId = peerIdRef.current;
    teardownPeer();
    monitorActiveRef.current = false;
    busRef.current = null;
    setMonStatus("idle");
    if (peerId != null) {
      try {
        await signal("monitor_stop", { peer_id: peerId });
      } catch {
        // 连接可能已断：服务端会随连接回收，忽略。
      }
    }
  }, [setMonStatus, signal, teardownPeer]);

  // ------------------------------------------------------------------
  // 连接生命周期
  // ------------------------------------------------------------------
  useEffect(() => {
    aliveRef.current = true;
    let attempt = 0;
    let ws: WebSocket | null = null;
    let reconnectTimer: number | undefined;

    const connect = () => {
      if (!aliveRef.current) return;
      setStatus(attempt === 0 ? "connecting" : "reconnecting");
      ws = new WebSocket(`ws://${location.host}/ws`);
      wsRef.current = ws;

      ws.onopen = () => {
        attempt = 0;
        lastMessageAtRef.current = performance.now();
        setStatus("connected");
        // S3-4：回前台/网络切换恢复——有监听意图时自动重新协商。
        if (monitorActiveRef.current && busRef.current) {
          setMonStatus("reconnecting");
          void establish(busRef.current);
        }
      };
      ws.onmessage = (event) => {
        lastMessageAtRef.current = performance.now();
        if (event.data instanceof ArrayBuffer) {
          const entries = parseMeterFrame(event.data);
          if (entries.length === 0) return;
          const next: Record<string, [number, number]> = {};
          for (const entry of entries) {
            next[entry.id] = [entry.peak_db, entry.rms_db];
          }
          setMeters(next);
        } else if (typeof event.data === "string") {
          try {
            const message = JSON.parse(event.data) as Record<string, unknown>;
            if (message.event === "initial_state" && message.data) {
              const next = message.data as RemoteState;
              setState(next);
              setAvailableBuses(next.monitor_available ?? []);
              // 可监听通道消失（引擎停止/路由变更）：结束本地会话。
              if (
                busRef.current &&
                !(next.monitor_available ?? []).includes(busRef.current)
              ) {
                void stopMonitorInternal();
              }
            } else if (message.event === "webrtc_answer") {
              const data = (message as unknown as { data: { sdp: string } }).data;
              void applyAnswer(data.sdp);
            } else if (message.event === "webrtc_ice_candidate") {
              const data = (
                message as unknown as { data: { candidate: RTCIceCandidateInit } }
              ).data;
              pcRef.current?.addIceCandidate(data.candidate).catch(() => undefined);
            } else if (typeof message.seq === "number") {
              const seq = message.seq as number;
              const pending = pendingRef.current.get(seq);
              if (pending) {
                pendingRef.current.delete(seq);
                window.clearTimeout(pending.timer);
                const ack = message as unknown as AckMessage;
                if (ack.error) {
                  pending.reject({
                    code: ack.code ?? "rejected",
                    message: ack.message ?? ack.error,
                  });
                } else {
                  pending.resolve(ack);
                }
              }
            }
          } catch {
            // 非 JSON 消息忽略
          }
        }
      };
      ws.onclose = () => {
        if (!aliveRef.current) return;
        setStatus("reconnecting");
        // 监听会话随服务端连接回收，标记重连等待 onopen 自动恢复。
        if (monitorActiveRef.current) {
          teardownPeer();
          setMonStatus("reconnecting");
        }
        const delay = Math.min(RECONNECT_MAX_MS, RECONNECT_BASE_MS * 2 ** attempt);
        attempt += 1;
        reconnectTimer = window.setTimeout(connect, delay);
      };
      ws.onerror = () => {
        // onerror 后必然 onclose，由 onclose 负责重连。
        try {
          ws?.close();
        } catch {
          // 忽略关闭异常
        }
      };
    };
    connect();

    // 心跳：超过超时无任何消息（30Hz meter 帧常驻）则强制重连。
    const heartbeat = window.setInterval(() => {
      const current = wsRef.current;
      if (
        current &&
        current.readyState === WebSocket.OPEN &&
        performance.now() - lastMessageAtRef.current > HEARTBEAT_TIMEOUT_MS
      ) {
        try {
          current.close();
        } catch {
          // 忽略关闭异常
        }
      }
    }, HEARTBEAT_CHECK_INTERVAL_MS);

    // S3-4：回前台恢复（Wake Lock 重取 + 必要时重新协商）。
    const onVisibility = () => {
      if (document.visibilityState !== "visible") return;
      if (monitorActiveRef.current && busRef.current) {
        const pc = pcRef.current;
        // PC 已断或媒体元素停摆时重新协商；否则仅补 Wake Lock。
        if (!pc || pc.connectionState === "failed" || pc.connectionState === "closed") {
          void establish(busRef.current);
        } else {
          void acquireWakeLock();
        }
      }
    };
    document.addEventListener("visibilitychange", onVisibility);

    return () => {
      aliveRef.current = false;
      window.clearInterval(heartbeat);
      document.removeEventListener("visibilitychange", onVisibility);
      if (reconnectTimer) window.clearTimeout(reconnectTimer);
      teardownPeer();
      monitorActiveRef.current = false;
      busRef.current = null;
      try {
        ws?.close();
      } catch {
        // 忽略关闭异常
      }
    };
    // establish/stopMonitorInternal 为 useCallback 稳定引用（依赖均为 ref）。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const setSendGain = useCallback(
    (sendId: string, gainDb: number) => sendFire("set_send_gain", { send_id: sendId, gain_db: gainDb }),
    [sendFire],
  );
  const setSendMuted = useCallback(
    (sendId: string, muted: boolean) => sendFire("set_send_muted", { send_id: sendId, muted }),
    [sendFire],
  );

  const startMonitor = useCallback(
    async (busId?: string) => {
      const target = busId ?? availableBuses[0] ?? null;
      if (!target) {
        setMonitorError("无可监听的输出通道（引擎未运行）");
        setMonStatus("failed");
        return;
      }
      busRef.current = target;
      await establish(target);
    },
    [availableBuses, establish, setMonStatus],
  );

  const stopMonitor = useCallback(async () => {
    await stopMonitorInternal();
  }, [stopMonitorInternal]);

  /** 手势内恢复播放（自动播放被浏览器拦截时的兜底）。 */
  const resumeMonitor = useCallback(async () => {
    const audio = ensureAudio();
    try {
      await audio.play();
      setMonitorPaused(false);
    } catch {
      // 仍被拦截则保持提示
    }
  }, [ensureAudio]);

  return {
    state,
    status,
    meters,
    setSendGain,
    setSendMuted,
    monitor: {
      status: monitorStatus,
      availableBuses,
      error: monitorError,
      packets: phonePackets,
      paused: monitorPaused,
      start: startMonitor,
      stop: stopMonitor,
      resume: resumeMonitor,
    },
  };
}
