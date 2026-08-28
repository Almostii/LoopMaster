//! VBAN 节点的 mDNS 服务发布（Advertiser）与浏览（Browser）。
//!
//! 基于 `mdns-sd`（同步库，内部线程处理 IO），封装服务注册与发现：
//! - [`MdnsAdvertiser`]：把本机身份发布为 `_loopmaster-vban._udp.local.` 服务；
//! - [`MdnsBrowser`]：监听局域网内 VBAN 节点上线/下线事件，投影为
//!   [`NetworkEvent`]，供上层拓扑动态注入。
//!
//! 参考专项文档 6.2/6.3 节。

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;

use mdns_sd::{Receiver, ServiceDaemon, ServiceEvent, ServiceInfo};

use super::identity::{NodeIdentity, NodeMeta, TXT_NODE_ID, VBAN_SERVICE_TYPE};

/// 一个已发现的 VBAN 节点（由 mDNS TXT 与解析地址投影）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeInfo {
    /// 稳定节点 ID（TXT `node_id`）。
    pub node_id: String,
    /// 友好显示名（TXT `name`）。
    pub name: String,
    /// IPv4 地址集合。
    pub addresses: Vec<Ipv4Addr>,
    /// VBAN 音频接收端口。
    pub port: u16,
    /// 主采样率。
    pub sample_rate: u32,
    /// 默认声道数。
    pub channels: u8,
    /// 能力标识（逗号分隔）。
    pub caps: String,
}

/// 网络发现事件。
#[derive(Clone, Debug, PartialEq)]
pub enum NetworkEvent {
    /// 节点上线/刷新（已解析出稳定身份与地址）。
    NodeResolved(NodeInfo),
    /// 节点下线/超时（按 `node_id` 标记 Offline，保留连线）。
    NodeRemoved(String),
}

/// mDNS 服务发布端（Advertiser）。
///
/// 持有 `ServiceDaemon` 并注册本机 VBAN 服务；`Drop` 时注销并停止守护。
pub struct MdnsAdvertiser {
    daemon: ServiceDaemon,
    /// 已注册服务的 fullname（注销用）。
    fullname: String,
}

impl MdnsAdvertiser {
    /// 创建广告者并注册本机 VBAN 服务。
    ///
    /// `identity` 提供 `node_id`/`device_name`，`meta` 提供采样率/声道/能力等
    /// TXT 元数据。绑定失败或注册失败返回错误。
    pub fn register(identity: &NodeIdentity, meta: &NodeMeta) -> Result<Self, MdnsError> {
        let daemon = ServiceDaemon::new()?;
        let instance_name = identity.instance_name();
        let host_name = format!("{}.local.", identity.device_name.replace([' ', '.'], "-"));
        // 注册时启用 addr_auto：mdns-sd 会自动从主机填充本机 IP 地址；
        // 端口为 VBAN 接收端口。
        let service_info = ServiceInfo::new(
            VBAN_SERVICE_TYPE,
            &instance_name,
            &host_name,
            &["0.0.0.0"][..],
            super::identity::VBAN_SERVICE_PORT,
            meta.to_txt(),
        )?
        .enable_addr_auto();
        let fullname = service_info.get_fullname().to_owned();
        daemon.register(service_info)?;
        Ok(Self { daemon, fullname })
    }

    /// 注销已注册服务并停止守护（幂等）。
    pub fn shutdown(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }

    /// 已注册服务的 fullname。
    pub fn fullname(&self) -> &str {
        &self.fullname
    }
}

impl Drop for MdnsAdvertiser {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// mDNS 服务浏览端（Browser）。
///
/// 监听 `_loopmaster-vban._udp.local.` 事件，把 `ServiceEvent` 投影为
/// [`NetworkEvent`]，上层可轮询或阻塞接收。
pub struct MdnsBrowser {
    daemon: ServiceDaemon,
    receiver: Receiver<ServiceEvent>,
    /// 已解析并上报过的节点 ID（避免重复 `NodeResolved`）。
    resolved: HashSet<String>,
    /// 已解析服务的 fullname → node_id 映射（下线事件定位用）。
    fullname_to_node: HashMap<String, String>,
}

impl MdnsBrowser {
    /// 创建浏览端并开始监听 VBAN 服务类型。
    pub fn new() -> Result<Self, MdnsError> {
        let daemon = ServiceDaemon::new()?;
        let receiver = daemon.browse(VBAN_SERVICE_TYPE)?;
        Ok(Self {
            daemon,
            receiver,
            resolved: HashSet::new(),
            fullname_to_node: HashMap::new(),
        })
    }

    /// 取下一个网络事件；当前无事件或事件不可上报时返回 `None`（非阻塞）。
    pub fn try_recv(&mut self) -> Result<Option<NetworkEvent>, MdnsError> {
        loop {
            match self.receiver.try_recv() {
                Ok(event) => {
                    if let Some(network_event) = self.project(event) {
                        return Ok(Some(network_event));
                    }
                    // 事件被过滤（如 ServiceFound/Removed），继续读下一个。
                }
                Err(flume::TryRecvError::Empty) => return Ok(None),
                Err(flume::TryRecvError::Disconnected) => {
                    return Err(MdnsError::ChannelClosed);
                }
            }
        }
    }

