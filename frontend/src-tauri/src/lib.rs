//! LoopMaster 前端 Tauri 壳层 — command/event 适配层。
//!
//! 这是前端与 Rust 应用服务（app-service）之间的唯一命令/事件边界。React
//! 只维护展示模型和用户意图；WASAPI 枚举、引擎控制都在 Tauri command 执行
//! 的后台线程完成，不阻塞 UI 主线程，也不把实时音频结构暴露给前端。
//!
//! 本阶段（阶段 B/C）实现命令/事件闭环：
//! - 只读命令：`list_devices`、`list_audio_processes`、`get_route_snapshot`；
//! - 引擎控制：`start_engine`、`stop_engine`、`request_reconnect`、
//!   `apply_route_edit`（拓扑变化会返回“需要重启”）；
//! - 路由增强（阶段 C）：`set_source_name`、`set_output_channel_name`、
//!   `set_external_output_name`（节点重命名，在壳层通过重建编辑图实现）、
//!   `set_send_channel_map`（send 通道映射）；
//! - 事件：`engine-state-changed`、`engine-stats-changed`、
//!   `device-lost`、`device-restored`、`service-error`。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use serde::{Deserialize, Serialize};
use tauri::Manager;

use loopmaster_app_service::{
    DeviceFlow, DeviceModel, DeviceRepository, EngineCommand, EngineService, ProcessModel,
    ProcessRepository, RouteEdit, RouteEditor, ServiceError, ServiceEvent,
};
use loopmaster_app_service::{AppConfig, ConfigError};
use loopmaster_audio_core::{
    BusId, BusSpec, EndpointId, RouteGraph, RouteGraphError, SendId, SendSpec, SinkId, SinkSpec,
    SourceId, SourceKind, SourceSpec,
};
use loopmaster_audio_windows::{AudioEngineState, AudioEngineStats, AudioEngineStatus};
use tauri::Emitter;

// ---------------------------------------------------------------------------
// 前端 DTO（稳定、可审查，不直接暴露 Windows/引擎内部类型）
// ---------------------------------------------------------------------------

/// 设备概要。
#[derive(Clone, Serialize)]
struct DeviceBrief {
    id: String,
    name: String,
    flow: &'static str,
    category: &'static str,
    compatibility: String,
    status: String,
    format_description: Option<String>,
}

/// 音频进程概要（Process Loopback 目标）。
#[derive(Clone, Serialize)]
struct ProcessBrief {
    pid: u32,
    name: String,
    executable_path: Option<String>,
}

/// send（连接）视图模型，覆盖启用/静音/增益/通道映射。
#[derive(Clone, Serialize)]
struct SendBrief {
    id: String,
    source: Option<String>,
    output_channel: Option<String>,
    external_output: Option<String>,
    enabled: bool,
    muted: bool,
    gain_db: f32,
    channel_map: Vec<[u16; 2]>,
}

/// Route Profile 视图模型：Sources、Output Channels、External Outputs。
/// 不暴露内部 Bus/Sink 为产品概念，但 send 需能指回其两端。
#[derive(Clone, Serialize)]
struct RouteProfileSnapshot {
    sources: Vec<SourceBrief>,
    output_channels: Vec<ChannelBrief>,
    external_outputs: Vec<ExternalOutputBrief>,
    sends: Vec<SendBrief>,
}

#[derive(Clone, Serialize)]
struct SourceBrief {
    id: String,
    kind: String,
    display_name: String,
    endpoint_id: Option<String>,
    process_id: Option<u32>,
}

#[derive(Clone, Serialize)]
struct ChannelBrief {
    id: String,
    display_name: String,
}

#[derive(Clone, Serialize)]
struct ExternalOutputBrief {
    id: String,
    endpoint_id: String,
    display_name: String,
}

/// 引擎状态视图。
#[derive(Clone, Serialize)]
struct EngineStateBrief {
    state: &'static str,
    running: bool,
    failed: bool,
    last_error: Option<String>,
}

/// 引擎统计视图（有界快照，供状态徽标/诊断展示）。
#[derive(Clone, Serialize)]
struct EngineStatsBrief {
    capture_packets: u64,
    captured_frames: u64,
    rendered_frames: u64,
    render_writes: u64,
    fifo_overflows: u64,
    fifo_underflows: u64,
    discontinuities: u64,
    reconnect_attempts: u64,
    captured_peak: f32,
    /// 每条 send 的逐通道（L/R）峰值，键为 send id，值为 `[left, right]`（0.0~1.0）。
    send_peaks: std::collections::HashMap<String, Vec<f32>>,
}

/// 统一服务错误视图（保留分类、endpoint ID、HRESULT 与中文建议）。
#[derive(Clone, Serialize)]
struct ServiceErrorBrief {
    category: &'static str,
    message: String,
    endpoint_id: Option<String>,
    hresult: Option<i32>,
    hint: Option<String>,
}

