import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

// ---------- DTO（与 src-tauri 返回结构一致） ----------

interface DeviceBrief {
  id: string;
  name: string;
  flow: "capture" | "render";
  compatibility: "capture_ready" | "render_ready" | "unsupported";
  status: "active" | "unavailable" | "unsupported" | "error";
  format_description: string | null;
}

interface ProcessBrief {
  pid: number;
  name: string;
  executable_path: string | null;
}

interface SendBrief {
  id: string;
  source: string | null;
  output_channel: string | null;
  external_output: string | null;
  enabled: boolean;
  muted: boolean;
  gain_db: number;
  channel_map: [number, number][];
}

interface SourceBrief {
  id: string;
  kind: string;
  display_name: string;
  endpoint_id: string | null;
  process_id: number | null;
}

interface ChannelBrief {
  id: string;
  display_name: string;
}

interface ExternalOutputBrief {
  id: string;
  endpoint_id: string;
  display_name: string;
}

interface RouteProfileSnapshot {
  sources: SourceBrief[];
  output_channels: ChannelBrief[];
  external_outputs: ExternalOutputBrief[];
  sends: SendBrief[];
}

interface EngineStateBrief {
  state: string;
  running: boolean;
  failed: boolean;
  last_error: string | null;
}

interface ServiceErrorBrief {
  category: string;
  message: string;
  endpoint_id: string | null;
  hresult: number | null;
  hint: string | null;
}

interface EngineStateEvent {
  state: string;
  running: boolean;
}

interface EngineStatsEvent {
  capture_packets: number;
  captured_frames: number;
  rendered_frames: number;
  fifo_overflows: number;
  fifo_underflows: number;
  discontinuities: number;
  reconnect_attempts: number;
  captured_peak: number;
}

interface DeviceLostEvent {
  endpoint_id: string;
}

// ---------- 状态中文映射 ----------

const STATE_LABEL: Record<string, string> = {
  stopped: "已停止",
  running: "运行中",
  degraded: "降级",
  reconnecting: "重连中",
  failed: "失败",
};

const DEVICE_STATUS_LABEL: Record<string, string> = {
  active: "正常",
  unavailable: "不可用",
  unsupported: "不支持",
  error: "错误",
};