    /// 阻塞接收下一个可上报的网络事件。
    pub fn recv(&mut self) -> Result<NetworkEvent, MdnsError> {
        loop {
            let event = self.receiver.recv().map_err(|_| MdnsError::ChannelClosed)?;
            if let Some(network_event) = self.project(event) {
                return Ok(network_event);
            }
        }
    }

    /// 停止浏览并关闭守护。
    pub fn shutdown(&mut self) {
        let _ = self.daemon.shutdown();
    }

    /// 把 mdns-sd 的原始事件投影为网络事件。
    fn project(&mut self, event: ServiceEvent) -> Option<NetworkEvent> {
        match event {
            ServiceEvent::ServiceResolved(info) => {
                let Some(node_id) = info.get_property_val_str(TXT_NODE_ID) else {
                    // 第三方设备无 LoopMaster node_id，不纳入自动拓扑。
                    return None;
                };
                let node_id = node_id.to_owned();
                let node = NodeInfo {
                    node_id: node_id.clone(),
                    name: info.get_property_val_str("name").unwrap_or("").to_owned(),
                    addresses: info
                        .get_addresses_v4()
                        .into_iter()
                        .filter(|addr| !addr.is_unspecified())
                        .copied()
                        .collect(),
                    port: info.get_port(),
                    sample_rate: info
                        .get_property_val_str("sr")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(48_000),
                    channels: info
                        .get_property_val_str("ch")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(2),
                    caps: info.get_property_val_str("caps").unwrap_or("").to_owned(),
                };
                // 登记 fullname -> node_id 映射，供 ServiceRemoved 定位下线节点。
                self.fullname_to_node
                    .insert(info.get_fullname().to_owned(), node_id.clone());
                self.resolved.insert(node_id.clone());
                Some(NetworkEvent::NodeResolved(node))
            }
            ServiceEvent::ServiceRemoved(_ty, fullname) => {
                // 依据已登记的 fullname -> node_id 映射发下线事件；无法定位时
                // 返回 None，由上层依据超时/心跳判定下线（见 6.5）。
                let node_id = self.fullname_to_node.remove(&fullname)?;
                self.resolved.remove(&node_id);
                Some(NetworkEvent::NodeRemoved(node_id))
            }
            _ => None,
        }
    }
}

impl Drop for MdnsBrowser {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

/// 网络模块错误。
#[derive(Debug, thiserror::Error)]
pub enum MdnsError {
    #[error("mDNS 守护启动失败: {0}")]
    Daemon(#[from] mdns_sd::Error),
    #[error("mDNS 事件通道已关闭")]
    ChannelClosed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::identity::NodeMeta;

    fn test_identity() -> NodeIdentity {
        NodeIdentity {
            node_id: "a1b2c3d4-e5f6-4a7b-8c9d-0123456789ab".to_owned(),
            device_name: "Test-PC".to_owned(),
        }
    }

    fn test_meta() -> NodeMeta {
        NodeMeta {
            node_id: "a1b2c3d4-e5f6-4a7b-8c9d-0123456789ab".to_owned(),
            name: "Test-PC".to_owned(),
            version: "1.0.0".to_owned(),
            web_port: 0,
            sample_rate: 48_000,
            channels: 2,
            caps: super::super::identity::CAPS_VBAN_AUDIO.to_owned(),
        }
    }

    #[test]
    fn node_info_defaults_are_sane() {
        // 验证 NodeInfo 构造字段完整（用于投影，无网络）。
        let info = NodeInfo {
            node_id: "abc".to_owned(),
            name: "x".to_owned(),
            addresses: Vec::new(),
            port: 6980,
            sample_rate: 48_000,
            channels: 2,
            caps: String::new(),
        };
        assert_eq!(info.port, 6980);
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.channels, 2);
    }

    /// 自环 mDNS 测试：发布 + 浏览在同一进程能发现自己的服务。
    ///
    /// 依赖本机网络（UDP 组播），在 CI 或隔离环境可能失败，故标记 `ignore`，
    /// 需本机手动运行：`cargo test -p loopmaster-app-service network -- --ignored`。
    #[test]
    #[ignore]
    fn advertiser_and_browser_discover_each_other() {
        let identity = test_identity();
        let meta = test_meta();
        let mut advertiser = MdnsAdvertiser::register(&identity, &meta).unwrap();
        let mut browser = MdnsBrowser::new().unwrap();

        // 给 mDNS 足够时间解析自己的服务（自环通常 <2s）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut found = false;
        while std::time::Instant::now() < deadline {
            if let Some(NetworkEvent::NodeResolved(node)) = browser.try_recv().unwrap() {
                if node.node_id == identity.node_id {
                    found = true;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(found, "应在超时前发现本机 VBAN 服务");

        // 注销广告者后，浏览器应能收到 NodeRemoved 下线事件。
        advertiser.shutdown();
        let remove_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut removed = false;
        while std::time::Instant::now() < remove_deadline {
            if let Some(NetworkEvent::NodeRemoved(node_id)) = browser.try_recv().unwrap() {
                if node_id == identity.node_id {
                    removed = true;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(removed, "注销后应在超时前收到 NodeRemoved 下线事件");
        browser.shutdown();
    }
}
