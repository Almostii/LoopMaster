import { useEffect, useRef, useState } from "react";
import type { NetworkNodeBrief, NodeIdentityBrief } from "../types";
import {
  checkNetworkFirewall,
  enableNetworkFirewall,
  getNetworkNodes,
  getNodeIdentity,
  onNodeRemoved,
  onNodeResolved,
  setNetworkEnabled,
  type FirewallCheckResult,
} from "../api";

const emptyIdentity: NodeIdentityBrief = {
  node_id: "",
  device_name: "",
  network_enabled: false,
  web_port: 0,
};

/** 电脑显示器图标（紧凑卡片顶部使用）。 */
function MonitorDeviceIcon() {
  return (
    <svg
      width="46"
      height="38"
      viewBox="0 0 46 38"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.6"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <rect x="3" y="3" width="40" height="26" rx="2.5" />
      <line x1="15" y1="33" x2="31" y2="33" />
      <line x1="23" y1="29" x2="23" y2="33" />
    </svg>
  );
}

/** 节点 ID 短串（前 8 位），用于紧凑展示。 */
function shortId(nodeId: string): string {
  return nodeId.length > 8 ? `${nodeId.slice(0, 8)}…` : nodeId || "—";
}

/** 节点主地址：`IP:port` 短格式。 */
function primaryAddress(node: NetworkNodeBrief): string {
  return node.addresses[0] ? `${node.addresses[0]}:${node.port}` : "无地址";
}

