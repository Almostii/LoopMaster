import { useEffect, useRef, useState } from "react";
import { QRCodeSVG } from "qrcode.react";
import type { NetworkNodeBrief, NodeIdentityBrief } from "../types";
import {
  checkNetworkFirewall,
  enableNetworkFirewall,
  forgetDevice,
  getNetworkNodes,
  getNodeIdentity,
  getPairingRequired,
  getPairingStatus,
  listTrustedDevices,
  onNodeRemoved,
  onNodeResolved,
  resetTrust,
  setNetworkEnabled,
  setPairingRequired,
  startPairing,
  stopPairing,
  type FirewallCheckResult,
  type PairingInfo,
  type TrustedDevice,
} from "../api";

const emptyIdentity: NodeIdentityBrief = {
  node_id: "",
  device_name: "",
  network_enabled: false,
  web_port: 0,
};

/** VBAN 默认端口（与后端 VBAN_SERVICE_PORT 一致），仅用于状态展示。 */
const vbanPortLabel = "6980";

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
  // 防火墙放行状态（开启网络功能时展示两条规则各自状态）。
  const [firewall, setFirewall] = useState<FirewallCheckResult | null>(null);
  // 手动"重新放行"进行中标记与失败文案（UAC 被拒/规则校验失败时展示）。
  const [firewallToggling, setFirewallToggling] = useState(false);
  const [firewallError, setFirewallError] = useState<string | null>(null);
  // 配对开关与可信设备（默认开放访问；开启"要求配对"后展示二维码配对）。
  const [requirePairing, setRequirePairing] = useState(false);
  const [pairing, setPairing] = useState<PairingInfo | null>(null);
  const [devices, setDevices] = useState<TrustedDevice[]>([]);
  const [pairingBusy, setPairingBusy] = useState(false);
  const [pairingError, setPairingError] = useState<string | null>(null);

  // 刷新配对开关、可信设备与配对窗口状态。
  useEffect(() => {
    if (!identity.network_enabled) {
      setPairing(null);
      setDevices([]);
      return;
    }
    void Promise.all([getPairingRequired(), listTrustedDevices(), getPairingStatus()])
      .then(([required, deviceList, pairingStatus]) => {
        setRequirePairing(required);
        setDevices(deviceList);
        setPairing(pairingStatus);
      })
      .catch((e: unknown) => {
        setPairingError(e instanceof Error ? e.message : "读取配对状态失败。");
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [identity.network_enabled, identity.web_port]);

  async function handleTogglePairing(enabled: boolean) {
    if (pairingBusy) return;
    setPairingBusy(true);
    setPairingError(null);
    try {
      await setPairingRequired(enabled);
      setRequirePairing(enabled);
      if (!enabled) {
        // 关闭配对：结束配对窗口并收起可信设备管理。
        setPairing(null);
      }
    } catch (e) {
      setPairingError(e instanceof Error ? e.message : "切换配对模式失败。");
    } finally {
      setPairingBusy(false);
    }
  }

  async function handleAddDevice() {
    if (pairingBusy) return;
    setPairingBusy(true);
    setPairingError(null);
    try {
      setPairing(await startPairing());
    } catch (e) {
      setPairingError(e instanceof Error ? e.message : "开启配对窗口失败。");
    } finally {
      setPairingBusy(false);
    }
  }

  async function handleClosePairing() {
    setPairingBusy(true);
    try {
      await stopPairing();
    } catch {
      // 关闭失败不阻塞界面
    }
    setPairing(null);
    setPairingBusy(false);
  }

  async function handleForgetDevice(deviceId: string) {
    try {
      await forgetDevice(deviceId);
      setDevices((prev) => prev.filter((device) => device.id !== deviceId));
    } catch (e) {
      setPairingError(e instanceof Error ? e.message : "忘记设备失败。");
    }
  }

  async function handleResetAll() {
    if (!window.confirm("确定重置全部局域网信任？所有已配对设备都需重新扫码配对。")) {
      return;
    }
    try {
      await resetTrust();
      setDevices([]);
    } catch (e) {
      setPairingError(e instanceof Error ? e.message : "重置信任失败。");
    }
  }

  // 配对二维码地址：优先取 RFC1918 私网地址（真实局域网口），跳过虚拟网卡。
  const lanHost =
    identity.addresses?.find((addr) =>
      /^(10\.|192\.168\.|172\.(1[6-9]|2\d|3[01])\.)/.test(addr),
    ) ??
    identity.addresses?.[0] ??
    "127.0.0.1";
  const pairingUrl = pairing
    ? `http://${lanHost}:${identity.web_port > 0 ? identity.web_port : 8920}/pair#secret=${pairing.secret}`
    : "";

  // 确保防火墙放行 VBAN（UDP）与 Web 控制台（TCP）：规则缺失时自动提权放行，
  // 用户只需确认一次 UAC；已放行的规则不会重复提权。
  async function ensureFirewall(): Promise<FirewallCheckResult> {
    try {
      const current = await checkNetworkFirewall();
      if (current.rule_exists) {
        setFirewall(current); // 两条都已放行：展示状态，不展示错误
        setFirewallError(null);
        return current;
      }
      // 规则缺失：自动提权放行（弹一次 UAC）。
      const result = await enableNetworkFirewall();
      setFirewall(result);
      // 后端在规则校验失败时返回 Err，此处不会命中；仍保留兜底判断。
      setFirewallError(result.rule_exists ? null : result.message);
      return result;
    } catch (e) {
      // 自动放行被拒（用户拒绝 UAC）或规则校验失败：保留明确错误文案。
      const message =
        e instanceof Error
          ? e.message
          : "防火墙未放行。需要放行 VBAN（UDP 6980）与 Web 控制台（TCP）入站。";
      setFirewall({
        port_available: true,
        rule_exists: false,
        vban_rule_exists: false,
        web_rule_exists: false,
        checked: true,
        message,
      });
      setFirewallError(message);
      return {
        port_available: true,
        rule_exists: false,
        vban_rule_exists: false,
        web_rule_exists: false,
        checked: true,
        message,
      };
    }
  }

  // 手动重试放行：重新走一次检测 + 提权，并把失败原因直接显示出来。
  async function handleRetryFirewall() {
    setFirewallToggling(true);
    setFirewallError(null);
    try {
      const current = await checkNetworkFirewall();
      if (current.rule_exists) {
        setFirewall(current);
        return;
      }
      setFirewall(await enableNetworkFirewall());
    } catch (e) {
      const message = e instanceof Error ? e.message : "放行失败，请查看提示手动执行。";
      setFirewallError(message);
      setFirewall((prev) =>
        prev
          ? { ...prev, message }
          : {
              port_available: true,
              rule_exists: false,
              vban_rule_exists: false,
              web_rule_exists: false,
              checked: true,
              message,
            },
      );
    } finally {
      setFirewallToggling(false);
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
        setFirewallError(null);
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
              {identity.network_enabled && identity.web_port > 0
                ? `http://<本机IP>:${identity.web_port}`
                : "未开启"}
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
          {identity.network_enabled && (
            <div className="device-pair-toggle">
              <label className="device-pair-toggle-label">
                <input
                  type="checkbox"
                  checked={requirePairing}
                  disabled={pairingBusy}
                  onChange={(e) => void handleTogglePairing(e.target.checked)}
                />
                要求配对才能访问控制台
              </label>
              <span className="device-pair-toggle-hint">
                {requirePairing
                  ? "开启：设备需扫码/PIN 配对后才能控制"
                  : "关闭（默认）：局域网内设备直接访问"}
              </span>
            </div>
          )}
          {identity.network_enabled && identity.web_port > 0 && !requirePairing && (
            <div className="device-open-hint">
              局域网内设备（电脑/手机/平板）输入{" "}
              <span className="device-mono">
                http://&lt;本机IP&gt;:{identity.web_port}
              </span>{" "}
              即可访问控制台，无需配对。
            </div>
          )}
        </div>
      </section>

      {/* 防火墙放行状态（开启网络功能时展示；两条规则分别列出） */}
      {identity.network_enabled && firewall && (
        <section className="device-firewall-card">
          <div className="device-firewall-title">
            防火墙放行
            <button
              type="button"
              className="device-firewall-retry"
              onClick={() => void handleRetryFirewall()}
              disabled={firewallToggling}
            >
              {firewallToggling ? "放行中…" : "重新放行"}
            </button>
          </div>
          <div className="device-firewall-rules">
            <span
              className={`device-firewall-badge ${
                firewall.vban_rule_exists ? "ok" : "warn"
              }`}
            >
              VBAN UDP {vbanPortLabel}
              {firewall.vban_rule_exists ? " 已放行" : " 未放行"}
            </span>
            <span
              className={`device-firewall-badge ${
                firewall.web_rule_exists ? "ok" : "warn"
              }`}
            >
              Web TCP {identity.web_port > 0 ? identity.web_port : "—"}
              {firewall.web_rule_exists ? " 已放行" : " 未放行"}
            </span>
          </div>
          {(!firewall.rule_exists || firewallError) && (
            <div className="device-firewall-desc">
              {firewallError ?? firewall.message}
            </div>
          )}
        </section>
      )}

      {/* 信任的设备与配对（仅"要求配对"模式下展示） */}
      {identity.network_enabled && requirePairing && (
        <section className="device-trust-card">
          <div className="device-trust-title">
            信任的设备与配对
            <button
              type="button"
              className="device-trust-action"
              onClick={() => void handleAddDevice()}
              disabled={pairingBusy}
            >
              {pairing ? "重新生成" : "添加设备"}
            </button>
          </div>
          {pairingError && <div className="device-trust-desc">{pairingError}</div>}
          {pairing && (
            <div className="pairing-box">
              <div className="pairing-qr">
                <QRCodeSVG value={pairingUrl} size={168} bgColor="#ffffff" fgColor="#000000" />
              </div>
              <div className="pairing-meta">
                <div className="pairing-pin">
                  备用 PIN：<b>{pairing.pin}</b>
                </div>
                <div className="pairing-url">{pairingUrl}</div>
                <div className="device-trust-desc">
                  手机扫描二维码，用浏览器打开并按提示完成首次配对；窗口 5 分钟有效。
                </div>
                <button
                  type="button"
                  className="device-trust-action"
                  onClick={() => void handleClosePairing()}
                >
                  关闭配对窗口
                </button>
              </div>
            </div>
          )}
          <div className="device-trust-rules">
            {devices.length === 0 ? (
              <div className="device-trust-desc">尚无已配对设备。</div>
            ) : (
              devices.map((device) => (
                <div key={device.id} className="device-row">
                  <span className="device-row-name">
                    {device.name} <em>({device.permission})</em>
                  </span>
                  <button
                    type="button"
                    className="device-trust-action"
                    onClick={() => void handleForgetDevice(device.id)}
                  >
                    忘记
                  </button>
                </div>
              ))
            )}
          </div>
          {devices.length > 0 && (
            <button
              type="button"
              className="device-trust-action"
              onClick={() => void handleResetAll()}
            >
              重置全部局域网信任
            </button>
          )}
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
