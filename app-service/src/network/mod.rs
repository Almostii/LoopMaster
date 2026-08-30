//! 网络服务层：mDNS 服务发现与节点身份（Phase 1 第三项）。
//!
//! 本模块承载 VBAN 局域网节点的零配置发现（ZeroConf / mDNS）与稳定身份
//! 管理，是 `app-service` 面向网络部分的入口。UDP 音频收发、Web 控制台等
//! 后续在此扩展。
//!
//! 参考：[VBAN 局域网音频互通与传输方案]
//! （../../../../Doc/网络传输与本地节点互通方案计划/1.VBAN局域网音频互通与传输方案.md）6 节。

pub mod bridge;
pub mod identity;
pub mod mdns;
pub mod network_discovery;
pub mod receiver;
pub mod sender;

pub use bridge::{NetworkBridge, NetworkBridgeError, VbanSinkBridge, VbanSourceBridge};
pub use identity::{
    default_device_name, NodeIdentity, NodeMeta, CAPS_VBAN_AUDIO, CAPS_WEBRTC_MONITOR,
    VBAN_SERVICE_PORT, VBAN_SERVICE_TYPE,
};
pub use mdns::{MdnsAdvertiser, MdnsBrowser, MdnsError, NetworkEvent, NodeInfo};
pub use network_discovery::NetworkDiscovery;
pub use receiver::{VBanReceiveError, VBanReceiveStats, VBanReceiver};
pub use sender::{VBanSendError, VBanSender};
