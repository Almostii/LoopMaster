//! LoopMaster 应用入口（M1 主路由页）。
//!
//! UI 只维护展示模型和用户意图：所有引擎操作（启动/停止/路由提交/重连）
//! 通过命令通道交给后台服务线程执行，UI 通过事件通道接收结构化
//! [`ServiceEvent`] 更新状态。UI 不直接持有引擎、不轮询实时内部结构，
//! 不触碰 WASAPI 或引擎 worker。

slint::include_modules!();

use loopmaster_app_service::{
    DeviceCompatibility, DeviceFlow, DeviceRepository, EngineCommand, EngineService,
    ProcessRepository, RouteEdit, RouteEditor, ServiceEvent,
};
use loopmaster_audio_core::{
    EndpointId, RouteGraph, SendSpec, SinkId, SinkSpec, SourceId, SourceKind, SourceSpec,
};
use loopmaster_audio_windows::AudioEngineState;
use slint::{Model, SharedString, VecModel, Weak};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

/// UI → 后台服务线程的命令（用户意图，非实时）。
enum UiCommand {
    /// 以指定路由图启动引擎（首次启动或运行中切换拓扑的自动重启）。
    Start {
        graph: RouteGraph,
    },
    Stop,
    ApplyGain {
        updates: Vec<(SourceId, SinkId, f32)>,
    },
    ApplyMuted {
        updates: Vec<(SourceId, SinkId, bool)>,
    },
    /// 请求服务线程退出（应用退出时发送）。
    Shutdown,
}

/// 后台服务线程 → UI 的事件。
enum UiEvent {
    Engine(ServiceEvent),
    Error(String),
}

/// 后台服务线程状态：持有引擎与事件订阅。
struct ServiceThread {
    engine: Option<EngineService>,
    events: Option<Receiver<ServiceEvent>>,
}

impl ServiceThread {
    fn new() -> Self {
        Self {
            engine: None,
            events: None,
        }
    }

    /// 丢弃旧引擎并创建新引擎（首次启动 / 拓扑变化重启）。
    fn restart_with(&mut self, graph: RouteGraph, out: &Sender<UiEvent>) {
        self.engine = None; // drop 旧引擎：其事件线程随之停止
        self.events = None;
        match EngineService::new(graph) {
            Ok(mut engine) => {
                self.events = Some(engine.subscribe());
                if let Err(error) = engine.start() {
                    let _ = out.send(UiEvent::Error(error.to_string()));
                }
                self.engine = Some(engine);
            }
            Err(error) => {
                let _ = out.send(UiEvent::Error(error.to_string()));
            }
        }
    }

    fn stop(&mut self, out: &Sender<UiEvent>) {
        self.engine = None;
        self.events = None;
        let _ = out.send(UiEvent::Engine(ServiceEvent::StateChanged(
            AudioEngineState::Stopped,
        )));
    }

    /// 对运行中的引擎执行一条命令；引擎未运行时静默忽略（参数已由 UI 保存，
    /// 下次启动生效）。
    fn apply<F>(&self, op: F) -> Result<(), String>
    where
        F: FnOnce(&EngineService) -> Result<(), String>,
    {
        match self.engine.as_ref() {
            Some(engine) => op(engine),
            None => Ok(()),
        }
    }
}

