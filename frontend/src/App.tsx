import { useEffect, useMemo, useRef, useState } from "react";
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
import { listAudioProcesses, processIconDataUri } from "./api";

// 设备分组通用图标（进程使用真实应用图标）
function MicIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <path d="M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z" />
      <path d="M19 10v2a7 7 0 0 1-14 0v-2" />
      <line x1="12" y1="19" x2="12" y2="22" />
      <line x1="8" y1="22" x2="16" y2="22" />
    </svg>
  );
}
function LoopbackIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <path d="M21 12a9 9 0 0 0-9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" />
      <path d="M3 3v5h5" />
      <path d="M3 12a9 9 0 0 0 9 9 9.75 9.75 0 0 0 6.74-2.74L21 16" />
      <path d="M16 21h5v-5" />
    </svg>
  );
}
function VirtualIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <rect x="2" y="3" width="20" height="14" rx="2" ry="2" />
      <line x1="8" y1="21" x2="16" y2="21" />
      <line x1="12" y1="17" x2="12" y2="21" />
    </svg>
  );
}

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
  // 进程 PID -> 图标 data URI 缓存，打开来源 Picker 时按需加载。
  const [procIconMap, setProcIconMap] = useState<Record<number, string | null>>({});

  // 应用启动/路由变化时，为已存在的进程来源补齐图标。
  useEffect(() => {
    const hasProcessSource = route.sources.some((s) => s.process_id != null);
    if (hasProcessSource) {
      void loadProcessIcons();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [route.sources.map((s) => s.id).join(",")]);

  const wires = useMemo(() => computeWires(route), [route]);

  // 所有音频来源按用途分组到同一个 Picker：进程、麦克风、设备回环、虚拟设备。
  // 每个 capture 设备只按其后端分类（category）出现在其中一个组里，避免重复。
  // value 使用前缀区分类型，handleSelectSource 解析后调用对应添加函数。
  const sourceGroups = [
    {
      title: "进程 (Process Loopback)",
      options: processes.map((p) => ({
        value: `proc:${p.pid}`,
        label: p.name.replace(/\.exe$/i, ""),
        icon: procIconMap[p.pid] ? (
          <img className="dropdown-item-icon-img" src={procIconMap[p.pid]!} alt="" />
        ) : undefined,
      })),
    },
    {
      title: "麦克风 / 输入设备 (Microphone)",
      options: captureDevices
        .filter((d) => d.category === "input_mic")
        .map((d) => ({
          value: `mic:${d.id}`,
          label: d.name,
          icon: <MicIcon />,
        })),
    },
    {
      title: "设备回环 (Device Loopback)",
      options: captureDevices
        .filter((d) => d.category === "input_loopback")
        .map((d) => ({
          value: `loop:${d.id}`,
          label: d.name,
          icon: <LoopbackIcon />,
        })),
    },
    {
      title: "虚拟设备 (Virtual Device)",
      options: captureDevices
        .filter((d) => d.category === "input_virtual")
        .map((d) => ({
          value: `loop:${d.id}`,
          label: d.name,
          icon: <VirtualIcon />,
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

  /** 为进程并行加载应用图标（data URI），供 Picker 和 SourceCard 使用。 */
  async function loadProcessIcons() {
    try {
      const procs = await listAudioProcesses();
      const entries = await Promise.all(
        procs
          .filter((p) => p.executable_path)
          .map(async (p) => {
            const uri = await processIconDataUri(p.executable_path!);
            return [p.pid, uri] as const;
          }),
      );
      setProcIconMap(Object.fromEntries(entries));
    } catch {
      /* 图标加载失败不影响主流程 */
    }
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
              addNode={
                <PickerMenu
                  title="音频来源 (Audio Sources)"
                  trigger={
                    <button className="btn-add-node" title="添加音频来源">
                      +
                    </button>
                  }
                  groups={sourceGroups}
                  onSelect={handleSelectSource}
                  onOpen={() => {
                    void refreshProcesses();
                    void refreshDevices();
                    void loadProcessIcons();
                  }}
                />
              }
            >
              {route.sources.length === 0 ? (
                <div className="empty-card">点击上方 + 添加音源。</div>
              ) (
                route.sources.map((s) => (
                  <SourceCard
                    key={s.id}
                    source={s}
                    route={route}
                    meterLevel={meterLevel}
                    icon={s.process_id != null ? procIconMap[s.process_id] : undefined}
                    isOn={isSourceEnabled(route, s.id)}
                    onToggle={handleToggleSource}
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
              addNode={
                <PickerMenu
                  title="物理输出设备 (External Outputs)"
                  trigger={
                    <button className="btn-add-node" title="添加外部输出">
                      +
                    </button>
                  }
                  options={externalOptions}
                  onSelect={handleSelectExternal}
                  onOpen={() => void refreshDevices()}
                />
              }
            >
              {route.external_outputs.length === 0 ? (
                <div className="empty-card">点击上方 + 添加外部输出。</div>
              ) : (
                route.external_outputs.map((ext) => (
                  <MonitorCard
                    key={ext.id}
                    external={ext}
                    device={deviceById.get(ext.endpoint_id)}
                    meterLevel={meterLevel}
                    isOn={isExternalEnabled(route, ext.id)}
                    onToggle={handleToggleExternal}
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
