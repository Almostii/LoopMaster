//! 服务层错误类型：包装引擎/后端错误并附加用户可读恢复建议。

use loopmaster_audio_core::RouteGraphError;
use loopmaster_audio_windows::{AudioEngineError, WindowsAudioError, WindowsAudioFailureKind};
use thiserror::Error;

/// 服务层错误：包装引擎/后端错误并附加用户可读恢复建议。
#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("{source}；建议：{hint}")]
    Windows {
        source: WindowsAudioError,
        /// 面向用户的中文恢复建议。
        hint: String,
    },
    #[error("引擎错误: {0}")]
    Engine(#[from] AudioEngineError),
    #[error("路由图错误: {0}")]
    Graph(#[from] RouteGraphError),
    #[error("服务未就绪: {0}")]
    NotReady(String),
    #[error("命令被拒绝: {reason}")]
    Rejected { reason: String },
}

impl ServiceError {
    /// 保留原始 HRESULT（仅 `Windows` 变体携带）。
    pub const fn hresult(&self) -> Option<i32> {
        match self {
            Self::Windows { source, .. } => source.hresult(),
            _ => None,
        }
    }

    /// 保留关联的 endpoint ID（仅 `Windows` 变体携带）。
    pub fn endpoint_id(&self) -> Option<&str> {
        match self {
            Self::Windows { source, .. } => source.endpoint_id(),
            _ => None,
        }
    }

    /// 面向用户的中文恢复建议（仅 `Windows` 变体携带）。
    pub fn hint(&self) -> Option<&str> {
        match self {
            Self::Windows { hint, .. } => Some(hint),
            _ => None,
        }
    }
}

impl From<WindowsAudioError> for ServiceError {
    fn from(source: WindowsAudioError) -> Self {
        let hint = match source.failure_kind() {
            WindowsAudioFailureKind::DeviceUnavailable => {
                "设备不可用：请检查设备连接与供电，确认后重试；设备可能正在被系统重新枚举。"
                    .to_owned()
            }
            WindowsAudioFailureKind::Other => recovery_hint(&source),
        };
        Self::Windows { source, hint }
    }
}

fn recovery_hint(error: &WindowsAudioError) -> String {
    match error {
        WindowsAudioError::RenderFormatUnsupported { .. }
        | WindowsAudioError::CaptureFormatUnsupported { .. } => {
            "设备格式不受当前音频边界支持：请选择 48/44.1 kHz、16-bit PCM 或 32-bit IEEE float 的设备。"
                .to_owned()
        }
        WindowsAudioError::ComInitialization { .. } => {
            "COM 初始化失败：请确认 Windows 音频服务正在运行，或重启应用后重试。".to_owned()
        }
        WindowsAudioError::InvalidFormat { .. } => {
            "设备格式信息无效：请更新音频驱动程序后重试。".to_owned()
        }
        WindowsAudioError::ProcessLoopbackInvalidPid { .. } => {
            "目标进程已退出或 PID 无效：请重新选择音源进程。".to_owned()
        }
        WindowsAudioError::RenderResample { .. } | WindowsAudioError::CaptureConvert { .. } => {
            "音频转换失败：请尝试更换输出设备后重试。".to_owned()
        }
        WindowsAudioError::RenderBlockFramesInvalid { .. }
        | WindowsAudioError::RenderInputUnaligned { .. }
        | WindowsAudioError::RenderBlockTooLarge { .. } => {
            "输出 block 配置或输入数据无效：请检查应用配置后重试。".to_owned()
        }
        _ => "操作失败：请查看错误详情后重试。".to_owned(),
    }
}
