//! 设备与进程视图模型：由 audio-windows 的公开能力投影得到，供 UI 消费。

use loopmaster_audio_core::{AudioFormat, EndpointId};
use loopmaster_audio_windows::{
    EndpointFlow, EndpointFormat, EndpointInfo, ProcessInfo, SampleEncoding, WindowsAudioBackend,
};

use crate::ServiceError;

/// 设备流向（应用层枚举，独立于 Windows 实现）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceFlow {
    Capture,
    Render,
}

impl DeviceFlow {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::Render => "render",
        }
    }
}

/// 设备对 LoopMaster 契约的兼容性结论。
#[derive(Clone, Debug, PartialEq)]
pub enum DeviceCompatibility {
    /// 可作为 capture source；必要的编码、采样率和声道转换由音频边界完成。
    CaptureReady,
    /// 可作为 render sink；必要的编码、采样率和声道转换由音频边界完成。
    RenderReady,
    /// 不满足任一契约；reason 给出面向用户的原因。
    Unsupported { reason: String },
}

/// endpoint 原生格式进入 LoopMaster 实时链路时所需的处理。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceFormatSupport {
    /// 原生格式已等于内部 48 kHz / 32-bit float / 2 声道格式。
    Native,
    /// endpoint 可用，但音频边界必须执行编码、采样率或声道转换。
    ConversionRequired,
    /// endpoint 格式已读取，但当前音频边界不能处理。
    Unsupported,
    /// 无法读取足够的 endpoint 格式信息。
    Unknown,
}

/// 设备在 LoopMaster 中的用途分类，用于前端分组展示，避免同一 capture 设备
/// 被同时渲染成「麦克风」和「设备回环」两份。
///
/// 注意：这一分类依据设备友好名称中的关键词推断，并非 Windows 官方的设备角色。
/// 物理话筒/线路输入归为 [`DeviceCategory::InputMic`]，把播放总混音当输入用的
/// 回环设备归为 [`DeviceCategory::InputLoopback`]，纯软件虚拟声卡归为
/// [`DeviceCategory::InputVirtual`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceCategory {
    /// 物理话筒 / 线路输入（Capture endpoint）。
    InputMic,
    /// 设备回环（通常是 Render endpoint 的 loopback 混音，名称带有 loopback/cable 等）。
    InputLoopback,
    /// 软件虚拟声卡（VB-Audio、Voicemeeter 等）。
    InputVirtual,
    /// 渲染输出设备。
    Output,
}

impl DeviceCategory {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InputMic => "input_mic",
            Self::InputLoopback => "input_loopback",
            Self::InputVirtual => "input_virtual",
            Self::Output => "output",
        }
    }

    /// 由设备名称推断分类。`flow` 用于区分渲染/捕获。
    pub fn classify(name: &str, flow: DeviceFlow) -> Self {
        if flow == DeviceFlow::Render {
            return Self::Output;
        }
        let lower = name.to_ascii_lowercase();
        if lower.contains("virtual")
            || lower.contains("vb-audio")
            || lower.contains("voicemeeter")
            || lower.contains("cable")
        {
            Self::InputVirtual
        } else if lower.contains("loop")
            || lower.contains("loopback")
            || lower.contains("回环")
            || lower.contains("环回")
        {
            Self::InputLoopback
        } else {
            Self::InputMic
        }
    }
}

/// 设备运行状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceStatus {
    Active,
    /// 设备失效/未接入（例如按稳定 ID 加载配置时设备已拔出）。
    Unavailable,
    Unsupported,
    Error,
}

/// 应用层设备模型：由 audio-windows 的 `EndpointInfo` 投影得到。
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceModel {
    pub id: EndpointId,
    pub name: String,
    pub flow: DeviceFlow,
    pub native_format: Option<AudioFormat>,
    pub bits_per_sample: Option<u16>,
    pub channel_mask: Option<u32>,
    pub is_float: Option<bool>,
    pub is_pcm: Option<bool>,
    /// 面向 UI 和日志的完整原生格式摘要。
    pub native_format_description: Option<String>,
    /// 原生格式是否需要转换，独立于设备能否被路由选择。
    pub format_support: DeviceFormatSupport,
    /// 为什么原生支持、需要转换或不受支持。
    pub format_support_reason: String,
    pub compatibility: DeviceCompatibility,
    pub status: DeviceStatus,
    /// 用途分类（麦克风 / 回环 / 虚拟 / 输出），用于前端分组。
    pub category: DeviceCategory,
}

impl DeviceModel {
    fn from_endpoint(info: &EndpointInfo) -> Self {
        let flow = match info.flow {
            EndpointFlow::Capture => DeviceFlow::Capture,
            EndpointFlow::Render => DeviceFlow::Render,
        };
        let endpoint_format = info.endpoint_format();
        let (compatibility, status, format_support, format_support_reason) = match endpoint_format {
            Some(format) => project_format_support(flow, format),
            None => (
                DeviceCompatibility::Unsupported {
                    reason: "无法读取设备格式，不能确认音频边界是否可安全处理".into(),
                },
                DeviceStatus::Unsupported,
                DeviceFormatSupport::Unknown,
                "无法读取编码、采样率、位深或声道信息".into(),
            ),
        };
        Self {
            id: info.id.clone(),
            name: info.name.clone(),
            flow,
            native_format: info.format,
            bits_per_sample: info.bits_per_sample,
            channel_mask: info.channel_mask,
            is_float: info.is_float,
            is_pcm: info.is_pcm,
            native_format_description: endpoint_format.map(endpoint_format_description),
            format_support,
            format_support_reason,
            compatibility,
            status,
            category: DeviceCategory::classify(&info.name, flow),
        }
    }
}