/** 网络设备查看页：展示本机身份与局域网发现的 VBAN 节点。 */
export default function DeviceView() {
  const [identity, setIdentity] = useState<NodeIdentityBrief>(emptyIdentity);
  const [nodes, setNodes] = useState<NetworkNodeBrief[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const unlistenRefs = useRef<Array<() => void>>([]);

  // 加载本机身份与当前节点快照
  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    void Promise.all([getNodeIdentity(), getNetworkNodes()])
      .then(([id, list]) => {
        if (cancelled) return;
        setIdentity(id);
        setNodes(list);
        setError(null);
        // 应用启动时网络功能已开启：自动放行防火墙（规则缺失时提权 UAC）。
        if (id.network_enabled) {
          void ensureFirewall().catch(() => {});
        }
      })
      .catch((e) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // 订阅节点上线/下线事件，实时更新列表
  useEffect(() => {
    let active = true;
    let unlistenResolved: (() => void) | null = null;
    let unlistenRemoved: (() => void) | null = null;

    void onNodeResolved((node) => {
      if (!active) return;
      setNodes((prev) => {
        const exists = prev.some((n) => n.node_id === node.node_id);
        if (exists) return prev.map((n) => (n.node_id === node.node_id ? node : n));
        return [...prev, node];
      });
    }).then((un) => {
      unlistenResolved = un;
      unlistenRefs.current.push(un);
    });
    void onNodeRemoved((nodeId) => {
      if (!active) return;
      setNodes((prev) => prev.filter((n) => n.node_id !== nodeId));
    }).then((un) => {
      unlistenRemoved = un;
      unlistenRefs.current.push(un);
    });

    return () => {
      active = false;
      // 立即移除已注册的监听；尚未 resolve 的由 unlistenRefs 兜底。
      unlistenResolved?.();
      unlistenRemoved?.();
      unlistenRefs.current.forEach((un) => un());
      unlistenRefs.current = [];
    };
  }, []);

  // 切换网络功能开关
  const [toggling, setToggling] = useState(false);
  // 防火墙放行状态（开启时自动放行，失败才展示引导）。
  const [firewall, setFirewall] = useState<FirewallCheckResult | null>(null);

  // 确保防火墙放行 UDP 6980：规则缺失时自动提权放行，用户只需确认一次 UAC。
  async function ensureFirewall(): Promise<FirewallCheckResult> {
    try {
      const current = await checkNetworkFirewall();
      if (current.rule_exists) {
        return current; // 已放行，无需操作
      }
      // 规则缺失：自动提权放行（弹一次 UAC）。
      const result = await enableNetworkFirewall();
      setFirewall(result.rule_exists ? null : result);
      return result;
    } catch (e) {
      // 自动放行被拒（用户拒绝 UAC），保留引导提示。
      setFirewall({
        port_available: true,
        rule_exists: false,
        checked: true,
        message: e instanceof Error ? e.message : "防火墙未放行，请手动放行 UDP 6980 入站。",
      });
      return {
        port_available: true,
        rule_exists: false,
        checked: true,
        message: e instanceof Error ? e.message : "防火墙未放行。",
      };
    }
  }

  async function handleToggleNetwork(enabled: boolean) {
    if (toggling) return;
    setToggling(true);
    try {
      const updated = await setNetworkEnabled(enabled);
      setIdentity(updated);
      // 关闭时立即清空节点列表，不等远端 mDNS 缓存（TTL）过期，
      // 保证"局域网电脑"区与网络功能状态一致。
      if (!updated.network_enabled) {
        setNodes([]);
        setFirewall(null);
      } else {
        // 开启后自动放行防火墙（规则缺失时提权 UAC），无需手动。
        setFirewall(await ensureFirewall());
      }
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setToggling(false);
    }
  }

  // 网络功能开关变化时同步节点列表：关闭即清空（不展示已下线的本机/缓存节点）。
  useEffect(() => {
    if (!identity.network_enabled) {
      setNodes([]);
    }
  }, [identity.network_enabled]);

  // 排除本机自身：本机 Advertiser 的 mDNS 广播也会被本机 Browser 收到，
  // 会把本机误报为"其他电脑"。
  const displayNodes = nodes.filter((n) => n.node_id !== identity.node_id);

  return (
    <div className="device-view">
      {/* 本机身份卡 */}
      <section className="device-local-card">
        <div className="device-local-head">
          <span className="device-local-badge">本机</span>
          <span className="device-local-name">{identity.device_name || "LoopMaster-PC"}</span>
          <span
            className={`device-local-status ${
              identity.network_enabled ? "online" : "offline"
            }`}
          >
            {identity.network_enabled ? "网络功能已开启" : "网络功能未开启"}
          </span>
          <div className="device-local-switch">
            <label className="switch" title="网络功能开关">
              <input
                type="checkbox"
                checked={identity.network_enabled}
                disabled={toggling}
                onChange={(e) => void handleToggleNetwork(e.target.checked)}
              />
              <span className="slider-round" />
            </label>
          </div>
        </div>
        <div className="device-local-meta">
          <div className="device-meta-item">
            <span className="device-meta-label">节点 ID</span>
            <span className="device-meta-value device-mono">
              {identity.node_id || "（尚未生成）"}
            </span>
          </div>
          <div className="device-meta-item">
            <span className="device-meta-label">Web 控制台端口</span>
            <span className="device-meta-value">
              {identity.web_port > 0 ? identity.web_port : "未开启"}
            </span>
          </div>
          {/* 本机 IP：用户可在其他电脑上手动输入该地址进行连接 */}
          <div className="device-meta-item device-meta-item-wide">
            <span className="device-meta-label">本机 IP</span>
            <span className="device-meta-value device-mono device-meta-wrap">
              {identity.addresses && identity.addresses.length > 0
                ? identity.addresses.join("、")
                : "（未获取到局域网地址）"}
            </span>
          </div>
        </div>
      </section>

      {/* 防火墙放行引导（开启网络功能且规则缺失时提示） */}
      {identity.network_enabled && firewall && !firewall.rule_exists && (
        <section className="device-firewall-card">
          <div className="device-firewall-title">防火墙放行提示</div>
          <div className="device-firewall-desc">{firewall.message}</div>
        </section>
      )}

      {/* 局域网节点列表（排除本机自身） */}
      <section className="device-nodes-section">
        <div className="device-nodes-head">
          <h3 className="device-nodes-title">局域网电脑</h3>
          <span className="device-nodes-count">{displayNodes.length} 台在线</span>
        </div>

        {!identity.network_enabled ? (
          <div className="device-empty">
            网络功能未开启。
            <br />
            请打开上方开关以发现局域网中的其他电脑。
          </div>
        ) : loading ? (
          <div className="device-empty">正在扫描局域网…</div>
        ) : error ? (
          <div className="device-error">{error}</div>
        ) : displayNodes.length === 0 ? (
          <div className="device-empty">
            未发现其他 LoopMaster 设备。
            <br />
            请确认其他电脑已开启 LoopMaster 的网络功能。
          </div>
        ) : (
          <ul className="device-node-list">
            {displayNodes.map((node) => (
              <li
                key={node.node_id}
                className="device-node-card"
                title={`节点 ID：${node.node_id}\n地址：${node.addresses.join(", ") || "—"}\n音频：${node.sample_rate} Hz · ${node.channels} 声道\n能力：${node.caps || "—"}`}
              >
                <div className="device-node-icon">
                  <MonitorDeviceIcon />
                </div>
                <div className="device-node-card-name">
                  {node.name || shortId(node.node_id)}
                </div>
                <div className="device-node-card-id device-mono">{shortId(node.node_id)}</div>
                <div className="device-node-card-addr device-mono">{primaryAddress(node)}</div>
                <div className="device-node-card-foot">
                  <span className="device-node-dot" aria-hidden />
                  <span className="device-node-card-audio">
                    {node.sample_rate / 1000} kHz · {node.channels}ch
                  </span>
                </div>
              </li>
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}
