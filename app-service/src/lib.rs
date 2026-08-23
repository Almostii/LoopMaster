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
    EndpointInfo, ProcessInfo, WindowsAudioBackend, WindowsAudioError, WindowsAudioFailureKind,
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
    /// 可作为 capture source（32-bit float / 2 声道，采样率自动重采样）。
    CaptureReady,
    /// 可作为 render sink（32-bit float / 2 声道，采样率自动重采样）。
    RenderReady,
    /// 不满足任一契约；reason 给出面向用户的原因。
    Unsupported { reason: String },
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
    pub compatibility: DeviceCompatibility,
    pub status: DeviceStatus,
}

impl DeviceModel {
    fn from_endpoint(info: &EndpointInfo) -> Self {
        let flow = match info.flow {
            EndpointFlow::Capture => DeviceFlow::Capture,
            EndpointFlow::Render => DeviceFlow::Render,
        };
        let (compatibility, status) = match info.endpoint_format() {
            Some(format) => match flow {
                DeviceFlow::Capture if format.capture_compatible() => {
                    (DeviceCompatibility::CaptureReady, DeviceStatus::Active)
                }
                DeviceFlow::Capture => (
                    DeviceCompatibility::Unsupported {
                        reason: "格式不满足 capture 契约（需 32-bit float / 2 声道）".into(),
                    },
                    DeviceStatus::Unsupported,
                ),
                DeviceFlow::Render if format.render_compatible() => {
                    (DeviceCompatibility::RenderReady, DeviceStatus::Active)
                }
                DeviceFlow::Render => (
                    DeviceCompatibility::Unsupported {
                        reason: "格式不满足 render 契约（需 32-bit float / 2 声道）".into(),
                    },
                    DeviceStatus::Unsupported,
                ),
            },
            None => (
                DeviceCompatibility::Unsupported {
                    reason: "无法读取设备格式".into(),
                },
                DeviceStatus::Unsupported,
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
            compatibility,
            status,
        }
    }
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
    fn device_projection_marks_capture_ready() {
        let info = EndpointInfo {
            id: EndpointId("cap".into()),
            name: "Mic".into(),
            flow: EndpointFlow::Capture,
            format: Some(AudioFormat {
                sample_rate: 44_100,
                channels: 2,
            }),
            bits_per_sample: Some(32),
            channel_mask: Some(3),
            is_float: Some(true),
        };
        let model = DeviceModel::from_endpoint(&info);
        assert_eq!(model.compatibility, DeviceCompatibility::CaptureReady);
        assert_eq!(model.status, DeviceStatus::Active);
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