/// 前端发起的路由编辑意图。
#[derive(Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum RouteEditRequest {
    AddSource {
        id: String,
        kind: String,
        display_name: String,
        endpoint_id: Option<String>,
        process_id: Option<u32>,
    },
    RemoveSource {
        id: String,
    },
    AddOutputChannel {
        id: String,
        display_name: String,
    },
    RemoveOutputChannel {
        id: String,
    },
    AddExternalOutput {
        id: String,
        endpoint_id: String,
        display_name: String,
    },
    RemoveExternalOutput {
        id: String,
    },
    AddSend {
        id: String,
        source_id: String,
        output_channel_id: String,
    },
    AddSendToOutput {
        id: String,
        output_channel_id: String,
        external_output_id: String,
    },
    RemoveSend {
        id: String,
    },
    SetSendEnabled {
        id: String,
        enabled: bool,
    },
    SetSendMuted {
        id: String,
        muted: bool,
    },
    SetSendGain {
        id: String,
        gain_db: f32,
    },
    SetSourceName {
        id: String,
        display_name: String,
    },
    SetOutputChannelName {
        id: String,
        display_name: String,
    },
    SetExternalOutputName {
        id: String,
        display_name: String,
    },
    SetSendChannelMap {
        id: String,
        channel_map: Vec<[u16; 2]>,
    },
}

// ---------------------------------------------------------------------------
// Tauri 托管状态
// ---------------------------------------------------------------------------

/// 全局应用状态：暂存路由编辑器 + 惰性创建的引擎服务 + 配置持久化路径。
///
/// 引擎在首次 `start_engine` 时才创建（`RouteGraph` 至少需要一个 source 和
/// 一个 sink，空图不能初始化引擎），此后复用同一实例直到进程退出。
///
/// `config_path` 为自动保存的目标配置文件（位于 Tauri `app_config_dir`
/// 下的 `config.json`）；路径在启动时解析一次，命令层只负责读写。
struct AppState {
    editor: Mutex<RouteEditor>,
    engine: Mutex<Option<EngineService>>,
    config_path: PathBuf,
}

impl AppState {
    fn new(config_path: PathBuf) -> Self {
        Self {
            editor: Mutex::new(RouteEditor::new(RouteGraph::default())),
            engine: Mutex::new(None),
            config_path,
        }
    }
}

/// 解析配置文件路径：`<app_config_dir>/config.json`。
///
/// `app_config_dir` 由 Tauri 按平台返回标准配置目录（如 Windows 的
/// `%APPDATA%/LoopMaster`）。目录不存在时尝试创建，失败则回退到当前目录，
/// 保证命令层始终有可用路径。
fn resolve_config_path(app: &tauri::AppHandle) -> PathBuf {
    let dir: std::path::PathBuf = app
        .path()
        .app_config_dir()
        .ok()
        .filter(|p: &std::path::PathBuf| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("创建配置目录失败，回退到当前目录: {e}");
        return std::path::PathBuf::from("config.json");
    }
    dir.join("config.json")
}

/// 首次启动引擎时创建服务，并派生事件转发线程；重复调用返回 `false`。
fn ensure_engine(app: &tauri::AppHandle, state: &AppState) -> Result<bool, ServiceError> {
    let mut engine_slot = state.engine.lock().expect("引擎锁未中毒");
    if engine_slot.is_some() {
        return Ok(false);
    }
    let editor = state.editor.lock().expect("路由锁未中毒");
    let service = EngineService::new(editor.draft().clone())?;
    drop(editor);

    let receiver = service.subscribe();
    let handle = app.clone();
    thread::Builder::new()
        .name("loopmaster-tauri-events".into())
        .spawn(move || {
            for event in receiver {
                forward_event(&handle, event);
            }
        })
        .expect("创建事件转发线程失败");
    *engine_slot = Some(service);
    Ok(true)
}

fn forward_event(app: &tauri::AppHandle, event: ServiceEvent) {
    let (name, payload) = match event {
        ServiceEvent::StateChanged(s) => ("engine-state-changed", serialize_state(s)),
        ServiceEvent::StatsChanged(stats) => ("engine-stats-changed", serialize_stats(stats)),
        ServiceEvent::DeviceLost(id) => ("device-lost", serde_json::json!({ "endpoint_id": id.0 })),
        ServiceEvent::DeviceRestored(id) => (
            "device-restored",
            serde_json::json!({ "endpoint_id": id.0 }),
        ),
    };
    let _ = app.emit(name, payload);
}

// ---------------------------------------------------------------------------
// 只读命令
// ---------------------------------------------------------------------------

/// 连通性测试。
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {name}! You've been greeted from Rust!")
}

/// 枚举设备（后台执行，不阻塞 UI）。
#[tauri::command]
fn list_devices() -> Result<Vec<DeviceBrief>, ServiceErrorBrief> {
    let repository = DeviceRepository::new().map_err(service_error_brief)?;
    let devices = repository.list_devices().map_err(service_error_brief)?;
    Ok(devices.iter().map(DeviceBrief::from_model).collect())
}

/// 枚举当前存在音频会话的进程（Process Loopback 来源）。
#[tauri::command]
fn list_audio_processes() -> Result<Vec<ProcessBrief>, ServiceErrorBrief> {
    let repository = ProcessRepository::new().map_err(service_error_brief)?;
    let processes = repository
        .list_audio_processes()
        .map_err(service_error_brief)?;
    Ok(processes.iter().map(ProcessBrief::from_model).collect())
}

/// 返回进程可执行文件图标的 PNG data URI；无图标或平台不支持时返回 `None`。
#[tauri::command]
fn process_icon_data_uri(executable_path: String) -> Option<String> {
    loopmaster_audio_windows::process_icon_data_uri(&executable_path)
}