fn project_format_support(
    flow: DeviceFlow,
    format: EndpointFormat,
) -> (
    DeviceCompatibility,
    DeviceStatus,
    DeviceFormatSupport,
    String,
) {
    let compatible = match flow {
        DeviceFlow::Capture => format.capture_compatible(),
        DeviceFlow::Render => format.render_compatible(),
    };
    let description = endpoint_format_description(format);
    if !compatible {
        let reason = format!("当前音频边界不支持此 {} 格式：{description}", flow.as_str());
        return (
            DeviceCompatibility::Unsupported {
                reason: reason.clone(),
            },
            DeviceStatus::Unsupported,
            DeviceFormatSupport::Unsupported,
            reason,
        );
    }

    let compatibility = match flow {
        DeviceFlow::Capture => DeviceCompatibility::CaptureReady,
        DeviceFlow::Render => DeviceCompatibility::RenderReady,
    };
    if format.audio_format() == AudioFormat::INTERNAL
        && format.sample_encoding() == Some(SampleEncoding::Float32)
    {
        (
            compatibility,
            DeviceStatus::Active,
            DeviceFormatSupport::Native,
            "原生格式与内部 48 kHz / 32-bit IEEE float / 2 声道格式一致".into(),
        )
    } else {
        let direction = match flow {
            DeviceFlow::Capture => "转换为内部 48 kHz / 32-bit IEEE float / 2 声道",
            DeviceFlow::Render => "由内部 48 kHz / 32-bit IEEE float / 2 声道转换后写入",
        };
        (
            compatibility,
            DeviceStatus::Active,
            DeviceFormatSupport::ConversionRequired,
            format!("设备可用；原生格式为 {description}；将{direction}"),
        )
    }
}

fn endpoint_format_description(format: EndpointFormat) -> String {
    let sample_rate = if format.sample_rate.is_multiple_of(1_000) {
        format!("{} kHz", format.sample_rate / 1_000)
    } else if format.sample_rate.is_multiple_of(100) {
        format!("{:.1} kHz", format.sample_rate as f64 / 1_000.0)
    } else {
        format!("{} Hz", format.sample_rate)
    };
    let encoding = match format.sample_encoding() {
        Some(SampleEncoding::Float32) => "32-bit IEEE float".to_owned(),
        Some(SampleEncoding::Pcm16) => "16-bit PCM".to_owned(),
        None if format.is_float => format!("{}-bit IEEE float", format.bits_per_sample),
        None if format.is_pcm => format!("{}-bit PCM", format.bits_per_sample),
        None => format!("{}-bit 未知编码", format.bits_per_sample),
    };
    format!("{sample_rate} / {encoding} / {} 声道", format.channels)
}

/// 设备模型统一枚举入口。
pub struct DeviceRepository {
    backend: WindowsAudioBackend,
}

impl DeviceRepository {
    pub fn new() -> Result<Self, ServiceError> {
        Ok(Self {
            backend: WindowsAudioBackend::new()?,
        })
    }

    /// 全量枚举（每次刷新都走同一能力模型）。
    pub fn list_devices(&self) -> Result<Vec<DeviceModel>, ServiceError> {
        let endpoints = self.backend.enumerate_endpoints()?;
        Ok(endpoints.iter().map(DeviceModel::from_endpoint).collect())
    }

    /// 按稳定 ID 取单台设备。
    pub fn find_device(&self, id: &EndpointId) -> Result<Option<DeviceModel>, ServiceError> {
        Ok(self
            .list_devices()?
            .into_iter()
            .find(|device| &device.id == id))
    }
}

/// 应用层进程模型（Process Loopback 目标）。
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessModel {
    pub pid: u32,
    pub name: String,
    pub executable_path: Option<String>,
}

impl ProcessModel {
    fn from_info(info: &ProcessInfo) -> Self {
        Self {
            pid: info.pid,
            name: info.name.clone(),
            executable_path: info.executable_path.clone(),
        }
    }
}

/// 音频进程枚举入口。
pub struct ProcessRepository {
    backend: WindowsAudioBackend,
}

impl ProcessRepository {
    pub fn new() -> Result<Self, ServiceError> {
        Ok(Self {
            backend: WindowsAudioBackend::new()?,
        })
    }

    /// 枚举当前存在音频会话的进程。
    pub fn list_audio_processes(&self) -> Result<Vec<ProcessModel>, ServiceError> {
        let processes = self.backend.enumerate_processes()?;
        Ok(processes.iter().map(ProcessModel::from_info).collect())
    }

    /// 按 PID 取单个进程。
    pub fn find_process(&self, pid: u32) -> Result<Option<ProcessModel>, ServiceError> {
        Ok(self
            .list_audio_processes()?
            .into_iter()
            .find(|process| process.pid == pid))
    }
}
