//! 引擎服务：引擎的启动/停止/状态/路由提交/事件订阅入口。
//!
//! 本模块持有实时引擎的生命周期与事件轮询线程，并负责把引擎状态/统计
//! 变化投影为 [`ServiceEvent`] 广播给订阅者。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use loopmaster_audio_core::{EndpointId, RouteGraph, RouteGraphSnapshot, SendId, SendSpec};
use loopmaster_audio_windows::{
    AudioEngine, AudioEngineConfig, AudioEngineState, AudioEngineStats, AudioEngineStatus,
    NetworkIoHandles,
};

use crate::command::EngineCommand;
use crate::error::ServiceError;
use crate::event::ServiceEvent;

/// 服务事件轮询间隔：引擎状态/统计变化以有界频率投影为事件，避免
/// UI 直接轮询实时内部结构。
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);

struct EngineServiceInner {
    engine: Mutex<AudioEngine>,
    /// 最近一次成功提交的路由快照（send 级热更新的基准）。
    graph: Mutex<RouteGraphSnapshot>,
    subscribers: Mutex<Vec<mpsc::Sender<ServiceEvent>>>,
    /// 当前异常会话中已经确认失效的 endpoint，恢复后用于发送配对事件。
    faulted_endpoints: Mutex<Vec<EndpointId>>,
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
            faulted_endpoints: Mutex::new(Vec::new()),
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

    /// 取最近一次启动 session 的网络桥接句柄（若有）。
    ///
    /// 引擎启动后（supervisor 创建 worker 组时）网络源/目标的 FIFO 句柄可用；
    /// 供网络桥接层据此启动 VBAN 收发线程。返回 `None` 表示尚无可用句柄
    ///（引擎未运行或图中无网络节点）。
    pub fn take_network_handles(&self) -> Option<NetworkIoHandles> {
        self.inner
            .engine
            .lock()
            .expect("引擎锁未中毒")
            .recv_network_handles()
    }

