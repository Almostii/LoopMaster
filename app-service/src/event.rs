//! 服务层事件：引擎状态/统计变化与设备丢失/恢复通知。

use std::sync::mpsc;

use loopmaster_audio_core::EndpointId;
use loopmaster_audio_windows::{AudioEngineState, AudioEngineStats};

/// 服务层事件：引擎状态/统计变化与设备丢失/恢复通知。
///
/// 事件由服务内部的有界轮询线程产生（不暴露实时内部结构），UI 诊断页
/// 与状态徽标通过 `EngineService::subscribe` 接收。
///
/// `StatsChanged` 携带整份统计（含逐 send 峰值/RMS 映射），体积较大但事件
/// 频率有界（50ms 轮询），允许大枚举变体。
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ServiceEvent {
    StateChanged(AudioEngineState),
    StatsChanged(AudioEngineStats),
    DeviceLost(EndpointId),
    DeviceRestored(EndpointId),
}

/// 服务事件订阅接收端。
pub type ServiceEventReceiver = mpsc::Receiver<ServiceEvent>;
