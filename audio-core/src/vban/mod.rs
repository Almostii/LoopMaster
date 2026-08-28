//! VBAN Audio 协议的平台无关字节编解码与缓冲算法。
//!
//! 本模块只处理协议字节与算法，不持有 UDP Socket、Tokio 任务或 mDNS 生命周期；
//! 网络层位于 `app-service::network`，通过本模块的 packet/frame API 与主混音
//! 图谱交换 PCM。
//!
//! 参考：[VBAN 局域网音频互通与传输方案](../../../../Doc/网络传输与本地节点互通方案计划/1.VBAN局域网音频互通与传输方案.md)

pub mod packet;
