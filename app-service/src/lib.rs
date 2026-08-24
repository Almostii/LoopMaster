//! 应用服务层：UI 与音频引擎之间的唯一入口（阶段 C / M1+）。
//!
//! UI 只允许调用本层：`DeviceRepository` / `ProcessRepository` /
//! `RouteEditor` / `EngineService`，以及 `DeviceModel` / `ProcessModel`
//! 视图模型。本层只依赖 audio-core 的模型与 audio-windows 的公开能力，
//! 不暴露 WASAPI 对象、`AudioEngine` worker 或实时线程数据。
//!
//! M1 范围：设备/进程枚举投影、路由编辑（增删 source/sink/send、增益、
//! 静音、启停、通道映射）、引擎命令（`EngineCommand`）、状态/统计事件
//! 订阅（`ServiceEvent`）、手动重连（`request_reconnect`）。预设与
//! schema version 属 M2 演进。

use loopmaster_audio_core::{
    AudioFormat, EndpointId, RouteGraph, RouteGraphError, RouteGraphSnapshot, SendSpec, SinkId,
    SinkSpec, SourceId, SourceSpec,
};
use loopmaster_audio_windows::{
    AudioEngine, AudioEngineConfig, AudioEngineError, AudioEngineState, AudioEngineStats,
    AudioEngineStatus, EndpointFlow, EndpointFormat, EndpointInfo, ProcessInfo, SampleEncoding,
    WindowsAudioBackend, WindowsAudioError, WindowsAudioFailureKind,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use thiserror::Error;

/// 服务事件轮询间隔：引擎状态/统计变化以有界频率投影为事件，避免
/// UI 直接轮询实时内部结构。
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);

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
    /// 启用/禁用一条 send。`enabled=false` 保留增益/静音/通道映射配置，
    /// 但整条 send 从混音计划跳过；与 `SetSendMuted`（混音增益静音）语义不同。
    SetSendEnabled {
        source_id: SourceId,
        sink_id: SinkId,
        enabled: bool,
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
            RouteEdit::SetSendEnabled {
                source_id,
                sink_id,
                enabled,
            } => {
                let send = self
                    .draft
                    .sends
                    .iter_mut()
                    .find(|s| s.source_id == source_id && s.sink_id == sink_id)
                    .ok_or_else(|| {
                        RouteGraphError::MissingSend(format!("{}->{}", source_id.0, sink_id.0))
                    })?;
                send.enabled = enabled;
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
// 引擎命令
// ---------------------------------------------------------------------------

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
        source_id: SourceId,
        sink_id: SinkId,
        gain_db: f32,
    },
    SetMuted {
        source_id: SourceId,
        sink_id: SinkId,
        muted: bool,
    },
    SetSendEnabled {
        source_id: SourceId,
        sink_id: SinkId,
        enabled: bool,
    },
}

// ---------------------------------------------------------------------------
// 服务事件
// ---------------------------------------------------------------------------

/// 服务层事件：引擎状态/统计变化与设备丢失/恢复通知。
///
/// 事件由服务内部的有界轮询线程产生（不暴露实时内部结构），UI 诊断页
/// 与状态徽标通过 `EngineService::subscribe` 接收。
#[derive(Clone, Debug, PartialEq)]
pub enum ServiceEvent {
    StateChanged(AudioEngineState),
    StatsChanged(AudioEngineStats),
    DeviceLost(EndpointId),
    DeviceRestored(EndpointId),
}

/// 服务事件订阅接收端。
pub type ServiceEventReceiver = mpsc::Receiver<ServiceEvent>;

// ---------------------------------------------------------------------------
// 引擎服务
// ---------------------------------------------------------------------------

struct EngineServiceInner {
    engine: Mutex<AudioEngine>,
    /// 最近一次成功提交的路由快照（send 级热更新的基准）。
    graph: Mutex<RouteGraphSnapshot>,
    subscribers: Mutex<Vec<mpsc::Sender<ServiceEvent>>>,
}

