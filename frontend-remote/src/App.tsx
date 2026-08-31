import { useState } from "react";

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

  if (!state) {
    return (
      <main className="remote-shell">
        <h1>LoopMaster Remote</h1>
        <p>{STATUS_TEXT[status]}</p>
        <p className="remote-hint">
          无法连接宿主，正在自动重连。请确认宿主已开启网络功能并保持在同一局域网。
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
