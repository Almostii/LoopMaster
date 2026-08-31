import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  AppSettings,
  DeviceBrief,
  EngineStateBrief,
  EngineStatsEvent,
  NetworkNodeBrief,
  NodeIdentityBrief,
  NodeRemovedEvent,
  NodeResolvedEvent,
  ProcessBrief,
  RouteProfileSnapshot,
} from "./types";
import { isTauriRuntime } from "./runtime";

const emptyRoute: RouteProfileSnapshot = {
  sources: [],
  output_channels: [],
  external_outputs: [],
  sends: [],
};

const emptySettings: AppSettings = {
  theme: "light",
  start_on_boot: false,
  launch_hidden: false,
};

const emptyEngineStats: EngineStatsEvent = {
  capture_packets: 0,
  captured_frames: 0,
  rendered_frames: 0,
  render_writes: 0,
  fifo_overflows: 0,
  fifo_underflows: 0,
  discontinuities: 0,
  reconnect_attempts: 0,
  captured_peak: 0,
  send_peaks: {},
};

function unavailableInBrowser(): Error {
  return new Error("当前页面未运行在 LoopMaster 桌面壳中");
}

// ---------- 只读命令 ----------

export function listDevices(): Promise<DeviceBrief[]> {
  if (!isTauriRuntime) return Promise.resolve([]);
  return invoke<DeviceBrief[]>("list_devices");
}

export function listAudioProcesses(): Promise<ProcessBrief[]> {
  if (!isTauriRuntime) return Promise.resolve([]);
  return invoke<ProcessBrief[]>("list_audio_processes");
}

const emptyIdentity: NodeIdentityBrief = {
  node_id: "",
  device_name: "",
  network_enabled: false,
  web_port: 0,
};

/** 返回本机网络身份（node_id/device_name/network_enabled/web_port）。 */
export function getNodeIdentity(): Promise<NodeIdentityBrief> {
  if (!isTauriRuntime) return Promise.resolve(emptyIdentity);
  return invoke<NodeIdentityBrief>("get_node_identity");
}

/** 开启/关闭网络功能，返回更新后的本机身份。 */
export function setNetworkEnabled(enabled: boolean): Promise<NodeIdentityBrief> {
  if (!isTauriRuntime) return Promise.resolve(emptyIdentity);
  return invoke<NodeIdentityBrief>("set_network_enabled", { enabled });
}

/** 返回当前局域网发现的 VBAN 节点列表快照。 */
export function getNetworkNodes(): Promise<NetworkNodeBrief[]> {
  if (!isTauriRuntime) return Promise.resolve([]);
  return invoke<NetworkNodeBrief[]>("get_network_nodes");
}

export interface FirewallCheckResult {
  port_available: boolean;
  /** 两条规则（VBAN UDP + Web TCP）是否都已存在。 */
  rule_exists: boolean;
  /** VBAN（UDP 6980）入站规则是否存在。 */
  vban_rule_exists: boolean;
  /** Web 控制台（TCP）入站规则是否存在。 */
  web_rule_exists: boolean;
  checked: boolean;
  message: string;
}

/** 检测网络功能所需的防火墙放行情况（VBAN UDP 6980 + Web 控制台 TCP）。 */
export function checkNetworkFirewall(): Promise<FirewallCheckResult> {
  if (!isTauriRuntime) {
    return Promise.resolve({
      port_available: true,
      rule_exists: true,
      vban_rule_exists: true,
      web_rule_exists: true,
      checked: false,
      message: "",
    });
  }
  return invoke<FirewallCheckResult>("check_network_firewall");
}

/**
 * 自动放行 VBAN（UDP）与 Web 控制台（TCP）入站防火墙（提权，一次 UAC）。
 *
 * 幂等：规则已存在时不触发 UAC；规则校验失败时 reject，错误信息含可复制的
 * 手动 netsh 命令。
 */
export function enableNetworkFirewall(): Promise<FirewallCheckResult> {
  if (!isTauriRuntime) {
    return Promise.resolve({
      port_available: true,
      rule_exists: true,
      vban_rule_exists: true,
      web_rule_exists: true,
      checked: false,
      message: "",
    });
  }
  return invoke<FirewallCheckResult>("enable_network_firewall");
}

