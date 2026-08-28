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

use loopmaster_app_service::{AppConfig, ConfigError, DeviceCompatibility, DeviceStatus};
use loopmaster_app_service::{
    DeviceFlow, DeviceModel, DeviceRepository, EngineCommand, EngineService, NetworkBridge,
    NetworkDiscovery, NetworkEvent, NodeIdentity, NodeInfo, NodeMeta, ProcessModel,
    ProcessRepository, RouteEdit, RouteEditor, ServiceError, ServiceEvent, VBAN_SERVICE_PORT,
    CAPS_VBAN_AUDIO,
};
use loopmaster_audio_core::{
    BusId, BusSpec, EndpointId, RouteGraph, RouteGraphError, SendId, SendSpec, SinkId, SinkKind,
    SinkSpec, SourceId, SourceKind, SourceSpec,
};
use loopmaster_audio_windows::{AudioEngineState, AudioEngineStats, AudioEngineStatus};
use tauri::menu::{Menu, MenuItem};
use tauri::path::BaseDirectory;
use tauri::tray::TrayIconBuilder;
use tauri::Emitter;
use tauri_plugin_autostart::ManagerExt;

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

/// 本机网络身份概要。
#[derive(Clone, Serialize)]
struct NodeIdentityBrief {
    node_id: String,
    device_name: String,
    network_enabled: bool,
    web_port: u16,
}

/// 局域网发现的 VBAN 节点概要。
#[derive(Clone, Serialize)]
struct NetworkNodeBrief {
    node_id: String,
    name: String,
    addresses: Vec<String>,
    port: u16,
    sample_rate: u32,
    channels: u8,
    caps: String,
}

impl NetworkNodeBrief {
    fn from_node(node: &NodeInfo) -> Self {
        Self {
            node_id: node.node_id.clone(),
            name: node.name.clone(),
            addresses: node.addresses.iter().map(|ip| ip.to_string()).collect(),
            port: node.port,
            sample_rate: node.sample_rate,
            channels: node.channels,
            caps: node.caps.clone(),
        }
    }
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
    #[serde(default)]
    executable_path: Option<String>,
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
    #[serde(default = "default_device_kind")]
    kind: String,
    #[serde(default)]
    stream_name: Option<String>,
}

#[allow(dead_code)]
fn default_device_kind() -> String {
    "device".to_owned()
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
#[derive(Clone, Debug, Serialize)]
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
        #[serde(default)]
        executable_path: Option<String>,
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
    ReplaceProcessSourceWithDevice {
        old_source_id: String,
        new_source_id: String,
        endpoint_id: String,
        display_name: String,
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
/// 应用设置 DTO（前端设置页持久化的内容）。
#[derive(Clone, Debug, Serialize, Deserialize)]
struct AppSettings {
    theme: String,
    start_on_boot: bool,
    launch_hidden: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: "light".into(),
            start_on_boot: false,
            launch_hidden: false,
        }
    }
}

impl AppSettings {
    /// 从配置文件的 `ui_state` 提取设置，读取失败或文件缺失时回退默认。
    fn load_from_config(config_path: &std::path::Path) -> Self {
        match AppConfig::load_from(config_path) {
            Ok(config) => Self {
                theme: config.ui_state.theme().to_string(),
                start_on_boot: config.ui_state.start_on_boot,
                launch_hidden: config.ui_state.launch_hidden,
            },
            Err(_) => Self::default(),
        }
    }

    /// 将设置合并回现有配置（保留路由图与缺失设备标记）并写盘。
    fn save_to_config(&self, config_path: &std::path::Path) -> Result<(), ServiceErrorBrief> {
        let mut config = match AppConfig::load_from(config_path) {
            Ok(config) => config,
            Err(ConfigError::NotFound(_)) => {
                // 尚无配置文件：以空图构造，再由路由保存流程补充图内容。
                AppConfig::new(loopmaster_audio_core::RouteGraph::default())
            }
            Err(e) => return Err(config_error_brief(e)),
        };
        config.ui_state.theme = self.theme.clone();
        config.ui_state.start_on_boot = self.start_on_boot;
        config.ui_state.launch_hidden = self.launch_hidden;
        config.save_to(config_path).map_err(config_error_brief)
    }
}

struct AppState {
    route_operation: Mutex<()>,
    editor: Mutex<RouteEditor>,
    engine: Mutex<Option<EngineService>>,
    config_path: PathBuf,
    settings: Mutex<AppSettings>,
    /// 本机网络身份缓存（首次启动时生成 node_id 并持久化）。
    node_identity: Mutex<NodeIdentityBrief>,
    /// 后台局域网节点监听服务。
    discovery: Mutex<Option<NetworkDiscovery>>,
    /// VBAN 网络桥接（网络 FIFO 与 UDP 收发对接）。
    bridge: Mutex<Option<NetworkBridge>>,
}

