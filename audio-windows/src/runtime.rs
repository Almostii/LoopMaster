//! 正式音频引擎运行时骨架。
//!
//! 运行时将 WASAPI capture、平台无关混音和 WASAPI render 分成三个 worker。
//! worker 之间只通过固定容量 SPSC FIFO 交换 interleaved `f32` block；配置更新
//! 通过控制通道发送，由 mixer 在 block 边界应用。

use crate::{
    EndpointFlow, ProcessLoopbackSource, WasapiCaptureSource, WindowsAudioBackend,
    WindowsAudioError,
};
use loopmaster_audio_core::{
    AudioFifo, AudioFifoConsumer, FixedOutputResampler, MixerPlan, RouteGraphSnapshot, SourceKind,
    SourceSpec, DEFAULT_BLOCK_FRAMES, INTERNAL_CHANNELS,
};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use thiserror::Error;

const STATE_STOPPED: u8 = 0;
const STATE_RUNNING: u8 = 1;
const STATE_DEGRADED: u8 = 2;
const STATE_RECONNECTING: u8 = 3;
const STATE_FAILED: u8 = 4;
// 设备启动和共享模式调度在最初几个周期内存在抖动。让管线先积累
// 两个 block，再开始计入运行期欠载，避免把启动窗口误当作稳定性故障。
const STARTUP_PREFILL_BLOCKS: usize = 2;
// 共享模式的 packet 到达和 worker 唤醒都不是硬实时的。只有连续两个
// block 没有足够输入时才计为一次欠载；真实的 WASAPI discontinuity 仍独立统计。
const UNDERFLOW_GRACE_BLOCKS: usize = 2;
const DEFAULT_FIFO_CAPACITY_BLOCKS: usize = 32;
// 设备拔插后，Windows 音频服务和驱动重新枚举 endpoint 通常需要数秒；
// 2.5 秒（5 次重试）不足以覆盖这个窗口。保留有限上限，避免设备永久拔出
// 时 supervisor 无止境占用线程，同时给系统约 30 秒完成重新枚举。
const DEFAULT_RECONNECT_ATTEMPTS: usize = 60;
const RECONNECT_DELAY: Duration = Duration::from_millis(500);
// 捕获峰值低于该幅度视为静音（-80 dBFS），用于区分"有 packet"和"有有效音频"。
const NON_SILENT_PEAK_THRESHOLD: f32 = 1e-4;

fn underflow_grace_period(block_period: Duration) -> Duration {
    block_period * UNDERFLOW_GRACE_BLOCKS as u32
}

/// 交错 `f32` 样本的最大绝对值（峰值幅度 0.0~1.0）。NaN 样本按
/// `f32::max` 的语义被忽略；空切片返回 0。
fn packet_peak(samples: &[f32]) -> f32 {
    samples
        .iter()
        .fold(0.0f32, |peak, &sample| peak.max(sample.abs()))
}

/// 峰值幅度是否超过静音阈值（-80 dBFS），即是否承载有效音频内容。
fn is_non_silent(peak: f32) -> bool {
    peak > NON_SILENT_PEAK_THRESHOLD
}

fn should_report_source_underflow(
    available_frames: usize,
    block_frames: usize,
    starvation_elapsed: Duration,
    already_reported: bool,
    grace: Duration,
) -> bool {
    available_frames < block_frames && !already_reported && starvation_elapsed >= grace
}

#[derive(Clone, Debug)]
pub struct AudioEngineConfig {
    pub graph: RouteGraphSnapshot,
    pub block_frames: usize,
    pub fifo_capacity_frames: usize,
}

/// 音频引擎的运行状态。
///
/// `Degraded` 表示 worker 因设备失效退出，错误具备重连条件；
/// `Reconnecting` 表示 supervisor 正在停止旧会话并重建 stream；
/// 重试耗尽后进入 `Failed`。当前重连仍依赖原 endpoint 的稳定 ID。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioEngineState {
    Stopped,
    Running,
    Degraded,
    Reconnecting,
    Failed,
}

impl AudioEngineState {
    fn from_raw(raw: u8) -> Self {
        match raw {
            STATE_RUNNING => Self::Running,
            STATE_DEGRADED => Self::Degraded,
            STATE_RECONNECTING => Self::Reconnecting,
            STATE_FAILED => Self::Failed,
            _ => Self::Stopped,
        }
    }

    /// 返回稳定的中文状态标签，供诊断工具和日志使用。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "Stopped",
            Self::Running => "Running",
            Self::Degraded => "Degraded",
            Self::Reconnecting => "Reconnecting",
            Self::Failed => "Failed",
        }
    }
}

impl AudioEngineConfig {
    pub fn new(graph: RouteGraphSnapshot) -> Self {
        Self {
            graph,
            block_frames: DEFAULT_BLOCK_FRAMES,
            fifo_capacity_frames: DEFAULT_BLOCK_FRAMES * DEFAULT_FIFO_CAPACITY_BLOCKS,
        }
    }
}

