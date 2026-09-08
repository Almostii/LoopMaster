import type { RouteProfileSnapshot, SendBrief } from "./types";

// ---------- 通用 ----------

let idCounter = 0;

/** 生成前端本地临时 id（最终是否落库由后端决定） */
export function freshId(prefix: string): string {
  return `${prefix}-${Date.now()}-${idCounter++}`;
}

/**
 * 清洗 VBAN 流名：协议要求 1..=16 字节可打印 ASCII。
 * 去除非可打印 ASCII 字符、按字节截断到 16、空则回退 "Stream1"。
 * 典型场景：发现节点的 name 是中文/超长主机名，不能直接当流名。
 */
export function sanitizeVbanStreamName(raw: string): string {
  let bytes = 0;
  let out = "";
  for (const ch of raw) {
    const code = ch.codePointAt(0) ?? 0;
    if (code < 32 || code > 126) continue;
    if (bytes + ch.length > 16) break;
    out += ch;
    bytes += ch.length;
  }
  out = out.trim();
  return out.length > 0 ? out : "Stream1";
}

/** 格式化错误信息 */
export function formatError(e: unknown): string {
  const brief = e as { message?: string; hint?: string | null };
  if (brief && brief.hint) return `${brief.message ?? String(e)}；${brief.hint}`;
  if (brief && brief.message) return brief.message;
  return String(e);
}

/** 将 dB 值格式化为可读文本 */
export function formatDb(db: number): string {
  return `${db.toFixed(1)} dB`;
}

// ---------- 贝塞尔连线 ----------

/** 三次贝塞尔曲线路径（经典 Loopback S-Curve） */
export function createBezierPath(x1: number, y1: number, x2: number, y2: number): string {
  const dx = Math.max(Math.abs(x2 - x1) * 0.45, 30);
  return `M ${x1},${y1} C ${x1 + dx},${y1} ${x2 - dx},${y2} ${x2},${y2}`;
}

// ---------- 连线计算 ----------

/**
 * 由 sends 推导出需要绘制的拓扑连线。
 * 一个 send 最多可产生两条可见连线：
 *   - source → output_channel
 *   - output_channel → external_output
 * 若 send 同时携带 source 与 external_output（无中间通道）则画直达线。
 */
export interface WireSpec {
  id: string;
  fromSocketId: string;
  toSocketId: string;
  enabled: boolean;
}

export function computeWires(route: RouteProfileSnapshot): WireSpec[] {
  const wires: WireSpec[] = [];
  for (const send of route.sends) {
    if (send.source && send.output_channel) {
      wires.push({
        id: `${send.id}-s2c`,
        fromSocketId: send.source,
        toSocketId: `${send.output_channel}-in`,
        enabled: send.enabled,
      });
    }
    if (send.output_channel && send.external_output) {
      wires.push({
        id: `${send.id}-c2e`,
        fromSocketId: `${send.output_channel}-out`,
        toSocketId: `${send.external_output}-in`,
        enabled: send.enabled,
      });
    }
    if (send.source && !send.output_channel && send.external_output) {
      wires.push({
        id: `${send.id}-s2e`,
        fromSocketId: send.source,
        toSocketId: `${send.external_output}-in`,
        enabled: send.enabled,
      });
    }
  }
  return wires;
}

/** 获取某个 source 的全部 sends */
export function sendsForSource(route: RouteProfileSnapshot, sourceId: string): SendBrief[] {
  return route.sends.filter((s) => s.source === sourceId);
}

/** 获取某个 output_channel 的全部 sends */
export function sendsForChannel(route: RouteProfileSnapshot, channelId: string): SendBrief[] {
  return route.sends.filter((s) => s.output_channel === channelId);
}

/** 获取某个 external_output 的全部 sends */
export function sendsForExternal(route: RouteProfileSnapshot, externalId: string): SendBrief[] {
  return route.sends.filter((s) => s.external_output === externalId);
}

/** 判断某个 source 是否处于开启状态（存在至少一条启用的 send） */
export function isSourceEnabled(route: RouteProfileSnapshot, sourceId: string): boolean {
  return sendsForSource(route, sourceId).some((s) => s.enabled);
}

/** 判断某个 external_output 是否处于开启状态 */
export function isExternalEnabled(route: RouteProfileSnapshot, externalId: string): boolean {
  return sendsForExternal(route, externalId).some((s) => s.enabled);
}
