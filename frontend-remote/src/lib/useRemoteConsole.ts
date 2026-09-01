import { useCallback, useEffect, useRef, useState } from "react";

import { controlMessage, parseMeterFrame, type RemoteState } from "./protocol";

/** 连接状态（方案 3 §6 网络状态机的浏览器端投影）。 */
export type ConnectionStatus = "connecting" | "connected" | "reconnecting" | "disconnected";

/** 心跳判定：超过该时长未收到任何消息即认为连接失效并强制重连。 */
const HEARTBEAT_TIMEOUT_MS = 6000;
const HEARTBEAT_CHECK_INTERVAL_MS = 2000;
const RECONNECT_BASE_MS = 1000;
const RECONNECT_MAX_MS = 10000;

export interface RemoteConsole {
  state: RemoteState | null;
  status: ConnectionStatus;
  /** send_id → [peak_db, rms_db]（最近一帧二进制 meter 的读数）。 */
  meters: Record<string, [number, number]>;
  setSendGain: (sendId: string, gainDb: number) => void;
  setSendMuted: (sendId: string, muted: boolean) => void;
}

/** 远程控制台连接状态机 + 指令发送。 */
export function useRemoteConsole(): RemoteConsole {
  const [state, setState] = useState<RemoteState | null>(null);
  const [status, setStatus] = useState<ConnectionStatus>("connecting");
  const [meters, setMeters] = useState<Record<string, [number, number]>>({});

  const wsRef = useRef<WebSocket | null>(null);
  const seqRef = useRef(0);
  const lastMessageAtRef = useRef(performance.now());
  const aliveRef = useRef(true);

  const send = useCallback((action: string, data: unknown) => {
    const ws = wsRef.current;
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    ws.send(controlMessage(++seqRef.current, action, data));
  }, []);

  const setSendGain = useCallback(
    (sendId: string, gainDb: number) => send("set_send_gain", { send_id: sendId, gain_db: gainDb }),
    [send],
  );
  const setSendMuted = useCallback(
    (sendId: string, muted: boolean) => send("set_send_muted", { send_id: sendId, muted }),
    [send],
  );

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
            const message = JSON.parse(event.data) as {
              event?: string;
              data?: RemoteState;
            };
            if (message.event === "initial_state" && message.data) {
              setState(message.data);
            }
          } catch {
            // 非 JSON 消息忽略
          }
        }
      };
      ws.onclose = () => {
        if (!aliveRef.current) return;
        setStatus("reconnecting");
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

    return () => {
      aliveRef.current = false;
      window.clearInterval(heartbeat);
      if (reconnectTimer) window.clearTimeout(reconnectTimer);
      try {
        ws?.close();
      } catch {
        // 忽略关闭异常
      }
    };
  }, []);

  return { state, status, meters, setSendGain, setSendMuted };
}
