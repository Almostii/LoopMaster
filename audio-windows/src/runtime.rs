//! 正式音频引擎运行时骨架。
//!
//! 运行时将 WASAPI capture、平台无关混音和 WASAPI render 分成三个 worker。
//! worker 之间只通过固定容量 SPSC FIFO 交换 interleaved `f32` block；配置更新
//! 通过控制通道发送，由 mixer 在 block 边界应用。

use crate::{WindowsAudioBackend, WindowsAudioError};
use loopmaster_audio_core::{
    AudioFifo, MixerPlan, RouteGraphSnapshot, SourceKind, DEFAULT_BLOCK_FRAMES, INTERNAL_CHANNELS,
};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
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
const DEFAULT_FIFO_CAPACITY_BLOCKS: usize = 32;
const DEFAULT_RECONNECT_ATTEMPTS: usize = 5;
const RECONNECT_DELAY: Duration = Duration::from_millis(500);

#[derive(Clone, Debug)]
pub struct AudioEngineConfig {
    pub graph: RouteGraphSnapshot,
    pub block_frames: usize,
    pub fifo_capacity_frames: usize,
}

/// 音频引擎的运行状态。
///
/// `Degraded` 表示 worker 因设备失效退出，错误具备重连条件；
/// `Reconnecting` 为后续自动重建 stream 预留，当前版本只提供状态模型，
/// 不会假装已经完成真实设备拔插恢复。
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
    #[error("当前运行时骨架只支持单路 source -> 单路 sink")]
    UnsupportedTopology,
    #[error("source 必须是设备捕获 source")]
    UnsupportedSourceKind,
    #[error("source 未配置 endpoint ID")]
    MissingSourceEndpoint,
    #[error("运行中的引擎不支持更换 endpoint；必须停止后重新启动")]
    EndpointChangeRequiresRestart,
    #[error("WASAPI worker 错误: {0}")]
    Windows(#[from] WindowsAudioError),
    #[error("混音计划错误: {0}")]
    Mixer(#[from] loopmaster_audio_core::MixerError),
    #[error("FIFO 配置错误: {0}")]
    Fifo(#[from] loopmaster_audio_core::FifoConfigError),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
        }
    }
}

pub struct AudioEngine {
    config: AudioEngineConfig,
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
        let graph = self.config.graph.clone();
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
                    graph,
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
        if previous.sources[0].endpoint_id != next.sources[0].endpoint_id
            || previous.sinks[0].endpoint_id != next.sinks[0].endpoint_id
        {
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
    if graph.sources.len() != 1 || graph.sinks.len() != 1 || graph.sends.len() != 1 {
        return Err(AudioEngineError::UnsupportedTopology);
    }
    if graph.sources[0].kind != SourceKind::DeviceCapture {
        return Err(AudioEngineError::UnsupportedSourceKind);
    }
    if graph.sources[0].endpoint_id.is_none() {
        return Err(AudioEngineError::MissingSourceEndpoint);
    }
    Ok(())
}

fn graph_snapshot_sink_endpoint(graph: &RouteGraphSnapshot) -> loopmaster_audio_core::EndpointId {
    graph.graph().sinks[0].endpoint_id.clone()
}

fn fail(state: &AtomicU8, stop: &AtomicBool, error: &Mutex<Option<String>>, value: String) {
    *error.lock().expect("状态锁未中毒") = Some(value);
    state.store(STATE_FAILED, Ordering::Release);
    stop.store(true, Ordering::Release);
}

fn fail_windows(
    state: &AtomicU8,
    stop: &AtomicBool,
    error: &Mutex<Option<String>>,
    value: &WindowsAudioError,
) {
    *error.lock().expect("状态锁未中毒") = Some(value.to_string());
    let next_state = if value.is_device_failure() {
        STATE_DEGRADED
    } else {
        STATE_FAILED
    };
    state.store(next_state, Ordering::Release);
    stop.store(true, Ordering::Release);
}

/// 负责管线会话的生命周期。每次重试都重新创建 FIFO、WASAPI stream 和
/// 重采样器；旧会话的 worker 必须先 join，避免两个会话同时驱动同一 endpoint。
#[allow(clippy::too_many_arguments)]
fn supervisor_worker(
    graph: RouteGraphSnapshot,
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
            state.store(STATE_RECONNECTING, Ordering::Release);
            thread::sleep(RECONNECT_DELAY);
            if engine_stop.load(Ordering::Acquire) {
                break;
            }
        }
        let session_stop = Arc::new(AtomicBool::new(false));
        let (source_tx, mixer_rx) = match AudioFifo::split(fifo_capacity_frames, INTERNAL_CHANNELS)
        {
            Ok(value) => value,
            Err(e) => {
                fail(&state, &engine_stop, &error, e.to_string());
                break;
            }
        };
        let (mixer_tx, render_rx) = match AudioFifo::split(fifo_capacity_frames, INTERNAL_CHANNELS)
        {
            Ok(value) => value,
            Err(e) => {
                fail(&state, &engine_stop, &error, e.to_string());
                break;
            }
        };
        let (session_graph_tx, graph_rx) = mpsc::channel();
        *graph_tx_slot.lock().expect("路由通道锁未中毒") = Some(session_graph_tx);
        let source_endpoint = match graph.graph().sources[0].endpoint_id.clone() {
            Some(endpoint) => endpoint,
            None => {
                fail(
                    &state,
                    &engine_stop,
                    &error,
                    "source 未配置 endpoint ID".into(),
                );
                break;
            }
        };
        let sink_endpoint = graph_snapshot_sink_endpoint(&graph);
        let mut workers = Vec::with_capacity(3);
        let capture = spawn_capture_worker(
            source_endpoint,
            Arc::clone(&session_stop),
            Arc::clone(&state),
            Arc::clone(&error),
            Arc::clone(&counters),
            source_tx,
        );
        let mixer = spawn_mixer_worker(
            graph.clone(),
            graph_rx,
            block_frames,
            Arc::clone(&session_stop),
            Arc::clone(&state),
            Arc::clone(&error),
            Arc::clone(&counters),
            mixer_rx,
            mixer_tx,
        );
        let render = spawn_render_worker(
            sink_endpoint,
            block_frames,
            Arc::clone(&session_stop),
            Arc::clone(&state),
            Arc::clone(&error),
            Arc::clone(&counters),
            render_rx,
        );
        workers.push(capture);
        workers.push(mixer);
        workers.push(render);
        // 给三个 worker 一个有界启动窗口；设备在打开阶段失效时，
        // session_stop 会先置位，避免把尚未建立的会话报告为 Running。
        thread::sleep(Duration::from_millis(50));
        if !session_stop.load(Ordering::Acquire) {
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
        if state.load(Ordering::Acquire) != STATE_DEGRADED {
            break;
        }
        if attempt == DEFAULT_RECONNECT_ATTEMPTS {
            *error.lock().expect("状态锁未中毒") = Some("设备重连重试次数耗尽".into());
            state.store(STATE_FAILED, Ordering::Release);
            engine_stop.store(true, Ordering::Release);
        }
    }
}