/// 应用服务：引擎的启动/停止/状态/路由提交/事件订阅入口。
pub struct EngineService {
    inner: Arc<EngineServiceInner>,
    event_thread: Option<JoinHandle<()>>,
    event_stop: Arc<AtomicBool>,
}

impl EngineService {
    /// 以初始路由图创建服务（引擎处于 Stopped，未启动）。
    pub fn new(graph: RouteGraph) -> Result<Self, ServiceError> {
        let snapshot = RouteGraphSnapshot::new(graph)?;
        let config = AudioEngineConfig::new(snapshot.clone());
        let engine = AudioEngine::new(config)?;
        let inner = Arc::new(EngineServiceInner {
            engine: Mutex::new(engine),
            graph: Mutex::new(snapshot),
            subscribers: Mutex::new(Vec::new()),
        });
        let event_stop = Arc::new(AtomicBool::new(false));
        let event_thread = spawn_event_loop(Arc::clone(&inner), Arc::clone(&event_stop));
        Ok(Self {
            inner,
            event_thread: Some(event_thread),
            event_stop,
        })
    }

    /// 当前状态 + 统计（线程安全快照）。
    pub fn status(&self) -> AudioEngineStatus {
        self.inner.engine.lock().expect("引擎锁未中毒").status()
    }

    /// 提交命令；非法命令返回错误，引擎状态不变。
    pub fn command(&self, command: EngineCommand) -> Result<(), ServiceError> {
        match command {
            EngineCommand::Start => self.inner.engine.lock().expect("引擎锁未中毒").start()?,
            EngineCommand::Stop => self.inner.engine.lock().expect("引擎锁未中毒").stop()?,
            EngineCommand::ApplyRoute(snapshot) => self.apply_route(snapshot)?,
            EngineCommand::SetGain {
                source_id,
                sink_id,
                gain_db,
            } => self.update_send(source_id, sink_id, |send| send.gain_db = gain_db)?,
            EngineCommand::SetMuted {
                source_id,
                sink_id,
                muted,
            } => self.update_send(source_id, sink_id, |send| send.muted = muted)?,
            EngineCommand::SetSendEnabled {
                source_id,
                sink_id,
                enabled,
            } => self.update_send(source_id, sink_id, |send| send.enabled = enabled)?,
        }
        Ok(())
    }

    /// 从 Degraded/Reconnecting/Failed 手动触发一次新的重试循环。
    ///
    /// 引擎运行正常或未启动时拒绝；重连通过停止并重建会话实现，当前图
    /// 保持不变。
    pub fn request_reconnect(&self) -> Result<(), ServiceError> {
        let state = self.status().state;
        match state {
            AudioEngineState::Stopped => {
                Err(ServiceError::NotReady("引擎未启动，无需重连".to_owned()))
            }
            AudioEngineState::Running => Err(ServiceError::Rejected {
                reason: "引擎运行正常，无需重连".to_owned(),
            }),
            AudioEngineState::Degraded
            | AudioEngineState::Reconnecting
            | AudioEngineState::Failed => {
                let mut engine = self.inner.engine.lock().expect("引擎锁未中毒");
                if engine.status().state != AudioEngineState::Stopped {
                    // 无论 supervisor 是否仍在自动重连，先停止旧会话再重建，
                    // 避免两个会话同时驱动同一 endpoint。
                    let _ = engine.stop();
                }
                engine.start()?;
                Ok(())
            }
        }
    }

    /// 订阅状态/统计/设备事件（诊断页与状态徽标实时刷新用）。
    ///
    /// 新订阅者立即收到一帧当前状态快照（`StateChanged` + `StatsChanged`），
    /// 之后收到增量事件，避免订阅瞬间漏掉已发生的变化。
    pub fn subscribe(&self) -> ServiceEventReceiver {
        let (sender, receiver) = mpsc::channel();
        let status = self.status();
        let _ = sender.send(ServiceEvent::StateChanged(status.state));
        let _ = sender.send(ServiceEvent::StatsChanged(status.stats));
        self.inner
            .subscribers
            .lock()
            .expect("订阅锁未中毒")
            .push(sender);
        receiver
    }