/// 当前 Route Profile 视图模型（只读快照）。
#[tauri::command]
fn get_route_snapshot(
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<RouteProfileSnapshot, String> {
    let editor = state
        .editor
        .lock()
        .map_err(|_| "路由编辑器锁中毒".to_owned())?;
    Ok(RouteProfileSnapshot::from_graph(editor.draft()))
}

/// 引擎尚未创建时的默认 Stopped 状态。
fn stopped_status() -> AudioEngineStatus {
    AudioEngineStatus {
        state: AudioEngineState::Stopped,
        running: false,
        failed: false,
        last_error: None,
        stats: AudioEngineStats::default(),
    }
}

/// 当前引擎状态（只读快照）。引擎尚未创建时返回 Stopped。
#[tauri::command]
fn get_engine_state(state: tauri::State<'_, Arc<AppState>>) -> EngineStateBrief {
    let status = match &*state.engine.lock().expect("引擎锁未中毒") {
        Some(engine) => engine.status(),
        None => stopped_status(),
    };
    EngineStateBrief::from_status(status)
}

/// 当前引擎统计（只读快照）。
#[tauri::command]
fn get_engine_stats(state: tauri::State<'_, Arc<AppState>>) -> EngineStatsBrief {
    let status = match &*state.engine.lock().expect("引擎锁未中毒") {
        Some(engine) => engine.status(),
        None => stopped_status(),
    };
    EngineStatsBrief::from_status(status)
}

// ---------------------------------------------------------------------------
// 引擎控制命令
// ---------------------------------------------------------------------------

/// 启动引擎。每次启动都使用当前编辑器暂存路由重建 EngineService 并启动，
/// 以保证引擎图与编辑器一致；`update_graph` 依赖运行中的 supervisor 写入
/// `graph_tx`，但 supervisor 仅在 `Start` 后建立 `graph_tx`，因此不能在
/// `Start` 之前调用 `ApplyRoute`（否则会收到 `AudioEngineError::NotRunning`）。
///
/// 旧实例若存在则先 stop 并丢弃，避免两次启动间图不一致。
#[tauri::command]
fn start_engine(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<(), ServiceErrorBrief> {
    // 1) 若已有引擎实例，先停止并丢弃，确保用最新图重建。
    {
        let mut engine_slot = state.engine.lock().expect("引擎锁未中毒");
        if let Some(old) = engine_slot.take() {
            drop(engine_slot);
            let _ = old.command(EngineCommand::Stop);
        }
    }
    // 2) 用当前编辑器草图创建 EngineService 并启动。
    ensure_engine(&app, &state).map_err(service_error_brief)?;
    let engine = state.engine.lock().expect("引擎锁未中毒");
    let engine = engine.as_ref().expect("引擎已创建");
    engine
        .command(EngineCommand::Start)
        .map_err(service_error_brief)
}

/// 停止引擎。引擎尚未创建时返回错误。
#[tauri::command]
fn stop_engine(state: tauri::State<'_, Arc<AppState>>) -> Result<(), ServiceErrorBrief> {
    let engine = state.engine.lock().expect("引擎锁未中毒");
    match engine.as_ref() {
        Some(engine) => engine
            .command(EngineCommand::Stop)
            .map_err(service_error_brief),
        None => Err(ServiceErrorBrief {
            category: "not_ready",
            message: "引擎尚未启动".into(),
            endpoint_id: None,
            hresult: None,
            hint: Some("请先启动引擎".into()),
        }),
    }
}

/// 从 Degraded/Reconnecting/Failed 手动触发重连。引擎尚未创建时返回错误。
#[tauri::command]
fn request_reconnect(state: tauri::State<'_, Arc<AppState>>) -> Result<(), ServiceErrorBrief> {
    let engine = state.engine.lock().expect("引擎锁未中毒");
    match engine.as_ref() {
        Some(engine) => engine.request_reconnect().map_err(service_error_brief),
        None => Err(ServiceErrorBrief {
            category: "not_ready",
            message: "引擎尚未启动".into(),
            endpoint_id: None,
            hresult: None,
            hint: Some("请先启动引擎".into()),
        }),
    }
}

/// 应用一次路由编辑（写入暂存图并校验）。拓扑变化需重启会在
/// `apply_route_edit` 的返回值或后续状态中体现，前端不得静默丢弃修改。
///
/// send 级热更新（`SetSendEnabled`/`SetSendMuted`/`SetSendGain`）除写入草稿外，
/// 若引擎正在运行还会转发为 `EngineCommand` 立即生效；非运行态仅更新草稿，
/// 下次 `start_engine` 基于草稿重建引擎时生效。
#[tauri::command]
fn apply_route_edit(
    state: tauri::State<'_, Arc<AppState>>,
    request: RouteEditRequest,
) -> Result<(), ServiceErrorBrief> {
    // 1) 写入暂存图（编辑器草稿），保持与拓扑/显示一致。
    // 锁范围收窄到仅改草稿，避免与引擎锁形成循环等待（start_engine 为 engine→editor）。
    {
        let mut editor = state
            .editor
            .lock()
            .map_err(|_| ServiceErrorBrief::lock_poisoned())?;
        // 节点重命名在壳层处理：app-service 的 RouteEdit 不直接支持 display_name
        // 覆盖，故重建编辑图（仅改 display_name，不影响拓扑）。其余 op 走标准映射。
        match &request {
            RouteEditRequest::SetSourceName { id, display_name } => {
                apply_rename(&mut editor, RenameTarget::Source, id, display_name.clone())
                    .map_err(|e| ServiceErrorBrief::graph(e.to_string()))?;
            }
            RouteEditRequest::SetOutputChannelName { id, display_name } => {
                apply_rename(
                    &mut editor,
                    RenameTarget::OutputChannel,
                    id,
                    display_name.clone(),
                )
                .map_err(|e| ServiceErrorBrief::graph(e.to_string()))?;
            }
            RouteEditRequest::SetExternalOutputName { id, display_name } => {
                apply_rename(
                    &mut editor,
                    RenameTarget::ExternalOutput,
                    id,
                    display_name.clone(),
                )
                .map_err(|e| ServiceErrorBrief::graph(e.to_string()))?;
            }
            _ => {
                let edit =
                    request_to_route_edit(request.clone()).map_err(ServiceErrorBrief::graph)?;
                editor
                    .apply(edit)
                    .map_err(|e| ServiceErrorBrief::graph(e.to_string()))?;
            }
        }
    }

    // 2) send 级热更新转发到运行中的引擎（仅 Running 态，否则草稿已在步骤 1 更新）。
    forward_send_to_engine(&state, &request)
}

/// 将 send 级路由编辑转发给运行中的引擎，使其立即生效。
///
/// 仅对 `Running` 引擎执行热更新；引擎未创建或未运行时不转发（草稿已在
/// `apply_route_edit` 步骤 1 更新，下次 `start_engine` 会基于草稿重建引擎并生效）。
///
/// 注意：`SetSendChannelMap` 暂无对应的 `EngineCommand` 热更新变体，故走整图
/// 替换路径（重启生效），此处不转发。
fn forward_send_to_engine(
    state: &AppState,
    request: &RouteEditRequest,
) -> Result<(), ServiceErrorBrief> {
    let command = match request {
        RouteEditRequest::SetSendEnabled { id, enabled } => Some(EngineCommand::SetSendEnabled {
            send_id: SendId(id.clone()),
            enabled: *enabled,
        }),
        RouteEditRequest::SetSendMuted { id, muted } => Some(EngineCommand::SetMuted {
            send_id: SendId(id.clone()),
            muted: *muted,
        }),
        RouteEditRequest::SetSendGain { id, gain_db } => Some(EngineCommand::SetGain {
            send_id: SendId(id.clone()),
            gain_db: *gain_db,
        }),
        _ => None,
    };
    let command = match command {
        Some(c) => c,
        None => return Ok(()),
    };
    let engine_slot = state
        .engine
        .lock()
        .map_err(|_| ServiceErrorBrief::lock_poisoned())?;
    let engine = match engine_slot.as_ref() {
        Some(e) => e,
        None => return Ok(()), // 引擎尚未创建
    };
    if engine.status().state != AudioEngineState::Running {
        return Ok(()); // 未运行：草稿已更新，下次启动生效
    }
    engine.command(command).map_err(service_error_brief)
}

// ---------------------------------------------------------------------------
// 配置持久化命令（阶段 D：自动保存当前路由，启动加载上次配置）
// ---------------------------------------------------------------------------

/// 把当前编辑器草稿保存为配置文件（原子写入）。
///
/// 保存的是**草稿图**（`RouteEditor.draft()`），即当前 UI 展示的拓扑，与引擎
/// 是否运行无关；引擎运行中的热更新也已同步进草稿，故保存即反映最新状态。
#[tauri::command]
fn save_config(
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<(), ServiceErrorBrief> {
    let editor = state
        .editor
        .lock()
        .map_err(|_| ServiceErrorBrief::lock_poisoned())?;
    let config = AppConfig::new(editor.draft().clone());
    drop(editor);
    config
        .save_to(&state.config_path)
        .map_err(config_error_brief)
}

/// 从配置文件加载路由，替换当前编辑器草稿。
///
/// 文件不存在（`ConfigError::NotFound`）时返回 `Ok(false)`，表示无需加载，
/// 交由前端决定是否建立默认拓扑；其余错误（损坏/版本不支持/校验失败）返回
/// `Err` 以便前端提示。加载成功后标记缺失设备，后续 UI 按 endpoint 可用性
/// 决定是否自动启动引擎。
#[tauri::command]
fn load_config(
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<bool, ServiceErrorBrief> {
    let config = match AppConfig::load_from(&state.config_path) {
        Ok(config) => config,
        Err(ConfigError::NotFound(_)) => return Ok(false),
        Err(e) => return Err(config_error_brief(e)),
    };
    let graph = config.graph;
    let mut editor = state
        .editor
        .lock()
        .map_err(|_| ServiceErrorBrief::lock_poisoned())?;
    *editor = RouteEditor::new(graph);
    Ok(true)
}

/// 可重命名节点类型。
#[derive(Clone, Copy)]
enum RenameTarget {
    Source,
    OutputChannel,
    ExternalOutput,
}

/// 重命名节点：在编辑图副本上覆盖 `display_name` 并整体校验后重建编辑器。
///
/// app-service 的 `RouteEdit` 没有覆盖节点 `display_name` 的变体，且根 workspace
/// 契约不可修改，因此此处通过 `RouteEditor::new` 重建编辑图（仅显示字段变化，
/// 不改变拓扑与 send 关系）。
fn apply_rename(
    editor: &mut RouteEditor,
    target: RenameTarget,
    id: &str,
    display_name: String,
) -> Result<(), RouteGraphError> {
    let mut graph = editor.draft().clone();
    let mut found = false;
    match target {
        RenameTarget::Source => {
            for source in graph.sources.iter_mut() {
                if source.id.0 == id {
                    source.display_name = display_name;
                    found = true;
                    break;
                }
            }
        }
        RenameTarget::OutputChannel => {
            for bus in graph.buses.iter_mut() {
                if bus.id.0 == id {
                    bus.display_name = display_name;
                    found = true;
                    break;
                }
            }
        }
        RenameTarget::ExternalOutput => {
            for sink in graph.sinks.iter_mut() {
                if sink.id.0 == id {
                    sink.display_name = display_name;
                    found = true;
                    break;
                }
            }
        }
    }
    if !found {
        return Err(match target {
            RenameTarget::Source => RouteGraphError::MissingSource(id.to_owned()),
            RenameTarget::OutputChannel => RouteGraphError::MissingBus(id.to_owned()),
            RenameTarget::ExternalOutput => RouteGraphError::MissingSink(id.to_owned()),
        });
    }
    graph.validate()?;
    *editor = RouteEditor::new(graph);
    Ok(())
}

// ---------------------------------------------------------------------------
// 事件序列化
// ---------------------------------------------------------------------------

fn serialize_state(state: AudioEngineState) -> serde_json::Value {
    serde_json::json!({
        "state": state.as_str(),
        "running": state == AudioEngineState::Running,
    })
}

fn serialize_stats(stats: AudioEngineStats) -> serde_json::Value {
    serde_json::json!({
        "capture_packets": stats.capture_packets,
        "captured_frames": stats.captured_frames,
        "rendered_frames": stats.rendered_frames,
        "render_writes": stats.render_writes,
        "fifo_overflows": stats.fifo_overflows,
        "fifo_underflows": stats.fifo_underflows,
        "discontinuities": stats.discontinuities,
        "reconnect_attempts": stats.reconnect_attempts,
        "captured_peak": stats.captured_peak,
        "send_peaks": stats
            .send_peaks
            .iter()
            .map(|(id, peaks)| (id.clone(), vec![peaks[0], peaks[1]]))
            .collect::<std::collections::HashMap<_, _>>(),
    })
}

// ---------------------------------------------------------------------------
// 投影辅助
// ---------------------------------------------------------------------------

impl DeviceBrief {
    fn from_model(model: &DeviceModel) -> Self {
        use loopmaster_app_service::DeviceCompatibility;
        let compatibility = match &model.compatibility {
            DeviceCompatibility::CaptureReady => "capture_ready",
            DeviceCompatibility::RenderReady => "render_ready",
            DeviceCompatibility::Unsupported { .. } => "unsupported",
        };
        Self {
            id: model.id.0.clone(),
            name: model.name.clone(),
            flow: match model.flow {
                DeviceFlow::Capture => "capture",
                DeviceFlow::Render => "render",
            },
            category: model.category.as_str(),
            compatibility: compatibility.to_string(),
            status: device_status_str(model.status).to_string(),
            format_description: model.native_format_description.clone(),
        }
    }
}

fn device_status_str(status: loopmaster_app_service::DeviceStatus) -> &'static str {
    use loopmaster_app_service::DeviceStatus;
    match status {
        DeviceStatus::Active => "active",
        DeviceStatus::Unavailable => "unavailable",
        DeviceStatus::Unsupported => "unsupported",
        DeviceStatus::Error => "error",
    }
}

impl ProcessBrief {
    fn from_model(model: &ProcessModel) -> Self {
        Self {
            pid: model.pid,
            name: model.name.clone(),
            executable_path: model.executable_path.clone(),
        }
    }
}

impl EngineStateBrief {
    fn from_status(status: AudioEngineStatus) -> Self {
        Self {
            state: status.state.as_str(),
            running: status.running,
            failed: status.failed,
            last_error: status.last_error,
        }
    }
}

impl EngineStatsBrief {
    fn from_status(status: AudioEngineStatus) -> Self {
        let stats = status.stats;
        Self {
            capture_packets: stats.capture_packets,
            captured_frames: stats.captured_frames,
            rendered_frames: stats.rendered_frames,
            render_writes: stats.render_writes,
            fifo_overflows: stats.fifo_overflows,
            fifo_underflows: stats.fifo_underflows,
            discontinuities: stats.discontinuities,
        reconnect_attempts: stats.reconnect_attempts,
        captured_peak: stats.captured_peak,
        send_peaks: stats
            .send_peaks
            .iter()
            .map(|(id, peaks)| (id.clone(), vec![peaks[0], peaks[1]]))
            .collect(),
    }
    }
}

impl RouteProfileSnapshot {
    fn from_graph(graph: &RouteGraph) -> Self {
        let sources = graph
            .sources
            .iter()
            .map(|s| SourceBrief {
                id: s.id.0.clone(),
                kind: source_kind_str(s.kind.clone()).to_string(),
                display_name: s.display_name.clone(),
                endpoint_id: s.endpoint_id.as_ref().map(|e| e.0.clone()),
                process_id: s.process_id,
            })
            .collect();

        let output_channels = graph
            .buses
            .iter()
            .map(|b| ChannelBrief {
                id: b.id.0.clone(),
                display_name: b.display_name.clone(),
            })
            .collect();

        let external_outputs = graph
            .sinks
            .iter()
            .map(|s| ExternalOutputBrief {
                id: s.id.0.clone(),
                endpoint_id: s.endpoint_id.0.clone(),
                display_name: s.display_name.clone(),
            })
            .collect();

        let sends = graph
            .sends
            .iter()
            .map(|send| match send {
                SendSpec::SourceToBus {
                    id,
                    source_id,
                    bus_id,
                    gain_db,
                    muted,
                    enabled,
                    channel_map,
                } => SendBrief {
                    id: id.0.clone(),
                    source: Some(source_id.0.clone()),
                    output_channel: Some(bus_id.0.clone()),
                    external_output: None,
                    enabled: *enabled,
                    muted: *muted,
                    gain_db: *gain_db,
                    channel_map: channel_map.iter().map(|&(a, b)| [a, b]).collect(),
                },
                SendSpec::BusToSink {
                    id,
                    bus_id,
                    sink_id,
                    gain_db,
                    muted,
                    enabled,
                    channel_map,
                } => SendBrief {
                    id: id.0.clone(),
                    source: None,
                    output_channel: Some(bus_id.0.clone()),
                    external_output: Some(sink_id.0.clone()),
                    enabled: *enabled,
                    muted: *muted,
                    gain_db: *gain_db,
                    channel_map: channel_map.iter().map(|&(a, b)| [a, b]).collect(),
                },
            })
            .collect();

        Self {
            sources,
            output_channels,
            external_outputs,
            sends,
        }
    }
}

fn source_kind_str(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::DeviceCapture => "device_capture",
        SourceKind::DeviceLoopback => "device_loopback",
        SourceKind::ProcessLoopback => "process_loopback",
    }
}

impl ServiceErrorBrief {
    fn lock_poisoned() -> Self {
        Self {
            category: "internal",
            message: "内部状态锁中毒".into(),
            endpoint_id: None,
            hresult: None,
            hint: Some("请重启应用后重试".into()),
        }
    }

    fn graph(message: String) -> Self {
        Self {
            category: "graph",
            message,
            endpoint_id: None,
            hresult: None,
            hint: None,
        }
    }
}

fn service_error_brief(error: ServiceError) -> ServiceErrorBrief {
    let category = match &error {
        ServiceError::Windows { .. } => "windows",
        ServiceError::Engine(_) => "engine",
        ServiceError::Graph(_) => "graph",
        ServiceError::NotReady(_) => "not_ready",
        ServiceError::Rejected { .. } => "rejected",
    };
    ServiceErrorBrief {
        category,
        message: error.to_string(),
        endpoint_id: error.endpoint_id().map(|s| s.to_owned()),
        hresult: error.hresult(),
        hint: error.hint().map(|s| s.to_owned()),
    }
}

/// 把配置错误映射为前端错误视图。
fn config_error_brief(error: ConfigError) -> ServiceErrorBrief {
    let (category, hint) = match &error {
        ConfigError::NotFound(_) => ("config_not_found", Some("尚无已保存的配置".into())),
        ConfigError::Io(_) => ("config_io", Some("配置文件读写失败".into())),
        ConfigError::Json(_) => ("config_json", Some("配置文件格式损坏，已忽略".into())),
        ConfigError::UnsupportedSchemaVersion(v) => (
            "config_schema",
            Some(format!("配置文件版本 {v} 不受支持").into()),
        ),
        ConfigError::Graph(_) => ("config_graph", Some("配置文件路由图校验失败".into())),
    };
    ServiceErrorBrief {
        category,
        message: error.to_string(),
        endpoint_id: None,
        hresult: None,
        hint,
    }
}

fn request_to_route_edit(request: RouteEditRequest) -> Result<RouteEdit, String> {
    Ok(match request {
        RouteEditRequest::AddSource {
            id,
            kind,
            display_name,
            endpoint_id,
            process_id,
        } => {
            let kind = match kind.as_str() {
                "device_capture" => SourceKind::DeviceCapture,
                "device_loopback" => SourceKind::DeviceLoopback,
                "process_loopback" => SourceKind::ProcessLoopback,
                other => return Err(format!("未知 source 类型: {other}")),
            };
            RouteEdit::AddSource(SourceSpec {
                id: SourceId(id),
                kind,
                endpoint_id: endpoint_id.map(EndpointId),
                process_id,
                display_name,
            })
        }
        RouteEditRequest::RemoveSource { id } => RouteEdit::RemoveSource(SourceId(id)),
        RouteEditRequest::AddOutputChannel { id, display_name } => RouteEdit::AddBus(BusSpec {
            id: BusId(id),
            display_name,
        }),
        RouteEditRequest::RemoveOutputChannel { id } => RouteEdit::RemoveBus(BusId(id)),
        RouteEditRequest::AddExternalOutput {
            id,
            endpoint_id,
            display_name,
        } => RouteEdit::AddSink(SinkSpec {
            id: SinkId(id),
            endpoint_id: EndpointId(endpoint_id),
            display_name,
        }),
        RouteEditRequest::RemoveExternalOutput { id } => RouteEdit::RemoveSink(SinkId(id)),
        RouteEditRequest::AddSend {
            id,
            source_id,
            output_channel_id,
        } => RouteEdit::SetSend(SendSpec::SourceToBus {
            id: SendId(id),
            source_id: SourceId(source_id),
            bus_id: BusId(output_channel_id),
            gain_db: 0.0,
            muted: false,
            enabled: true,
            channel_map: Vec::new(),
        }),
        RouteEditRequest::AddSendToOutput {
            id,
            output_channel_id,
            external_output_id,
        } => RouteEdit::SetSend(SendSpec::BusToSink {
            id: SendId(id),
            bus_id: BusId(output_channel_id),
            sink_id: SinkId(external_output_id),
            gain_db: 0.0,
            muted: false,
            enabled: true,
            channel_map: Vec::new(),
        }),
        RouteEditRequest::RemoveSend { id } => RouteEdit::RemoveSend(SendId(id)),
        RouteEditRequest::SetSendEnabled { id, enabled } => RouteEdit::SetSendEnabled {
            send_id: SendId(id),
            enabled,
        },
        RouteEditRequest::SetSendMuted { id, muted } => RouteEdit::SetSendMuted {
            send_id: SendId(id),
            muted,
        },
        RouteEditRequest::SetSendGain { id, gain_db } => RouteEdit::SetSendGain {
            send_id: SendId(id),
            gain_db,
        },
        RouteEditRequest::SetSendChannelMap { id, channel_map } => RouteEdit::SetSendChannelMap {
            send_id: SendId(id),
            channel_map: channel_map
                .into_iter()
                .map(|[input, output]| (input, output))
                .collect(),
        },
        // 节点重命名（SetSourceName / SetOutputChannelName / SetExternalOutputName）
        // 在 apply_route_edit 中单独处理，不走此函数。
        RouteEditRequest::SetSourceName { .. }
        | RouteEditRequest::SetOutputChannelName { .. }
        | RouteEditRequest::SetExternalOutputName { .. } => {
            unreachable!("重命名 op 应在 apply_route_edit 中提前处理")
        }
    })
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let handle = app.handle().clone();
            let config_path = resolve_config_path(&handle);
            app.manage(Arc::new(AppState::new(config_path)));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            greet,
            list_devices,
            list_audio_processes,
            get_route_snapshot,
            get_engine_state,
            get_engine_stats,
            start_engine,
            stop_engine,
            request_reconnect,
            apply_route_edit,
            process_icon_data_uri,
            save_config,
            load_config,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

// ---------------------------------------------------------------------------
// 单元测试（不依赖真实设备）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_graph() -> RouteGraph {
        RouteGraph {
            sources: vec![SourceSpec {
                id: SourceId("src-a".into()),
                kind: SourceKind::ProcessLoopback,
                endpoint_id: None,
                process_id: Some(42),
                display_name: "应用 A".into(),
            }],
            buses: vec![BusSpec {
                id: BusId("ch-1".into()),
                display_name: "输出通道 1".into(),
            }],
            sinks: vec![SinkSpec {
                id: SinkId("out-1".into()),
                endpoint_id: EndpointId("endpoint-1".into()),
                display_name: "扬声器".into(),
            }],
            sends: vec![
                SendSpec::SourceToBus {
                    id: SendId("s1".into()),
                    source_id: SourceId("src-a".into()),
                    bus_id: BusId("ch-1".into()),
                    gain_db: -3.0,
                    muted: true,
                    enabled: true,
                    channel_map: Vec::new(),
                },
                SendSpec::BusToSink {
                    id: SendId("s2".into()),
                    bus_id: BusId("ch-1".into()),
                    sink_id: SinkId("out-1".into()),
                    gain_db: 0.0,
                    muted: false,
                    enabled: false,
                    channel_map: vec![(0, 0)],
                },
            ],
        }
    }

    #[test]
    fn snapshot_projects_route_profile_model() {
        let graph = sample_graph();
        let snap = RouteProfileSnapshot::from_graph(&graph);
        assert_eq!(snap.sources.len(), 1);
        assert_eq!(snap.sources[0].id, "src-a");
        assert_eq!(snap.sources[0].kind, "process_loopback");
        assert_eq!(snap.sources[0].process_id, Some(42));
        assert_eq!(snap.output_channels.len(), 1);
        assert_eq!(snap.output_channels[0].id, "ch-1");
        assert_eq!(snap.external_outputs.len(), 1);
        assert_eq!(snap.external_outputs[0].endpoint_id, "endpoint-1");
        assert_eq!(snap.sends.len(), 2);

        // SourceToBus：source + output_channel，无 external_output
        let s1 = snap.sends.iter().find(|s| s.id == "s1").unwrap();
        assert_eq!(s1.source.as_deref(), Some("src-a"));
        assert_eq!(s1.output_channel.as_deref(), Some("ch-1"));
        assert!(s1.external_output.is_none());
        assert!(s1.muted);
        assert_eq!(s1.gain_db, -3.0);

        // BusToSink：output_channel + external_output，无 source
        let s2 = snap.sends.iter().find(|s| s.id == "s2").unwrap();
        assert!(s2.source.is_none());
        assert_eq!(s2.output_channel.as_deref(), Some("ch-1"));
        assert_eq!(s2.external_output.as_deref(), Some("out-1"));
        assert!(!s2.enabled);
        assert_eq!(s2.channel_map, vec![[0, 0]]);
    }

    #[test]
    fn add_and_remove_edit_maps_to_route_editor() {
        let mut editor = RouteEditor::new(RouteGraph::default());
        // 添加 source
        editor
            .apply(
                request_to_route_edit(RouteEditRequest::AddSource {
                    id: "src-a".into(),
                    kind: "process_loopback".into(),
                    display_name: "应用 A".into(),
                    endpoint_id: None,
                    process_id: Some(42),
                })
                .unwrap(),
            )
            .unwrap();
        assert_eq!(editor.draft().sources.len(), 1);

        // 添加输出通道（bus）与输出目标（sink）
        editor
            .apply(
                request_to_route_edit(RouteEditRequest::AddOutputChannel {
                    id: "ch-1".into(),
                    display_name: "通道 1".into(),
                })
                .unwrap(),
            )
            .unwrap();
        editor
            .apply(
                request_to_route_edit(RouteEditRequest::AddExternalOutput {
                    id: "out-1".into(),
                    endpoint_id: "endpoint-1".into(),
                    display_name: "扬声器".into(),
                })
                .unwrap(),
            )
            .unwrap();

        // 建立连线
        editor
            .apply(
                request_to_route_edit(RouteEditRequest::AddSend {
                    id: "s1".into(),
                    source_id: "src-a".into(),
                    output_channel_id: "ch-1".into(),
                })
                .unwrap(),
            )
            .unwrap();
        editor
            .apply(
                request_to_route_edit(RouteEditRequest::AddSendToOutput {
                    id: "s2".into(),
                    output_channel_id: "ch-1".into(),
                    external_output_id: "out-1".into(),
                })
                .unwrap(),
            )
            .unwrap();
        assert_eq!(editor.draft().sends.len(), 2);

        // 关闭一条 send 静音并启用
        editor
            .apply(
                request_to_route_edit(RouteEditRequest::SetSendMuted {
                    id: "s1".into(),
                    muted: true,
                })
                .unwrap(),
            )
            .unwrap();
        assert!(editor.draft().sends[0].muted());

        // 含 source + sink 的图可提交为快照（引擎创建所需）
        assert!(editor.commit().is_ok());

        // 移除 source 会级联删除关联 send
        editor
            .apply(
                request_to_route_edit(RouteEditRequest::RemoveSource { id: "src-a".into() })
                    .unwrap(),
            )
            .unwrap();
        assert!(editor.draft().sources.is_empty());
    }

    #[test]
    fn set_send_channel_map_maps_to_route_editor() {
        let mut editor = RouteEditor::new(sample_graph());
        editor
            .apply(
                request_to_route_edit(RouteEditRequest::SetSendChannelMap {
                    id: "s1".into(),
                    channel_map: vec![[0, 1], [1, 0]],
                })
                .unwrap(),
            )
            .unwrap();
        let s1 = editor
            .draft()
            .sends
            .iter()
            .find(|s| s.id() == &SendId("s1".into()))
            .unwrap();
        assert_eq!(s1.channel_map(), &[(0, 1), (1, 0)]);
    }

    #[test]
    fn rename_source_rebuilds_editor_display_name() {
        let mut editor = RouteEditor::new(sample_graph());
        apply_rename(
            &mut editor,
            RenameTarget::Source,
            "src-a",
            "改名应用".into(),
        )
        .unwrap();
        let graph = editor.draft();
        assert_eq!(graph.sources.len(), 1);
        assert_eq!(graph.sources[0].display_name, "改名应用");
        // 拓扑与 send 关系保持不变
        assert_eq!(graph.sends.len(), 2);
        assert_eq!(graph.buses[0].id, BusId("ch-1".into()));
    }

    #[test]
    fn rename_output_channel_and_external_output_rebuild_display_name() {
        let mut editor = RouteEditor::new(sample_graph());
        apply_rename(
            &mut editor,
            RenameTarget::OutputChannel,
            "ch-1",
            "主通道".into(),
        )
        .unwrap();
        assert_eq!(editor.draft().buses[0].display_name, "主通道");

        apply_rename(
            &mut editor,
            RenameTarget::ExternalOutput,
            "out-1",
            "主扬声器".into(),
        )
        .unwrap();
        assert_eq!(editor.draft().sinks[0].display_name, "主扬声器");
        // 三条 send 均保留
        assert_eq!(editor.draft().sends.len(), 2);
    }

    #[test]
    fn rename_missing_node_is_rejected_without_replacing_editor() {
        let mut editor = RouteEditor::new(sample_graph());
        let before = editor.draft().clone();
        let error =
            apply_rename(&mut editor, RenameTarget::Source, "ghost", "x".into()).unwrap_err();
        assert_eq!(error, RouteGraphError::MissingSource("ghost".into()));
        assert_eq!(editor.draft(), &before);
    }

    #[test]
    fn unknown_source_kind_is_rejected() {
        let error = request_to_route_edit(RouteEditRequest::AddSource {
            id: "x".into(),
            kind: "bogus".into(),
            display_name: "x".into(),
            endpoint_id: None,
            process_id: None,
        });
        assert!(error.is_err());
    }
}