function App() {
  // 设备 / 进程 / 路由 / 引擎状态
  const [captureDevices, setCaptureDevices] = useState<DeviceBrief[]>([]);
  const [renderDevices, setRenderDevices] = useState<DeviceBrief[]>([]);
  const [processes, setProcesses] = useState<ProcessBrief[]>([]);
  const [route, setRoute] = useState<RouteProfileSnapshot>({
    sources: [],
    output_channels: [],
    external_outputs: [],
    sends: [],
  });
  const [engineState, setEngineState] = useState<EngineStateBrief>({
    state: "stopped",
    running: false,
    failed: false,
    last_error: null,
  });
  const [stats, setStats] = useState<EngineStatsEvent | null>(null);

  // 交互状态
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  // 新增 source 的选择
  const [selectedProcessPid, setSelectedProcessPid] = useState<number | null>(null);
  const [selectedRenderDevice, setSelectedRenderDevice] = useState<string | null>(null);
  const nextId = useRef(0);

  const freshId = (prefix: string) =>
    `${prefix}-${Date.now()}-${nextId.current++}`;

  // ---------- 命令调用 ----------

  async function refreshDevices() {
    setLoading(true);
    setError(null);
    try {
      const devices = await invoke<DeviceBrief[]>("list_devices");
      setCaptureDevices(devices.filter((d) => d.flow === "capture"));
      setRenderDevices(devices.filter((d) => d.flow === "render"));
    } catch (e) {
      setError(formatError(e));
    } finally {
      setLoading(false);
    }
  }

  async function refreshProcesses() {
    try {
      const list = await invoke<ProcessBrief[]>("list_audio_processes");
      setProcesses(list);
    } catch (e) {
      setError(formatError(e));
    }
  }

  async function refreshRoute() {
    try {
      setRoute(await invoke<RouteProfileSnapshot>("get_route_snapshot"));
    } catch (e) {
      setError(formatError(e));
    }
  }

  async function refreshEngineState() {
    try {
      setEngineState(await invoke<EngineStateBrief>("get_engine_state"));
    } catch (e) {
      setError(formatError(e));
    }
  }

  async function refreshAll() {
    await Promise.all([refreshDevices(), refreshProcesses(), refreshRoute(), refreshEngineState()]);
  }

  async function callEngine(cmd: () => Promise<unknown>, okMessage: string) {
    setError(null);
    setNotice(null);
    try {
      await cmd();
      setNotice(okMessage);
      await refreshRoute();
      await refreshEngineState();
    } catch (e) {
      const brief = e as ServiceErrorBrief;
      setError(brief.hint ? `${brief.message}；${brief.hint}` : formatError(e));
    }
  }

  async function startEngine() {
    await callEngine(() => invoke("start_engine"), "引擎已启动");
  }

  async function stopEngine() {
    await callEngine(() => invoke("stop_engine"), "引擎已停止");
  }

  async function reconnect() {
    await callEngine(() => invoke("request_reconnect"), "已触发重连");
  }

  async function addSource() {
    if (selectedProcessPid === null) return;
    const process = processes.find((p) => p.pid === selectedProcessPid);
    if (!process) return;
    await callEngine(
      () =>
        invoke("apply_route_edit", {
          request: {
            op: "add_source",
            id: freshId("src"),
            kind: "process_loopback",
            display_name: `${process.name}（PID ${process.pid}）`,
            endpoint_id: null,
            process_id: process.pid,
          },
        }),
      "已添加音源（拓扑变更需重启引擎生效）",
    );
  }

  async function addExternalOutput() {
    if (!selectedRenderDevice) return;
    const device = renderDevices.find((d) => d.id === selectedRenderDevice);
    if (!device) return;
    await callEngine(
      () =>
        invoke("apply_route_edit", {
          request: {
            op: "add_external_output",
            id: freshId("out"),
            endpoint_id: device.id,
            display_name: device.name,
          },
        }),
      "已添加输出目标（拓扑变更需重启引擎生效）",
    );
  }

  async function addSendToOutput(outputChannelId: string) {
    if (route.external_outputs.length === 0) {
      setNotice("请先添加输出目标");
      return;
    }
    const output = route.external_outputs[0];
    await callEngine(
      () =>
        invoke("apply_route_edit", {
          request: {
            op: "add_send_to_output",
            id: freshId("send"),
            output_channel_id: outputChannelId,
            external_output_id: output.id,
          },
        }),
      "已添加连线（拓扑变更需重启引擎生效）",
    );
  }

  function formatError(e: unknown): string {
    const brief = e as ServiceErrorBrief;
    if (brief && brief.message) return brief.message;
    return String(e);
  }

  // ---------- 事件订阅 ----------

  useEffect(() => {
    refreshAll();

    const unState = listen<EngineStateEvent>("engine-state-changed", (e) => {
      const { state, running } = e.payload;
      setEngineState((prev) => ({ ...prev, state, running }));
      setNotice(`引擎状态：${STATE_LABEL[state] ?? state}`);
    });
    const unStats = listen<EngineStatsEvent>("engine-stats-changed", (e) => {
      setStats(e.payload);
    });
    const unLost = listen<DeviceLostEvent>("device-lost", (e) => {
      setNotice(`设备已丢失：${e.payload.endpoint_id}`);
    });
    const unRestored = listen<DeviceLostEvent>("device-restored", (e) => {
      setNotice(`设备已恢复：${e.payload.endpoint_id}`);
    });

    return () => {
      void unState.then((fn) => fn());
      void unStats.then((fn) => fn());
      void unLost.then((fn) => fn());
      void unRestored.then((fn) => fn());
    };
  }, []);

  const stateClass = `badge state-${engineState.state}`;

  return (
    <main className="container">
      <header className="header">
        <h1>LoopMaster 音频路由</h1>
        <span className={stateClass}>
          {STATE_LABEL[engineState.state] ?? engineState.state}
        </span>
      </header>

      {error && <p className="error">错误：{error}</p>}
      {notice && <p className="notice">{notice}</p>}

      <section className="panel">
        <div className="panel-title">
          <h2>引擎控制</h2>
        </div>
        <div className="row controls">
          <button onClick={startEngine} disabled={engineState.running}>
            启动引擎
          </button>
          <button onClick={stopEngine} disabled={!engineState.running}>
            停止引擎
          </button>
          <button
            onClick={reconnect}
            disabled={
              engineState.state !== "degraded" &&
              engineState.state !== "reconnecting" &&
              engineState.state !== "failed"
            }
          >
            手动重连
          </button>
          <button onClick={refreshAll} disabled={loading}>
            {loading ? "刷新中…" : "刷新"}
          </button>
        </div>
        {stats && (
          <p className="stats">
            捕获包 {stats.capture_packets} · 帧 {stats.captured_frames} ·
            溢出 {stats.fifo_overflows} · 下溢 {stats.fifo_underflows} ·
            重连 {stats.reconnect_attempts} 次 · 峰值 {stats.captured_peak.toFixed(3)}
          </p>
        )}
        {engineState.last_error && (
          <p className="stats warn">最近错误：{engineState.last_error}</p>
        )}
      </section>

      <section className="panel">
        <h2>音源（Sources）</h2>
        <div className="row">
          <select
            value={selectedProcessPid ?? ""}
            onChange={(e) =>
              setSelectedProcessPid(e.target.value ? Number(e.target.value) : null)
            }
          >
            <option value="">选择有音频会话的进程…</option>
            {processes.map((p) => (
              <option key={p.pid} value={p.pid}>
                {p.name}（PID {p.pid}）
              </option>
            ))}
          </select>
          <button onClick={addSource} disabled={selectedProcessPid === null}>
            添加音源
          </button>
          <button onClick={refreshProcesses}>刷新进程</button>
        </div>
        {route.sources.length === 0 ? (
          <p className="empty">尚未添加音源。</p>
        ) : (
          <ul className="item-list">
            {route.sources.map((s) => (
              <li key={s.id}>{s.display_name}</li>
            ))}
          </ul>
        )}
      </section>

      <section className="panel">
        <h2>输出目标（External Outputs）</h2>
        <div className="row">
          <select
            value={selectedRenderDevice ?? ""}
            onChange={(e) => setSelectedRenderDevice(e.target.value || null)}
          >
            <option value="">选择输出设备…</option>
            {renderDevices.map((d) => (
              <option key={d.id} value={d.id}>
                {d.name}（{DEVICE_STATUS_LABEL[d.status]}）
              </option>
            ))}
          </select>
          <button onClick={addExternalOutput} disabled={!selectedRenderDevice}>
            添加输出
          </button>
        </div>
        {route.external_outputs.length === 0 ? (
          <p className="empty">尚未添加输出目标。</p>
        ) : (
          <ul className="item-list">
            {route.external_outputs.map((o) => (
              <li key={o.id}>{o.display_name}</li>
            ))}
          </ul>
        )}
      </section>

      <section className="panel">
        <h2>路由连线（Sends）</h2>
        {route.output_channels.length === 0 && (
          <p className="empty">输出通道为空，请先添加音源。</p>
        )}
        {route.output_channels.map((ch) => {
          const send = route.sends.find((s) => s.output_channel === ch.id);
          return (
            <div key={ch.id} className="send-row">
              <span className="send-name">
                {send
                  ? `${send.source ?? "输出通道"} → ${send.external_output ?? "（未连线到输出）"}`
                  : `输出通道 ${ch.display_name} 未连线`}
              </span>
              <button
                onClick={() => addSendToOutput(ch.id)}
                disabled={route.external_outputs.length === 0}
              >
                连线到输出
              </button>
            </div>
          );
        })}
      </section>

      <section className="panel">
        <h2>设备列表</h2>
        <h3>输入设备</h3>
        {captureDevices.length === 0 ? (
          <p className="empty">无输入设备。</p>
        ) : (
          <ul className="item-list">
            {captureDevices.map((d) => (
              <li key={d.id}>
                <span>{d.name}</span>
                <span className="tag">{DEVICE_STATUS_LABEL[d.status]}</span>
              </li>
            ))}
          </ul>
        )}
        <h3>输出设备</h3>
        {renderDevices.length === 0 ? (
          <p className="empty">无输出设备。</p>
        ) : (
          <ul className="item-list">
            {renderDevices.map((d) => (
              <li key={d.id}>
                <span>{d.name}</span>
                <span className="tag">{DEVICE_STATUS_LABEL[d.status]}</span>
                {d.format_description && (
                  <span className="tag subtle">{d.format_description}</span>
                )}
              </li>
            ))}
          </ul>
        )}
      </section>
    </main>
  );
}

export default App;
