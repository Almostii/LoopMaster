import { useEffect, useMemo, useRef, useState } from "react";
import "./App.css";
import ChannelCard from "./components/ChannelCard";
import Column from "./components/Column";
import MonitorCard from "./components/MonitorCard";
import PickerMenu, { type PickerOption } from "./components/PickerMenu";
import SourceCard from "./components/SourceCard";
import TitleBar from "./components/TitleBar";
import Sidebar, { type SidebarItem } from "./components/Sidebar";
import SettingsView from "./components/SettingsView";
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
    nodeMeter,
    sourceSendIds,
    externalSendIds,
    channelSendIds,
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
    renameSource,
    renameOutputChannel,
    renameExternalOutput,
  } = useLoopMaster();

  const svgRef = useRef<SVGSVGElement | null>(null);
  const [selectedWireId, setSelectedWireId] = useState<string | null>(null);
  const [selectedCard, setSelectedCard] = useState<
    { type: "source" | "channel" | "external"; id: string } | null
  >(null);
  // 进程 PID -> 图标 data URI 缓存，打开来源 Picker 时按需加载。
  const [procIconMap, setProcIconMap] = useState<Record<number, string | null>>({});
  // 侧边栏状态 (默认展开, 向窗口内弹出, 不遮挡 sources)
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  const [activeView, setActiveView] = useState<string>("router");

  // 应用启动/路由变化时，为已存在的进程来源补齐图标。
  useEffect(() => {
    const hasProcessSource = route.sources.some((s) => s.process_id != null);
    if (hasProcessSource) {
      void loadProcessIcons();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [route.sources.map((s) => s.id).join(",")]);

  const wires = useMemo(() => computeWires(route), [route]);

  // 侧边栏菜单(占位骨架, 待确认实际页面后再调整顺序/标签)
  const sidebarTopItems: SidebarItem[] = useMemo(
    () => [
      {
        key: "home",
        label: "首页",
        icon: (
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
            <path d="M3 11l9-8 9 8" />
            <path d="M5 10v10a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1V10" />
          </svg>
        ),
      },
      {
        key: "router",
        label: "音频路由",
        icon: (
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
            <circle cx="6" cy="6" r="2.5" />
            <circle cx="18" cy="6" r="2.5" />
            <circle cx="12" cy="18" r="2.5" />
            <path d="M8.2 7.5l3 9" />
            <path d="M15.8 7.5l-3 9" />
            <path d="M8 6h8" />
          </svg>
        ),
      },
      {
        key: "devices",
        label: "设备",
        icon: (
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
            <rect x="3" y="5" width="18" height="12" rx="2" />
            <line x1="8" y1="21" x2="16" y2="21" />
            <line x1="12" y1="17" x2="12" y2="21" />
          </svg>
        ),
      },
      {
        key: "logs",
        label: "日志",
        icon: (
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
            <path d="M4 4h16v16H4z" />
            <line x1="8" y1="9" x2="16" y2="9" />
            <line x1="8" y1="13" x2="16" y2="13" />
            <line x1="8" y1="17" x2="12" y2="17" />
          </svg>
        ),
      },
    ],
    [],
  );
  const sidebarBottomItems: SidebarItem[] = useMemo(
    () => [
      {
        key: "settings",
        label: "设置",
        icon: (
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
            <circle cx="12" cy="12" r="3" />
            <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09a1.65 1.65 0 0 0-1-1.51 1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09a1.65 1.65 0 0 0 1.51-1 1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33h.01a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51h.01a1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82v.01a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
          </svg>
        ),
      },
    ],
    [],
  );

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

  /** 点击连线选中 */
  function handleWireClick(wireId: string) {
    setSelectedWireId(wireId);
    setSelectedCard(null);
  }

  function handleDeleteWire() {
    if (!selectedWireId) return;
    // wire id 格式：<sendId>-s2c / <sendId>-c2e / <sendId>-s2e
    const sendId = selectedWireId.replace(/-(s2c|c2e|s2e)$/, "");
    void removeSend(sendId);
    setSelectedWireId(null);
  }

  function handleDeleteSelectedCard() {
    if (!selectedCard) return;
    if (selectedCard.type === "source") {
      void removeSource(selectedCard.id);
    } else if (selectedCard.type === "channel") {
      void removeOutputChannel(selectedCard.id);
    } else {
      void removeExternalOutput(selectedCard.id);
    }
    setSelectedCard(null);
  }

  function handleCanvasClick() {
    setSelectedWireId(null);
    setSelectedCard(null);
  }

  /** 键盘 Delete 删除当前选中项 */
  useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      if (e.key === "Delete" || e.key === "Backspace") {
        if (selectedCard) {
          handleDeleteSelectedCard();
        } else if (selectedWireId) {
          handleDeleteWire();
        }
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [selectedCard, selectedWireId]);

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
      <TitleBar
        engineState={engineState}
        onToggleEngine={handleToggleEngine}
        onToggleSidebar={() => setSidebarCollapsed((v) => !v)}
        sidebarCollapsed={sidebarCollapsed}
      />

      {notice && (
        <div className={`toast-msg show toast-${notice.kind}`} onClick={() => setNotice(null)}>
          {notice.text}
        </div>
      )}

      <div className="app-main">
        <Sidebar
          collapsed={sidebarCollapsed}
          activeKey={activeView}
          onSelect={setActiveView}
          topItems={sidebarTopItems}
          bottomItems={sidebarBottomItems}
        />

        {activeView === "settings" ? (
          <SettingsView />
        ) : activeView === "router" ? (
          <div className="router-canvas-wrap">
        <div className="topology-viewport" id="topology-viewport" onClick={handleCanvasClick}>
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
              title="Sources"
              subtitle={`${route.sources.length} ${route.sources.length <= 1 ? "source" : "sources"}`}
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
              ) : (
                route.sources.map((s) => {
                  const [ml, mr] = nodeMeter(sourceSendIds(s.id));
                  return (
                  <SourceCard
                    key={s.id}
                    source={s}
                    route={route}
                    meterL={ml}
                    meterR={mr}
                    icon={s.process_id != null ? procIconMap[s.process_id] : undefined}
                    isOn={isSourceEnabled(route, s.id)}
                    isSelected={selectedCard?.type === "source" && selectedCard.id === s.id}
                    onToggle={handleToggleSource}
                    onSetGain={(sendId, g) => void setSendGain(sendId, g)}
                    onSetMuted={(sendId, m) => void setSendMuted(sendId, m)}
                    onRename={(id, name) => void renameSource(id, name)}
                    onSelect={() => setSelectedCard({ type: "source", id: s.id })}
                  />
                  );
                })
              )}
            </Column>

            {/* 输出通道列 */}
            <Column
              title="Output Channels"
              subtitle={`${route.output_channels.length} ${route.output_channels.length <= 1 ? "channel" : "channels"}`}
              addTitle="添加输出通道"
              onAdd={() => void addOutputChannel()}
            >
              {route.output_channels.length === 0 ? (
                <div className="empty-card">尚未添加输出通道。点击上方 + 按钮添加。</div>
              ) : (
                route.output_channels.map((ch) => {
                  const [ml, mr] = nodeMeter(channelSendIds(ch.id));
                  return (
                  <ChannelCard
                    key={ch.id}
                    channel={ch}
                    meterL={ml}
                    meterR={mr}
                    isSelected={selectedCard?.type === "channel" && selectedCard.id === ch.id}
                    onRemove={(id) => void removeOutputChannel(id)}
                    onRename={(id, name) => void renameOutputChannel(id, name)}
                    onSelect={() => setSelectedCard({ type: "channel", id: ch.id })}
                  />
                  );
                })
              )}
            </Column>

            {/* 外部输出列 */}
            <Column
              title="Monitors"
              subtitle={`${route.external_outputs.length} ${route.external_outputs.length <= 1 ? "device" : "devices"}`}
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
                route.external_outputs.map((ext) => {
                  const [ml, mr] = nodeMeter(externalSendIds(ext.id));
                  return (
                  <MonitorCard
                    key={ext.id}
                    external={ext}
                    device={deviceById.get(ext.endpoint_id)}
                    route={route}
                    meterL={ml}
                    meterR={mr}
                    isOn={isExternalEnabled(route, ext.id)}
                    isSelected={selectedCard?.type === "external" && selectedCard.id === ext.id}
                    onToggle={handleToggleExternal}
                    onSetGain={(sendId, g) => void setSendGain(sendId, g)}
                    onRename={(id, name) => void renameExternalOutput(id, name)}
                    onSelect={() => setSelectedCard({ type: "external", id: ext.id })}
                  />
                  );
                })
              )}
            </Column>
          </div>
        </div>

        <footer className="canvas-footer">
          <div className="footer-left">
            <button
              className="btn-secondary"
              disabled={!selectedWireId && !selectedCard}
              onClick={() => {
                if (selectedCard) handleDeleteSelectedCard();
                else if (selectedWireId) handleDeleteWire();
              }}
              title="删除选中的项目"
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
              <span>删除 (Delete)</span>
            </button>
          </div>
          <div className="footer-right">
            <span className="footer-hint">点击卡片或连线选中，按 Delete 删除</span>
          </div>
        </footer>
      </div>
        ) : (
          <div className="router-canvas-wrap placeholder-view">
            <div className="placeholder-view-inner">
              <div className="placeholder-view-title">
                {sidebarTopItems.find((i) => i.key === activeView)?.label
                  ?? "未知页面"}
              </div>
              <div className="placeholder-view-hint">
                此页面尚未实现，待确认功能后再补充内容。
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

export default App;