    /// 启动引擎（等价于 `command(EngineCommand::Start)`；兼容 M1 调用方）。
    pub fn start(&mut self) -> Result<(), ServiceError> {
        self.inner.engine.lock().expect("引擎锁未中毒").start()?;
        Ok(())
    }

    /// 停止引擎（等价于 `command(EngineCommand::Stop)`；兼容 M1 调用方）。
    pub fn stop(&mut self) -> Result<(), ServiceError> {
        self.inner.engine.lock().expect("引擎锁未中毒").stop()?;
        Ok(())
    }

    /// 提交整图变更；运行中只允许 send 级变更（拓扑变化需重启）。
    pub fn update_graph(&mut self, graph: RouteGraph) -> Result<(), ServiceError> {
        let snapshot = RouteGraphSnapshot::new(graph)?;
        self.apply_route(snapshot)
    }

    fn apply_route(&self, snapshot: RouteGraphSnapshot) -> Result<(), ServiceError> {
        self.inner
            .engine
            .lock()
            .expect("引擎锁未中毒")
            .update_graph(snapshot.clone())?;
        *self.inner.graph.lock().expect("路由锁未中毒") = snapshot;
        Ok(())
    }

    /// 对运行中引擎的一条 send 做热更新：修改暂存图 → 整图替换（block 边界
    /// 生效）。失败时暂存图不变，符合"非法命令不改变状态"。
    fn update_send<F>(
        &self,
        source_id: SourceId,
        sink_id: SinkId,
        update: F,
    ) -> Result<(), ServiceError>
    where
        F: FnOnce(&mut SendSpec),
    {
        let state = self.status().state;
        if state != AudioEngineState::Running {
            return Err(ServiceError::Rejected {
                reason: format!(
                    "引擎当前处于 {}，send 级命令仅对运行中的引擎生效",
                    state.as_str()
                ),
            });
        }
        let mut graph_guard = self.inner.graph.lock().expect("路由锁未中毒");
        let mut next_graph = graph_guard.graph().clone();
        let send = next_graph
            .sends
            .iter_mut()
            .find(|send| send.source_id == source_id && send.sink_id == sink_id)
            .ok_or_else(|| ServiceError::Rejected {
                reason: format!("send 不存在: {}->{}", source_id.0, sink_id.0),
            })?;
        update(send);
        let snapshot = RouteGraphSnapshot::new(next_graph)?;
        self.inner
            .engine
            .lock()
            .expect("引擎锁未中毒")
            .update_graph(snapshot.clone())?;
        *graph_guard = snapshot;
        Ok(())
    }
}

impl Drop for EngineService {
    fn drop(&mut self) {
        self.event_stop.store(true, Ordering::Release);
        if let Some(thread) = self.event_thread.take() {
            let _ = thread.join();
        }
    }
}

fn spawn_event_loop(inner: Arc<EngineServiceInner>, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    thread::Builder::new()
        .name("loopmaster-service-events".into())
        .spawn(move || {
            let mut last_state: Option<AudioEngineState> = None;
            let mut last_stats: Option<AudioEngineStats> = None;
            while !stop.load(Ordering::Acquire) {
                let status = inner.engine.lock().expect("引擎锁未中毒").status();
                if last_state != Some(status.state) {
                    publish_transition(&inner, last_state, status.state);
                    last_state = Some(status.state);
                }
                if last_stats != Some(status.stats) {
                    last_stats = Some(status.stats);
                    broadcast(&inner, ServiceEvent::StatsChanged(status.stats));
                }
                thread::sleep(EVENT_POLL_INTERVAL);
            }
        })
        .expect("创建服务事件线程失败")
}

