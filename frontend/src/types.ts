// 与 src-tauri 返回结构一致的数据类型（DTO）

export interface DeviceBrief {
  id: string;
  name: string;
  flow: "capture" | "render";
  category: "input_mic" | "input_loopback" | "input_virtual" | "output";
  compatibility: "capture_ready" | "render_ready" | "unsupported";
  status: "active" | "unavailable" | "unsupported" | "error";
  format_description: string | null;
}

export interface ProcessBrief {
  pid: number;
  name: string;
  executable_path: string | null;
}

export interface SourceBrief {
  id: string;
  kind: string;
  display_name: string;
  endpoint_id: string | null;
  process_id: number | null;
}

export interface ChannelBrief {
  id: string;
  display_name: string;
}

export interface ExternalOutputBrief {
  id: string;
  endpoint_id: string;
  display_name: string;
}

export interface SendBrief {
  id: string;
  source: string | null;
  output_channel: string | null;
  external_output: string | null;
  enabled: boolean;
  muted: boolean;
  gain_db: number;
  channel_map: [number, number][];
}

export interface RouteProfileSnapshot {
  sources: SourceBrief[];
  output_channels: ChannelBrief[];
  external_outputs: ExternalOutputBrief[];
  sends: SendBrief[];
}

export interface EngineStateBrief {
  state: string;
  running: boolean;
  failed: boolean;
  last_error: string | null;
}

export interface EngineStateEvent {
  state: string;
  running: boolean;
}

export interface EngineStatsEvent {
  capture_packets: number;
  captured_frames: number;
  rendered_frames: number;
  fifo_overflows: number;
  fifo_underflows: number;
  discontinuities: number;
  reconnect_attempts: number;
  captured_peak: number;
  /** 每条 send 的逐通道（L/R）峰值，键为 send id，值为 [left, right]（0.0~1.0）。 */
  send_peaks: Record<string, [number, number]>;
}

export interface DeviceLostEvent {
  endpoint_id: string;
}

// 引擎状态中文映射
export const STATE_LABEL: Record<string, string> = {
  stopped: "已停止",
  running: "运行中",
  degraded: "降级",
  reconnecting: "重连中",
  failed: "失败",
};

// 设备状态中文映射
export const DEVICE_STATUS_LABEL: Record<string, string> = {
  active: "正常",
  unavailable: "不可用",
  unsupported: "不支持",
  error: "错误",
};