export interface CaTrustStatus {
  /** 本机（当前用户受信任根证书存储）是否已信任该根证书。 */
  installed: boolean;
  /** 状态是否成功检测（Windows 且 PowerShell 可用）。 */
  checked: boolean;
  /** CA 证书文件路径（供 Firefox 等场景手动导入）。 */
  ca_path: string | null;
  message: string;
}

/** 查询本机根证书信任状态（只读）。 */
export function getLocalCaStatus(): Promise<CaTrustStatus> {
  if (!isTauriRuntime) {
    return Promise.resolve({
      installed: false,
      checked: false,
      ca_path: null,
      message: "",
    });
  }
  return invoke<CaTrustStatus>("get_local_ca_status");
}

/**
 * 把本机根证书安装到当前用户的受信任根证书存储（无需管理员）。
 *
 * 安装后 Chrome / Edge 访问 https://<本机IP>:<端口> 不再告警；
 * Firefox 不读 Windows 证书存储，需要手动导入或在 about:config 打开
 * security.enterprise_roots.enabled。
 */
export function installLocalCa(): Promise<CaTrustStatus> {
  if (!isTauriRuntime) {
    return Promise.resolve({
      installed: false,
      checked: false,
      ca_path: null,
      message: "",
    });
  }
  return invoke<CaTrustStatus>("install_local_ca");
}

/** 从当前用户的受信任根证书存储中移除 LoopMaster 根证书。 */
export function removeLocalCa(): Promise<CaTrustStatus> {
  if (!isTauriRuntime) {
    return Promise.resolve({
      installed: false,
      checked: false,
      ca_path: null,
      message: "",
    });
  }
  return invoke<CaTrustStatus>("remove_local_ca");
}

/** 手动添加一个 VBAN 网络节点（mDNS 不可用时的回退）。 */
export function addManualVbanNode(params: {
  name: string;
  address: string;
  port: number;
  stream_name: string;
  sample_rate?: number;
  channels?: number;
}): Promise<NetworkNodeBrief> {
  if (!isTauriRuntime) {
    return Promise.reject(unavailableInBrowser());
  }
  return invoke<NetworkNodeBrief>("add_manual_vban_node", {
    name: params.name,
    address: params.address,
    port: params.port,
    streamName: params.stream_name,
    sampleRate: params.sample_rate,
    channels: params.channels,
  });
}

/** 返回进程可执行文件图标的 PNG data URI；无图标或平台不支持时返回 null。 */
export function processIconDataUri(executablePath: string): Promise<string | null> {
  if (!isTauriRuntime) return Promise.resolve(null);
  return invoke<string | null>("process_icon_data_uri", {
    executablePath,
  });
}

export function getRouteSnapshot(): Promise<RouteProfileSnapshot> {
  if (!isTauriRuntime) return Promise.resolve(emptyRoute);
  return invoke<RouteProfileSnapshot>("get_route_snapshot");
}

export function getEngineState(): Promise<EngineStateBrief> {
  if (!isTauriRuntime) {
    return Promise.resolve({ state: "stopped", running: false, failed: false, last_error: null });
  }
  return invoke<EngineStateBrief>("get_engine_state");
}

/** 返回当前应用版本号（如 "0.1.0"）。 */
export function getAppVersion(): Promise<string> {
  if (!isTauriRuntime) return Promise.resolve("1.0.0");
  return invoke<string>("get_app_version");
}

/** 返回当前引擎统计快照；引擎尚未创建或页面直开时返回零值。 */
export function getEngineStats(): Promise<EngineStatsEvent> {
  if (!isTauriRuntime) return Promise.resolve(emptyEngineStats);
  return invoke<EngineStatsEvent>("get_engine_stats");
}

// ---------- 写命令 ----------

export function startEngine(): Promise<void> {
  if (!isTauriRuntime) return Promise.reject(unavailableInBrowser());
  return invoke("start_engine");
}

export function stopEngine(): Promise<void> {
  if (!isTauriRuntime) return Promise.reject(unavailableInBrowser());
  return invoke("stop_engine");
}

export function requestReconnect(): Promise<void> {
  if (!isTauriRuntime) return Promise.reject(unavailableInBrowser());
  return invoke("request_reconnect");
}