fn publish_transition(
    inner: &EngineServiceInner,
    previous: Option<AudioEngineState>,
    current: AudioEngineState,
) {
    broadcast(inner, ServiceEvent::StateChanged(current));
    let graph = inner.graph.lock().expect("路由锁未中毒").graph().clone();
    let endpoints = graph_endpoints(&graph);
    let degraded = matches!(
        current,
        AudioEngineState::Degraded | AudioEngineState::Reconnecting
    );
    let was_running = matches!(previous, Some(AudioEngineState::Running));
    let restored = current == AudioEngineState::Running
        && matches!(
            previous,
            Some(
                AudioEngineState::Degraded
                    | AudioEngineState::Reconnecting
                    | AudioEngineState::Failed
            )
        );
    if degraded && was_running {
        for endpoint in &endpoints {
            broadcast(inner, ServiceEvent::DeviceLost(endpoint.clone()));
        }
    }
    if restored {
        for endpoint in &endpoints {
            broadcast(inner, ServiceEvent::DeviceRestored(endpoint.clone()));
        }
    }
}

/// 收集路由图中全部稳定 endpoint ID（用于设备丢失/恢复事件）。
fn graph_endpoints(graph: &RouteGraph) -> Vec<EndpointId> {
    let mut endpoints = Vec::new();
    for source in &graph.sources {
        if let Some(endpoint) = &source.endpoint_id {
            endpoints.push(endpoint.clone());
        }
    }
    for sink in &graph.sinks {
        endpoints.push(sink.endpoint_id.clone());
    }
    endpoints
}