impl AppState {
    fn new(config_path: PathBuf) -> Self {
        Self {
            route_operation: Mutex::new(()),
            editor: Mutex::new(RouteEditor::new(RouteGraph::default())),
            engine: Mutex::new(None),
            config_path,
            settings: Mutex::new(AppSettings::default()),
            node_identity: Mutex::new(NodeIdentityBrief {
                node_id: String::new(),
                device_name: String::new(),
                network_enabled: false,
                web_port: 0,
            }),
            discovery: Mutex::new(None),
            bridge: Mutex::new(None),
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

/// 确保本机网络身份存在：读取配置中的 `network`，缺失 node_id 时生成并持久化。
///
/// 返回当前身份快照（含 network_enabled）。
fn ensure_node_identity(state: &AppState) -> NodeIdentityBrief {
    let mut cached = state.node_identity.lock().expect("身份锁未中毒");
    if !cached.node_id.is_empty() {
        return cached.clone();
    }
    let mut config = match AppConfig::load_from(&state.config_path) {
        Ok(config) => config,
        Err(ConfigError::NotFound(_)) => {
            AppConfig::new(loopmaster_audio_core::RouteGraph::default())
        }
        Err(_) => return cached.clone(),
    };
    if config.network.node_id.is_none() {
        let identity = NodeIdentity::generate();
        config.network.node_id = Some(identity.node_id.clone());
        config.network.device_name = Some(identity.device_name);
        let _ = config.save_to(&state.config_path);
    }
    let brief = NodeIdentityBrief {
        node_id: config.network.node_id.clone().unwrap_or_default(),
        device_name: config
            .network
            .device_name
            .clone()
            .unwrap_or_else(loopmaster_app_service::default_device_name),
        network_enabled: config.network.network_enabled,
        web_port: config.network.web_port,
    };
    *cached = brief.clone();
    brief
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

fn validate_graph_endpoints(graph: &RouteGraph) -> Result<(), ServiceErrorBrief> {
    let repository = DeviceRepository::new().map_err(service_error_brief)?;
    let devices = repository.list_devices().map_err(service_error_brief)?;

    for source in &graph.sources {
        let expected = match source.kind {
            SourceKind::DeviceCapture => Some(DeviceFlow::Capture),
            SourceKind::DeviceLoopback => Some(DeviceFlow::Render),
            SourceKind::ProcessLoopback => None,
            // VBAN 网络源不依赖真实设备。
            SourceKind::Vban => None,
        };
        if let Some(expected) = expected {
            let endpoint = source.endpoint_id.as_ref().ok_or_else(|| {
                ServiceErrorBrief::invalid_endpoint(
                    None,
                    format!("音源“{}”缺少 endpoint ID", source.display_name),
                )
            })?;
            validate_endpoint(&devices, endpoint, expected, &source.display_name)?;
        }
    }
    for sink in &graph.sinks {
        // VBAN 网络目标不依赖真实渲染设备，跳过 endpoint 校验。
        if sink.kind == SinkKind::Vban {
            continue;
        }
        validate_endpoint(
            &devices,
            &sink.endpoint_id,
            DeviceFlow::Render,
            &sink.display_name,
        )?;
    }
    Ok(())
}

fn validate_endpoint(
    devices: &[DeviceModel],
    endpoint: &EndpointId,
    expected: DeviceFlow,
    display_name: &str,
) -> Result<(), ServiceErrorBrief> {
    let device = devices
        .iter()
        .find(|device| device.id == *endpoint)
        .ok_or_else(|| {
            ServiceErrorBrief::invalid_endpoint(
                Some(endpoint.0.clone()),
                format!("设备“{display_name}”当前不可用"),
            )
        })?;
    if device.flow != expected {
        return Err(ServiceErrorBrief::invalid_endpoint(
            Some(endpoint.0.clone()),
            format!(
                "设备“{display_name}”流向错误：需要 {} endpoint，实际为 {} endpoint",
                expected.as_str(),
                device.flow.as_str()
            ),
        ));
    }
    let compatible = matches!(
        (&device.compatibility, expected),
        (DeviceCompatibility::CaptureReady, DeviceFlow::Capture)
            | (DeviceCompatibility::RenderReady, DeviceFlow::Render)
    );
    if !compatible || device.status != DeviceStatus::Active {
        return Err(ServiceErrorBrief::invalid_endpoint(
            Some(endpoint.0.clone()),
            format!("设备“{display_name}”与当前音频格式不兼容"),
        ));
    }
    Ok(())
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

/// 返回当前应用版本号（来自 Cargo.toml，与 tauri.conf.json 保持一致）。
#[tauri::command]
fn get_app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 返回本机网络身份（node_id/device_name/network_enabled/web_port）。
#[tauri::command]
fn get_node_identity(state: tauri::State<'_, Arc<AppState>>) -> NodeIdentityBrief {
    ensure_node_identity(&state)
}

/// 返回当前局域网发现的 VBAN 节点列表快照。
#[tauri::command]
fn get_network_nodes(state: tauri::State<'_, Arc<AppState>>) -> Vec<NetworkNodeBrief> {
    let slot = state.discovery.lock().expect("监听锁未中毒");
    match slot.as_ref() {
        Some(discovery) => discovery
            .snapshot()
            .iter()
            .map(NetworkNodeBrief::from_node)
            .collect(),
        None => Vec::new(),
    }
}

/// 开启/关闭网络功能，并持久化到配置。
///
/// 开启：发布本机 VBAN 服务（Advertiser）+ 确保 Browser 监听。
/// 关闭：下架本机服务（Browser 仍持续监听，便于发现其他电脑）。
/// 返回更新后的本机身份（含新 `network_enabled`）。
#[tauri::command]
fn set_network_enabled(
    enabled: bool,
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<NodeIdentityBrief, ServiceErrorBrief> {
    if enabled {
        start_advertising(app, &state)?;
    }
    // 持久化 network_enabled 到配置；失败时若刚开启则回滚下架。
    let mut config = match AppConfig::load_from(&state.config_path) {
        Ok(config) => config,
        Err(ConfigError::NotFound(_)) => {
            AppConfig::new(loopmaster_audio_core::RouteGraph::default())
        }
        Err(e) => return Err(config_error_brief(e)),
    };
    config.network.network_enabled = enabled;
    if let Err(e) = config.save_to(&state.config_path) {
        if enabled {
            stop_advertising(&state); // 回滚已发布的 Advertiser
        }
        return Err(config_error_brief(e));
    }
    // 用缓存的身份 + 新的开关状态构造返回值，避免依赖 ensure_node_identity
    // 的缓存短路导致返回旧 network_enabled。
    let mut cached = state.node_identity.lock().expect("身份锁未中毒");
    let brief = NodeIdentityBrief {
        node_id: cached.node_id.clone(),
        device_name: cached.device_name.clone(),
        network_enabled: enabled,
        web_port: cached.web_port,
    };
    *cached = brief.clone();
    Ok(brief)
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
    let _operation = state
        .route_operation
        .lock()
        .map_err(|_| ServiceErrorBrief::lock_poisoned())?;
    {
        let editor = state
            .editor
            .lock()
            .map_err(|_| ServiceErrorBrief::lock_poisoned())?;
        validate_graph_endpoints(editor.draft())?;
    }
    // 1) 若已有引擎实例，先停止并丢弃（含其网络桥接），确保用最新图重建。
    stop_network_bridge(&state);
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
        .map_err(service_error_brief)?;
    // 3) 启动后异步轮询网络句柄，就绪后建立 VBAN 桥接。
    let bridge_state = state.inner().clone();
    spawn_network_bridge(app, bridge_state);
    Ok(())
}

/// 停止引擎。引擎尚未创建时返回错误。
#[tauri::command]
fn stop_engine(state: tauri::State<'_, Arc<AppState>>) -> Result<(), ServiceErrorBrief> {
    // 停止网络桥接（其句柄依赖当前引擎 session）。
    stop_network_bridge(&state);
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
fn request_reconnect(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<(), ServiceErrorBrief> {
    {
        let engine = state.engine.lock().expect("引擎锁未中毒");
        match engine.as_ref() {
            Some(engine) => engine.request_reconnect().map_err(service_error_brief)?,
            None => {
                return Err(ServiceErrorBrief {
                    category: "not_ready",
                    message: "引擎尚未启动".into(),
                    endpoint_id: None,
                    hresult: None,
                    hint: Some("请先启动引擎".into()),
                });
            }
        }
    }
    // 重连重建了引擎 session（新网络 FIFO），需重新建立网络桥接。
    stop_network_bridge(&state);
    let bridge_state = state.inner().clone();
    spawn_network_bridge(app, bridge_state);
    Ok(())
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
    let _operation = state
        .route_operation
        .lock()
        .map_err(|_| ServiceErrorBrief::lock_poisoned())?;
    let mut next_editor = state
        .editor
        .lock()
        .map_err(|_| ServiceErrorBrief::lock_poisoned())?
        .clone();
    apply_request_to_editor(&mut next_editor, &request)?;

    // 运行中先更新引擎；失败则不替换草稿，保证两者仍指向同一版本。
    forward_send_to_engine(&state, &request)?;
    let mut editor = state
        .editor
        .lock()
        .map_err(|_| ServiceErrorBrief::lock_poisoned())?;
    *editor = next_editor;
    Ok(())
}

fn apply_request_to_editor(
    editor: &mut RouteEditor,
    request: &RouteEditRequest,
) -> Result<(), ServiceErrorBrief> {
    match request {
        RouteEditRequest::SetSourceName { id, display_name } => {
            apply_rename(editor, RenameTarget::Source, id, display_name.clone())
                .map_err(|e| ServiceErrorBrief::graph(e.to_string()))
        }
        RouteEditRequest::SetOutputChannelName { id, display_name } => apply_rename(
            editor,
            RenameTarget::OutputChannel,
            id,
            display_name.clone(),
        )
        .map_err(|e| ServiceErrorBrief::graph(e.to_string())),
        RouteEditRequest::SetExternalOutputName { id, display_name } => apply_rename(
            editor,
            RenameTarget::ExternalOutput,
            id,
            display_name.clone(),
        )
        .map_err(|e| ServiceErrorBrief::graph(e.to_string())),
        RouteEditRequest::ReplaceProcessSourceWithDevice {
            old_source_id,
            new_source_id,
            endpoint_id,
            display_name,
        } => {
            let repository = DeviceRepository::new().map_err(service_error_brief)?;
            let devices = repository.list_devices().map_err(service_error_brief)?;
            validate_endpoint(
                &devices,
                &EndpointId(endpoint_id.clone()),
                DeviceFlow::Capture,
                display_name,
            )?;
            replace_process_source_with_device(
                editor,
                old_source_id,
                new_source_id,
                endpoint_id,
                display_name,
            )
        }
        RouteEditRequest::AddSource {
            kind,
            endpoint_id: Some(endpoint_id),
            display_name,
            ..
        } if kind == "device_capture" || kind == "device_loopback" => {
            let expected = if kind == "device_capture" {
                DeviceFlow::Capture
            } else {
                DeviceFlow::Render
            };
            let repository = DeviceRepository::new().map_err(service_error_brief)?;
            let devices = repository.list_devices().map_err(service_error_brief)?;
            validate_endpoint(
                &devices,
                &EndpointId(endpoint_id.clone()),
                expected,
                display_name,
            )?;
            let edit = request_to_route_edit(request.clone()).map_err(ServiceErrorBrief::graph)?;
            editor
                .apply(edit)
                .map_err(|e| ServiceErrorBrief::graph(e.to_string()))
        }
        _ => {
            let edit = request_to_route_edit(request.clone()).map_err(ServiceErrorBrief::graph)?;
            editor
                .apply(edit)
                .map_err(|e| ServiceErrorBrief::graph(e.to_string()))
        }
    }
}

fn replace_process_source_with_device(
    editor: &mut RouteEditor,
    old_source_id: &str,
    new_source_id: &str,
    endpoint_id: &str,
    display_name: &str,
) -> Result<(), ServiceErrorBrief> {
    let mut graph = editor.draft().clone();
    let source = graph
        .sources
        .iter_mut()
        .find(|source| source.id.0 == old_source_id)
        .ok_or_else(|| ServiceErrorBrief::graph(format!("source 不存在: {old_source_id}")))?;
    if source.kind != SourceKind::ProcessLoopback {
        return Err(ServiceErrorBrief::graph(format!(
            "source 不是 ProcessLoopback: {old_source_id}"
        )));
    }
    source.id = SourceId(new_source_id.to_owned());
    source.kind = SourceKind::DeviceCapture;
    source.endpoint_id = Some(EndpointId(endpoint_id.to_owned()));
    source.process_id = None;
    source.executable_path = None;
    source.display_name = display_name.to_owned();
    for send in &mut graph.sends {
        if let SendSpec::SourceToBus { source_id, .. } = send {
            if source_id.0 == old_source_id {
                *source_id = SourceId(new_source_id.to_owned());
            }
        }
    }
    graph
        .validate()
        .map_err(|error| ServiceErrorBrief::graph(error.to_string()))?;
    *editor = RouteEditor::new(graph);
    Ok(())
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
fn save_config(state: tauri::State<'_, Arc<AppState>>) -> Result<(), ServiceErrorBrief> {
    persist_config(&state)
}

/// 把当前编辑器草稿 + 运行期设置写入配置文件（原子写入）。
/// 供 `save_config` 命令与后台进程监控线程复用。
fn persist_config(state: &AppState) -> Result<(), ServiceErrorBrief> {
    let editor = state
        .editor
        .lock()
        .map_err(|_| ServiceErrorBrief::lock_poisoned())?;
    let mut config = AppConfig::new(editor.draft().clone());
    drop(editor);
    // 保留运行期设置，避免路由保存把设置字段重置为默认。
    let settings = state
        .settings
        .lock()
        .map_err(|_| ServiceErrorBrief::lock_poisoned())?
        .clone();
    config.ui_state.theme = settings.theme;
    config.ui_state.start_on_boot = settings.start_on_boot;
    config.ui_state.launch_hidden = settings.launch_hidden;
    config
        .save_to(&state.config_path)
        .map_err(config_error_brief)
}

/// 启动局域网节点监听（Browser），并把节点上线/下线事件转发为 Tauri event。
///
/// 已在运行则无操作；线程退出（daemon 停止）时清空槽位以便再次启动。
/// Browser 独立于本机服务发布（Advertiser）持续运行，便于随时发现其他电脑。
fn start_browser(app: tauri::AppHandle, state: &AppState) {
    let mut slot = state.discovery.lock().expect("监听锁未中毒");
    if slot.is_some() {
        return;
    }
    let mut discovery = match NetworkDiscovery::new() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("启动局域网监听失败: {e}");
            return;
        }
    };
    let events = discovery.subscribe();
    if let Err(e) = discovery.start() {
        eprintln!("启动局域网监听失败: {e}");
        return;
    }
    // 事件转发线程：把上线/下线事件 emit 给前端。
    let handle = app.clone();
    thread::Builder::new()
        .name("loopmaster-mdns-events".into())
        .spawn(move || {
            for event in events {
                match event {
                    NetworkEvent::NodeResolved(node) => {
                        let _ = handle.emit(
                            "node-resolved",
                            serde_json::json!({
                                "node": NetworkNodeBrief::from_node(&node),
                            }),
                        );
                    }
                    NetworkEvent::NodeRemoved(node_id) => {
                        let _ = handle.emit(
                            "node-removed",
                            serde_json::json!({ "node_id": node_id }),
                        );
                    }
                }
            }
        })
        .expect("创建 mDNS 事件转发线程失败");
    *slot = Some(discovery);
}

/// 发布本机 VBAN 服务（开启网络功能）。Browser 未启动时先启动它。
fn start_advertising(app: tauri::AppHandle, state: &AppState) -> Result<(), ServiceErrorBrief> {
    // 确保 Browser 在监听。
    if state.discovery.lock().expect("监听锁未中毒").is_none() {
        start_browser(app, state);
    }
    let identity = ensure_node_identity(state);
    if identity.node_id.is_empty() {
        return Err(ServiceErrorBrief {
            category: "network",
            message: "本机节点 ID 无效，无法发布服务".into(),
            endpoint_id: None,
            hresult: None,
            hint: Some("请检查配置文件后重试".into()),
        });
    }
    let meta = NodeMeta {
        node_id: identity.node_id.clone(),
        name: identity.device_name.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        web_port: identity.web_port,
        sample_rate: loopmaster_audio_core::INTERNAL_SAMPLE_RATE,
        channels: loopmaster_audio_core::INTERNAL_CHANNELS as u8,
        caps: format!("{CAPS_VBAN_AUDIO}"),
    };
    let node_identity = NodeIdentity {
        node_id: identity.node_id,
        device_name: identity.device_name,
    };
    let mut slot = state.discovery.lock().expect("监听锁未中毒");
    let discovery = match slot.as_mut() {
        Some(d) => d,
        None => {
            return Err(ServiceErrorBrief {
                category: "network",
                message: "局域网监听未启动".into(),
                endpoint_id: None,
                hresult: None,
                hint: Some("请先开启网络功能".into()),
            });
        }
    };
    discovery
        .start_advertiser(&node_identity, &meta)
        .map_err(|e| ServiceErrorBrief {
            category: "network",
            message: format!("发布本机服务失败: {e}"),
            endpoint_id: None,
            hresult: None,
            hint: Some("请检查网络连接与端口占用后重试".into()),
        })
}

/// 下架本机 VBAN 服务（关闭网络功能）。
fn stop_advertising(state: &AppState) {
    let mut slot = state.discovery.lock().expect("监听锁未中毒");
    if let Some(discovery) = slot.as_mut() {
        discovery.stop_advertiser();
    }
}

/// 停止 VBAN 网络桥接（幂等）。
fn stop_network_bridge(state: &AppState) {
    let mut slot = state.bridge.lock().expect("桥接锁未中毒");
    if let Some(bridge) = slot.take() {
        drop(bridge); // Drop 触发 shutdown + join
    }
}

/// 异步建立 VBAN 网络桥接：引擎 Start 后 supervisor 后台创建 session 并发送
/// 网络句柄，需轮询 `take_network_handles()` 直至就绪。
fn spawn_network_bridge(app: tauri::AppHandle, state: Arc<AppState>) {
    // 路由图中若无 VBAN 源/目标，无需桥接。
    let has_vban = {
        let editor = state.editor.lock().expect("路由锁未中毒");
        let draft = editor.draft();
        draft.sources.iter().any(|s| s.kind == SourceKind::Vban)
            || draft.sinks.iter().any(|s| s.kind == SinkKind::Vban)
    };
    if !has_vban {
        return;
    }
    let handle = app.clone();
    std::thread::Builder::new()
        .name("loopmaster-bridge-wait".into())
        .spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                let engine = state.engine.lock().expect("引擎锁未中毒");
                let handles = engine.as_ref().and_then(|e| e.take_network_handles());
                drop(engine);
                if let Some(handles) = handles {
                    let graph = {
                        let editor = state.editor.lock().expect("路由锁未中毒");
                        editor.draft().clone()
                    };
                    let receiver_bind = format!("0.0.0.0:{}", VBAN_SERVICE_PORT)
                        .parse()
                        .expect("VBAN 端口常量合法");
                    match NetworkBridge::from_handles(receiver_bind, &graph, handles) {
                        Ok(bridge) => {
                            let mut slot = state.bridge.lock().expect("桥接锁未中毒");
                            *slot = Some(bridge);
                            drop(slot);
                            let _ = handle.emit("network-bridge-ready", serde_json::json!({}));
                            return;
                        }
                        Err(e) => {
                            eprintln!("启动网络桥接失败: {e}");
                            return;
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        })
        .expect("创建网络桥接等待线程失败");
}

/// 进程声源自动重连：定期枚举当前音频进程，对失效的 ProcessLoopback
/// 声源按可执行路径重新匹配新 PID，更新编辑图并持久化，向前端 emit
/// `process-restored`。PID 未失效或无可执行路径时保持不动。
fn spawn_process_watcher(app: tauri::AppHandle, state: Arc<AppState>) {
    std::thread::Builder::new()
        .name("loopmaster-process-watcher".into())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                // 枚举当前有音频会话的进程
                let processes =
                    match ProcessRepository::new().and_then(|repo| repo.list_audio_processes()) {
                        Ok(list) => list,
                        Err(_) => continue,
                    };
                // 收集失效的 ProcessLoopback 声源：(id, executable_path, 旧 pid)
                let stale: Vec<(SourceId, String, u32)> = {
                    let editor = state.editor.lock().expect("路由锁未中毒");
                    editor
                        .draft()
                        .sources
                        .iter()
                        .filter(|s| s.kind == SourceKind::ProcessLoopback)
                        .filter_map(|s| {
                            let pid = s.process_id?;
                            let path = s.executable_path.clone()?;
                            let alive = processes.iter().any(|p| p.pid == pid);
                            if alive {
                                None
                            } else {
                                Some((s.id.clone(), path, pid))
                            }
                        })
                        .collect()
                };
                for (source_id, path, old_pid) in stale {
                    // 同一可执行路径下找新 PID
                    let new_pid = processes
                        .iter()
                        .find(|p| {
                            p.executable_path.as_deref() == Some(path.as_str()) && p.pid != old_pid
                        })
                        .map(|p| p.pid);
                    let Some(new_pid) = new_pid else {
                        continue;
                    };
                    let applied = {
                        let mut editor = state.editor.lock().expect("路由锁未中毒");
                        editor.apply(RouteEdit::SetSourceProcessId {
                            source_id: source_id.clone(),
                            process_id: Some(new_pid),
                        })
                    };
                    if applied.is_err() {
                        continue;
                    }
                    let _ = persist_config(&state);
                    let _ = app.emit(
                        "process-restored",
                        serde_json::json!({
                            "source_id": source_id.0,
                            "process_id": new_pid,
                        }),
                    );
                }
            }
        })
        .expect("创建进程监控线程失败");
}

/// 从配置文件加载路由，替换当前编辑器草稿。
///
/// 文件不存在（`ConfigError::NotFound`）时返回 `Ok(false)`，表示无需加载，
/// 交由前端决定是否建立默认拓扑；其余错误（损坏/版本不支持/校验失败）返回
/// `Err` 以便前端提示。加载成功后标记缺失设备，后续 UI 按 endpoint 可用性
/// 决定是否自动启动引擎。
#[tauri::command]
fn load_config(state: tauri::State<'_, Arc<AppState>>) -> Result<bool, ServiceErrorBrief> {
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

/// 读取当前应用设置。
#[tauri::command]
fn get_settings(state: tauri::State<'_, Arc<AppState>>) -> AppSettings {
    let settings = state.settings.lock().map(|g| g.clone()).unwrap_or_default();
    settings
}

/// 更新应用设置并持久化到配置文件。
///
/// `theme` 为 `"light"` / `"dark"`，非法值回退为 `"light"`。更新成功后写盘
/// 保留现有路由图，并同步到运行期设置缓存。
#[tauri::command]
fn update_settings(
    theme: Option<String>,
    start_on_boot: Option<bool>,
    launch_hidden: Option<bool>,
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<AppSettings, ServiceErrorBrief> {
    let previous = state
        .settings
        .lock()
        .map_err(|_| ServiceErrorBrief::lock_poisoned())?
        .clone();
    let mut settings = previous.clone();
    if let Some(t) = theme {
        settings.theme = if t == "dark" {
            "dark".into()
        } else {
            "light".into()
        };
    }
    if let Some(v) = start_on_boot {
        if v != previous.start_on_boot {
            set_autostart(&app, v)?;
        }
        settings.start_on_boot = v;
    }
    if let Some(v) = launch_hidden {
        settings.launch_hidden = v;
    }
    if let Err(error) = settings.save_to_config(&state.config_path) {
        if settings.start_on_boot != previous.start_on_boot {
            let _ = set_autostart(&app, previous.start_on_boot);
        }
        return Err(error);
    }
    let mut slot = state
        .settings
        .lock()
        .map_err(|_| ServiceErrorBrief::lock_poisoned())?;
    *slot = settings.clone();
    Ok(settings)
}

fn set_autostart(app: &tauri::AppHandle, enabled: bool) -> Result<(), ServiceErrorBrief> {
    let auto = app.autolaunch();
    let result = if enabled {
        auto.enable()
    } else {
        auto.disable()
    };
    result.map_err(|error| ServiceErrorBrief {
        category: "autostart",
        message: format!("更新开机自启失败: {error}"),
        endpoint_id: None,
        hresult: None,
        hint: Some("请检查系统权限后重试".into()),
    })
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
                executable_path: s.executable_path.clone(),
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
                kind: match s.kind {
                    SinkKind::Device => "device".to_owned(),
                    SinkKind::Vban => "vban".to_owned(),
                },
                stream_name: s.stream_name.clone(),
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
        SourceKind::Vban => "vban",
    }
}

impl ServiceErrorBrief {
    fn invalid_endpoint(endpoint_id: Option<String>, message: String) -> Self {
        Self {
            category: "device",
            message,
            endpoint_id,
            hresult: None,
            hint: Some("请刷新设备列表并选择与音频方向匹配的设备".into()),
        }
    }

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
        ConfigError::UnsupportedSchemaVersion(v) => {
            ("config_schema", Some(format!("配置文件版本 {v} 不受支持")))
        }
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
            executable_path,
        } => {
            let kind = match kind.as_str() {
                "device_capture" => SourceKind::DeviceCapture,
                "device_loopback" => SourceKind::DeviceLoopback,
                "process_loopback" => SourceKind::ProcessLoopback,
                "vban" => SourceKind::Vban,
                other => return Err(format!("未知 source 类型: {other}")),
            };
            RouteEdit::AddSource(SourceSpec {
                id: SourceId(id),
                kind,
                endpoint_id: endpoint_id.map(EndpointId),
                process_id,
                executable_path,
                stream_name: None,
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
            kind: SinkKind::Device,
            stream_name: None,
            remote_addr: None,
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
        | RouteEditRequest::SetExternalOutputName { .. }
        | RouteEditRequest::ReplaceProcessSourceWithDevice { .. } => {
            unreachable!("重命名 op 应在 apply_route_edit 中提前处理")
        }
    })
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// 创建系统托盘图标和右键菜单，实现应用常驻后台。
fn setup_tray(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let show_i = MenuItem::with_id(app, "show", "显示 LoopMaster", true, None::<&str>)?;
    let hide_i = MenuItem::with_id(app, "hide", "隐藏窗口", true, None::<&str>)?;
    let quit_i = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_i, &hide_i, &quit_i])?;

    // 托盘图标的候选加载路径（按优先级尝试），覆盖 dev 与各种打包布局：
    // 1. 打包后资源目录：<install>/resources/icons/tray-icon.png
    // 2. 打包/开发目录：<exe_dir>/icons/tray-icon.png（Tauri 默认会把 icons/ 复制到安装根）
    // 3. dev 模式相对路径：icons/tray-icon.png
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(p) = app
        .path()
        .resolve("icons/tray-icon.png", BaseDirectory::Resource)
    {
        candidates.push(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("icons/tray-icon.png"));
            candidates.push(dir.join("resources/icons/tray-icon.png"));
        }
    }
    candidates.push(std::path::PathBuf::from("icons/tray-icon.png"));

    let icon = candidates
        .iter()
        .find_map(|p| tauri::image::Image::from_path(p).ok())
        .ok_or_else(|| "所有候选路径均无法加载托盘图标".to_string())?;

    TrayIconBuilder::new()
        .icon(icon)
        .tooltip("LoopMaster")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.unminimize();
                    let _ = w.set_focus();
                }
            }
            "hide" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.hide();
                }
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            let handle = app.handle().clone();
            let config_path = resolve_config_path(&handle);
            let settings = AppSettings::load_from_config(&config_path);
            let state = Arc::new(AppState::new(config_path));
            {
                let mut slot = state.settings.lock().expect("设置锁未中毒");
                *slot = settings.clone();
            }
            app.manage(state);

            // 初始化本机网络身份（首次生成 node_id 并持久化），始终启动
            // Browser 监听局域网节点；Advertiser 由用户在设备页手动开启。
            let managed = app.state::<Arc<AppState>>().inner().clone();
            {
                let identity = ensure_node_identity(&managed);
                let handle = app.handle().clone();
                // Browser 持续监听（便于随时发现其他电脑）。
                start_browser(handle.clone(), &managed);
                // 若上次配置为已开启，恢复本机服务发布。
                if identity.network_enabled {
                    let _ = start_advertising(handle, &managed);
                }
            }

            // 系统托盘常驻：创建托盘图标与菜单（显示/隐藏/退出）
            let tray_ok = setup_tray(app).is_ok();
            if !tray_ok {
                eprintln!("创建系统托盘失败");
            }

            // 关闭主窗口时隐藏而非退出，使音频路由在后台持续运行
            if let Some(window) = app.get_webview_window("main") {
                let win = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        if let Err(e) = win.hide() {
                            eprintln!("隐藏窗口失败: {e}");
                        }
                    }
                });
            }

            // 启动时若配置了"隐藏主窗口"，则不显示（仅驻留托盘）。
            // 若托盘创建失败，为避免应用完全不可控，强制显示主窗口。
            if settings.launch_hidden && tray_ok {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }

            // 后台进程监控：进程重启后按可执行路径自动重绑 ProcessLoopback 声源
            let watcher_state = app.state::<Arc<AppState>>().inner().clone();
            spawn_process_watcher(app.handle().clone(), watcher_state);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            greet,
            get_app_version,
            get_node_identity,
            get_network_nodes,
            set_network_enabled,
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
            get_settings,
            update_settings,
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
                executable_path: Some("C:/app-a.exe".into()),
                stream_name: None,
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
                kind: SinkKind::Device,
                stream_name: None,
                remote_addr: None,
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
                    executable_path: Some("C:/app-a.exe".into()),
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
            executable_path: None,
        });
        assert!(error.is_err());
    }

    #[test]
    fn process_source_replacement_preserves_send_parameters_atomically() {
        let mut editor = RouteEditor::new(sample_graph());
        replace_process_source_with_device(
            &mut editor,
            "src-a",
            "src-device",
            "capture-endpoint",
            "虚拟麦克风",
        )
        .unwrap();

        let graph = editor.draft();
        let source = &graph.sources[0];
        assert_eq!(source.id, SourceId("src-device".into()));
        assert_eq!(source.kind, SourceKind::DeviceCapture);
        assert_eq!(
            source.endpoint_id,
            Some(EndpointId("capture-endpoint".into()))
        );
        assert_eq!(source.process_id, None);
        assert_eq!(source.executable_path, None);

        let send = graph
            .sends
            .iter()
            .find(|send| send.id() == &SendId("s1".into()))
            .unwrap();
        assert!(matches!(
            send,
            SendSpec::SourceToBus { source_id, .. }
                if source_id == &SourceId("src-device".into())
        ));
        assert_eq!(send.gain_db(), -3.0);
        assert!(send.muted());
        assert!(send.enabled());
    }

    #[test]
    fn invalid_process_source_replacement_keeps_original_graph() {
        let mut editor = RouteEditor::new(sample_graph());
        let before = editor.draft().clone();
        assert!(replace_process_source_with_device(
            &mut editor,
            "missing",
            "src-device",
            "capture-endpoint",
            "虚拟麦克风",
        )
        .is_err());
        assert_eq!(editor.draft(), &before);
    }
}
