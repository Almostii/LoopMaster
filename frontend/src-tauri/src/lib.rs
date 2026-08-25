//! LoopMaster 前端 Tauri 壳层 — command/event 适配层。
//!
//! 这是前端与 Rust 应用服务（app-service）之间的唯一命令/事件边界。React
//! 只维护展示模型和用户意图；WASAPI 枚举、引擎控制都在 Tauri command 执行
//! 的后台线程完成，不阻塞 UI 主线程，也不把实时音频结构暴露给前端。
//!
//! 本阶段（阶段 B）实现命令/事件闭环：
//! - 只读命令：`list_devices`、`list_audio_processes`、`get_route_snapshot`；
//! - 引擎控制：`start_engine`、`stop_engine`、`request_reconnect`、
//!   `apply_route_edit`（拓扑变化会返回“需要重启”）；
//! - 事件：`engine-state-changed`、`engine-stats-changed`、
//!   `device-lost`、`device-restored`、`service-error`。

use std::sync::{Arc, Mutex};
use std::thread;

use serde::{Deserialize, Serialize};

use loopmaster_app_service::{
    DeviceFlow, DeviceModel, DeviceRepository, EngineCommand, EngineService, ProcessModel,
    ProcessRepository, RouteEdit, RouteEditor, ServiceError, ServiceEvent,
};
use loopmaster_audio_core::{
    BusId, BusSpec, EndpointId, RouteGraph, SendId, SendSpec, SinkId, SinkSpec, SourceId,
    SourceKind, SourceSpec,
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
}

// ---------------------------------------------------------------------------
// Tauri 托管状态
// ---------------------------------------------------------------------------

/// 全局应用状态：暂存路由编辑器 + 惰性创建的引擎服务。
///
/// 引擎在首次 `start_engine` 时才创建（`RouteGraph` 至少需要一个 source 和
/// 一个 sink，空图不能初始化引擎），此后复用同一实例直到进程退出。
struct AppState {
    editor: Mutex<RouteEditor>,
    engine: Mutex<Option<EngineService>>,
}

impl AppState {
    fn new() -> Self {
        Self {
            editor: Mutex::new(RouteEditor::new(RouteGraph::default())),
            engine: Mutex::new(None),
        }
    }
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

/// 启动引擎。首次启动会创建引擎服务；拓扑变更（source/sink 变化）会
/// 返回“需要重启”结构化错误，前端不得静默丢弃。
#[tauri::command]
fn start_engine(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<(), ServiceErrorBrief> {
    // 首次启动创建引擎服务（需要图至少含一个 source 和一个 sink）。
    ensure_engine(&app, &state).map_err(service_error_brief)?;
    let engine = state.engine.lock().expect("引擎锁未中毒");
    let engine = engine.as_ref().expect("引擎已创建");
    // 用当前暂存路由提交到引擎。
    let snapshot = {
        let editor = state.editor.lock().expect("路由锁未中毒");
        editor
            .commit()
            .map_err(|e| ServiceErrorBrief::graph(e.to_string()))?
    };
    engine
        .command(EngineCommand::ApplyRoute(snapshot))
        .map_err(service_error_brief)?;
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
#[tauri::command]
fn apply_route_edit(
    state: tauri::State<'_, Arc<AppState>>,
    request: RouteEditRequest,
) -> Result<(), ServiceErrorBrief> {
    let edit = request_to_route_edit(request).map_err(ServiceErrorBrief::graph)?;
    let mut editor = state
        .editor
        .lock()
        .map_err(|_| ServiceErrorBrief::lock_poisoned())?;
    editor
        .apply(edit)
        .map_err(|e| ServiceErrorBrief::graph(e.to_string()))
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
    })
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app_state = Arc::new(AppState::new());
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(app_state)
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
