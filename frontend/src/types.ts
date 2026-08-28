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
  executable_path: string | null;
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

/** 应用设置（设置页持久化内容）。 */
export interface AppSettings {
  theme: string;
  start_on_boot: boolean;
  launch_hidden: boolean;
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
  render_writes: number;
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

// ---------- 网络设备（局域网 VBAN 节点） ----------

/** 本机网络身份概要。 */
export interface NodeIdentityBrief {
  node_id: string;
  device_name: string;
  network_enabled: boolean;
  web_port: number;
}

/** 局域网发现的 VBAN 节点概要。 */
export interface NetworkNodeBrief {
  node_id: string;
  name: string;
  addresses: string[];
  port: number;
  sample_rate: number;
  channels: number;
  caps: string;
}

/** 节点上线事件负载。 */
export interface NodeResolvedEvent {
  node: NetworkNodeBrief;
}

/** 节点下线事件负载。 */
export interface NodeRemovedEvent {
  node_id: string;
}