// 路由编辑请求体（部分字段可选，按 op 决定使用哪些）
export interface RouteEditRequest {
  op: string;
  id?: string;
  kind?: string;
  display_name?: string;
  endpoint_id?: string | null;
  process_id?: number | null;
  executable_path?: string | null;
  /** VBAN 源/目标的流名（kind === "vban" 时使用）。 */
  stream_name?: string | null;
  /** VBAN 目标的远端地址（ip:port）。 */
  remote_addr?: string | null;
  source_id?: string;
  output_channel_id?: string;
  external_output_id?: string;
  send_id?: string;
  enabled?: boolean;
  muted?: boolean;
  gain_db?: number;
  channel_map?: [number, number][];
  old_source_id?: string;
  new_source_id?: string;
}

export function applyRouteEdit(request: RouteEditRequest): Promise<void> {
  if (!isTauriRuntime) return Promise.reject(unavailableInBrowser());
  return invoke("apply_route_edit", { request });
}

// ---------- 配置持久化 ----------

/** 保存当前路由配置到本地配置文件（原子写入）。 */
export function saveConfig(): Promise<void> {
  if (!isTauriRuntime) return Promise.resolve();
  return invoke("save_config");
}

/**
 * 从本地配置文件加载路由，替换当前编辑草稿。
 * 返回 true 表示已加载配置，false 表示文件不存在（需建立默认拓扑）。
 */
export function loadConfig(): Promise<boolean> {
  if (!isTauriRuntime) return Promise.resolve(false);
  return invoke<boolean>("load_config");
}

// ---------- 应用设置 ----------

/** 读取当前应用设置（主题/开机自启/启动隐藏）。 */
export function getSettings(): Promise<AppSettings> {
  if (!isTauriRuntime) return Promise.resolve(emptySettings);
  return invoke<AppSettings>("get_settings");
}

/**
 * 更新应用设置并持久化。未提供的字段保持不变。
 * @returns 更新后的完整设置。
 */
export function updateSettings(patch: {
  theme?: string;
  start_on_boot?: boolean;
  launch_hidden?: boolean;
}): Promise<AppSettings> {
  if (!isTauriRuntime) return Promise.resolve(emptySettings);
  return invoke<AppSettings>("update_settings", {
    theme: patch.theme,
    startOnBoot: patch.start_on_boot,
    launchHidden: patch.launch_hidden,
  });
}

// ---------- 事件订阅 ----------

export function onEngineStateChanged(
  handler: (payload: { state: string; running: boolean }) => void,
): Promise<UnlistenFn> {
  if (!isTauriRuntime) return Promise.resolve(() => {});
  return listen<{ state: string; running: boolean }>(
    "engine-state-changed",
    (e) => handler(e.payload),
  );
}

export function onEngineStatsChanged(
  handler: (payload: EngineStatsEvent) => void,
): Promise<UnlistenFn> {
  if (!isTauriRuntime) return Promise.resolve(() => {});
  return listen<EngineStatsEvent>("engine-stats-changed", (e) =>
    handler(e.payload),
  );
}

export function onDeviceLost(handler: (endpointId: string) => void): Promise<UnlistenFn> {
  if (!isTauriRuntime) return Promise.resolve(() => {});
  return listen<{ endpoint_id: string }>("device-lost", (e) =>
    handler(e.payload.endpoint_id),
  );
}

export function onDeviceRestored(
  handler: (endpointId: string) => void,
): Promise<UnlistenFn> {
  if (!isTauriRuntime) return Promise.resolve(() => {});
  return listen<{ endpoint_id: string }>("device-restored", (e) =>
    handler(e.payload.endpoint_id),
  );
}

export function onNodeResolved(
  handler: (node: NetworkNodeBrief) => void,
): Promise<UnlistenFn> {
  if (!isTauriRuntime) return Promise.resolve(() => {});
  return listen<NodeResolvedEvent>("node-resolved", (e) => handler(e.payload.node));
}

export function onNodeRemoved(
  handler: (nodeId: string) => void,
): Promise<UnlistenFn> {
  if (!isTauriRuntime) return Promise.resolve(() => {});
  return listen<NodeRemovedEvent>("node-removed", (e) =>
    handler(e.payload.node_id),
  );
}

/** 进程声源自动重连事件：ProcessLoopback 声源已按可执行路径重新绑定到新 PID。 */
export function onProcessRestored(
  handler: (sourceId: string, processId: number) => void,
): Promise<UnlistenFn> {
  if (!isTauriRuntime) return Promise.resolve(() => {});
  return listen<{ source_id: string; process_id: number }>("process-restored", (e) =>
    handler(e.payload.source_id, e.payload.process_id),
  );
}
