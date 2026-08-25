import { useMemo, useRef, useState } from "react";
import "./App.css";
import ChannelCard from "./components/ChannelCard";
import Column from "./components/Column";
import MonitorCard from "./components/MonitorCard";
import PickerMenu, { type PickerOption } from "./components/PickerMenu";
import SourceCard from "./components/SourceCard";
import TitleBar from "./components/TitleBar";
import WireLayer from "./components/WireLayer";
import { computeWires, isExternalEnabled, isSourceEnabled } from "./lib";
import { useLoopMaster } from "./useLoopMaster";

function App() {
  const {
    captureDevices,
    renderDevices,
    processes,
    route,
    engineState,
    notice,
    meterLevel,
    setNotice,
    refreshProcesses,
    refreshDevices,
    doStartEngine,
    doStopEngine,
    addSourceFromProcess,
    addSourceFromDevice,
    addOutputChannel,
    addExternalOutput,
    removeSource,
    removeOutputChannel,
    removeExternalOutput,
    addSend,
    addSendToOutput,
    removeSend,
    setSendEnabled,
    setSendMuted,
    setSendGain,
  } = useLoopMaster();

  const svgRef = useRef<SVGSVGElement | null>(null);
  const [selectedWireId, setSelectedWireId] = useState<string | null>(null);

  const wires = useMemo(() => computeWires(route), [route]);

  // 所有音频来源按用途分组到同一个 Picker：进程、麦克风、设备回环、虚拟设备。
  // 每个 capture 设备只按其后端分类（category）出现在其中一个组里，避免重复。
  // value 使用前缀区分类型，handleSelectSource 解析后调用对应添加函数。
  const sourceGroups = [
    {
      title: "进程 (Process Loopback)",
      options: processes.map((p) => ({
        value: `proc:${p.pid}`,
        label: p.name,
        hint: `PID ${p.pid}`,
      })),
    },
    {
      title: "麦克风 / 输入设备 (Microphone)",
      options: captureDevices
        .filter((d) => d.category === "input_mic")
        .map((d) => ({
          value: `mic:${d.id}`,
          label: d.name,
          hint: "麦克风",
        })),
    },
    {
      title: "设备回环 (Device Loopback)",
      options: captureDevices
        .filter((d) => d.category === "input_loopback")
        .map((d) => ({
          value: `loop:${d.id}`,
          label: d.name,
          hint: "设备回环",
        })),
    },
    {
      title: "虚拟设备 (Virtual Device)",
      options: captureDevices
        .filter((d) => d.category === "input_virtual")
        .map((d) => ({
          value: `loop:${d.id}`,
          label: d.name,
          hint: "虚拟设备",
        })),
    },
  ];

  const externalOptions: PickerOption[] = renderDevices.map((d) => ({
    value: d.id,
    label: d.name,
    hint: d.status,
  }));

  const deviceById = useMemo(() => {
    const m = new Map<string, (typeof renderDevices)[number]>();
    for (const d of renderDevices) m.set(d.id, d);
    return m;
  }, [renderDevices]);

  // ---------- 连线交互 ----------

  function handleToggleEngine(running: boolean) {
    if (running) void doStartEngine();
    else void doStopEngine();
  }

  /** 由 socketId 推导节点信息 */
  function resolveSocket(socketId: string): {
    kind: "source" | "output" | "external";
    id: string;
    side: "in" | "out";
  } | null {
    for (const s of route.sources) if (s.id === socketId) return { kind: "source", id: s.id, side: "out" };
    for (const ch of route.output_channels) {
      if (socketId === `${ch.id}-in`) return { kind: "output", id: ch.id, side: "in" };
      if (socketId === `${ch.id}-out`) return { kind: "output", id: ch.id, side: "out" };
    }
    for (const ext of route.external_outputs) {
      if (socketId === `${ext.id}-in`) return { kind: "external", id: ext.id, side: "in" };
    }
    return null;
  }

  function handleConnect(fromSocketId: string, toSocketId: string) {
    const from = resolveSocket(fromSocketId);
    const to = resolveSocket(toSocketId);
    if (!from || !to) return;
    // 音源右插孔 → 输出通道左插孔
    if (from.kind === "source" && to.kind === "output") {
      void addSend(from.id, to.id);
      return;
    }
    // 输出通道右插孔 → 外部输出左插孔
    if (from.kind === "output" && to.kind === "external") {
      void addSendToOutput(from.id, to.id);
      return;
    }
  }

  /** 点击连线删除 */
  function handleWireClick(wireId: string) {
    setSelectedWireId(wireId);
  }

  function handleDeleteWire() {
    if (!selectedWireId) return;
    // wire id 格式：<sendId>-s2c / <sendId>-c2e / <sendId>-s2e
    const sendId = selectedWireId.replace(/-(s2c|c2e|s2e)$/, "");
    void removeSend(sendId);
    setSelectedWireId(null);
  }

  /** 切换音源开关：对该音源所有 send 统一 enabled */
  async function handleToggleSource(sourceId: string, on: boolean) {
    const sends = route.sends.filter((s) => s.source === sourceId);
    if (sends.length === 0) {
      setNotice({ text: "请先拖动连线到输出通道，再开启音源", kind: "info" });
      return;
    }
    await Promise.all(sends.map((s) => setSendEnabled(s.id, on)));
  }

  /** 切换外部输出开关 */
  async function handleToggleExternal(externalId: string, on: boolean) {
    const sends = route.sends.filter((s) => s.external_output === externalId);
    if (sends.length === 0) {
      setNotice({ text: "请先拖动连线到该外部输出，再开启", kind: "info" });
      return;
    }
    await Promise.all(sends.map((s) => setSendEnabled(s.id, on)));
  }

  function handleSelectSource(value: string) {
    const [kind, id] = value.split(":", 2);
    if (kind === "proc") {
      const pid = Number(id);
      const process = processes.find((p) => p.pid === pid);
      if (process) void addSourceFromProcess(process);
    } else if (kind === "mic") {
      const device = captureDevices.find((d) => d.id === id);
      if (device) void addSourceFromDevice(device, "device_capture");
    } else if (kind === "loop") {
      const device = captureDevices.find((d) => d.id === id);
      if (device) void addSourceFromDevice(device, "device_loopback");
    }
  }

  function handleSelectExternal(value: string) {
    const device = renderDevices.find((d) => d.id === value);
    if (device) void addExternalOutput(device);
  }

  return (
    <div className="app-container mode-stereo">
      <TitleBar engineState={engineState} onToggleEngine={handleToggleEngine} />

      {notice && (
        <div className={`toast-msg show toast-${notice.kind}`} onClick={() => setNotice(null)}>
          {notice.text}
        </div>
      )}

      <div className="router-canvas-wrap">
        <div className="topology-viewport" id="topology-viewport">
          <WireLayer
            svgRef={svgRef}
            wires={wires}
            onConnect={handleConnect}
            onWireClick={handleWireClick}
            selectedWireId={selectedWireId}
          />

          <div className="topology-grid">
            {/* 音源列 */}
            <Column
              title="Sources 音频来源"
              subtitle={`${route.sources.length} 个音源`}
              addTitle="添加音频来源"
              onAdd={undefined}
            >
              <div className="col-add-picker">
                <PickerMenu
                  title="音频来源 (Audio Sources)"
                  trigger={
                    <button className="btn-add-node-wide">
                      <span>＋ 添加音频来源</span>
                    </button>
                  }
                  groups={sourceGroups}
                  onSelect={handleSelectSource}
                  onOpen={() => {
                    void refreshProcesses();
                    void refreshDevices();
                  }}
                />
              </div>
              {route.sources.length === 0 ? (
                <div className="empty-card">尚未添加音源。点击上方按钮选择进程、麦克风或设备回环来源。</div>
              ) : (
                route.sources.map((s) => (
                  <SourceCard
                    key={s.id}
                    source={s}
                    route={route}
                    meterLevel={meterLevel}
                    meterHint="全局捕获峰值"
                    isOn={isSourceEnabled(route, s.id)}
                    onToggle={handleToggleSource}
                    onRemove={(id) => void removeSource(id)}
                    onSetGain={(sendId, g) => void setSendGain(sendId, g)}
                    onSetMuted={(sendId, m) => void setSendMuted(sendId, m)}
                  />
                ))
              )}
            </Column>

            {/* 输出通道列 */}
            <Column
              title="Output Channels 输出通道"
              subtitle={`${route.output_channels.length} 个通道`}
              addTitle="添加输出通道"
              onAdd={() => void addOutputChannel()}
            >
              {route.output_channels.length === 0 ? (
                <div className="empty-card">尚未添加输出通道。点击上方 + 按钮添加。</div>
              ) : (
                route.output_channels.map((ch) => (
                  <ChannelCard
                    key={ch.id}
                    channel={ch}
                    meterLevel={meterLevel}
                    meterHint="全局捕获峰值"
                    onRemove={(id) => void removeOutputChannel(id)}
                  />
                ))
              )}
            </Column>

            {/* 外部输出列 */}
            <Column
              title="External Outputs 外部输出"
              subtitle={`${route.external_outputs.length} 个外部输出`}
              addTitle="添加外部输出"
              onAdd={undefined}
            >
              <div className="col-add-picker">
                <PickerMenu
                  title="物理输出设备 (External Outputs)"
                  trigger={
                    <button className="btn-add-node-wide">
                      <span>＋ 添加外部输出</span>
                    </button>
                  }
                  options={externalOptions}
                  onSelect={handleSelectExternal}
                  onOpen={() => void refreshDevices()}
                />
              </div>
              {route.external_outputs.length === 0 ? (
                <div className="empty-card">尚未添加外部输出。点击上方按钮选择设备。</div>
              ) : (
                route.external_outputs.map((ext) => (
                  <MonitorCard
                    key={ext.id}
                    external={ext}
                    device={deviceById.get(ext.endpoint_id)}
                    meterLevel={meterLevel}
                    meterHint="全局捕获峰值"
                    isOn={isExternalEnabled(route, ext.id)}
                    onToggle={handleToggleExternal}
                    onRemove={(id) => void removeExternalOutput(id)}
                  />
                ))
              )}
            </Column>
          </div>
        </div>

        <footer className="canvas-footer">
          <div className="footer-left">
            <button
              className="btn-secondary"
              disabled={!selectedWireId}
              onClick={handleDeleteWire}
              title="删除选中的连线"
            >
              <svg
                width="14"
                height="14"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
              >
                <polyline points="3 6 5 6 21 6" />
                <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
              </svg>
              <span>删除连线 (Delete)</span>
            </button>
          </div>
          <div className="footer-right">
            <span className="footer-hint">拖动插孔连接，点击连线可选中删除</span>
          </div>
        </footer>
      </div>
    </div>
  );
}

export default App;