fn broadcast(inner: &EngineServiceInner, event: ServiceEvent) {
    let mut subscribers = inner.subscribers.lock().expect("订阅锁未中毒");
    subscribers.retain(|sender| sender.send(event.clone()).is_ok());
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
            enabled: true,
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

    #[test]
    fn set_send_enabled_keeps_muted_and_channel_map() {
        let mut editor = editor();
        let mut expected = send("a", "out");
        expected.muted = true;
        expected.gain_db = -3.0;
        expected.channel_map = vec![(0, 0), (1, 1)];
        editor.apply(RouteEdit::SetSend(expected.clone())).unwrap();

        // 禁用 send：保留增益/静音/通道映射配置，仅 enabled 翻转。
        editor
            .apply(RouteEdit::SetSendEnabled {
                source_id: SourceId("a".into()),
                sink_id: SinkId("out".into()),
                enabled: false,
            })
            .unwrap();
        let disabled = editor
            .draft()
            .sends
            .iter()
            .find(|s| s.source_id.0 == "a")
            .unwrap();
        assert!(!disabled.enabled);
        assert!(disabled.muted);
        assert_eq!(disabled.gain_db, -3.0);
        assert_eq!(disabled.channel_map, vec![(0, 0), (1, 1)]);

        // 重新启用后配置原样恢复，与 muted（静音）语义明确区分。
        editor
            .apply(RouteEdit::SetSendEnabled {
                source_id: SourceId("a".into()),
                sink_id: SinkId("out".into()),
                enabled: true,
            })
            .unwrap();
        let re_enabled = editor
            .draft()
            .sends
            .iter()
            .find(|s| s.source_id.0 == "a")
            .unwrap();
        assert!(re_enabled.enabled);
        assert!(re_enabled.muted);
        assert_eq!(re_enabled.gain_db, -3.0);
        editor.commit().unwrap();
    }

    #[test]
    fn set_send_enabled_rejects_missing_send_without_mutating_draft() {
        let mut editor = editor();
        let before = editor.draft().clone();
        let error = editor
            .apply(RouteEdit::SetSendEnabled {
                source_id: SourceId("ghost".into()),
                sink_id: SinkId("out".into()),
                enabled: false,
            })
            .unwrap_err();
        assert_eq!(error, RouteGraphError::MissingSend("ghost->out".into()));
        assert_eq!(editor.draft(), &before);
    }

    #[test]
    fn send_commands_reject_when_engine_not_running() {
        let graph = RouteGraph {
            sources: vec![source("a")],
            sinks: vec![sink("out")],
            sends: vec![send("a", "out")],
        };
        let service = EngineService::new(graph).unwrap();
        let before = service.status();

        // send 级命令只对运行中的引擎生效；引擎 Stopped 时拒绝且状态不变。
        let gain_error = service
            .command(EngineCommand::SetGain {
                source_id: SourceId("a".into()),
                sink_id: SinkId("out".into()),
                gain_db: -6.0,
            })
            .unwrap_err();
        assert!(matches!(gain_error, ServiceError::Rejected { .. }));

        let unknown_send = service
            .command(EngineCommand::SetMuted {
                source_id: SourceId("ghost".into()),
                sink_id: SinkId("out".into()),
                muted: true,
            })
            .unwrap_err();
        assert!(matches!(unknown_send, ServiceError::Rejected { .. }));

        assert_eq!(service.status(), before);
    }

    #[test]
    fn apply_route_topology_change_requires_restart() {
        let graph = RouteGraph {
            sources: vec![source("a")],
            sinks: vec![sink("out")],
            sends: vec![send("a", "out")],
        };
        let service = EngineService::new(graph).unwrap();

        // 拓扑变化（新增 sink）必须显式重启，引擎返回 EndpointChangeRequiresRestart。
        let changed = RouteGraph {
            sources: vec![source("a")],
            sinks: vec![sink("out"), sink("second")],
            sends: vec![send("a", "out")],
        };
        let snapshot = RouteGraphSnapshot::new(changed).unwrap();
        let error = service
            .command(EngineCommand::ApplyRoute(snapshot))
            .unwrap_err();
        assert!(matches!(
            error,
            ServiceError::Engine(AudioEngineError::EndpointChangeRequiresRestart)
        ));
    }

    #[test]
    fn service_error_preserves_hresult_endpoint_and_hint() {
        let error = ServiceError::from(WindowsAudioError::HResult {
            operation: "IAudioClient::Initialize",
            hresult: -2,
            endpoint_id: Some("endpoint-1".into()),
        });
        assert_eq!(error.hresult(), Some(-2));
        assert_eq!(error.endpoint_id(), Some("endpoint-1"));
        assert!(error.hint().is_some());
        assert!(error.to_string().contains("HRESULT"));
    }

    #[test]
    fn service_error_gives_device_specific_hint_for_format_errors() {
        let error = ServiceError::from(WindowsAudioError::CaptureFormatUnsupported {
            endpoint_id: "mic".into(),
            sample_rate: 96_000,
            bits_per_sample: 24,
            channels: 2,
        });
        assert_eq!(error.hresult(), None);
        assert_eq!(error.endpoint_id(), Some("mic"));
        assert!(error.hint().unwrap().contains("格式"));
    }

    #[test]
    fn request_reconnect_rejects_when_engine_not_started() {
        let graph = RouteGraph {
            sources: vec![source("a")],
            sinks: vec![sink("out")],
            sends: vec![send("a", "out")],
        };
        let service = EngineService::new(graph).unwrap();
        assert!(matches!(
            service.request_reconnect().unwrap_err(),
            ServiceError::NotReady(_)
        ));
    }

    #[test]
    fn subscribe_delivers_initial_state_snapshot() {
        let graph = RouteGraph {
            sources: vec![source("a")],
            sinks: vec![sink("out")],
            sends: vec![send("a", "out")],
        };
        let service = EngineService::new(graph).unwrap();
        let receiver = service.subscribe();
        let first = receiver.recv_timeout(Duration::from_millis(500)).unwrap();
        assert_eq!(first, ServiceEvent::StateChanged(AudioEngineState::Stopped));
        let second = receiver.recv_timeout(Duration::from_millis(500)).unwrap();
        assert!(matches!(second, ServiceEvent::StatsChanged(_)));
    }
}
