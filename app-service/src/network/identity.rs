//! VBAN 节点稳定身份与 mDNS TXT 元数据编解码。
//!
//! 每个宿主电脑具备双重身份：`node_id`（UUID v4，权威设备 Key，拓扑/路由
//! 存储依据）与 `device_name`（用户友好显示名，默认取 Windows 计算机名）。
//! 参考专项文档 6.1 节。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// mDNS 服务类型（含 domain 后缀）。
pub const VBAN_SERVICE_TYPE: &str = "_loopmaster-vban._udp.local.";
/// VBAN 音频接收端口。
pub const VBAN_SERVICE_PORT: u16 = 6980;

/// TXT 元数据键名（与专项文档 6.2 表一致）。
pub const TXT_NODE_ID: &str = "node_id";
pub const TXT_NAME: &str = "name";
pub const TXT_VER: &str = "ver";
pub const TXT_WEB_PORT: &str = "web_port";
pub const TXT_SR: &str = "sr";
pub const TXT_CH: &str = "ch";
pub const TXT_CAPS: &str = "caps";

/// 节点能力标识（逗号分隔），仅发布稳定能力。
pub const CAPS_VBAN_AUDIO: &str = "vban_audio";
pub const CAPS_WEBRTC_MONITOR: &str = "webrtc_monitor";

/// 节点稳定身份。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeIdentity {
    /// 全局唯一节点 ID（UUID v4 字符串）。
    pub node_id: String,
    /// 用户友好显示名。
    pub device_name: String,
}

impl NodeIdentity {
    /// 生成新的稳定身份：`node_id` 为随机 UUID v4，`device_name` 默认取
    /// Windows 计算机名（`COMPUTERNAME`），失败时回退到 `"LoopMaster-PC"`。
    pub fn generate() -> Self {
        Self {
            node_id: Uuid::new_v4().to_string(),
            device_name: default_device_name(),
        }
    }

    /// mDNS Instance Name：`LoopMaster-{ShortNodeId}`（前 8 位短 ID）。
    pub fn instance_name(&self) -> String {
        let short = self.node_id.get(..8).unwrap_or(&self.node_id);
        format!("LoopMaster-{short}")
    }
}

/// 取 Windows 计算机名；非 Windows 或读取失败时回退到默认值。
pub fn default_device_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "LoopMaster-PC".to_owned())
}

/// 节点发布的 mDNS TXT 元数据（文档 6.2 表）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMeta {
    pub node_id: String,
    pub name: String,
    pub version: String,
    /// 内嵌 Web 控制台端口（0 表示未开启）。
    pub web_port: u16,
    /// 主采样率。
    pub sample_rate: u32,
    /// 默认声道数。
    pub channels: u8,
    /// 能力标识（逗号分隔）。
    pub caps: String,
}

impl NodeMeta {
    /// 把元数据编码为 mDNS TXT 键值对（`HashMap<String, String>`，供
    /// `ServiceInfo::new` 的 `IntoTxtProperties` 消费）。
    pub fn to_txt(&self) -> HashMap<String, String> {
        [
            (TXT_NODE_ID, self.node_id.clone()),
            (TXT_NAME, self.name.clone()),
            (TXT_VER, self.version.clone()),
            (TXT_WEB_PORT, self.web_port.to_string()),
            (TXT_SR, self.sample_rate.to_string()),
            (TXT_CH, self.channels.to_string()),
            (TXT_CAPS, self.caps.clone()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect()
    }

    /// 从解码出的 TXT 键值查询构造元数据；缺失键回退到默认值。
    ///
    /// `node_id` 是唯一强必需键，缺失返回 `None`（无法建立稳定身份）。
    pub fn from_txt_props(node_id: Option<&str>, props: &[(&str, &str)]) -> Option<NodeMeta> {
        let node_id = node_id?;
        let get = |key: &str| props.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
        Some(NodeMeta {
            node_id: node_id.to_owned(),
            name: get(TXT_NAME).unwrap_or("").to_owned(),
            version: get(TXT_VER).unwrap_or("").to_owned(),
            web_port: get(TXT_WEB_PORT).and_then(|v| v.parse().ok()).unwrap_or(0),
            sample_rate: get(TXT_SR).and_then(|v| v.parse().ok()).unwrap_or(48_000),
            channels: get(TXT_CH).and_then(|v| v.parse().ok()).unwrap_or(2),
            caps: get(TXT_CAPS).unwrap_or("").to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_unique_node_ids() {
        let a = NodeIdentity::generate();
        let b = NodeIdentity::generate();
        assert_ne!(a.node_id, b.node_id);
        // UUID v4 格式：8-4-4-4-12。
        assert_eq!(a.node_id.len(), 36);
        assert!(a.node_id.chars().filter(|c| *c == '-').count() == 4);
    }

    #[test]
    fn instance_name_uses_short_node_id() {
        let identity = NodeIdentity {
            node_id: "a1b2c3d4-e5f6-4a7b-8c9d-0123456789ab".to_owned(),
            device_name: "Studio-PC".to_owned(),
        };
        assert_eq!(identity.instance_name(), "LoopMaster-a1b2c3d4");
    }

    #[test]
    fn txt_metadata_round_trips() {
        let meta = NodeMeta {
            node_id: "a1b2c3d4-e5f6-4a7b-8c9d-0123456789ab".to_owned(),
            name: "Studio-PC".to_owned(),
            version: "1.0.0".to_owned(),
            web_port: 8920,
            sample_rate: 48_000,
            channels: 2,
            caps: format!("{CAPS_VBAN_AUDIO},{CAPS_WEBRTC_MONITOR}"),
        };
        let txt = meta.to_txt();
        let props: Vec<(&str, &str)> = txt.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let decoded = NodeMeta::from_txt_props(
            props
                .iter()
                .find(|(k, _)| *k == TXT_NODE_ID)
                .map(|(_, v)| *v),
            &props,
        )
        .unwrap();
        assert_eq!(decoded, meta);
    }

    #[test]
    fn from_txt_props_missing_node_id_returns_none() {
        let props = vec![("name", "x")];
        assert!(NodeMeta::from_txt_props(None, &props).is_none());
    }

    #[test]
    fn from_txt_props_defaults_missing_numerics() {
        let props = vec![(TXT_NODE_ID, "abc"), (TXT_NAME, "x")];
        let meta = NodeMeta::from_txt_props(Some("abc"), &props).unwrap();
        assert_eq!(meta.web_port, 0);
        assert_eq!(meta.sample_rate, 48_000);
        assert_eq!(meta.channels, 2);
    }

    #[test]
    fn default_device_name_is_non_empty() {
        assert!(!default_device_name().is_empty());
    }
}
