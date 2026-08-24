//! 应用服务层：UI 与音频引擎之间的唯一入口（阶段 C / M1）。
//!
//! UI 只允许调用本层：`DeviceRepository` / `ProcessRepository` /
//! `RouteEditor` / `EngineService`，以及 `DeviceModel` / `ProcessModel`
//! 视图模型。本层只依赖 audio-core 的模型与 audio-windows 的公开能力，
//! 不暴露 WASAPI 对象、`AudioEngine` worker 或实时线程数据。
//!
//! M1 范围：设备/进程枚举投影、路由编辑（增删 source/sink/send、增益、
//! 静音、通道映射）、引擎启动/停止/状态/路由提交。预设、schema version、
//! 事件订阅、send 启停属 M2 演进。

use loopmaster_audio_core::{
    AudioFormat, EndpointId, RouteGraph, RouteGraphError, RouteGraphSnapshot, SendSpec, SinkId,
    SinkSpec, SourceId, SourceSpec,
};
use loopmaster_audio_windows::{
    AudioEngine, AudioEngineConfig, AudioEngineError, AudioEngineStatus, EndpointFlow,
    EndpointFormat, EndpointInfo, ProcessInfo, SampleEncoding, WindowsAudioBackend,
    WindowsAudioError, WindowsAudioFailureKind,
};
use thiserror::Error;

// ---------------------------------------------------------------------------
// 服务层错误
// ---------------------------------------------------------------------------

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
}

impl From<WindowsAudioError> for ServiceError {
    fn from(source: WindowsAudioError) -> Self {
        let hint = match source.failure_kind() {
            WindowsAudioFailureKind::DeviceUnavailable => {
                "设备不可用：请检查设备连接，或等待系统重新枚举后重试。".to_owned()
            }
            WindowsAudioFailureKind::Other => "操作失败：请查看错误详情。".to_owned(),
        };
        Self::Windows { source, hint }
    }
}

// ---------------------------------------------------------------------------
// 设备模型
// ---------------------------------------------------------------------------

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

/// 设备运行状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceStatus {
    Active,
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

// ---------------------------------------------------------------------------
// 进程模型
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// 路由编辑
// ---------------------------------------------------------------------------

/// 路由编辑操作：应用后整体校验，任何一步失败都不落盘。
#[derive(Clone, Debug, PartialEq)]
pub enum RouteEdit {
    AddSource(SourceSpec),
    RemoveSource(SourceId),
    AddSink(SinkSpec),
    RemoveSink(SinkId),
    /// 新增或覆盖一条 send（含 gain/muted/channel_map）。
    SetSend(SendSpec),
    RemoveSend {
        source_id: SourceId,
        sink_id: SinkId,
    },
    SetSendGain {
        source_id: SourceId,
        sink_id: SinkId,
        gain_db: f32,
    },
    SetSendMuted {
        source_id: SourceId,
        sink_id: SinkId,
        muted: bool,
    },
    SetSendChannelMap {
        source_id: SourceId,
        sink_id: SinkId,
        channel_map: Vec<(u16, u16)>,
    },
}

/// 路由编辑会话：UI 编辑暂存配置，提交后整体校验并冻结为快照。
#[derive(Clone, Debug)]
pub struct RouteEditor {
    draft: RouteGraph,
}

impl RouteEditor {
    pub fn new(draft: RouteGraph) -> Self {
        Self { draft }
    }

    /// 当前暂存路由图（UI 渲染依据）。
    pub fn draft(&self) -> &RouteGraph {
        &self.draft
    }

