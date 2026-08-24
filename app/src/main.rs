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
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

struct RefreshResult {
    process_entries: Option<Vec<(u32, String)>>,
    sink_entries: Option<Vec<(String, String)>>,
    errors: Vec<String>,
}

/// UI 侧应用状态（Rc<RefCell> 供回调共享）。
struct AppState {
    service: Option<EngineService>,
    process_pids: Vec<u32>,
    sink_ids: Vec<String>,
    gain_db: f32,
    muted: bool,
    running: bool,
    refresh_receiver: Option<Receiver<RefreshResult>>,
    refreshing: bool,
    selected_process_pid: Option<u32>,
    selected_sink_id: Option<String>,
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
            refresh_receiver: None,
            refreshing: false,
            selected_process_pid: None,
            selected_sink_id: None,
        }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;
    let state = Rc::new(RefCell::new(AppState::new()));

    // 初始填充设备/进程；枚举在后台线程执行，避免阻塞 UI。
    request_refresh(&ui.as_weak(), &state);

    // 事件绑定。
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_refresh(move || {
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
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_stop_engine(move || {
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
        ui.on_toggle_route(move |enabled: bool| {
            let monitor_enabled = ui_weak
                .upgrade()
                .map(|ui| ui.get_monitor_enabled())
                .unwrap_or(true);
            apply_send_change(
                &ui_weak,
                &state_rc,
                None,
                Some(effective_mute(enabled, monitor_enabled)),
            );
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_toggle_monitor(move |enabled: bool| {
            let source_enabled = ui_weak
                .upgrade()
                .map(|ui| ui.get_route_enabled())
                .unwrap_or(true);
            apply_send_change(
                &ui_weak,
                &state_rc,
                None,
                Some(effective_mute(source_enabled, enabled)),
            );
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_source_selected(move |_| {
            remember_selection(&ui_weak, &state_rc);
            apply_route_selection(&ui_weak, &state_rc);
        });
    }
    {
        let ui_weak = ui.as_weak();
        let state_rc = Rc::clone(&state);
        ui.on_output_selected(move |_| {
            remember_selection(&ui_weak, &state_rc);
            apply_route_selection(&ui_weak, &state_rc);
        });
    }

    // 高频刷新峰值与状态，让电平表保持可读的响应速度。
    let ui_weak = ui.as_weak();
    let state_rc = Rc::clone(&state);
    let _timer = slint::Timer::default();
    _timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            poll_refresh(&ui_weak, &state_rc);
            poll_status(&ui_weak, &state_rc);
        },
    );
    ui.run()
}

fn effective_mute(source_enabled: bool, monitor_enabled: bool) -> bool {
    !source_enabled || !monitor_enabled
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
    ui.set_engine_state(SharedString::from("正在刷新音源和设备"));

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
        let index =
            preserve_selection_index(state.selected_sink_id.as_ref(), &ids, ui.get_output_index());
        state.sink_ids = ids;
        state.selected_sink_id = usize::try_from(index)
            .ok()
            .and_then(|index| state.sink_ids.get(index).cloned());
        ui.set_output_model(Rc::new(VecModel::from(names)).into());
        ui.set_output_index(index);
    }
    ui.set_refreshing(false);
    if !result.errors.is_empty() {
        ui.set_engine_state(SharedString::from(result.errors.join("；")));
    } else if !state.running {
        ui.set_engine_state(SharedString::from("Stopped"));
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

/// 按当前 UI 选择构造路由图。
fn build_graph(state: &mut AppState, ui: &MainWindow) -> Result<RouteGraph, String> {
    let pid = *state
        .process_pids
        .get(ui.get_source_index() as usize)
        .ok_or("请先选择音频来源进程")?;
    let sink_id = state
        .sink_ids
        .get(ui.get_output_index() as usize)
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
                state_borrow.running = false;
                ui.set_running(false);
                ui.set_engine_state(SharedString::from(error.to_string()));
            }
        },
        Err(error) => {
            state_borrow.running = false;
            ui.set_running(false);
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
    ui.set_source_meter(0.0);
    ui.set_output_meter_l(0.0);
    ui.set_output_meter_r(0.0);
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
        // Source/monitor 开关分别由 UI 双向绑定维护；这里仅保存两者合成后的
        // effective mute，避免更新一个开关时反向改写另一个开关。
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

/// 运行中更换 source/sink 时重建单路引擎；停止时选择结果会在下次启动使用。
fn apply_route_selection(ui: &Weak<MainWindow>, state: &Rc<RefCell<AppState>>) {
    let mut state_borrow = state.borrow_mut();
    if !state_borrow.running {
        return;
    }
    let Some(mut old_service) = state_borrow.service.take() else {
        state_borrow.running = false;
        return;
    };
    let _ = old_service.stop();
    state_borrow.running = false;

    let Some(ui) = ui.upgrade() else {
        return;
    };
    ui.set_running(false);

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
            Err(error) => ui.set_engine_state(SharedString::from(error.to_string())),
        },
        Err(error) => ui.set_engine_state(SharedString::from(error.to_string())),
    }
}

/// 轮询引擎状态并刷新统计文本。
fn poll_status(ui: &Weak<MainWindow>, state: &Rc<RefCell<AppState>>) {
    let mut state_borrow = state.borrow_mut();
    let Some(ui) = ui.upgrade() else {
        return;
    };
    if let Some(service) = state_borrow.service.as_ref() {
        let status = service.status();
        ui.set_engine_state(SharedString::from(status.state.as_str()));
        ui.set_running(status.running);
        state_borrow.running = status.running;
        let stats = status.stats;
        let peak = stats.rendered_peak.clamp(0.0, 1.0);
        ui.set_source_meter(peak);
        ui.set_output_meter_l(peak);
        ui.set_output_meter_r(peak);
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

#[cfg(test)]
mod tests {
    use super::{effective_mute, preserve_selection_index};

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
}
