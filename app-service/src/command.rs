//! 引擎原子命令：在非实时线程构造，交给引擎后立即返回。

use loopmaster_audio_core::{RouteGraphSnapshot, SendId};

/// 面向引擎的原子命令（在非实时线程构造，交给引擎后立即返回）。
///
/// 语义约定：`ApplyRoute` 为整图替换，拓扑变化（source/sink 集合）需要
/// 重启，引擎返回 `EndpointChangeRequiresRestart`；`SetGain`/`SetMuted`/
/// `SetSendEnabled` 是 send 级热更新，跳过整图拓扑校验，直接在 block 边界
/// 生效，仅对运行中的引擎有效。
#[derive(Clone, Debug, PartialEq)]
pub enum EngineCommand {
    Start,
    Stop,
    /// 整图替换（引擎在 block 边界应用）。
    ApplyRoute(RouteGraphSnapshot),
    SetGain {
        send_id: SendId,
        gain_db: f32,
    },
    SetMuted {
        send_id: SendId,
        muted: bool,
    },
    SetSendEnabled {
        send_id: SendId,
        enabled: bool,
    },
}