    /// 应用一次原子编辑；非法编辑立即返回错误且 draft 不变。
    pub fn apply(&mut self, edit: RouteEdit) -> Result<(), RouteGraphError> {
        let previous = self.draft.clone();
        match edit {
            RouteEdit::AddSource(source) => self.draft.sources.push(source),
            RouteEdit::RemoveSource(id) => {
                if !self.draft.sources.iter().any(|s| s.id == id) {
                    return Err(RouteGraphError::MissingSource(id.0.clone()));
                }
                self.draft.sources.retain(|s| s.id != id);
                self.draft.sends.retain(|s| s.source_id != id);
            }
            RouteEdit::AddSink(sink) => self.draft.sinks.push(sink),
            RouteEdit::RemoveSink(id) => {
                if !self.draft.sinks.iter().any(|s| s.id == id) {
                    return Err(RouteGraphError::MissingSink(id.0.clone()));
                }
                self.draft.sinks.retain(|s| s.id != id);
                self.draft.sends.retain(|s| s.sink_id != id);
            }
            RouteEdit::SetSend(send) => {
                let key = (send.source_id.clone(), send.sink_id.clone());
                if let Some(existing) = self
                    .draft
                    .sends
                    .iter_mut()
                    .find(|s| (s.source_id.clone(), s.sink_id.clone()) == key)
                {
                    *existing = send;
                } else {
                    self.draft.sends.push(send);
                }
            }
            RouteEdit::RemoveSend { source_id, sink_id } => {
                self.draft
                    .sends
                    .retain(|s| !(s.source_id == source_id && s.sink_id == sink_id));
            }
            RouteEdit::SetSendGain {
                source_id,
                sink_id,
                gain_db,
            } => {
                let send = self
                    .draft
                    .sends
                    .iter_mut()
                    .find(|s| s.source_id == source_id && s.sink_id == sink_id)
                    .ok_or_else(|| {
                        RouteGraphError::MissingSend(format!("{}->{}", source_id.0, sink_id.0))
                    })?;
                send.gain_db = gain_db;
            }
            RouteEdit::SetSendMuted {
                source_id,
                sink_id,
                muted,
            } => {
                let send = self
                    .draft
                    .sends
                    .iter_mut()
                    .find(|s| s.source_id == source_id && s.sink_id == sink_id)
                    .ok_or_else(|| {
                        RouteGraphError::MissingSend(format!("{}->{}", source_id.0, sink_id.0))
                    })?;
                send.muted = muted;
            }
            RouteEdit::SetSendChannelMap {
                source_id,
                sink_id,
                channel_map,
            } => {
                let send = self
                    .draft
                    .sends
                    .iter_mut()
                    .find(|s| s.source_id == source_id && s.sink_id == sink_id)
                    .ok_or_else(|| {
                        RouteGraphError::MissingSend(format!("{}->{}", source_id.0, sink_id.0))
                    })?;
                send.channel_map = channel_map;
            }
        }
        if let Err(error) = self.draft.validate() {
            self.draft = previous;
            return Err(error);
        }
        Ok(())
    }

    /// 校验并通过不可变快照交给引擎；成功后 draft 与快照一致。
    pub fn commit(&self) -> Result<RouteGraphSnapshot, RouteGraphError> {
        RouteGraphSnapshot::new(self.draft.clone())
    }
}

// ---------------------------------------------------------------------------
// 引擎服务
// ---------------------------------------------------------------------------

/// 应用服务：引擎的启动/停止/状态/路由提交入口。
pub struct EngineService {
    engine: AudioEngine,
}

impl EngineService {
    /// 以初始路由图创建服务（引擎处于 Stopped，未启动）。
    pub fn new(graph: RouteGraph) -> Result<Self, ServiceError> {
        let snapshot = RouteGraphSnapshot::new(graph)?;
        let config = AudioEngineConfig::new(snapshot);
        let engine = AudioEngine::new(config)?;
        Ok(Self { engine })
    }

    /// 当前状态 + 统计（线程安全快照）。
    pub fn status(&self) -> AudioEngineStatus {
        self.engine.status()
    }

    pub fn start(&mut self) -> Result<(), ServiceError> {
        self.engine.start()?;
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), ServiceError> {
        self.engine.stop()?;
        Ok(())
    }