#[derive(Debug, Error)]
pub enum AudioEngineError {
    #[error("引擎已经启动；如需重新启动，必须先停止")]
    AlreadyRunning,
    #[error("引擎没有运行")]
    NotRunning,
    #[error("block frame 数必须大于 0")]
    ZeroBlockFrames,
    #[error("FIFO 容量必须不小于 block frame 数")]
    FifoTooSmall,
    #[error("路由图至少需要一个 source 和一个 sink")]
    UnsupportedTopology,
    #[error("source 必须是设备捕获 source")]
    UnsupportedSourceKind,
    #[error("source 未配置 endpoint ID")]
    MissingSourceEndpoint,
    #[error("Process Loopback source 未配置有效 process ID")]
    MissingProcessId,
    #[error("运行中的引擎不支持更换 endpoint；必须停止后重新启动")]
    EndpointChangeRequiresRestart,
    #[error("WASAPI worker 错误: {0}")]
    Windows(#[from] WindowsAudioError),
    #[error("混音计划错误: {0}")]
    Mixer(#[from] loopmaster_audio_core::MixerError),
    #[error("FIFO 配置错误: {0}")]
    Fifo(#[from] loopmaster_audio_core::FifoConfigError),
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AudioEngineStats {
    pub capture_packets: u64,
    pub captured_frames: u64,
    pub rendered_frames: u64,
    pub render_writes: u64,
    pub fifo_overflows: u64,
    /// 因 FIFO 满而丢弃的音频 frame 数；事件数见 [`fifo_overflows`]。
    pub fifo_dropped_frames: u64,
    pub fifo_underflows: u64,
    pub discontinuities: u64,
    pub startup_discontinuities: u64,
    pub runtime_discontinuities: u64,
    pub timestamp_errors: u64,
    pub render_no_space: u64,
    pub graph_updates: u64,
    /// supervisor 因设备失效启动的重连尝试次数；初次启动不计入。
    pub reconnect_attempts: u64,
    /// 捕获音频的全局峰值幅度（0.0~1.0，静音为 0.0）。用于区分"有
    /// packet"和"有有效音频"；换算 dBFS 由展示层完成。
    pub captured_peak: f32,
    /// 捕获到超过静音阈值（-80 dBFS）内容的 packet 数。
    pub non_silent_packets: u64,
    /// 混音后写入 render 的全局峰值幅度（0.0~1.0）。
    pub rendered_peak: f32,
    /// 混音后写入 render 且超过静音阈值的 block 数。运行中静音切换后
    /// 该计数停止增长，用于验证 send 级路由变更是否在块边界生效。
    pub rendered_non_silent_blocks: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioEngineStatus {
    pub state: AudioEngineState,
    pub running: bool,
    pub failed: bool,
    pub last_error: Option<String>,
    pub stats: AudioEngineStats,
}

struct Counters {
    capture_packets: AtomicU64,
    captured_frames: AtomicU64,
    rendered_frames: AtomicU64,
    render_writes: AtomicU64,
    fifo_overflows: AtomicU64,
    fifo_dropped_frames: AtomicU64,
    fifo_underflows: AtomicU64,
    discontinuities: AtomicU64,
    startup_discontinuities: AtomicU64,
    runtime_discontinuities: AtomicU64,
    timestamp_errors: AtomicU64,
    render_no_space: AtomicU64,
    graph_updates: AtomicU64,
    reconnect_attempts: AtomicU64,
    captured_peak: AtomicU32,
    non_silent_packets: AtomicU64,
    rendered_peak: AtomicU32,
    rendered_non_silent_blocks: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> AudioEngineStats {
        AudioEngineStats {
            capture_packets: self.capture_packets.load(Ordering::Relaxed),
            captured_frames: self.captured_frames.load(Ordering::Relaxed),
            rendered_frames: self.rendered_frames.load(Ordering::Relaxed),
            render_writes: self.render_writes.load(Ordering::Relaxed),
            fifo_overflows: self.fifo_overflows.load(Ordering::Relaxed),
            fifo_dropped_frames: self.fifo_dropped_frames.load(Ordering::Relaxed),
            fifo_underflows: self.fifo_underflows.load(Ordering::Relaxed),
            discontinuities: self.discontinuities.load(Ordering::Relaxed),
            startup_discontinuities: self.startup_discontinuities.load(Ordering::Relaxed),
            runtime_discontinuities: self.runtime_discontinuities.load(Ordering::Relaxed),
            timestamp_errors: self.timestamp_errors.load(Ordering::Relaxed),
            render_no_space: self.render_no_space.load(Ordering::Relaxed),
            graph_updates: self.graph_updates.load(Ordering::Relaxed),
            reconnect_attempts: self.reconnect_attempts.load(Ordering::Relaxed),
            captured_peak: f32::from_bits(self.captured_peak.load(Ordering::Relaxed)),
            non_silent_packets: self.non_silent_packets.load(Ordering::Relaxed),
            rendered_peak: f32::from_bits(self.rendered_peak.load(Ordering::Relaxed)),
            rendered_non_silent_blocks: self.rendered_non_silent_blocks.load(Ordering::Relaxed),
        }
    }
}

pub struct AudioEngine {
    config: AudioEngineConfig,
    graph_config: Arc<Mutex<RouteGraphSnapshot>>,
    state: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
    last_error: Arc<Mutex<Option<String>>>,
    graph_tx: Arc<Mutex<Option<mpsc::Sender<RouteGraphSnapshot>>>>,
    workers: Vec<JoinHandle<()>>,
}

impl AudioEngine {
    pub fn new(config: AudioEngineConfig) -> Result<Self, AudioEngineError> {
        validate_config(&config)?;
        Ok(Self {
            graph_config: Arc::new(Mutex::new(config.graph.clone())),
            config,
            state: Arc::new(AtomicU8::new(STATE_STOPPED)),
            stop: Arc::new(AtomicBool::new(false)),
            counters: Arc::new(Counters {
                capture_packets: AtomicU64::new(0),
                captured_frames: AtomicU64::new(0),
                rendered_frames: AtomicU64::new(0),
                render_writes: AtomicU64::new(0),
                fifo_overflows: AtomicU64::new(0),
                fifo_dropped_frames: AtomicU64::new(0),
                fifo_underflows: AtomicU64::new(0),
                discontinuities: AtomicU64::new(0),
                startup_discontinuities: AtomicU64::new(0),
                runtime_discontinuities: AtomicU64::new(0),
                timestamp_errors: AtomicU64::new(0),
                render_no_space: AtomicU64::new(0),
                graph_updates: AtomicU64::new(0),
                reconnect_attempts: AtomicU64::new(0),
                captured_peak: AtomicU32::new(0),
                non_silent_packets: AtomicU64::new(0),
                rendered_peak: AtomicU32::new(0),
                rendered_non_silent_blocks: AtomicU64::new(0),
            }),
            last_error: Arc::new(Mutex::new(None)),
            graph_tx: Arc::new(Mutex::new(None)),
            workers: Vec::new(),
        })
    }

    pub fn start(&mut self) -> Result<(), AudioEngineError> {
        if self.state.load(Ordering::Acquire) != STATE_STOPPED {
            return Err(AudioEngineError::AlreadyRunning);
        }
        *self.last_error.lock().expect("状态锁未中毒") = None;
        self.stop.store(false, Ordering::Release);
        self.state.store(STATE_RUNNING, Ordering::Release);
        let graph_config = Arc::clone(&self.graph_config);
        let block_frames = self.config.block_frames;
        let fifo_capacity_frames = self.config.fifo_capacity_frames;
        let stop = Arc::clone(&self.stop);
        let state = Arc::clone(&self.state);
        let counters = Arc::clone(&self.counters);
        let last_error = Arc::clone(&self.last_error);
        let graph_tx = Arc::clone(&self.graph_tx);
        let supervisor = thread::Builder::new()
            .name("loopmaster-audio-supervisor".into())
            .spawn(move || {
                supervisor_worker(
                    graph_config,
                    block_frames,
                    fifo_capacity_frames,
                    stop,
                    state,
                    last_error,
                    counters,
                    graph_tx,
                );
            })
            .expect("创建音频 supervisor 失败");
        self.workers = vec![supervisor];
        Ok(())
    }

    pub fn update_graph(&mut self, graph: RouteGraphSnapshot) -> Result<(), AudioEngineError> {
        validate_config(&AudioEngineConfig {
            graph: graph.clone(),
            block_frames: self.config.block_frames,
            fifo_capacity_frames: self.config.fifo_capacity_frames,
        })?;
        let previous = self.config.graph.graph();
        let next = graph.graph();
        // 运行中只允许 send 级变更（增益/静音/通道映射/启停）。source/sink 的
        // 数量或端点集合变化需要重建 worker 组，必须显式重启（阶段 B.2 语义）。
        let topology_changed = topology_changed(previous, next);
        if topology_changed {
            return Err(AudioEngineError::EndpointChangeRequiresRestart);
        }
        let tx = self
            .graph_tx
            .lock()
            .expect("路由通道锁未中毒")
            .clone()
            .ok_or(AudioEngineError::NotRunning)?;
        tx.send(graph.clone())
            .map_err(|_| AudioEngineError::NotRunning)?;
        self.config.graph = graph;
        *self.graph_config.lock().expect("路由快照锁未中毒") = self.config.graph.clone();
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), AudioEngineError> {
        if self.state.load(Ordering::Acquire) == STATE_STOPPED {
            return Err(AudioEngineError::NotRunning);
        }
        self.stop.store(true, Ordering::Release);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        *self.graph_tx.lock().expect("路由通道锁未中毒") = None;
        self.state.store(STATE_STOPPED, Ordering::Release);
        Ok(())
    }

    pub fn status(&self) -> AudioEngineStatus {
        let state = self.state.load(Ordering::Acquire);
        let state = AudioEngineState::from_raw(state);
        AudioEngineStatus {
            state,
            running: state == AudioEngineState::Running,
            failed: state == AudioEngineState::Failed,
            last_error: self.last_error.lock().expect("状态锁未中毒").clone(),
            stats: self.counters.snapshot(),
        }
    }
}

/// 判断运行中是否发生了需要重建 worker 的 source/sink 拓扑变化。
/// send 的增益、静音和通道映射变化不属于拓扑变化，可以交给 mixer 热更新。
fn topology_changed(
    previous: &loopmaster_audio_core::RouteGraph,
    next: &loopmaster_audio_core::RouteGraph,
) -> bool {
    previous.sources.len() != next.sources.len()
        || previous.sinks.len() != next.sinks.len()
        || previous.sources.iter().zip(&next.sources).any(|(a, b)| {
            a.id != b.id
                || a.kind != b.kind
                || a.endpoint_id != b.endpoint_id
                || a.process_id != b.process_id
        })
        || previous
            .sinks
            .iter()
            .zip(&next.sinks)
            .any(|(a, b)| a.id != b.id || a.endpoint_id != b.endpoint_id)
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        if self.state.load(Ordering::Acquire) != STATE_STOPPED {
            let _ = self.stop();
        }
    }
}

fn validate_config(config: &AudioEngineConfig) -> Result<(), AudioEngineError> {
    if config.block_frames == 0 {
        return Err(AudioEngineError::ZeroBlockFrames);
    }
    if config.fifo_capacity_frames < config.block_frames {
        return Err(AudioEngineError::FifoTooSmall);
    }
    let graph = config.graph.graph();
    if graph.sources.is_empty() || graph.sinks.is_empty() {
        return Err(AudioEngineError::UnsupportedTopology);
    }
    for source in &graph.sources {
        match source.kind {
            SourceKind::DeviceCapture | SourceKind::DeviceLoopback
                if source.endpoint_id.is_none() =>
            {
                return Err(AudioEngineError::MissingSourceEndpoint);
            }
            SourceKind::ProcessLoopback if source.process_id.unwrap_or(0) == 0 => {
                return Err(AudioEngineError::MissingProcessId);
            }
            SourceKind::DeviceCapture
            | SourceKind::DeviceLoopback
            | SourceKind::ProcessLoopback => {}
        }
    }
    Ok(())
}

fn graph_sink_endpoints(graph: &RouteGraphSnapshot) -> Vec<loopmaster_audio_core::EndpointId> {
    graph
        .graph()
        .sinks
        .iter()
        .map(|sink| sink.endpoint_id.clone())
        .collect()
}

/// 路由图中是否存在需要按端点 active 检查的普通设备捕获/回环 source。
fn endpoints_need_check(graph: &RouteGraphSnapshot) -> bool {
    graph.graph().sources.iter().any(|source| {
        matches!(
            source.kind,
            SourceKind::DeviceCapture | SourceKind::DeviceLoopback
        )
    })
}

/// 重连前确认图中所有 DeviceCapture/DeviceLoopback source 的 endpoint 与所有
/// sink endpoint 均 active。
///
/// 任一 endpoint 失效返回 `Ok(false)`；设备类错误由调用方按重试处理。
fn all_endpoints_active(graph: &RouteGraphSnapshot) -> Result<bool, WindowsAudioError> {
    let backend = WindowsAudioBackend::new()?;
    for source in &graph.graph().sources {
        match source.kind {
            SourceKind::DeviceCapture => {
                let Some(endpoint) = &source.endpoint_id else {
                    // validate_config 保证必带 endpoint；防御性视为不可用。
                    return Ok(false);
                };
                if !backend.is_endpoint_active(endpoint, EndpointFlow::Capture)? {
                    return Ok(false);
                }
            }
            SourceKind::DeviceLoopback => {
                let Some(endpoint) = &source.endpoint_id else {
                    return Ok(false);
                };
                if !backend.is_endpoint_active(endpoint, EndpointFlow::Render)? {
                    return Ok(false);
                }
            }
            SourceKind::ProcessLoopback => {}
        }
    }
    for sink in &graph.graph().sinks {
        if !backend.is_endpoint_active(&sink.endpoint_id, EndpointFlow::Render)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 创建多条固定容量 FIFO，返回 (producers, consumers)，顺序与调用方传入的
/// source/sink 列表一一对应。
fn split_fifo_vec(
    count: usize,
    capacity_frames: usize,
) -> Result<
    (
        Vec<loopmaster_audio_core::AudioFifoProducer>,
        Vec<loopmaster_audio_core::AudioFifoConsumer>,
    ),
    loopmaster_audio_core::FifoConfigError,
> {
    let mut producers = Vec::with_capacity(count);
    let mut consumers = Vec::with_capacity(count);
    for _ in 0..count {
        let (producer, consumer) = AudioFifo::split(capacity_frames, INTERNAL_CHANNELS)?;
        producers.push(producer);
        consumers.push(consumer);
    }
    Ok((producers, consumers))
}

fn fail(state: &AtomicU8, stop: &AtomicBool, error: &Mutex<Option<String>>, value: String) {
    // 同一 session 可能有多个 worker 同时退出。设备失效先将 session
    // 标记为 Degraded 后，其他 worker 的收尾错误不能把它覆盖成 Failed，
    // 否则 supervisor 会跳过后续重连。
    if stop.swap(true, Ordering::AcqRel) {
        return;
    }
    *error.lock().expect("状态锁未中毒") = Some(value);
    state.store(STATE_FAILED, Ordering::Release);
}

fn fail_windows(
    state: &AtomicU8,
    stop: &AtomicBool,
    error: &Mutex<Option<String>>,
    value: &WindowsAudioError,
) {
    let next_state = if value.is_device_failure() {
        // 首次 session 失效需要发布 Degraded；重连阶段的探测 session
        // 失败仍停留在 Reconnecting，避免一次故障被统计为多次 Degraded。
        if state.load(Ordering::Acquire) == STATE_RECONNECTING {
            STATE_RECONNECTING
        } else {
            STATE_DEGRADED
        }
    } else {
        STATE_FAILED
    };
    if stop.swap(true, Ordering::AcqRel) {
        return;
    }
    *error.lock().expect("状态锁未中毒") = Some(value.to_string());
    state.store(next_state, Ordering::Release);
}

/// 负责管线会话的生命周期。每次重试都重新创建 FIFO、WASAPI stream 和
/// 重采样器；旧会话的 worker 必须先 join，避免两个会话同时驱动同一 endpoint。
#[allow(clippy::too_many_arguments)]
fn supervisor_worker(
    graph_config: Arc<Mutex<RouteGraphSnapshot>>,
    block_frames: usize,
    fifo_capacity_frames: usize,
    engine_stop: Arc<AtomicBool>,
    state: Arc<AtomicU8>,
    error: Arc<Mutex<Option<String>>>,
    counters: Arc<Counters>,
    graph_tx_slot: Arc<Mutex<Option<mpsc::Sender<RouteGraphSnapshot>>>>,
) {
    for attempt in 0..=DEFAULT_RECONNECT_ATTEMPTS {
        if engine_stop.load(Ordering::Acquire) {
            break;
        }
        if attempt > 0 {
            counters.reconnect_attempts.fetch_add(1, Ordering::Relaxed);
            state.store(STATE_RECONNECTING, Ordering::Release);
            thread::sleep(RECONNECT_DELAY);
            if engine_stop.load(Ordering::Acquire) {
                break;
            }
        }
        let graph = graph_config.lock().expect("路由快照锁未中毒").clone();
        if attempt > 0 && endpoints_need_check(&graph) {
            match all_endpoints_active(&graph) {
                Ok(true) => {}
                Ok(false) => {
                    if attempt == DEFAULT_RECONNECT_ATTEMPTS {
                        mark_reconnect_exhausted(&state, &engine_stop, &error);
                        break;
                    }
                    continue;
                }
                Err(e) if e.is_device_failure() => {
                    if attempt == DEFAULT_RECONNECT_ATTEMPTS {
                        mark_reconnect_exhausted(&state, &engine_stop, &error);
                        break;
                    }
                    continue;
                }
                Err(e) => {
                    fail(&state, &engine_stop, &error, e.to_string());
                    break;
                }
            }
        }
        let session_stop = Arc::new(AtomicBool::new(false));
        // 多路由：每个 source 一条 capture→mixer FIFO，每个 sink 一条 mixer→render FIFO。
        let (source_producers, mixer_consumers) =
            match split_fifo_vec(graph.graph().sources.len(), fifo_capacity_frames) {
                Ok(value) => value,
                Err(e) => {
                    fail(&state, &engine_stop, &error, e.to_string());
                    break;
                }
            };
        let (mixer_producers, render_consumers) =
            match split_fifo_vec(graph.graph().sinks.len(), fifo_capacity_frames) {
                Ok(value) => value,
                Err(e) => {
                    fail(&state, &engine_stop, &error, e.to_string());
                    break;
                }
            };
        let (session_graph_tx, graph_rx) = mpsc::channel();
        *graph_tx_slot.lock().expect("路由通道锁未中毒") = Some(session_graph_tx);
        let sink_endpoints = graph_sink_endpoints(&graph);
        let mut workers =
            Vec::with_capacity(graph.graph().sources.len() + graph.graph().sinks.len() + 1);
        for (source, producer) in graph.graph().sources.iter().cloned().zip(source_producers) {
            workers.push(spawn_capture_worker(
                source,
                block_frames,
                Arc::clone(&session_stop),
                Arc::clone(&state),
                Arc::clone(&error),
                Arc::clone(&counters),
                producer,
            ));
        }
        workers.push(spawn_mixer_worker(
            graph.clone(),
            graph_rx,
            block_frames,
            Arc::clone(&session_stop),
            Arc::clone(&state),
            Arc::clone(&error),
            Arc::clone(&counters),
            mixer_consumers,
            mixer_producers,
        ));
        for (endpoint, consumer) in sink_endpoints.into_iter().zip(render_consumers) {
            workers.push(spawn_render_worker(
                endpoint,
                block_frames,
                Arc::clone(&session_stop),
                Arc::clone(&state),
                Arc::clone(&error),
                Arc::clone(&counters),
                consumer,
            ));
        }
        // 给三个 worker 一个有界启动窗口；设备在打开阶段失效时，
        // session_stop 会先置位，避免把尚未建立的会话报告为 Running。
        thread::sleep(Duration::from_millis(50));
        if !session_stop.load(Ordering::Acquire) {
            *error.lock().expect("状态锁未中毒") = None;
            state.store(STATE_RUNNING, Ordering::Release);
        }
        while !engine_stop.load(Ordering::Acquire) && !session_stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(10));
        }
        session_stop.store(true, Ordering::Release);
        for worker in workers {
            let _ = worker.join();
        }
        *graph_tx_slot.lock().expect("路由通道锁未中毒") = None;
        if engine_stop.load(Ordering::Acquire) {
            break;
        }
        let session_state = state.load(Ordering::Acquire);
        if session_state != STATE_DEGRADED && session_state != STATE_RECONNECTING {
            break;
        }
        if attempt == DEFAULT_RECONNECT_ATTEMPTS {
            mark_reconnect_exhausted(&state, &engine_stop, &error);
        }
    }
}

fn mark_reconnect_exhausted(
    state: &AtomicU8,
    engine_stop: &AtomicBool,
    error: &Mutex<Option<String>>,
) {
    *error.lock().expect("状态锁未中毒") = Some("设备重连等待窗口耗尽".into());
    state.store(STATE_FAILED, Ordering::Release);
    engine_stop.store(true, Ordering::Release);
}

fn spawn_capture_worker(
    source: SourceSpec,
    block_frames: usize,
    stop: Arc<AtomicBool>,
    state: Arc<AtomicU8>,
    error: Arc<Mutex<Option<String>>>,
    counters: Arc<Counters>,
    producer: loopmaster_audio_core::AudioFifoProducer,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("loopmaster-capture".into())
        .spawn(move || {
            let _ = capture_worker(source, block_frames, stop, state, error, counters, producer);
        })
        .expect("创建 capture worker 失败")
}

#[allow(clippy::too_many_arguments)]
fn spawn_mixer_worker(
    graph: RouteGraphSnapshot,
    graph_rx: mpsc::Receiver<RouteGraphSnapshot>,
    block_frames: usize,
    stop: Arc<AtomicBool>,
    state: Arc<AtomicU8>,
    error: Arc<Mutex<Option<String>>>,
    counters: Arc<Counters>,
    consumers: Vec<loopmaster_audio_core::AudioFifoConsumer>,
    producers: Vec<loopmaster_audio_core::AudioFifoProducer>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("loopmaster-mixer".into())
        .spawn(move || {
            let _ = mixer_worker(
                graph,
                graph_rx,
                block_frames,
                stop,
                state,
                error,
                counters,
                consumers,
                producers,
            );
        })
        .expect("创建 mixer worker 失败")
}

fn spawn_render_worker(
    endpoint: loopmaster_audio_core::EndpointId,
    block_frames: usize,
    stop: Arc<AtomicBool>,
    state: Arc<AtomicU8>,
    error: Arc<Mutex<Option<String>>>,
    counters: Arc<Counters>,
    consumer: loopmaster_audio_core::AudioFifoConsumer,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("loopmaster-render".into())
        .spawn(move || {
            let _ = render_worker(
                endpoint,
                block_frames,
                stop,
                state,
                error,
                counters,
                consumer,
            );
        })
        .expect("创建 render worker 失败")
}

enum CaptureSource {
    Device(WasapiCaptureSource),
    Loopback(WasapiCaptureSource),
    Process(ProcessLoopbackSource),
}

impl CaptureSource {
    fn drain_packets<F>(
        &mut self,
        on_packet: F,
    ) -> Result<crate::CaptureDrainResult, WindowsAudioError>
    where
        F: FnMut(crate::CapturePacket, Option<&[f32]>),
    {
        match self {
            Self::Device(source) | Self::Loopback(source) => source.drain_packets(on_packet),
            Self::Process(source) => source.drain_packets(on_packet),
        }
    }

    /// 边界转换后的 capture 格式。WASAPI 原生 packet 已先解码并映射为
    /// 双声道 f32，因此这里只保留原生采样率供重采样器使用。
    fn source_format(&self) -> Option<crate::EndpointFormat> {
        match self {
            Self::Device(source) | Self::Loopback(source) => {
                let native = source.format();
                Some(crate::EndpointFormat {
                    sample_rate: native.sample_rate,
                    bits_per_sample: 32,
                    channels: INTERNAL_CHANNELS as u16,
                    channel_mask: 0x3,
                    is_float: true,
                    is_pcm: false,
                })
            }
            Self::Process(_) => None,
        }
    }

    fn max_packet_frames(&self) -> usize {
        match self {
            Self::Device(source) | Self::Loopback(source) => source.buffer_frames() as usize,
            Self::Process(_) => 0,
        }
    }
}

/// 非 48 kHz capture source 的输入重采样状态（阶段 B.5）。
struct CaptureResampler {
    /// 原生采样率 == 内部 48 kHz 时为 `None`（透传）。
    resampler: Option<FixedOutputResampler>,
    /// 累积的原生交错样本，等待凑齐 `resampler.input_frames()`。
    input_buffer: Vec<f32>,
    /// 复用的内部 48 kHz 输出 block。
    output_block: Vec<f32>,
}

impl CaptureResampler {
    fn new(
        source_format: Option<crate::EndpointFormat>,
        block_frames: usize,
        max_packet_frames: usize,
    ) -> Result<Self, loopmaster_audio_core::ResamplerConfigError> {
        let resampler = match source_format {
            Some(format) if format.sample_rate != loopmaster_audio_core::INTERNAL_SAMPLE_RATE => {
                Some(FixedOutputResampler::new(
                    format.sample_rate,
                    loopmaster_audio_core::INTERNAL_SAMPLE_RATE,
                    format.channels as usize,
                    block_frames,
                )?)
            }
            _ => None,
        };
        let input_capacity = resampler
            .as_ref()
            .map(|value| {
                max_packet_frames
                    .saturating_add(value.input_frames().saturating_mul(2))
                    .saturating_mul(value.channels())
            })
            .unwrap_or(0);
        let mut input_buffer = Vec::with_capacity(input_capacity);
        input_buffer.clear();
        Ok(Self {
            resampler,
            input_buffer,
            output_block: vec![0.0f32; block_frames * INTERNAL_CHANNELS],
        })
    }

    fn append_silence(&mut self, frames: usize) -> bool {
        let samples = frames.saturating_mul(INTERNAL_CHANNELS);
        if self.input_buffer.len().saturating_add(samples) > self.input_buffer.capacity() {
            return false;
        }
        self.input_buffer
            .resize(self.input_buffer.len() + samples, 0.0);
        true
    }

    fn append_samples(&mut self, samples: &[f32]) -> bool {
        if self.input_buffer.len().saturating_add(samples.len()) > self.input_buffer.capacity() {
            return false;
        }
        self.input_buffer.extend_from_slice(samples);
        true
    }
}

fn capture_worker(
    source_spec: SourceSpec,
    block_frames: usize,
    stop: Arc<AtomicBool>,
    state: Arc<AtomicU8>,
    error: Arc<Mutex<Option<String>>>,
    counters: Arc<Counters>,
    mut producer: loopmaster_audio_core::AudioFifoProducer,
) -> Result<(), ()> {
    let mut source = match source_spec.kind {
        SourceKind::DeviceCapture => {
            let endpoint = source_spec.endpoint_id.ok_or(()).map_err(|_| {
                fail(&state, &stop, &error, "source 未配置 endpoint ID".into());
            })?;
            let backend = WindowsAudioBackend::new().map_err(|e| {
                fail_windows(&state, &stop, &error, &e);
            })?;
            CaptureSource::Device(backend.open_capture_source(&endpoint).map_err(|e| {
                fail_windows(&state, &stop, &error, &e);
            })?)
        }
        SourceKind::ProcessLoopback => {
            let pid = source_spec
                .process_id
                .filter(|pid| *pid != 0)
                .ok_or(())
                .map_err(|_| {
                    fail(
                        &state,
                        &stop,
                        &error,
                        "Process Loopback source 未配置有效 process ID".into(),
                    );
                })?;
            CaptureSource::Process(ProcessLoopbackSource::open(pid).map_err(|e| {
                fail_windows(&state, &stop, &error, &e);
            })?)
        }
        SourceKind::DeviceLoopback => {
            let endpoint = source_spec.endpoint_id.ok_or(()).map_err(|_| {
                fail(&state, &stop, &error, "source 未配置 endpoint ID".into());
            })?;
            let backend = WindowsAudioBackend::new().map_err(|e| {
                fail_windows(&state, &stop, &error, &e);
            })?;
            CaptureSource::Loopback(backend.open_device_loopback_source(&endpoint).map_err(
                |e| {
                    fail_windows(&state, &stop, &error, &e);
                },
            )?)
        }
    };
    let silence = vec![0.0f32; block_frames * INTERNAL_CHANNELS];
    let mut capture_resampler = CaptureResampler::new(
        source.source_format(),
        block_frames,
        source.max_packet_frames(),
    )
    .map_err(|e| {
        fail(&state, &stop, &error, e.to_string());
    })?;
    let mut first_packet = true;
    let mut conversion_failed = None::<String>;
    while !stop.load(Ordering::Acquire) {
        let result = source.drain_packets(|packet, data| {
            counters.capture_packets.fetch_add(1, Ordering::Relaxed);
            counters
                .captured_frames
                .fetch_add(u64::from(packet.frames), Ordering::Relaxed);
            if packet.discontinuity {
                counters.discontinuities.fetch_add(1, Ordering::Relaxed);
                if first_packet {
                    counters
                        .startup_discontinuities
                        .fetch_add(1, Ordering::Relaxed);
                } else {
                    counters
                        .runtime_discontinuities
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            counters
                .timestamp_errors
                .fetch_add(u64::from(packet.timestamp_error), Ordering::Relaxed);
            // 数据路径与峰值统计：非 48 kHz source 先经 FixedOutputResampler
            // 重采样到内部 48 kHz（阶段 B.5），再计算峰值并写入 FIFO。
            let peak;
            if capture_resampler.resampler.is_some() {
                let frames = packet.frames as usize;
                if packet.silent {
                    if !capture_resampler.append_silence(frames) {
                        conversion_failed = Some("capture 重采样输入缓冲区容量不足".to_owned());
                        return;
                    }
                } else if let Some(samples) = data {
                    if !capture_resampler.append_samples(samples) {
                        conversion_failed = Some("capture 重采样输入缓冲区容量不足".to_owned());
                        return;
                    }
                } else if !capture_resampler.append_silence(frames) {
                    conversion_failed = Some("capture 重采样输入缓冲区容量不足".to_owned());
                    return;
                }
                let Some(resampler) = capture_resampler.resampler.as_mut() else {
                    return;
                };
                let mut cursor = 0usize;
                let mut written = 0usize;
                let mut expected_output = 0usize;
                let mut block_peak = 0.0f32;
                while capture_resampler.input_buffer.len() - cursor
                    >= resampler.input_frames() * INTERNAL_CHANNELS
                {
                    let needed = resampler.input_frames() * INTERNAL_CHANNELS;
                    let input = &capture_resampler.input_buffer[cursor..cursor + needed];
                    if let Err(e) =
                        resampler.process_interleaved(input, &mut capture_resampler.output_block)
                    {
                        fail(&state, &stop, &error, e.to_string());
                        break;
                    }
                    expected_output += capture_resampler.output_block.len() / INTERNAL_CHANNELS;
                    block_peak = block_peak.max(packet_peak(&capture_resampler.output_block));
                    let pushed = producer
                        .push_interleaved(&capture_resampler.output_block)
                        .map(|result| result.frames())
                        .unwrap_or(0);
                    written += pushed;
                    cursor += needed;
                }
                capture_resampler.input_buffer.drain(..cursor);
                let dropped_output = expected_output.saturating_sub(written);
                if dropped_output > 0 {
                    counters.fifo_overflows.fetch_add(1, Ordering::Relaxed);
                    counters
                        .fifo_dropped_frames
                        .fetch_add(dropped_output as u64, Ordering::Relaxed);
                }
                peak = block_peak;
            } else {
                peak = if packet.silent {
                    0.0
                } else {
                    data.map(packet_peak).unwrap_or(0.0)
                };
                let written_frames = if packet.silent {
                    push_silence(&mut producer, packet.frames as usize, &silence)
                } else if let Some(samples) = data {
                    producer
                        .push_interleaved(samples)
                        .map(|result| result.frames())
                        .unwrap_or(0)
                } else {
                    0
                };
                let dropped_frames = (packet.frames as usize).saturating_sub(written_frames);
                if dropped_frames > 0 {
                    counters.fifo_overflows.fetch_add(1, Ordering::Relaxed);
                    counters
                        .fifo_dropped_frames
                        .fetch_add(dropped_frames as u64, Ordering::Relaxed);
                }
            }
            if peak.is_finite() && peak > 0.0 {
                counters
                    .captured_peak
                    .fetch_max(peak.to_bits(), Ordering::Relaxed);
            }
            if is_non_silent(peak) {
                counters.non_silent_packets.fetch_add(1, Ordering::Relaxed);
            }
            first_packet = false;
        });
        if let Some(reason) = conversion_failed.take() {
            fail(&state, &stop, &error, reason);
            return Err(());
        }
        match result {
            Ok(stats) if stats.packets == 0 => thread::sleep(Duration::from_millis(1)),
            Ok(_) => {}
            Err(e) => {
                fail_windows(&state, &stop, &error, &e);
                return Err(());
            }
        }
    }
    Ok(())
}

fn push_silence(
    producer: &mut loopmaster_audio_core::AudioFifoProducer,
    frames: usize,
    silence: &[f32],
) -> usize {
    let mut remaining = frames * INTERNAL_CHANNELS;
    let mut written_samples = 0;
    while remaining > 0 {
        let count = remaining.min(silence.len());
        let written = producer
            .push_interleaved(&silence[..count])
            .map(|r| r.frames() * INTERNAL_CHANNELS)
            .unwrap_or(0);
        remaining -= written;
        written_samples += written;
        if written == 0 {
            break;
        }
    }
    written_samples / INTERNAL_CHANNELS
}

#[allow(clippy::too_many_arguments)]
fn mixer_worker(
    graph: RouteGraphSnapshot,
    graph_rx: mpsc::Receiver<RouteGraphSnapshot>,
    block_frames: usize,
    stop: Arc<AtomicBool>,
    state: Arc<AtomicU8>,
    error: Arc<Mutex<Option<String>>>,
    counters: Arc<Counters>,
    mut consumers: Vec<loopmaster_audio_core::AudioFifoConsumer>,
    mut producers: Vec<loopmaster_audio_core::AudioFifoProducer>,
) -> Result<(), ()> {
    let mut plan = MixerPlan::new(
        graph.graph(),
        block_frames,
        INTERNAL_CHANNELS,
        INTERNAL_CHANNELS,
    )
    .map_err(|e| {
        fail(&state, &stop, &error, e.to_string());
    })?;
    let mut source_blocks = vec![vec![0.0f32; block_frames * INTERNAL_CHANNELS]; consumers.len()];
    let mut sink_blocks = vec![vec![0.0f32; block_frames * INTERNAL_CHANNELS]; producers.len()];
    let block_period = Duration::from_secs_f64(
        block_frames as f64 / loopmaster_audio_core::INTERNAL_SAMPLE_RATE as f64,
    );
    let mut next_deadline = Instant::now();
    let startup_prefill_frames = block_frames.saturating_mul(STARTUP_PREFILL_BLOCKS).min(
        consumers
            .first()
            .map_or(0, AudioFifoConsumer::capacity_frames),
    );
    let mut primed = false;
    let mut source_starvation_since = vec![None; consumers.len()];
    let mut source_starvation_reported = vec![false; consumers.len()];
    while !stop.load(Ordering::Acquire) {
        while let Ok(next) = graph_rx.try_recv() {
            match MixerPlan::new(
                next.graph(),
                block_frames,
                INTERNAL_CHANNELS,
                INTERNAL_CHANNELS,
            ) {
                Ok(next_plan) => {
                    plan = next_plan;
                    counters.graph_updates.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    fail(&state, &stop, &error, e.to_string());
                    return Err(());
                }
            }
        }
        // 任一 source 有足够数据即推进主节拍；其他 source 的缺失按静音补足
        //（MixerPlan::process 支持短 source 补静音）。
        let max_available = consumers
            .iter()
            .map(AudioFifoConsumer::available_frames)
            .max()
            .unwrap_or(0);
        if !primed {
            if max_available < startup_prefill_frames {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            primed = true;
            // 预填充期间 next_deadline 已经过期；从当前时刻重新建立节拍，
            // 避免首次处理时连续追赶多个过期 block，造成 FIFO 水位瞬时下降。
            next_deadline = Instant::now();
        }

        // Track each source independently. One healthy source must not hide another
        // source that is starving and being replaced with silence.
        let grace = underflow_grace_period(block_period);
        for (index, consumer) in consumers.iter().enumerate() {
            let available = consumer.available_frames();
            if available < block_frames {
                let started = source_starvation_since[index].get_or_insert_with(Instant::now);
                if should_report_source_underflow(
                    available,
                    block_frames,
                    started.elapsed(),
                    source_starvation_reported[index],
                    grace,
                ) {
                    counters.fifo_underflows.fetch_add(1, Ordering::Relaxed);
                    source_starvation_reported[index] = true;
                }
            } else {
                source_starvation_since[index] = None;
                source_starvation_reported[index] = false;
            }
        }
        if max_available < block_frames {
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        for (index, consumer) in consumers.iter_mut().enumerate() {
            source_blocks[index].fill(0.0);
            // 短读的剩余帧保持为 0，由 MixerPlan 按静音处理。
            let _ = consumer
                .pop_interleaved(&mut source_blocks[index])
                .map(|result| result.frames())
                .unwrap_or(0);
        }
        let source_refs: Vec<&[f32]> = source_blocks.iter().map(Vec::as_slice).collect();
        let mut sink_refs: Vec<&mut [f32]> =
            sink_blocks.iter_mut().map(Vec::as_mut_slice).collect();
        if let Err(e) = plan.process(&source_refs, &mut sink_refs) {
            fail(&state, &stop, &error, e.to_string());
            return Err(());
        }
        for (index, producer) in producers.iter_mut().enumerate() {
            let written = producer
                .push_interleaved(&sink_blocks[index])
                .map(|result| result.frames())
                .unwrap_or(0);
            if written < block_frames {
                counters.fifo_overflows.fetch_add(1, Ordering::Relaxed);
                counters
                    .fifo_dropped_frames
                    .fetch_add((block_frames - written) as u64, Ordering::Relaxed);
            }
        }
        next_deadline += block_period;
        let now = Instant::now();
        if next_deadline > now {
            thread::sleep(next_deadline - now);
        } else {
            next_deadline = now;
        }
    }
    Ok(())
}

fn render_worker(
    endpoint: loopmaster_audio_core::EndpointId,
    block_frames: usize,
    stop: Arc<AtomicBool>,
    state: Arc<AtomicU8>,
    error: Arc<Mutex<Option<String>>>,
    counters: Arc<Counters>,
    mut consumer: loopmaster_audio_core::AudioFifoConsumer,
) -> Result<(), ()> {
    let backend = WindowsAudioBackend::new().map_err(|e| {
        fail_windows(&state, &stop, &error, &e);
    })?;
    let mut sink = backend
        .open_render_sink(&endpoint, block_frames)
        .map_err(|e| {
            fail_windows(&state, &stop, &error, &e);
        })?;
    let mut block = vec![0.0f32; block_frames * INTERNAL_CHANNELS];
    let mut pending = false;
    let mut starvation_since = None;
    let mut starvation_reported = false;
    let block_period = Duration::from_secs_f64(
        block_frames as f64 / loopmaster_audio_core::INTERNAL_SAMPLE_RATE as f64,
    );
    let mut next_deadline = Instant::now();
    let startup_prefill_frames = block_frames
        .saturating_mul(STARTUP_PREFILL_BLOCKS)
        .min(consumer.capacity_frames());
    let mut primed = false;
    while !stop.load(Ordering::Acquire) {
        if !pending {
            let available = consumer.available_frames();
            if !primed {
                if available < startup_prefill_frames {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                primed = true;
                // 与 mixer 相同，丢弃预填充期间累积的旧 deadline，避免启动突发。
                next_deadline = Instant::now();
            }
            if available < block_frames {
                let started = starvation_since.get_or_insert_with(Instant::now);
                let grace = underflow_grace_period(block_period);
                if !starvation_reported && started.elapsed() >= grace {
                    counters.fifo_underflows.fetch_add(1, Ordering::Relaxed);
                    starvation_reported = true;
                }
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            starvation_since = None;
            starvation_reported = false;
            block.fill(0.0);
            let read = consumer
                .pop_interleaved(&mut block)
                .map(|r| r.frames())
                .unwrap_or(0);
            if read != block_frames {
                fail(
                    &state,
                    &stop,
                    &error,
                    "render FIFO 在完整 block 检查后仍未提供完整数据".to_owned(),
                );
                return Err(());
            }
            pending = true;
        }
        match sink.write_f32_block(&block) {
            Ok(crate::RenderWriteResult::Written { frames }) => {
                // 对重采样 sink 而言，返回 frame 数是设备输出 frame 数；输入的
                // 整个内部 block 已在本次调用中被 resampler 消费，不能按该数切片。
                pending = false;
                counters.render_writes.fetch_add(1, Ordering::Relaxed);
                counters
                    .rendered_frames
                    .fetch_add(u64::from(frames), Ordering::Relaxed);
                // render 侧峰值统计：验证 send 级路由变更是否在块边界生效。
                let peak = packet_peak(&block);
                if peak.is_finite() && peak > 0.0 {
                    counters
                        .rendered_peak
                        .fetch_max(peak.to_bits(), Ordering::Relaxed);
                }
                if is_non_silent(peak) {
                    counters
                        .rendered_non_silent_blocks
                        .fetch_add(1, Ordering::Relaxed);
                }
                next_deadline += block_period;
                let now = Instant::now();
                if next_deadline > now {
                    thread::sleep(next_deadline - now);
                } else {
                    next_deadline = now;
                }
            }
            Ok(crate::RenderWriteResult::NoSpace) => {
                counters.render_no_space.fetch_add(1, Ordering::Relaxed);
                thread::sleep(Duration::from_millis(1));
            }
            Err(e) => {
                fail_windows(&state, &stop, &error, &e);
                return Err(());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AUDCLNT_E_DEVICE_INVALIDATED;
    use loopmaster_audio_core::{
        EndpointId, RouteGraph, SendSpec, SinkId, SinkSpec, SourceId, SourceKind, SourceSpec,
    };

    fn config(source_kind: SourceKind, source_count: usize) -> AudioEngineConfig {
        let sources = (0..source_count)
            .map(|index| SourceSpec {
                id: SourceId(format!("source-{index}")),
                kind: source_kind.clone(),
                endpoint_id: Some(EndpointId(format!("capture-{index}"))),
                process_id: None,
                display_name: format!("Source {index}"),
            })
            .collect::<Vec<_>>();
        let graph = RouteGraph {
            sources,
            sinks: vec![SinkSpec {
                id: SinkId("sink".into()),
                endpoint_id: EndpointId("render".into()),
                display_name: "Render".into(),
            }],
            sends: vec![SendSpec {
                source_id: SourceId("source-0".into()),
                sink_id: SinkId("sink".into()),
                gain_db: 0.0,
                muted: false,
                channel_map: Vec::new(),
            }],
        };
        AudioEngineConfig::new(RouteGraphSnapshot::new(graph).unwrap())
    }

    #[test]
    fn validates_minimum_runtime_topology() {
        let engine = AudioEngine::new(config(SourceKind::DeviceCapture, 1)).unwrap();
        assert!(!engine.status().running);
        assert_eq!(engine.status().state, AudioEngineState::Stopped);

        // 阶段 B.1/B.2：Device Loopback 与多 source 均已支持；只有空拓扑被拒绝。
        assert!(AudioEngine::new(config(SourceKind::DeviceLoopback, 1)).is_ok());
        assert!(AudioEngine::new(config(SourceKind::DeviceCapture, 2)).is_ok());
        let empty_graph = RouteGraph {
            sources: Vec::new(),
            sinks: Vec::new(),
            sends: Vec::new(),
        };
        assert!(matches!(
            AudioEngine::new(AudioEngineConfig::new(
                RouteGraphSnapshot::new(empty_graph).unwrap()
            )),
            Err(AudioEngineError::UnsupportedTopology)
        ));

        let mut process_config = config(SourceKind::ProcessLoopback, 1);
        process_config.graph = RouteGraphSnapshot::new(RouteGraph {
            sources: vec![SourceSpec {
                id: SourceId("source-0".into()),
                kind: SourceKind::ProcessLoopback,
                endpoint_id: None,
                process_id: Some(1234),
                display_name: "process".into(),
            }],
            sinks: vec![SinkSpec {
                id: SinkId("sink".into()),
                endpoint_id: EndpointId("render".into()),
                display_name: "Render".into(),
            }],
            sends: vec![SendSpec {
                source_id: SourceId("source-0".into()),
                sink_id: SinkId("sink".into()),
                gain_db: 0.0,
                muted: false,
                channel_map: Vec::new(),
            }],
        })
        .unwrap();
        assert!(AudioEngine::new(process_config).is_ok());

        let mut missing_pid = config(SourceKind::ProcessLoopback, 1);
        missing_pid.graph = RouteGraphSnapshot::new(RouteGraph {
            sources: vec![SourceSpec {
                id: SourceId("source-0".into()),
                kind: SourceKind::ProcessLoopback,
                endpoint_id: None,
                process_id: None,
                display_name: "process".into(),
            }],
            sinks: vec![SinkSpec {
                id: SinkId("sink".into()),
                endpoint_id: EndpointId("render".into()),
                display_name: "Render".into(),
            }],
            sends: vec![SendSpec {
                source_id: SourceId("source-0".into()),
                sink_id: SinkId("sink".into()),
                gain_db: 0.0,
                muted: false,
                channel_map: Vec::new(),
            }],
        })
        .unwrap();
        assert!(matches!(
            AudioEngine::new(missing_pid),
            Err(AudioEngineError::MissingProcessId)
        ));
    }

    #[test]
    fn treats_process_pid_change_as_topology_change() {
        let previous = loopmaster_audio_core::RouteGraph {
            sources: vec![SourceSpec {
                id: SourceId("source-0".into()),
                kind: SourceKind::ProcessLoopback,
                endpoint_id: None,
                process_id: Some(1234),
                display_name: "process-1".into(),
            }],
            sinks: vec![SinkSpec {
                id: SinkId("sink".into()),
                endpoint_id: EndpointId("render".into()),
                display_name: "Render".into(),
            }],
            sends: vec![SendSpec {
                source_id: SourceId("source-0".into()),
                sink_id: SinkId("sink".into()),
                gain_db: 0.0,
                muted: false,
                channel_map: Vec::new(),
            }],
        };
        let mut next = previous.clone();
        next.sources[0].process_id = Some(5678);
        assert!(topology_changed(&previous, &next));
    }

    #[test]
    fn publishes_degraded_for_device_failure_and_failed_for_other_errors() {
        let state = AtomicU8::new(STATE_RUNNING);
        let stop = AtomicBool::new(false);
        let error = Mutex::new(None);
        let device_error = WindowsAudioError::HResult {
            operation: "IAudioClient::Start",
            hresult: AUDCLNT_E_DEVICE_INVALIDATED,
            endpoint_id: Some("capture-id".into()),
        };
        fail_windows(&state, &stop, &error, &device_error);
        assert_eq!(
            AudioEngineState::from_raw(state.load(Ordering::Acquire)),
            AudioEngineState::Degraded
        );
        assert!(stop.load(Ordering::Acquire));
        assert!(error.lock().unwrap().is_some());

        state.store(STATE_RUNNING, Ordering::Release);
        stop.store(false, Ordering::Release);
        let ordinary_error = WindowsAudioError::HResult {
            operation: "IAudioClient::Initialize",
            hresult: -2,
            endpoint_id: Some("capture-id".into()),
        };
        fail_windows(&state, &stop, &error, &ordinary_error);
        assert_eq!(
            AudioEngineState::from_raw(state.load(Ordering::Acquire)),
            AudioEngineState::Failed
        );
    }

    #[test]
    fn keeps_degraded_when_another_worker_reports_after_device_failure() {
        let state = AtomicU8::new(STATE_RUNNING);
        let stop = AtomicBool::new(false);
        let error = Mutex::new(None);
        let device_error = WindowsAudioError::CaptureState {
            reason: "endpoint 不是 active 状态",
            endpoint_id: "capture-id".into(),
        };
        let expected_error = device_error.to_string();
        fail_windows(&state, &stop, &error, &device_error);
        fail(
            &state,
            &stop,
            &error,
            "render worker 收尾时 FIFO 已停止".into(),
        );
        assert_eq!(
            AudioEngineState::from_raw(state.load(Ordering::Acquire)),
            AudioEngineState::Degraded
        );
        assert_eq!(
            error.lock().unwrap().as_deref(),
            Some(expected_error.as_str())
        );
    }

    #[test]
    fn keeps_reconnecting_when_probe_session_is_unavailable() {
        let state = AtomicU8::new(STATE_RECONNECTING);
        let stop = AtomicBool::new(false);
        let error = Mutex::new(None);
        let device_error = WindowsAudioError::CaptureState {
            reason: "endpoint 不是 active 状态",
            endpoint_id: "capture-id".into(),
        };
        fail_windows(&state, &stop, &error, &device_error);
        assert_eq!(
            AudioEngineState::from_raw(state.load(Ordering::Acquire)),
            AudioEngineState::Reconnecting
        );
        assert!(stop.load(Ordering::Acquire));
    }

    #[test]
    fn reconnect_window_covers_driver_reenumeration_delay() {
        let attempts = DEFAULT_RECONNECT_ATTEMPTS;
        let delay = RECONNECT_DELAY;
        #[allow(clippy::assertions_on_constants)]
        {
            assert!(attempts >= 60);
            assert!(delay >= Duration::from_millis(250));
            assert!(delay * attempts as u32 >= Duration::from_secs(30));
        }
    }

    #[test]
    fn exposes_all_recovery_states() {
        assert_eq!(
            AudioEngineState::from_raw(STATE_RUNNING),
            AudioEngineState::Running
        );
        assert_eq!(
            AudioEngineState::from_raw(STATE_DEGRADED),
            AudioEngineState::Degraded
        );
        assert_eq!(
            AudioEngineState::from_raw(STATE_RECONNECTING),
            AudioEngineState::Reconnecting
        );
        assert_eq!(
            AudioEngineState::from_raw(STATE_FAILED),
            AudioEngineState::Failed
        );
        assert_eq!(
            AudioEngineState::from_raw(STATE_STOPPED),
            AudioEngineState::Stopped
        );
    }

    #[test]
    fn rejects_invalid_timing_configuration() {
        let mut zero_block_config = config(SourceKind::DeviceCapture, 1);
        zero_block_config.block_frames = 0;
        assert!(matches!(
            AudioEngine::new(zero_block_config),
            Err(AudioEngineError::ZeroBlockFrames)
        ));

        let mut small_fifo_config = config(SourceKind::DeviceCapture, 1);
        small_fifo_config.fifo_capacity_frames = 1;
        assert!(matches!(
            AudioEngine::new(small_fifo_config),
            Err(AudioEngineError::FifoTooSmall)
        ));
    }

    #[test]
    fn defaults_to_a_buffer_large_enough_for_startup_prefill() {
        let config = config(SourceKind::DeviceCapture, 1);
        assert_eq!(
            config.fifo_capacity_frames,
            config.block_frames * DEFAULT_FIFO_CAPACITY_BLOCKS
        );
        assert!(config.fifo_capacity_frames >= config.block_frames * STARTUP_PREFILL_BLOCKS);
    }

    #[test]
    fn underflow_grace_requires_two_audio_blocks() {
        let block_period = Duration::from_millis(10);
        assert_eq!(
            underflow_grace_period(block_period),
            Duration::from_millis(20)
        );
    }

    #[test]
    fn preserves_available_audio_when_render_fifo_underflows() {
        let mut block = vec![1.0, -1.0, 0.0, 0.0];
        let silence = [0.0; 4];
        let read = 1;
        let written_samples = read * INTERNAL_CHANNELS;
        block[written_samples..].copy_from_slice(&silence[written_samples..]);
        assert_eq!(block, vec![1.0, -1.0, 0.0, 0.0]);
    }

    #[cfg(not(windows))]
    #[test]
    fn start_publishes_backend_failure_without_panicking() {
        let mut engine = AudioEngine::new(config(SourceKind::DeviceCapture, 1)).unwrap();
        engine.start().unwrap();
        for _ in 0..20 {
            if engine.status().failed {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let status = engine.status();
        assert!(status.failed);
        assert!(status.last_error.is_some());
        engine.stop().unwrap();
    }

    #[test]
    fn packet_peak_empty_returns_zero() {
        assert_eq!(packet_peak(&[]), 0.0);
    }

    #[test]
    fn packet_peak_ignores_silence() {
        assert_eq!(packet_peak(&[0.0, 0.0, 0.0, 0.0]), 0.0);
    }

    #[test]
    fn packet_peak_takes_absolute_maximum() {
        let samples = [0.5, -0.8, 0.1, -0.3, 0.02];
        assert_eq!(packet_peak(&samples), 0.8);
    }

    #[test]
    fn packet_peak_ignores_nan_samples() {
        let samples = [0.5, f32::NAN, -0.2];
        assert_eq!(packet_peak(&samples), 0.5);
    }

    #[test]
    fn non_silent_threshold_is_minus_80_dbfs() {
        assert!(!is_non_silent(0.0));
        assert!(!is_non_silent(1e-5)); // 低于 -80 dBFS 阈值
        assert!(is_non_silent(1e-3)); // 高于 -80 dBFS 阈值
        assert!(is_non_silent(0.5));
    }

    #[test]
    fn reports_underflow_for_one_starved_source_even_when_another_is_ready() {
        assert!(should_report_source_underflow(
            0,
            480,
            Duration::from_millis(20),
            false,
            Duration::from_millis(20),
        ));
        assert!(!should_report_source_underflow(
            480,
            480,
            Duration::from_millis(20),
            false,
            Duration::from_millis(20),
        ));
        assert!(!should_report_source_underflow(
            0,
            480,
            Duration::from_millis(20),
            true,
            Duration::from_millis(20),
        ));
    }

    #[test]
    fn capture_resampler_uses_configured_block_size() {
        let source_format = crate::EndpointFormat {
            sample_rate: 44_100,
            bits_per_sample: 32,
            channels: INTERNAL_CHANNELS as u16,
            channel_mask: 3,
            is_float: true,
            is_pcm: false,
        };
        for block_frames in [240, 960] {
            let resampler =
                CaptureResampler::new(Some(source_format), block_frames, block_frames * 4).unwrap();
            assert_eq!(
                resampler.output_block.len(),
                block_frames * INTERNAL_CHANNELS
            );
            assert_eq!(
                resampler.resampler.as_ref().unwrap().output_frames(),
                block_frames
            );
        }
    }
}
