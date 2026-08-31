import { useEffect, useRef, useState } from "react";

import { useRemoteConsole, type ConnectionStatus } from "./lib/useRemoteConsole";
import { dbToFaderPos, faderPosToDb, type RemoteState, type Send } from "./lib/protocol";

type Tab = "sources" | "channels";

const STATUS_TEXT: Record<ConnectionStatus, string> = {
  connecting: "连接中…",
  connected: "已连接",
  reconnecting: "重连中…",
  disconnected: "未连接",
};

/** 引擎状态 → 中文（后端下发原始字符串，做兜底映射）。 */
function engineLabel(status: string): string {
  if (status === "running") return "运行中";
  if (status === "stopped") return "已停止";
  return status || "未知";
}

const THEME_STORAGE_KEY = "loopmaster-remote-theme";
type Theme = "light" | "dark";

/** 主题：localStorage 优先，其次跟随系统；切换时把 `data-theme` 写到 `<html>`。 */
function useTheme(): { theme: Theme; toggle: () => void } {
  const [theme, setTheme] = useState<Theme>(() => {
    const stored = window.localStorage.getItem(THEME_STORAGE_KEY);
    if (stored === "light" || stored === "dark") return stored;
    return window.matchMedia?.("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  });
  useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
    window.localStorage.setItem(THEME_STORAGE_KEY, theme);
  }, [theme]);
  return { theme, toggle: () => setTheme((prev) => (prev === "light" ? "dark" : "light")) };
}

function busName(state: RemoteState, busId: string): string {
  return state.output_channels.find((bus) => bus.id === busId)?.display_name ?? busId;
}

function sinkName(state: RemoteState, sinkId: string): string {
  return state.external_outputs.find((sink) => sink.id === sinkId)?.display_name ?? sinkId;
}

/** 从 URL fragment（`#secret=...` 或 `#pin=...`）读取配对凭据（桌面端扫码打开）。 */
function usePairingFromUrl() {
  const [pairing, setPairing] = useState<{ secret?: string; pin?: string } | null>(null);
  useEffect(() => {
    const params = new URLSearchParams(window.location.hash.replace(/^#/, ""));
    const secret = params.get("secret");
    const pin = params.get("pin");
    if (secret || pin) {
      setPairing({ secret: secret ?? undefined, pin: pin ?? undefined });
    }
  }, []);
  return pairing;
}

/** 扫码配对面板：携带 fragment 凭据完成首次配对（成功后重载走凭证 Cookie）。 */
function PairingPanel({
  pairing,
  onDone,
}: {
  pairing: { secret?: string; pin?: string };
  onDone: () => void;
}) {
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const doPair = async () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      const body: Record<string, string> = { client_name: name.trim() || "我的设备" };
      if (pairing.secret) body.secret = pairing.secret;
      if (pairing.pin) body.pin = pairing.pin;
      const response = await fetch("/api/auth/pair", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      if (!response.ok) {
        const detail = await response.json().catch(() => null);
        throw new Error(detail?.message ?? `配对失败（HTTP ${response.status}）`);
      }
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : "配对失败，请重试。");
    } finally {
      setBusy(false);
    }
  };

  return (
    <main className="remote-shell">
      <h1>配对 LoopMaster</h1>
      <p>输入设备名称，然后点击配对。</p>
      <input
        className="pair-name"
        value={name}
        onChange={(event) => setName(event.target.value)}
        placeholder="我的设备"
        autoFocus
      />
      <button type="button" className="pair-button" onClick={() => void doPair()} disabled={busy}>
        {busy ? "配对中…" : "配对"}
      </button>
      {error && <p className="pair-error">{error}</p>}
    </main>
  );
}

/** strip 卡片头部：首字母头像 + 名称/类型（MUTE 按钮已足够表达静音状态，不再显示文字徽章）。 */
function StripGroupHead({ name, kind }: { name: string; kind: string }) {
  const initial = (name.trim()[0] ?? "?").toUpperCase();
  return (
    <div className="strip-group-head">
      <span className="strip-group-avatar">{initial}</span>
      <div className="strip-group-titles">
        <div className="strip-group-title">{name}</div>
        <div className="strip-group-kind">{kind}</div>
      </div>
    </div>
  );
}

/** 半透明音量条：单一竖向渐变条，显示当前 dB 并支持点击/拖动调整（双击复位 0 dB）。
 *  去掉原来的小 VU 电平表与独立推子手柄，整条既是显示也是控件。 */
const VOLUME_BAR_HEIGHT = 160;
function VolumeBar({
  gainDb,
  onGainChange,
  disabled,
}: {
  gainDb: number;
  onGainChange: (db: number) => void;
  disabled?: boolean;
}) {
  const [pos, setPos] = useState(() => dbToFaderPos(gainDb));
  const draggingRef = useRef(false);
  const dragRef = useRef<{
    pointerId: number;
    startY: number;
    startPos: number;
    lastTap: number;
  }>({ pointerId: -1, startY: 0, startPos: 0, lastTap: 0 });

  // 外部 gainDb 变化（来自其他客户端）→ 同步本地 pos（拖动中不打断）。
  useEffect(() => {
    if (!draggingRef.current) setPos(dbToFaderPos(gainDb));
  }, [gainDb]);

  const onPointerDown = (event: React.PointerEvent<HTMLDivElement>) => {
    if (disabled) return;
    event.currentTarget.setPointerCapture(event.pointerId);
    const now = performance.now();
    const state = dragRef.current;
    if (now - state.lastTap < 300) {
      // 双击复位 0 dB
      onGainChange(0);
      setPos(dbToFaderPos(0));
      state.lastTap = 0;
      draggingRef.current = false;
      return;
    }
    state.lastTap = now;
    state.pointerId = event.pointerId;
    state.startY = event.clientY;
    state.startPos = pos;
    draggingRef.current = true;
  };

  const onPointerMove = (event: React.PointerEvent<HTMLDivElement>) => {
    if (!draggingRef.current || event.pointerId !== dragRef.current.pointerId) return;
    const delta = (event.clientY - dragRef.current.startY) / VOLUME_BAR_HEIGHT;
    const next = Math.min(1, Math.max(0, dragRef.current.startPos - delta));
    setPos(next);
    onGainChange(faderPosToDb(next));
  };

  const onPointerUp = (event: React.PointerEvent<HTMLDivElement>) => {
    if (event.pointerId !== dragRef.current.pointerId) return;
    draggingRef.current = false;
    dragRef.current.pointerId = -1;
  };

  const db = faderPosToDb(pos);
  const percent = pos * 100;
  const muted = db <= -59.5;

  return (
    <div
      className={`volume-bar ${disabled ? "is-disabled" : ""}`}
      role="slider"
      aria-valuemin={-60}
      aria-valuemax={6}
      aria-valuenow={Math.round(db)}
      aria-valuetext={`${db.toFixed(1)} dB`}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerUp}
      onDoubleClick={() => {
        onGainChange(0);
        setPos(dbToFaderPos(0));
      }}
    >
      <div className="volume-bar-track" />
      <div className="volume-bar-fill" style={{ height: `${percent}%` }} />
      <div className="volume-bar-label">
        {muted ? "−∞" : db > 0 ? `+${db.toFixed(1)}` : db.toFixed(1)}
      </div>
    </div>
  );
}