    /// 提交整图变更；运行中只允许 send 级变更（拓扑变化需重启）。
    pub fn update_graph(&mut self, graph: RouteGraph) -> Result<(), ServiceError> {
        let snapshot = RouteGraphSnapshot::new(graph)?;
        self.engine.update_graph(snapshot)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loopmaster_audio_windows::AudioEngineState;

    fn source(id: &str) -> SourceSpec {
        SourceSpec {
            id: SourceId(id.into()),
            kind: loopmaster_audio_core::SourceKind::ProcessLoopback,
            endpoint_id: None,
            process_id: Some(1),
            display_name: id.into(),
        }
    }

    fn sink(id: &str) -> SinkSpec {
        SinkSpec {
            id: SinkId(id.into()),
            endpoint_id: EndpointId(format!("endpoint-{id}")),
            display_name: id.into(),
        }
    }

    fn send(source_id: &str, sink_id: &str) -> SendSpec {
        SendSpec {
            source_id: SourceId(source_id.into()),
            sink_id: SinkId(sink_id.into()),
            gain_db: 0.0,
            muted: false,
            channel_map: Vec::new(),
        }
    }

    fn editor() -> RouteEditor {
        RouteEditor::new(RouteGraph {
            sources: vec![source("a"), source("b")],
            sinks: vec![sink("out")],
            sends: vec![send("a", "out"), send("b", "out")],
        })
    }

    fn endpoint(
        flow: EndpointFlow,
        sample_rate: u32,
        bits_per_sample: u16,
        channels: u16,
        is_float: bool,
        is_pcm: bool,
    ) -> EndpointInfo {
        EndpointInfo {
            id: EndpointId("device".into()),
            name: "Device".into(),
            flow,
            format: Some(AudioFormat {
                sample_rate,
                channels,
            }),
            bits_per_sample: Some(bits_per_sample),
            channel_mask: Some(if channels == 2 { 3 } else { 0 }),
            is_float: Some(is_float),
            is_pcm: Some(is_pcm),
        }
    }

    #[test]
    fn applies_gain_and_mute_to_existing_send() {
        let mut editor = editor();
        editor
            .apply(RouteEdit::SetSendGain {
                source_id: SourceId("a".into()),
                sink_id: SinkId("out".into()),
                gain_db: -6.0,
            })
            .unwrap();
        editor
            .apply(RouteEdit::SetSendMuted {
                source_id: SourceId("b".into()),
                sink_id: SinkId("out".into()),
                muted: true,
            })
            .unwrap();
        let draft = editor.draft();
        assert_eq!(draft.sends[0].gain_db, -6.0);
        assert!(draft.sends[1].muted);
        editor.commit().unwrap();
    }

    #[test]
    fn rejects_edits_on_missing_send_and_source() {
        let mut editor = editor();
        assert_eq!(
            editor
                .apply(RouteEdit::SetSendGain {
                    source_id: SourceId("ghost".into()),
                    sink_id: SinkId("out".into()),
                    gain_db: 0.0,
                })
                .unwrap_err(),
            RouteGraphError::MissingSend("ghost->out".into())
        );
        assert_eq!(
            editor
                .apply(RouteEdit::RemoveSource(SourceId("ghost".into())))
                .unwrap_err(),
            RouteGraphError::MissingSource("ghost".into())
        );
    }

    #[test]
    fn removing_source_cascades_sends() {
        let mut editor = editor();
        editor
            .apply(RouteEdit::RemoveSource(SourceId("a".into())))
            .unwrap();
        assert_eq!(editor.draft().sources.len(), 1);
        assert_eq!(editor.draft().sends.len(), 1);
        assert_eq!(editor.draft().sends[0].source_id, SourceId("b".into()));
        editor.commit().unwrap();
    }

    #[test]
    fn invalid_edit_does_not_mutate_draft() {
        let mut editor = editor();
        let before = editor.draft().clone();
        assert!(editor.apply(RouteEdit::AddSource(source("a"))).is_err());
        assert_eq!(editor.draft(), &before);
    }

    #[test]
    fn device_projection_marks_native_capture_ready() {
        let info = endpoint(EndpointFlow::Capture, 48_000, 32, 2, true, false);
        let model = DeviceModel::from_endpoint(&info);
        assert_eq!(model.compatibility, DeviceCompatibility::CaptureReady);
        assert_eq!(model.status, DeviceStatus::Active);
        assert_eq!(model.format_support, DeviceFormatSupport::Native);
        assert_eq!(
            model.native_format_description.as_deref(),
            Some("48 kHz / 32-bit IEEE float / 2 声道")
        );
    }

    #[test]
    fn device_projection_keeps_pcm_44k_mono_selectable_with_conversion() {
        let info = endpoint(EndpointFlow::Capture, 44_100, 16, 1, false, true);
        let model = DeviceModel::from_endpoint(&info);
        assert_eq!(model.compatibility, DeviceCompatibility::CaptureReady);
        assert_eq!(model.status, DeviceStatus::Active);
        assert_eq!(
            model.format_support,
            DeviceFormatSupport::ConversionRequired
        );
        assert_eq!(
            model.native_format_description.as_deref(),
            Some("44.1 kHz / 16-bit PCM / 1 声道")
        );
        assert!(model.format_support_reason.contains("转换为内部"));
    }

    #[test]
    fn device_projection_marks_multichannel_render_as_conversion() {
        let info = endpoint(EndpointFlow::Render, 48_000, 32, 6, true, false);
        let model = DeviceModel::from_endpoint(&info);
        assert_eq!(model.compatibility, DeviceCompatibility::RenderReady);
        assert_eq!(
            model.format_support,
            DeviceFormatSupport::ConversionRequired
        );
        assert!(model.format_support_reason.contains("转换后写入"));
    }

    #[test]
    fn device_projection_explains_unsupported_pcm_depth() {
        let info = endpoint(EndpointFlow::Render, 48_000, 24, 2, false, true);
        let model = DeviceModel::from_endpoint(&info);
        assert_eq!(model.status, DeviceStatus::Unsupported);
        assert_eq!(model.format_support, DeviceFormatSupport::Unsupported);
        assert_eq!(
            model.native_format_description.as_deref(),
            Some("48 kHz / 24-bit PCM / 2 声道")
        );
        assert!(matches!(
            model.compatibility,
            DeviceCompatibility::Unsupported { ref reason }
                if reason.contains("48 kHz / 24-bit PCM / 2 声道")
        ));
    }

    #[test]
    fn windows_error_includes_recovery_hint() {
        let error = ServiceError::from(WindowsAudioError::HResult {
            operation: "test",
            hresult: -1,
            endpoint_id: Some("endpoint".into()),
        });
        assert!(error.to_string().contains("建议："));
    }

    #[test]
    fn engine_service_creates_stopped_engine() {
        let graph = RouteGraph {
            sources: vec![source("a")],
            sinks: vec![sink("out")],
            sends: vec![send("a", "out")],
        };
        let service = EngineService::new(graph).unwrap();
        assert!(!service.status().running);
        assert_eq!(service.status().state, AudioEngineState::Stopped);
    }
}
