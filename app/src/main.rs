//! LoopMaster 应用入口（M1 主路由页）。
//!
//! UI 只通过 `loopmaster-app-service` 访问音频能力：设备/进程枚举、
//! 路由编辑、引擎启动/停止/状态。不直接触碰 WASAPI 或引擎 worker。

slint::include_modules!();

use loopmaster_app_service::{
    DeviceCompatibility, DeviceFlow, DeviceRepository, EngineService, ProcessRepository,
};
use loopmaster_audio_core::{
    EndpointId, RouteGraph, SendSpec, SinkId, SinkSpec, SourceId, SourceKind, SourceSpec,
};
use slint::{SharedString, VecModel, Weak};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

/// UI 侧应用状态（Rc<RefCell> 供回调共享）。
struct AppState {
    service: Option<EngineService>,
    process_pids: Vec<u32>,
    sink_ids: Vec<String>,
    gain_db: f32,
    muted: bool,
    running: bool,
}

impl AppState {
    fn new() -> Self {
        Self {
            service: None,
            process_pids: Vec::new(),
            sink_ids: Vec::new(),
            gain_db: 0.0,
            muted: false,
            running: false,
        }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;
    let state = Rc::new(RefCell::new(AppState::new()));

    // 初始填充设备/进程。
    refresh_lists(&ui.as_weak(), &state);

    // 事件绑定。
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_refresh(move || {
            refresh_lists(&ui_weak, &state_rc);
            let _ = state_rc.borrow_mut();
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_start(move || {
            start_engine(&ui_weak, &state_rc);
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_stop(move || {
            stop_engine(&ui_weak, &state_rc);
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_apply_gain(move |value: f32| {
            apply_send_change(&ui_weak, &state_rc, Some(value), None);
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_toggle_mute(move |muted: bool| {
            apply_send_change(&ui_weak, &state_rc, None, Some(muted));
        });
    }

    // 状态轮询（1 Hz）。
    let ui_weak = ui.as_weak();
    let state_rc = Rc::clone(&state);
    let _timer = slint::Timer::default();
    _timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(1),
        move || {
            poll_status(&ui_weak, &state_rc);
        },
    );
    ui.run()
}

/// 刷新进程与 sink 设备列表。
fn refresh_lists(ui: &Weak<MainWindow>, state: &Rc<RefCell<AppState>>) {
    let devices = DeviceRepository::new();
    let processes = ProcessRepository::new();
    let mut state_borrow = state.borrow_mut();
    let mut errors = Vec::new();

    // 进程列表。
    let process_names: Vec<SharedString> = match processes {
        Ok(repo) => match repo.list_audio_processes() {
            Ok(list) => {
                state_borrow.process_pids = list.iter().map(|p| p.pid).collect();
                list.iter()
                    .map(|p| SharedString::from(format!("{} (PID {})", p.name, p.pid)))
                    .collect()
            }
            Err(error) => {
                state_borrow.process_pids.clear();
                errors.push(format!("进程枚举失败: {error}"));
                Vec::new()
            }
        },
        Err(error) => {
            state_borrow.process_pids.clear();
            errors.push(format!("进程枚举失败: {error}"));
            Vec::new()
        }
    };
    // sink 设备列表（render 且 RenderReady）。
    let sink_names: Vec<SharedString> = match devices {
        Ok(repo) => match repo.list_devices() {
            Ok(list) => {
                state_borrow.sink_ids = list
                    .iter()
                    .filter(|d| {
                        d.flow == DeviceFlow::Render
                            && d.compatibility == DeviceCompatibility::RenderReady
                    })
                    .map(|d| d.id.0.clone())
                    .collect();
                list.iter()
                    .filter(|d| {
                        d.flow == DeviceFlow::Render
                            && d.compatibility == DeviceCompatibility::RenderReady
                    })
                    .map(|d| SharedString::from(d.name.clone()))
                    .collect()
            }
            Err(error) => {
                state_borrow.sink_ids.clear();
                errors.push(format!("输出设备枚举失败: {error}"));
                Vec::new()
            }
        },
        Err(error) => {
            state_borrow.sink_ids.clear();
            errors.push(format!("输出设备枚举失败: {error}"));
            Vec::new()
        }
    };

    if let Some(ui) = ui.upgrade() {
        ui.set_process_model(Rc::new(VecModel::from(process_names)).into());
        ui.set_sink_model(Rc::new(VecModel::from(sink_names)).into());
        // 列表刷新后确保索引仍然有效；枚举失败时清除旧索引，避免使用陈旧 ID。
        if state_borrow.process_pids.is_empty() {
            ui.set_process_index(-1);
        } else if ui.get_process_index() < 0
            || ui.get_process_index() as usize >= state_borrow.process_pids.len()
        {
            ui.set_process_index(0);
        }
        if state_borrow.sink_ids.is_empty() {
            ui.set_sink_index(-1);
        } else if ui.get_sink_index() < 0
            || ui.get_sink_index() as usize >= state_borrow.sink_ids.len()
        {
            ui.set_sink_index(0);
        }
        if !errors.is_empty() {
            ui.set_engine_state(SharedString::from(errors.join("；")));
        }
    }
}

/// 按当前 UI 选择构造路由图。
fn build_graph(state: &mut AppState, ui: &MainWindow) -> Result<RouteGraph, String> {
    let pid = *state
        .process_pids
        .get(ui.get_process_index() as usize)
        .ok_or("请先选择音频来源进程")?;
    let sink_id = state
        .sink_ids
        .get(ui.get_sink_index() as usize)
        .ok_or("请先选择输出设备")?;
    Ok(RouteGraph {
        sources: vec![SourceSpec {
            id: SourceId("process".into()),
            kind: SourceKind::ProcessLoopback,
            endpoint_id: None,
            process_id: Some(pid),
            display_name: format!("process:{pid}"),
        }],
        sinks: vec![SinkSpec {
            id: SinkId("sink".into()),
            endpoint_id: EndpointId(sink_id.clone()),
            display_name: "sink".into(),
        }],
        sends: vec![SendSpec {
            source_id: SourceId("process".into()),
            sink_id: SinkId("sink".into()),
            gain_db: state.gain_db,
            muted: state.muted,
            channel_map: Vec::new(),
        }],
    })
}

/// 启动引擎。
fn start_engine(ui: &Weak<MainWindow>, state: &Rc<RefCell<AppState>>) {
    let mut state_borrow = state.borrow_mut();
    let Some(ui) = ui.upgrade() else {
        return;
    };
    let graph = match build_graph(&mut state_borrow, &ui) {
        Ok(graph) => graph,
        Err(message) => {
            ui.set_engine_state(SharedString::from(message));
            return;
        }
    };
    match EngineService::new(graph) {
        Ok(mut service) => match service.start() {
            Ok(()) => {
                state_borrow.service = Some(service);
                state_borrow.running = true;
                ui.set_running(true);
                ui.set_engine_state(SharedString::from("Running"));
            }
            Err(error) => {
                ui.set_engine_state(SharedString::from(error.to_string()));
            }
        },
        Err(error) => {
            ui.set_engine_state(SharedString::from(error.to_string()));
        }
    }
}

/// 停止引擎。
fn stop_engine(ui: &Weak<MainWindow>, state: &Rc<RefCell<AppState>>) {
    let mut state_borrow = state.borrow_mut();
    let Some(ui) = ui.upgrade() else {
        return;
    };
    if let Some(mut service) = state_borrow.service.take() {
        let _ = service.stop();
    }
    state_borrow.running = false;
    ui.set_running(false);
    ui.set_engine_state(SharedString::from("Stopped"));
}

/// 应用 send 级变更（增益或静音），运行中经 update_graph 生效。
fn apply_send_change(
    ui: &Weak<MainWindow>,
    state: &Rc<RefCell<AppState>>,
    gain_db: Option<f32>,
    muted: Option<bool>,
) {
    let mut state_borrow = state.borrow_mut();
    let Some(ui) = ui.upgrade() else {
        return;
    };
    // 记录参数（未启动时也生效，启动时用最新参数建图）。
    if let Some(gain) = gain_db {
        state_borrow.gain_db = gain;
    }
    if let Some(mute) = muted {
        state_borrow.muted = mute;
    }
    // 先构建图（结束对 state 的可变借用），再取服务应用。
    let graph = match build_graph(&mut state_borrow, &ui) {
        Ok(graph) => graph,
        Err(_) => return,
    };
    if let Some(service) = state_borrow.service.as_mut() {
        if let Err(error) = service.update_graph(graph) {
            ui.set_engine_state(SharedString::from(error.to_string()));
        }
    }
}

/// 轮询引擎状态并刷新统计文本。
fn poll_status(ui: &Weak<MainWindow>, state: &Rc<RefCell<AppState>>) {
    let state_borrow = state.borrow();
    let Some(ui) = ui.upgrade() else {
        return;
    };
    if let Some(service) = state_borrow.service.as_ref() {
        let status = service.status();
        ui.set_engine_state(SharedString::from(status.state.as_str()));
        ui.set_running(status.running);
        let stats = status.stats;
        let text = format!(
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
        );
        ui.set_stats_text(SharedString::from(text));
    } else {
        ui.set_stats_text(SharedString::from("引擎未启动"));
    }
}