/** 音源 tab 的竖向 send 条：Mute + 半透明音量条 + 目标名（音源默认竖排更像调音台）。 */
function SendStrip({
  send,
  targetName,
  onGain,
  onMute,
}: {
  send: Send;
  targetName: string;
  onGain: (db: number) => void;
  onMute: (muted: boolean) => void;
}) {
  return (
    <div className="send-strip">
      <button
        type="button"
        className={`mute ${send.muted ? "mute-on" : ""}`}
        onClick={() => onMute(!send.muted)}
        aria-pressed={send.muted}
      >
        MUTE
      </button>
      <VolumeBar
        gainDb={send.gain_db}
        onGainChange={onGain}
        disabled={!send.enabled}
      />
      <div className="send-target">{targetName}</div>
    </div>
  );
}

/** 输出通道 tab 的横向 send 行：Mute + 紧凑音量条 + 源→目标（更像路由矩阵）。 */
function SendRow({
  send,
  sourceName,
  targetName,
  onGain,
  onMute,
}: {
  send: Send;
  sourceName: string;
  targetName: string;
  onGain: (db: number) => void;
  onMute: (muted: boolean) => void;
}) {
  return (
    <div className="send-row">
      <button
        type="button"
        className={`mute mute-sm ${send.muted ? "mute-on" : ""}`}
        onClick={() => onMute(!send.muted)}
        aria-pressed={send.muted}
      >
        MUTE
      </button>
      <VolumeBar
        gainDb={send.gain_db}
        onGainChange={onGain}
        disabled={!send.enabled}
      />
      <div className="send-row-info">
        <div className="send-row-from">{sourceName}</div>
        <div className="send-row-to">→ {targetName}</div>
      </div>
    </div>
  );
}