    /// 提交命令；非法命令返回错误，引擎状态不变。
    pub fn command(&self, command: EngineCommand) -> Result<(), ServiceError> {
        match command {
            EngineCommand::Start => self.inner.engine.lock().expect("引擎锁未中毒").start()?,
            EngineCommand::Stop => self.inner.engine.lock().expect("引擎锁未中毒").stop()?,
            EngineCommand::ApplyRoute(snapshot) => self.apply_route(snapshot)?,
            EngineCommand::SetGain { send_id, gain_db } => {
                self.update_send(send_id, |send| match send {
                    SendSpec::SourceToBus { gain_db: value, .. }
                    | SendSpec::BusToSink { gain_db: value, .. } => *value = gain_db,
                })?
            }
            EngineCommand::SetMuted { send_id, muted } => {
                self.update_send(send_id, |send| match send {
                    SendSpec::SourceToBus { muted: value, .. }
                    | SendSpec::BusToSink { muted: value, .. } => *value = muted,
                })?
            }
            EngineCommand::SetSendEnabled { send_id, enabled } => {
                self.update_send(send_id, |send| match send {
                    SendSpec::SourceToBus { enabled: value, .. }
                    | SendSpec::BusToSink { enabled: value, .. } => *value = enabled,
                })?
            }
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
    pub fn subscribe(&self) -> mpsc::Receiver<ServiceEvent> {
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
        // 锁顺序统一为 graph → engine，与 update_send 一致，避免并发命令死锁。
        let mut graph_guard = self.inner.graph.lock().expect("路由锁未中毒");
        self.inner
            .engine
            .lock()
            .expect("引擎锁未中毒")
            .update_graph(snapshot.clone())?;
        *graph_guard = snapshot;
        Ok(())
    }

    /// 对运行中引擎的一条 send 做热更新：修改暂存图 → 整图替换（block 边界
    /// 生效）。失败时暂存图不变，符合"非法命令不改变状态"。
    fn update_send<F>(&self, send_id: SendId, update: F) -> Result<(), ServiceError>
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
            .find(|send| send.id() == &send_id)
            .ok_or_else(|| ServiceError::Rejected {
                reason: format!("send 不存在: {}", send_id.0),
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
                    publish_transition(
                        &inner,
                        last_state,
                        status.state,
                        status.last_error.as_deref(),
                    );
                    last_state = Some(status.state);
                }
                if last_stats.as_ref() != Some(&status.stats) {
                    last_stats = Some(status.stats.clone());
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
    last_error: Option<&str>,
) {
    broadcast(inner, ServiceEvent::StateChanged(current));
    let graph = inner.graph.lock().expect("路由锁未中毒").graph().clone();
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
        let endpoints = failed_graph_endpoints(&graph, last_error);
        *inner
            .faulted_endpoints
            .lock()
            .expect("故障 endpoint 锁未中毒") = endpoints.clone();
        for endpoint in &endpoints {
            broadcast(inner, ServiceEvent::DeviceLost(endpoint.clone()));
        }
    }
    if restored {
        let endpoints = std::mem::take(
            &mut *inner
                .faulted_endpoints
                .lock()
                .expect("故障 endpoint 锁未中毒"),
        );
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

fn failed_graph_endpoints(graph: &RouteGraph, last_error: Option<&str>) -> Vec<EndpointId> {
    let Some(last_error) = last_error else {
        return Vec::new();
    };
    graph_endpoints(graph)
        .into_iter()
        .filter(|endpoint| error_mentions_endpoint(last_error, &endpoint.0))
        .collect()
}

fn error_mentions_endpoint(error: &str, endpoint: &str) -> bool {
    [
        format!("endpoint={endpoint}"),
        format!("endpoint=Some(\"{endpoint}\")"),
        format!("endpoint_id={endpoint}"),
        format!("endpoint_id=Some(\"{endpoint}\")"),
    ]
    .iter()
    .any(|marker| error.contains(marker))
}

fn broadcast(inner: &EngineServiceInner, event: ServiceEvent) {
    let mut subscribers = inner.subscribers.lock().expect("订阅锁未中毒");
    subscribers.retain(|sender| sender.send(event.clone()).is_ok());
}

#[cfg(test)]
mod tests {
    use super::*;
    use loopmaster_audio_core::{
        BusId, BusSpec, RouteGraph, SendId, SinkId, SinkSpec, SourceId, SourceKind, SourceSpec,
    };

    fn source(id: &str) -> SourceSpec {
        SourceSpec {
            id: SourceId(id.into()),
            kind: SourceKind::ProcessLoopback,
            endpoint_id: None,
            process_id: Some(1),
            executable_path: None,
            display_name: id.into(),
        }
    }

    fn bus(id: &str) -> BusSpec {
        BusSpec {
            id: BusId(id.into()),
            display_name: id.into(),
        }
    }

    fn sink(id: &str) -> SinkSpec {
        SinkSpec {
            id: SinkId(id.into()),
            endpoint_id: EndpointId(format!("endpoint-{id}")),
            display_name: id.into(),
            kind: loopmaster_audio_core::SinkKind::Device,
            stream_name: None,
        }
    }

    fn graph() -> RouteGraph {
        RouteGraph {
            sources: vec![source("a"), source("b")],
            buses: vec![bus("mix")],
            sinks: vec![sink("out")],
            sends: vec![
                SendSpec::SourceToBus {
                    id: SendId("a-mix".into()),
                    source_id: SourceId("a".into()),
                    bus_id: BusId("mix".into()),
                    gain_db: 0.0,
                    muted: false,
                    enabled: true,
                    channel_map: Vec::new(),
                },
                SendSpec::SourceToBus {
                    id: SendId("b-mix".into()),
                    source_id: SourceId("b".into()),
                    bus_id: BusId("mix".into()),
                    gain_db: 0.0,
                    muted: false,
                    enabled: true,
                    channel_map: Vec::new(),
                },
                SendSpec::BusToSink {
                    id: SendId("mix-out".into()),
                    bus_id: BusId("mix".into()),
                    sink_id: SinkId("out".into()),
                    gain_db: 0.0,
                    muted: false,
                    enabled: true,
                    channel_map: Vec::new(),
                },
            ],
        }
    }

    #[test]
    fn send_commands_reject_when_engine_is_not_running() {
        let service = EngineService::new(graph()).unwrap();
        let error = service
            .command(EngineCommand::SetGain {
                send_id: SendId("a-mix".into()),
                gain_db: -6.0,
            })
            .unwrap_err();
        assert!(matches!(error, ServiceError::Rejected { .. }));
    }

    #[test]
    fn device_failure_events_only_target_the_reported_endpoint() {
        let graph = graph();
        let failed = failed_graph_endpoints(
            &graph,
            Some("WASAPI 设备失效, endpoint=Some(\"endpoint-out\")"),
        );
        assert_eq!(failed, vec![EndpointId("endpoint-out".into())]);
    }

    #[test]
    fn unlocated_device_failure_does_not_report_every_endpoint() {
        assert!(failed_graph_endpoints(&graph(), Some("设备失效但无 endpoint")).is_empty());
    }
}
