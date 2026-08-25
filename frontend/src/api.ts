import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
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
}

export function applyRouteEdit(request: RouteEditRequest): Promise<void> {
  return invoke("apply_route_edit", { request });
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