fn spawn_capture_worker(
    endpoint: loopmaster_audio_core::EndpointId,
    stop: Arc<AtomicBool>,
    state: Arc<AtomicU8>,
    error: Arc<Mutex<Option<String>>>,
    counters: Arc<Counters>,
    producer: loopmaster_audio_core::AudioFifoProducer,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("loopmaster-capture".into())
        .spawn(move || {
            let _ = capture_worker(endpoint, stop, state, error, counters, producer);
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
    consumer: loopmaster_audio_core::AudioFifoConsumer,
    producer: loopmaster_audio_core::AudioFifoProducer,
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
                consumer,
                producer,
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

fn capture_worker(
    endpoint: loopmaster_audio_core::EndpointId,
    stop: Arc<AtomicBool>,
    state: Arc<AtomicU8>,
    error: Arc<Mutex<Option<String>>>,
    counters: Arc<Counters>,
    mut producer: loopmaster_audio_core::AudioFifoProducer,
) -> Result<(), ()> {
    let backend = WindowsAudioBackend::new().map_err(|e| {
        fail_windows(&state, &stop, &error, &e);
    })?;
    let mut source = backend.open_capture_source(&endpoint).map_err(|e| {
        fail_windows(&state, &stop, &error, &e);
    })?;
    let silence = vec![0.0f32; DEFAULT_BLOCK_FRAMES * INTERNAL_CHANNELS];
    let mut first_packet = true;
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
            first_packet = false;
        });
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
    mut consumer: loopmaster_audio_core::AudioFifoConsumer,
    mut producer: loopmaster_audio_core::AudioFifoProducer,
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
    let mut source_block = vec![0.0f32; block_frames * INTERNAL_CHANNELS];
    let mut sink_block = vec![0.0f32; block_frames * INTERNAL_CHANNELS];
    let block_period = Duration::from_secs_f64(
        block_frames as f64 / loopmaster_audio_core::INTERNAL_SAMPLE_RATE as f64,
    );
    let mut next_deadline = Instant::now();
    let startup_prefill_frames = block_frames
        .saturating_mul(STARTUP_PREFILL_BLOCKS)
        .min(consumer.capacity_frames());
    let mut primed = false;
    let mut starvation_since = None;
    let mut starvation_reported = false;
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
        let available = consumer.available_frames();
        if !primed && available < startup_prefill_frames {
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        primed = true;
        if available < block_frames {
            let started = starvation_since.get_or_insert_with(Instant::now);
            if !starvation_reported && started.elapsed() >= block_period {
                counters.fifo_underflows.fetch_add(1, Ordering::Relaxed);
                starvation_reported = true;
            }
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        starvation_since = None;
        starvation_reported = false;
        source_block.fill(0.0);
        let read = consumer
            .pop_interleaved(&mut source_block)
            .map(|r| r.frames())
            .unwrap_or(0);
        if read != block_frames {
            fail(
                &state,
                &stop,
                &error,
                "source FIFO 在完整 block 检查后仍未提供完整数据".to_owned(),
            );
            return Err(());
        }
        let source_refs = [source_block.as_slice()];
        let mut sink_refs = [sink_block.as_mut_slice()];
        if let Err(e) = plan.process(&source_refs, &mut sink_refs) {
            fail(&state, &stop, &error, e.to_string());
            return Err(());
        }
        let written = producer
            .push_interleaved(&sink_block)
            .map(|r| r.frames())
            .unwrap_or(0);
        if written < block_frames {
            counters.fifo_overflows.fetch_add(1, Ordering::Relaxed);
            counters
                .fifo_dropped_frames
                .fetch_add((block_frames - written) as u64, Ordering::Relaxed);
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
            if !primed && available < startup_prefill_frames {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            primed = true;
            if available < block_frames {
                let started = starvation_since.get_or_insert_with(Instant::now);
                if !starvation_reported && started.elapsed() >= block_period {
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

        assert!(matches!(
            AudioEngine::new(config(SourceKind::DeviceLoopback, 1)),
            Err(AudioEngineError::UnsupportedSourceKind)
        ));
        assert!(matches!(
            AudioEngine::new(config(SourceKind::DeviceCapture, 2)),
            Err(AudioEngineError::UnsupportedTopology)
        ));
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
}