/// 后台服务线程主循环：中继引擎事件 + 执行命令。
///
/// 事件中继使用有界轮询（不阻塞），避免引擎事件堆积在订阅通道中；
/// 命令处理非实时，绝不阻塞音频线程。
fn service_loop(rx: Receiver<UiCommand>, out: Sender<UiEvent>) {
    let mut service = ServiceThread::new();
    loop {
        // 先中继引擎事件。
        if let Some(events) = service.events.as_ref() {
            while let Ok(event) = events.try_recv() {
                let _ = out.send(UiEvent::Engine(event));
            }
        }
        // 再处理命令。
        let command = match rx.try_recv() {
            Ok(UiCommand::Shutdown) => break,
            Ok(command) => command,
            Err(TryRecvError::Empty) => {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(TryRecvError::Disconnected) => break,
        };
        match command {
            UiCommand::Start { graph } => service.restart_with(graph, &out),
            UiCommand::Stop => service.stop(&out),
            UiCommand::ApplyGain { updates } => {
                for (source_id, sink_id, gain_db) in updates {
                    if let Err(error) = service.apply(|engine| {
                        engine
                            .command(EngineCommand::SetGain {
                                source_id,
                                sink_id,
                                gain_db,
                            })
                            .map_err(|error| error.to_string())
                    }) {
                        let _ = out.send(UiEvent::Error(error));
                    }
                }
            }
            UiCommand::ApplyMuted { updates } => {
                for (source_id, sink_id, muted) in updates {
                    if let Err(error) = service.apply(|engine| {
                        engine
                            .command(EngineCommand::SetMuted {
                                source_id,
                                sink_id,
                                muted,
                            })
                            .map_err(|error| error.to_string())
                    }) {
                        let _ = out.send(UiEvent::Error(error));
                    }
                }
            }
            UiCommand::Shutdown => break, // 实际在命令接收处处理，防御性覆盖
        }
    }
}

struct RefreshResult {
    process_entries: Option<Vec<(u32, String)>>,
    sink_entries: Option<Vec<(String, String)>>,
    errors: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AddTarget {
    Source,
    Sink,
}

/// UI 侧应用状态（Rc<RefCell> 供回调共享）。
struct AppState {
    command_tx: Option<Sender<UiCommand>>,
    event_receiver: Receiver<UiEvent>,
    process_pids: Vec<u32>,
    sink_ids: Vec<String>,
    gain_db: f32,
    muted: bool,
    running: bool,
    /// 引擎当前是否活动（Running/Degraded/Reconnecting）。degraded 与
    /// reconnecting 时引擎仍在运行旧路由，拓扑变化同样需要重启。
    engine_active: bool,
    /// 最近一次已提交（启动/重启）的路由，用于检测拓扑变化。
    committed_route: Option<(u32, String)>,
    refresh_receiver: Option<Receiver<RefreshResult>>,
    refreshing: bool,
    selected_process_pid: Option<u32>,
    selected_sink_id: Option<String>,
    /// 主路由页的可编辑草稿；所有 source/sink/send 均使用稳定 ID。
    route_editor: RouteEditor,
    /// 加号点击后等待本轮异步枚举完成，再将当前选择加入草稿。
    pending_add: Option<AddTarget>,
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;
    let (command_tx, command_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let service_handle = thread::Builder::new()
        .name("loopmaster-ui-service".into())
        .spawn(move || service_loop(command_rx, event_tx))
        .expect("创建 UI 服务线程失败");
    let state = Rc::new(RefCell::new(AppState {
        command_tx: Some(command_tx),
        event_receiver: event_rx,
        process_pids: Vec::new(),
        sink_ids: Vec::new(),
        gain_db: 0.0,
        muted: false,
        running: false,
        engine_active: false,
        committed_route: None,
        refresh_receiver: None,
        refreshing: false,
        selected_process_pid: None,
        selected_sink_id: None,
        route_editor: RouteEditor::new(RouteGraph::default()),
        pending_add: None,
    }));

    // 初始填充设备/进程；枚举在后台线程执行，避免阻塞 UI。
    request_refresh(&ui.as_weak(), &state);

    // 事件绑定：回调只记录用户意图并发送命令，不做引擎操作。
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_refresh(move || {
            request_refresh(&ui_weak, &state_rc);
        });
    }
    // 加号先刷新可选列表，枚举完成后把当前选择实际加入 RouteGraph 草稿。
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_add_source(move || {
            state_rc.borrow_mut().pending_add = Some(AddTarget::Source);
            request_refresh(&ui_weak, &state_rc);
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_add_output(move || {
            state_rc.borrow_mut().pending_add = Some(AddTarget::Sink);
            request_refresh(&ui_weak, &state_rc);
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_add_monitor(move || {
            state_rc.borrow_mut().pending_add = Some(AddTarget::Sink);
            request_refresh(&ui_weak, &state_rc);
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_start_engine(move || {
            start_engine(&ui_weak, &state_rc);
        });
    }
    {
        let state_rc = Rc::clone(&state);
        ui.on_stop_engine(move || {
            send_command(&state_rc, UiCommand::Stop);
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_apply_gain(move |value: f32| {
            apply_gain(&ui_weak, &state_rc, value);
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_toggle_route(move |enabled: bool| {
            toggle_route(&ui_weak, &state_rc, enabled);
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_toggle_monitor(move |enabled: bool| {
            toggle_monitor(&ui_weak, &state_rc, enabled);
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_source_selected(move |_| {
            selection_changed(&ui_weak, &state_rc);
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_output_selected(move |_| {
            selection_changed(&ui_weak, &state_rc);
        });
    }

    // 低频定时器：接收后台刷新结果与引擎事件，更新展示模型。
    let ui_weak = ui.as_weak();
    let state_rc = Rc::clone(&state);
    let _timer = slint::Timer::default();
    _timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            poll_refresh(&ui_weak, &state_rc);
            poll_service_events(&ui_weak, &state_rc);
        },
    );
    let result = ui.run();
    // 优雅退出：通知服务线程停止并等待其结束。
    if let Some(tx) = state.borrow().command_tx.as_ref() {
        let _ = tx.send(UiCommand::Shutdown);
    }
    let _ = service_handle.join();
    result
}

fn effective_mute(source_enabled: bool, monitor_enabled: bool) -> bool {
    !source_enabled || !monitor_enabled
}

/// 引擎状态 → (结构化 phase, 中文显示文本)。
fn phase_and_label(engine_state: AudioEngineState) -> (&'static str, &'static str) {
    match engine_state {
        AudioEngineState::Stopped => ("stopped", "已停止"),
        AudioEngineState::Running => ("running", "运行中"),
        AudioEngineState::Degraded => ("degraded", "降级：设备异常，自动重连中"),
        AudioEngineState::Reconnecting => ("reconnecting", "重连中"),
        AudioEngineState::Failed => ("failed", "运行失败"),
    }
}

/// 统计快照 → 底部统计文本。
fn stats_text(stats: &loopmaster_audio_windows::AudioEngineStats) -> String {
    format!(
        "capture packet: {} | render writes: {} | underflow: {} | discontinuity: {} | peak: {:.1} dBFS",
        stats.capture_packets,
        stats.render_writes,
        stats.fifo_underflows,
        stats.discontinuities,
        if stats.rendered_peak > 0.0 {
            20.0 * stats.rendered_peak.log10()
        } else {
            -120.0
        }
    )
}

/// 发送一条命令到后台服务线程。
fn send_command(state: &Rc<RefCell<AppState>>, command: UiCommand) {
    if let Some(tx) = state.borrow().command_tx.as_ref() {
        let _ = tx.send(command);
    }
}

/// 记住当前稳定标识，而不是记住列表索引。
fn remember_selection(ui: &Weak<MainWindow>, state: &Rc<RefCell<AppState>>) {
    let Some(ui) = ui.upgrade() else { return };
    let mut state = state.borrow_mut();
    state.selected_process_pid = state
        .process_pids
        .get(ui.get_source_index() as usize)
        .copied();
    state.selected_sink_id = state.sink_ids.get(ui.get_output_index() as usize).cloned();
}

/// 请求异步刷新。刷新期间不会重复启动枚举线程。
fn request_refresh(ui: &Weak<MainWindow>, state: &Rc<RefCell<AppState>>) {
    let Some(ui) = ui.upgrade() else { return };
    let (sender, receiver) = mpsc::channel();
    {
        let mut state = state.borrow_mut();
        if state.refreshing {
            return;
        }
        state.selected_process_pid = state
            .process_pids
            .get(ui.get_source_index() as usize)
            .copied()
            .or(state.selected_process_pid);
        state.selected_sink_id = state
            .sink_ids
            .get(ui.get_output_index() as usize)
            .cloned()
            .or_else(|| state.selected_sink_id.clone());
        state.refresh_receiver = Some(receiver);
        state.refreshing = true;
    }
    ui.set_refreshing(true);

    thread::spawn(move || {
        let mut errors = Vec::new();
        let process_entries =
            match ProcessRepository::new().and_then(|repo| repo.list_audio_processes()) {
                Ok(list) => Some(list.into_iter().map(|p| (p.pid, p.name)).collect()),
                Err(error) => {
                    errors.push(format!("进程枚举失败: {error}"));
                    None
                }
            };
        let sink_entries = match DeviceRepository::new().and_then(|repo| repo.list_devices()) {
            Ok(list) => Some(
                list.into_iter()
                    .filter(|device| {
                        device.flow == DeviceFlow::Render
                            && device.compatibility == DeviceCompatibility::RenderReady
                    })
                    .map(|device| (device.id.0, device.name))
                    .collect(),
            ),
            Err(error) => {
                errors.push(format!("输出设备枚举失败: {error}"));
                None
            }
        };
        let _ = sender.send(RefreshResult {
            process_entries,
            sink_entries,
            errors,
        });
    });
}

/// 低频检查后台枚举结果；这里不执行任何 WASAPI/进程枚举。
fn poll_refresh(ui: &Weak<MainWindow>, state: &Rc<RefCell<AppState>>) {
    let result = {
        let mut state = state.borrow_mut();
        let Some(receiver) = state.refresh_receiver.as_ref() else {
            return;
        };
        match receiver.try_recv() {
            Ok(result) => {
                state.refresh_receiver = None;
                state.refreshing = false;
                Some(result)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                state.refresh_receiver = None;
                state.refreshing = false;
                Some(RefreshResult {
                    process_entries: None,
                    sink_entries: None,
                    errors: vec!["后台刷新线程异常退出".into()],
                })
            }
        }
    };
    if let Some(result) = result {
        apply_refresh_result(ui, state, result);
    }
}

fn apply_refresh_result(
    ui: &Weak<MainWindow>,
    state: &Rc<RefCell<AppState>>,
    result: RefreshResult,
) {
    let Some(ui) = ui.upgrade() else { return };
    let pending_add = {
        let mut state = state.borrow_mut();
        if let Some(entries) = result.process_entries {
            let ids: Vec<u32> = entries.iter().map(|(pid, _)| *pid).collect();
            let names: Vec<SharedString> = entries
                .iter()
                .map(|(pid, name)| SharedString::from(format!("{name} (PID {pid})")))
                .collect();
            let index = preserve_selection_index(
                state.selected_process_pid.as_ref(),
                &ids,
                ui.get_source_index(),
            );
            state.process_pids = ids;
            state.selected_process_pid = usize::try_from(index)
                .ok()
                .and_then(|index| state.process_pids.get(index).copied());
            ui.set_source_model(Rc::new(VecModel::from(names)).into());
            ui.set_source_index(index);
        }
        if let Some(entries) = result.sink_entries {
            let ids: Vec<String> = entries.iter().map(|(id, _)| id.clone()).collect();
            let names: Vec<SharedString> = entries
                .iter()
                .map(|(_, name)| SharedString::from(name.clone()))
                .collect();
            let index = preserve_selection_index(
                state.selected_sink_id.as_ref(),
                &ids,
                ui.get_output_index(),
            );
            state.sink_ids = ids;
            state.selected_sink_id = usize::try_from(index)
                .ok()
                .and_then(|index| state.sink_ids.get(index).cloned());
            ui.set_output_model(Rc::new(VecModel::from(names)).into());
            ui.set_output_index(index);
        }
        state.pending_add.take()
    };
    ui.set_refreshing(false);
    if !result.errors.is_empty() {
        ui.set_engine_state(SharedString::from(result.errors.join("；")));
    } else if let Some(target) = pending_add {
        match target {
            AddTarget::Source => add_selected_source(&ui, state),
            AddTarget::Sink => add_selected_sink(&ui, state),
        }
    } else if !state.borrow().running {
        ui.set_engine_phase(SharedString::from("stopped"));
        ui.set_engine_state(SharedString::from("已停止"));
    }
}

/// 根据稳定 ID 保留选择；已退出/不可用的对象返回 -1。
fn preserve_selection_index<T: PartialEq>(
    previous_id: Option<&T>,
    new_ids: &[T],
    fallback_index: i32,
) -> i32 {
    if let Some(previous_id) = previous_id {
        return new_ids
            .iter()
            .position(|id| id == previous_id)
            .map_or(-1, |index| index as i32);
    }
    if new_ids.is_empty() {
        -1
    } else if usize::try_from(fallback_index)
        .ok()
        .is_some_and(|index| index < new_ids.len())
    {
        fallback_index
    } else {
        0
    }
}

fn source_id_for_pid(pid: u32) -> SourceId {
    SourceId(format!("process:{pid}"))
}

fn sink_id_for_endpoint(endpoint_id: &str) -> SinkId {
    SinkId(format!("sink:{endpoint_id}"))
}

fn selected_source(state: &AppState, ui: &MainWindow) -> Result<(u32, String), String> {
    let index = usize::try_from(ui.get_source_index()).map_err(|_| "请先选择音频来源进程")?;
    let pid = *state
        .process_pids
        .get(index)
        .ok_or("请先选择音频来源进程")?;
    let name = ui
        .get_source_model()
        .row_data(index)
        .map(|value| value.to_string())
        .unwrap_or_else(|| format!("进程 PID {pid}"));
    Ok((pid, name))
}

fn selected_sink(state: &AppState, ui: &MainWindow) -> Result<(String, String), String> {
    let index = usize::try_from(ui.get_output_index()).map_err(|_| "请先选择输出设备")?;
    let endpoint_id = state
        .sink_ids
        .get(index)
        .cloned()
        .ok_or("请先选择输出设备")?;
    let name = ui
        .get_output_model()
        .row_data(index)
        .map(|value| value.to_string())
        .unwrap_or_else(|| endpoint_id.clone());
    Ok((endpoint_id, name))
}

fn add_selected_source(ui: &MainWindow, state: &Rc<RefCell<AppState>>) {
    let (pid, display_name) = match selected_source(&state.borrow(), ui) {
        Ok(value) => value,
        Err(message) => {
            ui.set_engine_state(SharedString::from(message));
            return;
        }
    };
    let source_id = source_id_for_pid(pid);
    let mut state_borrow = state.borrow_mut();
    if state_borrow
        .route_editor
        .draft()
        .sources
        .iter()
        .any(|source| source.id == source_id)
    {
        ui.set_engine_state(SharedString::from("该音频来源已在当前路由中"));
        return;
    }
    let source = SourceSpec {
        id: source_id.clone(),
        kind: SourceKind::ProcessLoopback,
        endpoint_id: None,
        process_id: Some(pid),
        display_name,
    };
    if let Err(error) = state_borrow
        .route_editor
        .apply(RouteEdit::AddSource(source))
    {
        ui.set_engine_state(SharedString::from(error.to_string()));
        return;
    }
    let sinks = state_borrow.route_editor.draft().sinks.clone();
    for sink in sinks {
        let send = SendSpec {
            source_id: source_id.clone(),
            sink_id: sink.id,
            gain_db: state_borrow.gain_db,
            muted: state_borrow.muted,
            enabled: true,
            channel_map: Vec::new(),
        };
        if let Err(error) = state_borrow.route_editor.apply(RouteEdit::SetSend(send)) {
            ui.set_engine_state(SharedString::from(error.to_string()));
            return;
        }
    }
    ui.set_engine_state(SharedString::from("已添加音频来源"));
    drop(state_borrow);
    sync_route_counts(ui, state);
    submit_draft_if_ready(ui, state);
}

fn add_selected_sink(ui: &MainWindow, state: &Rc<RefCell<AppState>>) {
    let (endpoint_id, display_name) = match selected_sink(&state.borrow(), ui) {
        Ok(value) => value,
        Err(message) => {
            ui.set_engine_state(SharedString::from(message));
            return;
        }
    };
    let sink_id = sink_id_for_endpoint(&endpoint_id);
    let mut state_borrow = state.borrow_mut();
    if state_borrow
        .route_editor
        .draft()
        .sinks
        .iter()
        .any(|sink| sink.id == sink_id)
    {
        ui.set_engine_state(SharedString::from("该输出设备已在当前路由中"));
        return;
    }
    let sink = SinkSpec {
        id: sink_id.clone(),
        endpoint_id: EndpointId(endpoint_id),
        display_name,
    };
    if let Err(error) = state_borrow.route_editor.apply(RouteEdit::AddSink(sink)) {
        ui.set_engine_state(SharedString::from(error.to_string()));
        return;
    }
    let sources = state_borrow.route_editor.draft().sources.clone();
    for source in sources {
        let send = SendSpec {
            source_id: source.id,
            sink_id: sink_id.clone(),
            gain_db: state_borrow.gain_db,
            muted: state_borrow.muted,
            enabled: true,
            channel_map: Vec::new(),
        };
        if let Err(error) = state_borrow.route_editor.apply(RouteEdit::SetSend(send)) {
            ui.set_engine_state(SharedString::from(error.to_string()));
            return;
        }
    }
    ui.set_engine_state(SharedString::from("已添加输出设备"));
    drop(state_borrow);
    sync_route_counts(ui, state);
    submit_draft_if_ready(ui, state);
}

fn submit_draft_if_ready(ui: &MainWindow, state: &Rc<RefCell<AppState>>) {
    let graph = state.borrow().route_editor.draft().clone();
    if graph.sources.is_empty() || graph.sinks.is_empty() || graph.sends.is_empty() {
        return;
    }
    if let Err(error) = graph.validate() {
        ui.set_engine_state(SharedString::from(error.to_string()));
        return;
    }
    if state.borrow().engine_active {
        ui.set_engine_phase(SharedString::from("starting"));
        ui.set_engine_state(SharedString::from("路由草稿已更新，正在重启…"));
        send_command(state, UiCommand::Start { graph });
    }
}

fn sync_route_counts(ui: &MainWindow, state: &Rc<RefCell<AppState>>) {
    let state_borrow = state.borrow();
    ui.set_route_source_count(state_borrow.route_editor.draft().sources.len() as i32);
    ui.set_route_output_count(state_borrow.route_editor.draft().sinks.len() as i32);
}

/// 按当前 UI 选择构造并校验路由图草稿。
fn build_graph(state: &AppState, _ui: &MainWindow) -> Result<RouteGraph, String> {
    let graph = state.route_editor.draft().clone();
    if graph.sources.is_empty() {
        return Err("请先点击音频来源列的 + 添加音源".into());
    }
    if graph.sinks.is_empty() {
        return Err("请先点击输出通道列的 + 添加输出设备".into());
    }
    if graph.sends.is_empty() {
        return Err("当前路由没有可用的发送连接".into());
    }
    graph.validate().map_err(|error| error.to_string())?;
    Ok(graph)
}

/// 启动引擎：构建路由图并交给后台服务线程。
fn start_engine(ui: &Weak<MainWindow>, state: &Rc<RefCell<AppState>>) {
    let Some(ui) = ui.upgrade() else { return };
    let graph = match build_graph(&state.borrow(), &ui) {
        Ok(graph) => graph,
        Err(message) => {
            ui.set_engine_state(SharedString::from(message));
            return;
        }
    };
    let committed = {
        let state_borrow = state.borrow();
        state_borrow
            .route_editor
            .draft()
            .sources
            .first()
            .and_then(|source| {
                source.process_id.and_then(|pid| {
                    state_borrow
                        .route_editor
                        .draft()
                        .sinks
                        .first()
                        .map(|sink| (pid, sink.endpoint_id.0.clone()))
                })
            })
    };
    state.borrow_mut().committed_route = committed;
    ui.set_engine_phase(SharedString::from("starting"));
    ui.set_engine_state(SharedString::from("正在启动…"));
    send_command(state, UiCommand::Start { graph });
}

/// 应用增益：记录参数（未启动时下次启动生效）并热更新运行中的引擎。
fn apply_gain(ui: &Weak<MainWindow>, state: &Rc<RefCell<AppState>>, gain_db: f32) {
    let updates = {
        let mut state_borrow = state.borrow_mut();
        state_borrow.gain_db = gain_db;
        let sends = state_borrow.route_editor.draft().sends.clone();
        let mut updates = Vec::with_capacity(sends.len());
        for send in sends {
            if state_borrow
                .route_editor
                .apply(RouteEdit::SetSendGain {
                    source_id: send.source_id.clone(),
                    sink_id: send.sink_id.clone(),
                    gain_db,
                })
                .is_ok()
            {
                updates.push((send.source_id, send.sink_id, gain_db));
            }
        }
        updates
    };
    if !updates.is_empty() {
        send_command(state, UiCommand::ApplyGain { updates });
    }
    // 提升感：UI 立即回显（引擎侧 block 边界生效）。
    if let Some(ui) = ui.upgrade() {
        ui.set_gain(gain_db);
    }
}

/// 应用路由开关（source 侧）：与 monitor 开关合成 effective mute。
fn toggle_route(ui: &Weak<MainWindow>, state: &Rc<RefCell<AppState>>, enabled: bool) {
    let monitor_enabled = ui
        .upgrade()
        .map(|ui| ui.get_monitor_enabled())
        .unwrap_or(true);
    apply_effective_mute(state, effective_mute(enabled, monitor_enabled));
}

/// 应用监听开关（monitor 侧）：与 route 开关合成 effective mute。
fn toggle_monitor(ui: &Weak<MainWindow>, state: &Rc<RefCell<AppState>>, enabled: bool) {
    let route_enabled = ui
        .upgrade()
        .map(|ui| ui.get_route_enabled())
        .unwrap_or(true);
    apply_effective_mute(state, effective_mute(route_enabled, enabled));
}

fn apply_effective_mute(state: &Rc<RefCell<AppState>>, muted: bool) {
    let updates = {
        let mut state_borrow = state.borrow_mut();
        state_borrow.muted = muted;
        let sends = state_borrow.route_editor.draft().sends.clone();
        let mut updates = Vec::with_capacity(sends.len());
        for send in sends {
            if state_borrow
                .route_editor
                .apply(RouteEdit::SetSendMuted {
                    source_id: send.source_id.clone(),
                    sink_id: send.sink_id.clone(),
                    muted,
                })
                .is_ok()
            {
                updates.push((send.source_id, send.sink_id, muted));
            }
        }
        updates
    };
    if !updates.is_empty() {
        send_command(state, UiCommand::ApplyMuted { updates });
    }
}

fn replace_source_selection(state: &mut AppState, pid: u32, display_name: String) {
    let next_id = source_id_for_pid(pid);
    let sources = &state.route_editor.draft().sources;
    if sources.first().is_some_and(|source| source.id == next_id)
        || sources.iter().skip(1).any(|source| source.id == next_id)
    {
        return;
    }
    if let Some(old) = state.route_editor.draft().sources.first().cloned() {
        let _ = state.route_editor.apply(RouteEdit::RemoveSource(old.id));
    }
    let source = SourceSpec {
        id: next_id.clone(),
        kind: SourceKind::ProcessLoopback,
        endpoint_id: None,
        process_id: Some(pid),
        display_name,
    };
    if state
        .route_editor
        .apply(RouteEdit::AddSource(source))
        .is_err()
    {
        return;
    }
    for sink in state.route_editor.draft().sinks.clone() {
        let _ = state.route_editor.apply(RouteEdit::SetSend(SendSpec {
            source_id: next_id.clone(),
            sink_id: sink.id,
            gain_db: state.gain_db,
            muted: state.muted,
            enabled: true,
            channel_map: Vec::new(),
        }));
    }
}

fn replace_sink_selection(state: &mut AppState, endpoint_id: String, display_name: String) {
    let next_id = sink_id_for_endpoint(&endpoint_id);
    let sinks = &state.route_editor.draft().sinks;
    if sinks.first().is_some_and(|sink| sink.id == next_id)
        || sinks.iter().skip(1).any(|sink| sink.id == next_id)
    {
        return;
    }
    if let Some(old) = state.route_editor.draft().sinks.first().cloned() {
        let _ = state.route_editor.apply(RouteEdit::RemoveSink(old.id));
    }
    if state
        .route_editor
        .apply(RouteEdit::AddSink(SinkSpec {
            id: next_id.clone(),
            endpoint_id: EndpointId(endpoint_id),
            display_name,
        }))
        .is_err()
    {
        return;
    }
    for source in state.route_editor.draft().sources.clone() {
        let _ = state.route_editor.apply(RouteEdit::SetSend(SendSpec {
            source_id: source.id,
            sink_id: next_id.clone(),
            gain_db: state.gain_db,
            muted: state.muted,
            enabled: true,
            channel_map: Vec::new(),
        }));
    }
}

/// 运行中切换 source/sink 属于拓扑变化：后台自动停止并重建引擎，
/// UI 显示"正在重启"，不悄悄丢弃修改。
fn selection_changed(ui: &Weak<MainWindow>, state: &Rc<RefCell<AppState>>) {
    remember_selection(ui, state);
    let Some(ui) = ui.upgrade() else { return };
    let mut state_borrow = state.borrow_mut();
    if let Ok((pid, name)) = selected_source(&state_borrow, &ui) {
        replace_source_selection(&mut state_borrow, pid, name);
    }
    if let Ok((endpoint_id, name)) = selected_sink(&state_borrow, &ui) {
        replace_sink_selection(&mut state_borrow, endpoint_id, name);
    }
    let graph = build_graph(&state_borrow, &ui).ok();
    let should_restart = state_borrow.engine_active && graph.is_some();
    drop(state_borrow);
    sync_route_counts(&ui, state);
    if should_restart {
        ui.set_engine_phase(SharedString::from("starting"));
        ui.set_engine_state(SharedString::from("路由变化，正在重启…"));
        send_command(
            state,
            UiCommand::Start {
                graph: graph.unwrap(),
            },
        );
    }
}

/// 低频消费服务事件，更新展示模型（不轮询实时内部结构）。
fn poll_service_events(ui: &Weak<MainWindow>, state: &Rc<RefCell<AppState>>) {
    let Some(ui) = ui.upgrade() else { return };
    loop {
        let event = match state.borrow_mut().event_receiver.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        };
        match event {
            UiEvent::Engine(ServiceEvent::StateChanged(engine_state)) => {
                let (phase, label) = phase_and_label(engine_state);
                let running = engine_state == AudioEngineState::Running;
                let active = matches!(
                    engine_state,
                    AudioEngineState::Running
                        | AudioEngineState::Degraded
                        | AudioEngineState::Reconnecting
                );
                let mut state_borrow = state.borrow_mut();
                state_borrow.running = running;
                state_borrow.engine_active = active;
                drop(state_borrow);
                ui.set_running(running);
                ui.set_engine_phase(SharedString::from(phase));
                ui.set_engine_state(SharedString::from(label));
            }
            UiEvent::Engine(ServiceEvent::StatsChanged(stats)) => {
                let peak = stats.rendered_peak.clamp(0.0, 1.0);
                ui.set_source_meter(peak);
                ui.set_output_meter_l(peak);
                ui.set_output_meter_r(peak);
                ui.set_stats_text(SharedString::from(stats_text(&stats)));
            }
            UiEvent::Engine(ServiceEvent::DeviceLost(endpoint)) => {
                let mut state_borrow = state.borrow_mut();
                state_borrow.running = false;
                state_borrow.engine_active = true; // 引擎仍在重连旧路由
                drop(state_borrow);
                ui.set_running(false);
                ui.set_engine_phase(SharedString::from("reconnecting"));
                ui.set_engine_state(SharedString::from(format!(
                    "设备丢失：{}，正在重连…",
                    endpoint.0
                )));
            }
            UiEvent::Engine(ServiceEvent::DeviceRestored(endpoint)) => {
                ui.set_engine_phase(SharedString::from("running"));
                ui.set_engine_state(SharedString::from(format!("设备已恢复：{}", endpoint.0)));
            }
            UiEvent::Error(message) => {
                let mut state_borrow = state.borrow_mut();
                state_borrow.running = false;
                state_borrow.engine_active = false;
                drop(state_borrow);
                ui.set_running(false);
                ui.set_engine_phase(SharedString::from("failed"));
                ui.set_engine_state(SharedString::from(message));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        effective_mute, phase_and_label, preserve_selection_index, sink_id_for_endpoint,
        source_id_for_pid, stats_text,
    };
    use loopmaster_audio_windows::{AudioEngineState, AudioEngineStats};

    #[test]
    fn route_is_muted_when_either_side_is_disabled() {
        assert!(!effective_mute(true, true));
        assert!(effective_mute(false, true));
        assert!(effective_mute(true, false));
        assert!(effective_mute(false, false));
    }

    #[test]
    fn refresh_preserves_existing_process_selection_by_pid() {
        let old_pid = 7440;
        assert_eq!(
            preserve_selection_index(Some(&old_pid), &[19556, old_pid, 3024], 0),
            1
        );
    }

    #[test]
    fn refresh_clears_selection_when_process_has_exited() {
        let old_pid = 7440;
        assert_eq!(
            preserve_selection_index(Some(&old_pid), &[19556, 3024], 1),
            -1
        );
    }

    #[test]
    fn refresh_uses_valid_fallback_when_no_previous_selection_exists() {
        assert_eq!(preserve_selection_index::<u32>(None, &[19556, 3024], 1), 1);
        assert_eq!(preserve_selection_index::<u32>(None, &[19556, 3024], 8), 0);
        assert_eq!(preserve_selection_index::<u32>(None, &[], -1), -1);
    }

    #[test]
    fn phase_and_label_covers_all_engine_states() {
        assert_eq!(phase_and_label(AudioEngineState::Stopped).0, "stopped");
        assert_eq!(phase_and_label(AudioEngineState::Running).0, "running");
        assert_eq!(phase_and_label(AudioEngineState::Degraded).0, "degraded");
        assert_eq!(
            phase_and_label(AudioEngineState::Reconnecting).0,
            "reconnecting"
        );
        assert_eq!(phase_and_label(AudioEngineState::Failed).0, "failed");
        for (_, label) in [
            AudioEngineState::Stopped,
            AudioEngineState::Running,
            AudioEngineState::Degraded,
            AudioEngineState::Reconnecting,
            AudioEngineState::Failed,
        ]
        .map(phase_and_label)
        {
            assert!(!label.is_empty());
        }
    }

    #[test]
    fn stats_text_renders_silence_peak_as_minus_120_dbfs() {
        let stats = AudioEngineStats::default();
        let text = stats_text(&stats);
        assert!(text.contains("-120.0 dBFS"));
    }

    #[test]
    fn route_node_ids_are_stable_for_same_backend_identity() {
        assert_eq!(source_id_for_pid(42), source_id_for_pid(42));
        assert_ne!(source_id_for_pid(42), source_id_for_pid(43));
        assert_eq!(
            sink_id_for_endpoint("{0.0.0}.{endpoint}"),
            sink_id_for_endpoint("{0.0.0}.{endpoint}")
        );
    }
}