export default function App() {
  const { state, status, setSendGain, setSendMuted } = useRemoteConsole();
  const [tab, setTab] = useState<Tab>("sources");
  const pairing = usePairingFromUrl();
  const { theme, toggle: toggleTheme } = useTheme();

  if (pairing) {
    // 扫码进入配对流程：完成配对后清空 fragment 并重载（使用凭证 Cookie）。
    return (
      <PairingPanel
        pairing={pairing}
        onDone={() => {
          window.location.hash = "";
          window.location.reload();
        }}
      />
    );
  }

  if (!state) {
    const connecting = status === "connecting" || status === "reconnecting";
    return (
      <main className="remote-shell">
        <img
          src="/loopmaster-logo.svg"
          alt="LoopMaster"
          className="cover-logo"
        />
        <h1>
          LoopMaster<span className="cover-accent"> Remote</span>
        </h1>
        <p className="cover-status">{STATUS_TEXT[status]}</p>
        <p className="remote-hint">
          {connecting
            ? "正在连接宿主，请稍候；若长时间无法连接，请确认宿主已开启网络功能、本机与宿主处于同一局域网。"
            : "无法连接宿主，正在自动重连。请确认宿主已开启网络功能、本机与宿主在同一局域网，并访问宿主的局域网 IP（不是本机自己的 IP）。"}
        </p>
      </main>
    );
  }

  const sendsForSource = (sourceId: string) =>
    state.sends.filter((send) => send.source === sourceId);
  const sendsForChannel = (busId: string) =>
    state.sends.filter((send) => send.output_channel === busId && send.external_output);

  return (
    <main className="console">
      <header className="console-header">
        <div className="console-brand">
          <img
            src="/loopmaster-logo.svg"
            alt="LoopMaster"
            className="console-logo"
          />
          <div className="console-title">LoopMaster Remote</div>
        </div>
        <div className="console-header-right">
          <button
            type="button"
            className="theme-toggle"
            onClick={toggleTheme}
            title={theme === "light" ? "切换到深色主题" : "切换到浅色主题"}
          >
            <span className="theme-toggle-glyph">{theme === "light" ? "☾" : "☀"}</span>
            <span className="theme-toggle-label">{theme === "light" ? "暗" : "亮"}</span>
          </button>
          <div className={`status ${status}`}>
            <span className="status-dot" />
            {STATUS_TEXT[status]}
            <span className="status-revision">rev {state.state_revision}</span>
          </div>
        </div>
      </header>

      <nav className="console-tabs">
        <button
          type="button"
          className={tab === "sources" ? "tab-active" : ""}
          onClick={() => setTab("sources")}
        >
          音源
        </button>
        <button
          type="button"
          className={tab === "channels" ? "tab-active" : ""}
          onClick={() => setTab("channels")}
        >
          输出通道
        </button>
      </nav>

      <section className="strip-shelf">
        {tab === "sources" ? (
          state.sources.length === 0 ? (
            <div className="empty-tip">暂无音源。请在桌面端添加音源后重试。</div>
          ) : (
            state.sources.map((source) => {
              const sends = sendsForSource(source.id);
              return (
                <div className="strip-group" key={source.id}>
                  <StripGroupHead name={source.display_name} kind="音源" />
                  {sends.length === 0 ? (
                    <div className="empty-tip">
                      <span className="empty-symbol">◇</span>该音源未连接到任何输出通道。
                    </div>
                  ) : (
                    <div className="strip-group-body">
                      {sends.map((send) => (
                        <SendStrip
                          key={send.send_id}
                          send={send}
                          targetName={busName(state, send.output_channel ?? "")}
                          onGain={(db) => setSendGain(send.send_id, db)}
                          onMute={(muted) => setSendMuted(send.send_id, muted)}
                        />
                      ))}
                    </div>
                  )}
                </div>
              );
            })
          )
        ) : state.output_channels.length === 0 ? (
          <div className="empty-tip">暂无输出通道。请在桌面端添加后重试。</div>
        ) : (
          state.output_channels.map((channel) => {
            const sends = sendsForChannel(channel.id);
            return (
              <div className="strip-group" key={channel.id}>
                <StripGroupHead name={channel.display_name} kind="输出通道" />
                {sends.length === 0 ? (
                  <div className="empty-tip">
                    <span className="empty-symbol">◇</span>该通道未连接到任何外部输出。
                  </div>
                ) : (
                  <div className="strip-group-body strip-group-body-rows">
                    {sends.map((send) => {
                      const sourceName =
                        state.sources.find((src) => src.id === send.source)?.display_name ??
                        send.source ??
                        "未知音源";
                      return (
                        <SendRow
                          key={send.send_id}
                          send={send}
                          sourceName={sourceName}
                          targetName={sinkName(state, send.external_output ?? "")}
                          onGain={(db) => setSendGain(send.send_id, db)}
                          onMute={(muted) => setSendMuted(send.send_id, muted)}
                        />
                      );
                    })}
                  </div>
                )}
              </div>
            );
          })
        )}
      </section>
      <footer className="console-footer">
        <span className="console-footer-dot" />
        引擎 {engineLabel(state.engine_status)} · {state.sample_rate} Hz · 远程控制台
      </footer>
    </main>
  );
}
