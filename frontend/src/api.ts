import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  AppSettings,
  DeviceBrief,
  EngineStateBrief,
  EngineStatsEvent,
  ProcessBrief,
  RouteProfileSnapshot,
} from "./types";

// ---------- 只读命令 ----------

export function listDevices(): Promise<DeviceBrief[]> {
  return invoke<DeviceBrief[]>("list_devices");
}

export function listAudioProcesses(): Promise<ProcessBrief[]> {
  return invoke<ProcessBrief[]>("list_audio_processes");
}

/** 返回进程可执行文件图标的 PNG data URI；无图标或平台不支持时返回 null。 */
export function processIconDataUri(executablePath: string): Promise<string | null> {
  return invoke<string | null>("process_icon_data_uri", {
    executablePath,
  });
}

export function getRouteSnapshot(): Promise<RouteProfileSnapshot> {
  return invoke<RouteProfileSnapshot>("get_route_snapshot");
}

export function getEngineState(): Promise<EngineStateBrief> {
  return invoke<EngineStateBrief>("get_engine_state");
}

// ---------- 写命令 ----------

export function startEngine(): Promise<void> {
  return invoke("start_engine");
}

export function stopEngine(): Promise<void> {
  return invoke("stop_engine");
}

export function requestReconnect(): Promise<void> {
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
  source_id?: string;
  output_channel_id?: string;
  external_output_id?: string;
  send_id?: string;
  enabled?: boolean;
  muted?: boolean;
  gain_db?: number;
  channel_map?: [number, number][];
}

export function applyRouteEdit(request: RouteEditRequest): Promise<void> {
  return invoke("apply_route_edit", { request });
}

// ---------- 配置持久化 ----------

/** 保存当前路由配置到本地配置文件（原子写入）。 */
export function saveConfig(): Promise<void> {
  return invoke("save_config");
}

/**
 * 从本地配置文件加载路由，替换当前编辑草稿。
 * 返回 true 表示已加载配置，false 表示文件不存在（需建立默认拓扑）。
 */
export function loadConfig(): Promise<boolean> {
  return invoke<boolean>("load_config");
}

// ---------- 应用设置 ----------

/** 读取当前应用设置（主题/开机自启/启动隐藏）。 */
export function getSettings(): Promise<AppSettings> {
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
  return listen<{ state: string; running: boolean }>(
    "engine-state-changed",
    (e) => handler(e.payload),
  );
}

export function onEngineStatsChanged(
  handler: (payload: EngineStatsEvent) => void,
): Promise<UnlistenFn> {
  return listen<EngineStatsEvent>("engine-stats-changed", (e) =>
    handler(e.payload),
  );
}

export function onDeviceLost(handler: (endpointId: string) => void): Promise<UnlistenFn> {
  return listen<{ endpoint_id: string }>("device-lost", (e) =>
    handler(e.payload.endpoint_id),
  );
}

export function onDeviceRestored(
  handler: (endpointId: string) => void,
): Promise<UnlistenFn> {
  return listen<{ endpoint_id: string }>("device-restored", (e) =>
    handler(e.payload.endpoint_id),
  );
}
