// 远程控制台协议层（与 app-service/src/web_server/ws.rs 严格对齐）。
// 协议冻结文档：Plan/2026-08-31-Web控制台DTO与可信设备模型冻结.md §1。

export interface Source {
  id: string;
  kind: string;
  display_name: string;
  endpoint_id: string | null;
  process_id: number | null;
}

export interface OutputChannel {
  id: string;
  display_name: string;
}

export interface ExternalOutput {
  id: string;
  endpoint_id: string;
  display_name: string;
  kind: string;
}

export interface Send {
  send_id: string;
  source: string | null;
  output_channel: string | null;
  external_output: string | null;
  gain_db: number;
  muted: boolean;
  enabled: boolean;
  channel_map: Array<[number, number]>;
}

/** `initial_state` 全量快照。 */
export interface RemoteState {
  state_revision: number;
  engine_status: string;
  sample_rate: number;
  sources: Source[];
  output_channels: OutputChannel[];
  external_outputs: ExternalOutput[];
  sends: Send[];
}

/** 一条二进制 meter 帧内的节点读数（dBFS）。 */
export interface MeterEntry {
  id: string;
  peak_db: number;
  rms_db: number;
}

/**
 * 解析二进制 meter 帧（帧类型 0x01）：
 * `0x01 | N(u8) | ts(u32 LE) | (len(u8) | id | peak dBFS f32 LE | rms dBFS f32 LE) × N`
 */
export function parseMeterFrame(buffer: ArrayBuffer): MeterEntry[] {
  const view = new DataView(buffer);
  if (view.byteLength < 6 || view.getUint8(0) !== 0x01) return [];
  const count = view.getUint8(1);
  const entries: MeterEntry[] = [];
  let offset = 6;
  for (let i = 0; i < count; i++) {
    if (offset + 1 > view.byteLength) break;
    const len = view.getUint8(offset++);
    if (offset + len + 8 > view.byteLength) break;
    const id = new TextDecoder().decode(new Uint8Array(buffer, offset, len));
    offset += len;
    const peak_db = view.getFloat32(offset, true);
    offset += 4;
    const rms_db = view.getFloat32(offset, true);
    offset += 4;
    entries.push({ id, peak_db, rms_db });
  }
  return entries;
}

/** 构建一条上行控制消息（服务器回含相同 seq 的 ack/error）。 */
export function controlMessage(seq: number, action: string, data: unknown): string {
  return JSON.stringify({ seq, action, data });
}

/**
 * 推子位置（0..1）→ 分贝（冻结文档 §1.3 / 方案 3 §3.1）：
 * P∈[0,0.75] → −60+80P；P∈(0.75,1] → 24(P−0.75)。
 */
export function faderPosToDb(pos: number): number {
  const p = Math.min(1, Math.max(0, pos));
  return p <= 0.75 ? -60 + 80 * p : 24 * (p - 0.75);
}

/** 分贝 → 推子位置（faderPosToDb 的逆函数，服务端钳制 −60..+6）。 */
export function dbToFaderPos(db: number): number {
  const clamped = Math.min(6, Math.max(-60, db));
  if (clamped <= 0) return clamped === -60 ? 0 : (clamped + 60) / 80;
  return 0.75 + clamped / 24;
}
