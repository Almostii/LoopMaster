import { useEffect, useState } from "react";

import Fader from "./components/Fader";
import Meter from "./components/Meter";
import { useRemoteConsole, type ConnectionStatus } from "./lib/useRemoteConsole";
import type { RemoteState, Send } from "./lib/protocol";

type Tab = "sources" | "channels";

const STATUS_TEXT: Record<ConnectionStatus, string> = {
  connecting: "连接中…",
  connected: "已连接",
  reconnecting: "重连中…",
  disconnected: "未连接",
};

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

/** 一条 send 的推子条：Mute + 电平表 + 推子。 */
function SendStrip({
  send,
  targetName,
  meter,
  onGain,
  onMute,
}: {
  send: Send;
  targetName: string;
  meter: [number, number] | undefined;
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
      <Meter value={meter} />
      <Fader
        label={targetName}
        gainDb={send.gain_db}
        onGainChange={onGain}
        disabled={!send.enabled}
      />
    </div>
  );
}

export default function App() {
  const { state, status, meters, setSendGain, setSendMuted } = useRemoteConsole();
  const [tab, setTab] = useState<Tab>("sources");
  const pairing = usePairingFromUrl();

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
    return (
      <main className="remote-shell">
        <h1>LoopMaster Remote</h1>
        <p>{STATUS_TEXT[status]}</p>
        <p className="remote-hint">
          无法连接宿主，正在自动重连。请确认宿主已开启网络功能、本机与宿主在同一局域网，
          并访问宿主的局域网 IP（不是本机自己的 IP）。
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
        <div className="console-title">LoopMaster Remote</div>
        <div className={`status ${status}`}>
          <span className="status-dot" />
          {STATUS_TEXT[status]}
          <span className="status-revision">rev {state.state_revision}</span>
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
                  <div className="strip-group-title">{source.display_name}</div>
                  {sends.length === 0 ? (
                    <div className="empty-tip">该音源未连接到任何输出通道。</div>
                  ) : (
                    <div className="strip-group-body">
                      {sends.map((send) => (
                        <SendStrip
                          key={send.send_id}
                          send={send}
                          targetName={busName(state, send.output_channel ?? "")}
                          meter={meters[send.send_id]}
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
                <div className="strip-group-title">{channel.display_name}</div>
                {sends.length === 0 ? (
                  <div className="empty-tip">该通道未连接到任何外部输出。</div>
                ) : (
                  <div className="strip-group-body">
                    {sends.map((send) => (
                      <SendStrip
                        key={send.send_id}
                        send={send}
                        targetName={sinkName(state, send.external_output ?? "")}
                        meter={meters[send.send_id]}
                        onGain={(db) => setSendGain(send.send_id, db)}
                        onMute={(muted) => setSendMuted(send.send_id, muted)}
                      />
                    ))}
                  </div>
                )}
              </div>
            );
          })
        )}
      </section>
    </main>
  );
}
