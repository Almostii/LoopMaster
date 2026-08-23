use loopmaster_audio_core::{
    AudioFifo, EndpointId, MixerPlan, RouteGraph, RouteGraphSnapshot, SendSpec, SinkId, SinkSpec,
    SourceId, SourceKind, SourceSpec,
};
use loopmaster_audio_windows::{
    AudioEngine, AudioEngineConfig, AudioEngineState, EndpointFlow, EndpointFormat, EndpointInfo,
    WindowsAudioBackend,
};
use std::env;
use std::thread;
use std::time::{Duration, Instant};

const ENGINE_STATUS_SAMPLE_INTERVAL: Duration = Duration::from_millis(20);
const ENGINE_LIVE_OUTPUT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Default)]
struct StateObservation {
    first: Option<AudioEngineState>,
    last: Option<AudioEngineState>,
    transitions: [u64; 5],
}

impl StateObservation {
    fn new(initial: AudioEngineState) -> Self {
        let mut observation = Self::default();
        observation.observe(initial);
        observation
    }

    fn observe(&mut self, state: AudioEngineState) {
        if self.first.is_none() {
            self.first = Some(state);
        }
        if self.last != Some(state) {
            if self.last.is_some() {
                self.transitions[state_index(state)] += 1;
            }
            self.last = Some(state);
        }
    }

    fn transition_count(&self, state: AudioEngineState) -> u64 {
        self.transitions[state_index(state)]
    }
}

fn state_index(state: AudioEngineState) -> usize {
    match state {
        AudioEngineState::Stopped => 0,
        AudioEngineState::Running => 1,
        AudioEngineState::Degraded => 2,
        AudioEngineState::Reconnecting => 3,
        AudioEngineState::Failed => 4,
    }
}

fn main() {
    let backend = match WindowsAudioBackend::new() {
        Ok(backend) => backend,
        Err(error) => exit_with_error("初始化 Windows 音频后端失败", error),
    };

    let endpoints = match backend.enumerate_endpoints() {
        Ok(endpoints) => endpoints,
        Err(error) => exit_with_error("枚举 Windows 音频 endpoint 失败", error),
    };

    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("--processes") {
        run_process_list(&backend);
    } else if args.get(1).map(String::as_str) == Some("--engine") && args.len() >= 4 {
        let seconds = args
            .get(4)
            .and_then(|value| value.parse().ok())
            .unwrap_or(10);
        run_engine_test(&args[2], &args[3], seconds);
    } else if args.get(1).map(String::as_str) == Some("--process-engine") && args.len() >= 4 {
        let pid = args[2].parse().unwrap_or(0);
        let seconds = args
            .get(4)
            .and_then(|value| value.parse().ok())
            .unwrap_or(10);
        run_process_engine_test(pid, &args[3], seconds);
    } else if args.get(1).map(String::as_str) == Some("--loopback-engine") && args.len() >= 4 {
        let seconds = args
            .get(4)
            .and_then(|value| value.parse().ok())
            .unwrap_or(10);
        run_loopback_engine_test(&args[2], &args[3], seconds);
    } else if args.get(1).map(String::as_str) == Some("--loopback-tone-test") && args.len() >= 3 {
        let seconds = args
            .get(3)
            .and_then(|value| value.parse().ok())
            .unwrap_or(10);
        run_loopback_tone_test(&args[2], seconds);
    } else if args.get(1).map(String::as_str) == Some("--update-test") && args.len() >= 4 {
        let pid = args[2].parse().unwrap_or(0);
        let seconds = args
            .get(4)
            .and_then(|value| value.parse().ok())
            .unwrap_or(10);
        run_update_test(pid, &args[3], seconds);
    } else if args.len() >= 3 {
        let capture_id = &args[1];
        let render_id = &args[2];
        let seconds = args
            .get(3)
            .and_then(|value| value.parse().ok())
            .unwrap_or(10);
        run_capture_render_test(&backend, capture_id, render_id, seconds);
    }

    println!("LoopMaster Windows endpoint diagnostics");
    println!("发现 {} 个 active endpoint", endpoints.len());
    for (index, endpoint) in endpoints.iter().enumerate() {
        print_endpoint(index + 1, endpoint);
    }
}

fn run_process_list(backend: &WindowsAudioBackend) -> ! {
    match backend.enumerate_processes() {
        Ok(processes) => {
            println!("LoopMaster 活动音频进程");
            println!("发现 {} 个可用于 Process Loopback 的进程", processes.len());
            for process in processes {
                match process.executable_path {
                    Some(path) => println!(
                        "PID={} | name={} | executable={}",
                        process.pid, process.name, path
                    ),
                    None => println!(
                        "PID={} | name={} | executable=<unavailable>",
                        process.pid, process.name
                    ),
                }
            }
            std::process::exit(0);
        }
        Err(error) => exit_with_error("枚举活动音频进程失败", error),
    }
}

fn run_engine_test(capture_id: &str, render_id: &str, seconds: u64) -> ! {
    let graph = RouteGraph {
        sources: vec![SourceSpec {
            id: SourceId("capture".to_owned()),
            kind: SourceKind::DeviceCapture,
            endpoint_id: Some(EndpointId(capture_id.to_owned())),
            process_id: None,
            display_name: "capture".to_owned(),
        }],
        sinks: vec![SinkSpec {
            id: SinkId("render".to_owned()),
            endpoint_id: EndpointId(render_id.to_owned()),
            display_name: "render".to_owned(),
        }],
        sends: vec![SendSpec {
            source_id: SourceId("capture".to_owned()),
            sink_id: SinkId("render".to_owned()),
            gain_db: 0.0,
            muted: false,
            channel_map: Vec::new(),
        }],
    };
    run_engine_graph_test(graph, seconds)
}

fn run_process_engine_test(pid: u32, render_id: &str, seconds: u64) -> ! {
    if pid == 0 {
        eprintln!("Process Loopback 需要有效的进程 PID");
        std::process::exit(1);
    }
    let graph = RouteGraph {
        sources: vec![SourceSpec {
            id: SourceId("process".to_owned()),
            kind: SourceKind::ProcessLoopback,
            endpoint_id: None,
            process_id: Some(pid),
            display_name: format!("process:{pid}"),
        }],
        sinks: vec![SinkSpec {
            id: SinkId("render".to_owned()),
            endpoint_id: EndpointId(render_id.to_owned()),
            display_name: "render".to_owned(),
        }],
        sends: vec![SendSpec {
            source_id: SourceId("process".to_owned()),
            sink_id: SinkId("render".to_owned()),
            gain_db: 0.0,
            muted: false,
            channel_map: Vec::new(),
        }],
    };
    run_engine_graph_test(graph, seconds)
}

fn run_loopback_engine_test(loopback_render_id: &str, sink_render_id: &str, seconds: u64) -> ! {
    let graph = RouteGraph {
        sources: vec![SourceSpec {
            id: SourceId("loopback".to_owned()),
            kind: SourceKind::DeviceLoopback,
            endpoint_id: Some(EndpointId(loopback_render_id.to_owned())),
            process_id: None,
            display_name: "loopback".to_owned(),
        }],
        sinks: vec![SinkSpec {
            id: SinkId("render".to_owned()),
            endpoint_id: EndpointId(sink_render_id.to_owned()),
            display_name: "render".to_owned(),
        }],
        sends: vec![SendSpec {
            source_id: SourceId("loopback".to_owned()),
            sink_id: SinkId("render".to_owned()),
            gain_db: 0.0,
            muted: false,
            channel_map: Vec::new(),
        }],
    };
    run_engine_graph_test(graph, seconds)
}

/// 自验证 Device Loopback：向同一 render endpoint 播放 440 Hz 正弦测试音，
/// 同时从该 endpoint 的 loopback 流抓回，统计抓到的 packet/电平。
/// 不依赖外部声音源，直接证明 loopback 数据链路完整（阶段 B.1/B.6）。
fn run_loopback_tone_test(render_id: &str, seconds: u64) -> ! {
    let backend = match WindowsAudioBackend::new() {
        Ok(backend) => backend,
        Err(error) => exit_with_error("初始化 Windows 音频后端失败", error),
    };
    let endpoint = EndpointId(render_id.to_owned());
    let mut sink = match backend.open_render_sink(&endpoint, 480) {
        Ok(sink) => sink,
        Err(error) => exit_with_error("打开 render sink 失败", error),
    };
    let mut loopback = match backend.open_device_loopback_source(&endpoint) {
        Ok(source) => source,
        Err(error) => exit_with_error("打开 Device Loopback 失败", error),
    };
    let sample_rate = 48_000.0f32;
    let frequency = 440.0f32;
    let mut phase = 0.0f32;
    let mut block = vec![0.0f32; 960];
    let deadline = Instant::now() + Duration::from_secs(seconds.max(1));
    let mut packets = 0u64;
    let mut captured_frames = 0u64;
    let mut peak = 0.0f32;
    let mut non_silent = 0u64;
    while Instant::now() < deadline {
        for frame in 0..480 {
            let sample = (2.0 * std::f32::consts::PI * frequency * phase / sample_rate).sin() * 0.5;
            block[frame * 2] = sample;
            block[frame * 2 + 1] = sample;
            phase += 1.0;
            if phase >= sample_rate {
                phase = 0.0;
            }
        }
        match sink.write_f32_block(&block) {
            Ok(loopmaster_audio_windows::RenderWriteResult::Written { .. }) => {}
            Ok(loopmaster_audio_windows::RenderWriteResult::NoSpace) => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => exit_with_error("写入测试音失败", error),
        }
        let result = loopback
            .drain_packets(|packet, data| {
                packets += 1;
                captured_frames += u64::from(packet.frames);
                if let Some(samples) = data {
                    let packet_peak = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
                    peak = peak.max(packet_peak);
                    if packet_peak > 1e-4 {
                        non_silent += 1;
                    }
                }
            })
            .unwrap_or_default();
        if result.packets == 0 {
            thread::sleep(Duration::from_millis(1));
        }
    }
    println!("LoopMaster Device Loopback 自验证（测试音 440 Hz）");
    println!("目标 endpoint: {render_id}");
    println!("运行时间: {} 秒", seconds.max(1));
    println!("loopback packets: {packets}");
    println!("loopback frames: {captured_frames}");
    println!("loopback peak: {:.1} dBFS", 20.0 * peak.log10().max(-120.0));
    println!("non-silent packets: {non_silent}");
    std::process::exit(if packets > 0 && non_silent > 0 { 0 } else { 2 });
}

/// 验证运行中 send 级路由变更在块边界生效（阶段 B.3）：
/// Process Loopback 引擎跑一段时间后调用 update_graph 静音，检查
/// rendered_non_silent_blocks 停止增长且状态保持 Running。
fn run_update_test(pid: u32, render_id: &str, seconds: u64) -> ! {
    if pid == 0 {
        eprintln!("需要有效的进程 PID");
        std::process::exit(1);
    }
    let make_graph = |muted: bool| RouteGraph {
        sources: vec![SourceSpec {
            id: SourceId("process".to_owned()),
            kind: SourceKind::ProcessLoopback,
            endpoint_id: None,
            process_id: Some(pid),
            display_name: format!("process:{pid}"),
        }],
        sinks: vec![SinkSpec {
            id: SinkId("render".to_owned()),
            endpoint_id: EndpointId(render_id.to_owned()),
            display_name: "render".to_owned(),
        }],
        sends: vec![SendSpec {
            source_id: SourceId("process".to_owned()),
            sink_id: SinkId("render".to_owned()),
            gain_db: 0.0,
            muted,
            channel_map: Vec::new(),
        }],
    };
    let mut engine = AudioEngine::new(AudioEngineConfig::new(
        RouteGraphSnapshot::new(make_graph(false)).expect("引擎验收路由图有效"),
    ))
    .expect("引擎验收配置有效");
    if let Err(error) = engine.start() {
        eprintln!("启动正式音频引擎失败: {error}");
        std::process::exit(1);
    }
    let half = (seconds.max(2) / 2).max(1);
    run_engine_for(&mut engine, half);
    let mid = engine.status();
    println!(
        "第一段（正常）: rendered_non_silent_blocks={} | rendered_peak={} dBFS | state={}",
        mid.stats.rendered_non_silent_blocks,
        peak_dbfs(mid.stats.rendered_peak),
        mid.state.as_str()
    );
    if let Err(error) =
        engine.update_graph(RouteGraphSnapshot::new(make_graph(true)).expect("静音路由图有效"))
    {
        eprintln!("update_graph 失败: {error}");
        std::process::exit(1);
    }
    println!("已执行 update_graph(muted=true)，等待块边界应用…");
    run_engine_for(&mut engine, half);
    let end = engine.status();
    // 切换边界的 mpsc 传播延迟允许少量残留（< 5%）：静音真正生效时
    // 第二段增长应远小于第一段。
    let growth_first = mid.stats.rendered_non_silent_blocks;
    let growth_second = end.stats.rendered_non_silent_blocks - mid.stats.rendered_non_silent_blocks;
    let muted_effective = growth_second * 100 <= growth_first.max(1) * 5;
    println!(
        "第二段（静音）: rendered_non_silent_blocks={}（增长 {growth_second}）| rendered_peak={} dBFS | state={}",
        end.stats.rendered_non_silent_blocks,
        peak_dbfs(end.stats.rendered_peak),
        end.state.as_str()
    );
    println!("graph_updates: {}", end.stats.graph_updates);
    println!(
        "静音切换生效: {}",
        if muted_effective {
            "是（第二段增长远小于第一段）"
        } else {
            "否"
        }
    );
    println!(
        "切换期间 underflow/discontinuity: {}/{}",
        end.stats.fifo_underflows, end.stats.discontinuities
    );
    let _ = engine.stop();
    std::process::exit(
        if muted_effective && end.stats.graph_updates >= 1 && !end.failed {
            0
        } else {
            2
        },
    );
}

fn run_engine_for(engine: &mut AudioEngine, duration_secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(duration_secs.max(1));
    while Instant::now() < deadline {
        let status = engine.status();
        if status.failed {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn run_engine_graph_test(graph: RouteGraph, seconds: u64) -> ! {
    let snapshot = RouteGraphSnapshot::new(graph).expect("引擎验收路由图有效");
    let mut engine = AudioEngine::new(AudioEngineConfig::new(snapshot)).expect("引擎验收配置有效");
    if let Err(error) = engine.start() {
        eprintln!("启动正式音频引擎失败: {error}");
        std::process::exit(1);
    }
    println!("提示：请在本命令运行期间拔出并重新插入声卡或 VB-CABLE，以验证自动恢复。");
    println!("提示：状态统计来自周期采样；持续时间短于采样间隔的状态可能不会被观察到。");
    let initial_status = engine.status();
    let mut state_observation = StateObservation::new(initial_status.state);
    let mut last_live_state = initial_status.state;
    let mut last_live_output = Instant::now();
    println!(
        "实时状态: {} | capture packet={} | render writes={} | reconnect attempts={}",
        initial_status.state.as_str(),
        initial_status.stats.capture_packets,
        initial_status.stats.render_writes,
        initial_status.stats.reconnect_attempts
    );
    let deadline = Instant::now() + Duration::from_secs(seconds.max(1));
    while Instant::now() < deadline {
        let status = engine.status();
        state_observation.observe(status.state);
        if status.state != last_live_state {
            println!(
                "实时状态变化: {} -> {} | capture packet={} | render writes={} | reconnect attempts={}",
                last_live_state.as_str(),
                status.state.as_str(),
                status.stats.capture_packets,
                status.stats.render_writes,
                status.stats.reconnect_attempts
            );
            last_live_state = status.state;
            last_live_output = Instant::now();
        } else if last_live_output.elapsed() >= ENGINE_LIVE_OUTPUT_INTERVAL {
            println!(
                "实时统计: state={} | capture packet={} | render writes={} | FIFO underflow={} | discontinuity={} | reconnect attempts={} | peak={} dBFS | non-silent={}",
                status.state.as_str(),
                status.stats.capture_packets,
                status.stats.render_writes,
                status.stats.fifo_underflows,
                status.stats.discontinuities,
                status.stats.reconnect_attempts,
                peak_dbfs(status.stats.captured_peak),
                status.stats.non_silent_packets
            );
            last_live_output = Instant::now();
        }
        if status.failed {
            break;
        }
        std::thread::sleep(ENGINE_STATUS_SAMPLE_INTERVAL);
    }
    let status = engine.status();
    state_observation.observe(status.state);
    let _ = engine.stop();
    println!("LoopMaster 正式音频引擎验收");
    println!("运行时间: {} 秒", seconds.max(1));
    println!("capture packet: {}", status.stats.capture_packets);
    println!("capture frames: {}", status.stats.captured_frames);
    println!(
        "captured peak: {} dBFS",
        peak_dbfs(status.stats.captured_peak)
    );
    println!("non-silent packets: {}", status.stats.non_silent_packets);
    println!("render writes: {}", status.stats.render_writes);
    println!("render frames: {}", status.stats.rendered_frames);
    println!("FIFO overflow events: {}", status.stats.fifo_overflows);
    println!("FIFO dropped frames: {}", status.stats.fifo_dropped_frames);
    println!("FIFO underflow events: {}", status.stats.fifo_underflows);
    println!("data discontinuity: {}", status.stats.discontinuities);
    println!(
        "startup discontinuity: {}",
        status.stats.startup_discontinuities
    );
    println!(
        "runtime discontinuity: {}",
        status.stats.runtime_discontinuities
    );
    println!("timestamp errors: {}", status.stats.timestamp_errors);
    println!("reconnect attempts: {}", status.stats.reconnect_attempts);
    println!(
        "状态首次/最后: {}/{}",
        state_observation
            .first
            .map(AudioEngineState::as_str)
            .unwrap_or("Unknown"),
        state_observation
            .last
            .map(AudioEngineState::as_str)
            .unwrap_or("Unknown")
    );
    println!(
        "状态转移次数（观测到进入该状态）: Running={}, Degraded={}, Reconnecting={}, Failed={}",
        state_observation.transition_count(AudioEngineState::Running),
        state_observation.transition_count(AudioEngineState::Degraded),
        state_observation.transition_count(AudioEngineState::Reconnecting),
        state_observation.transition_count(AudioEngineState::Failed),
    );
    if let Some(error) = status.last_error {
        eprintln!("引擎错误: {error}");
    }
    std::process::exit(
        if !status.failed && status.stats.capture_packets > 0 && status.stats.rendered_frames > 0 {
            0
        } else {
            2
        },
    );
}

fn run_capture_render_test(
    backend: &WindowsAudioBackend,
    capture_id: &str,
    render_id: &str,
    seconds: u64,
) -> ! {
    let capture_endpoint = EndpointId(capture_id.to_owned());
    let render_endpoint = EndpointId(render_id.to_owned());
    let mut capture = match backend.open_capture_source(&capture_endpoint) {
        Ok(source) => source,
        Err(error) => exit_with_error("打开 capture endpoint 失败", error),
    };
    let mut render = match backend.open_render_sink(&render_endpoint, 480) {
        Ok(sink) => sink,
        Err(error) => exit_with_error("打开 render endpoint 失败", error),
    };
    let (mut producer, mut consumer) = AudioFifo::split(4_800, 2).expect("固定 FIFO 配置有效");
    let graph = RouteGraph {
        sources: vec![SourceSpec {
            id: SourceId("capture".to_owned()),
            kind: SourceKind::DeviceCapture,
            endpoint_id: Some(capture_endpoint.clone()),
            process_id: None,
            display_name: "capture".to_owned(),
        }],
        sinks: vec![SinkSpec {
            id: SinkId("render".to_owned()),
            endpoint_id: render_endpoint.clone(),
            display_name: "render".to_owned(),
        }],
        sends: vec![SendSpec {
            source_id: SourceId("capture".to_owned()),
            sink_id: SinkId("render".to_owned()),
            gain_db: 0.0,
            muted: false,
            channel_map: Vec::new(),
        }],
    };
    let mixer = MixerPlan::new(&graph, 480, 2, 2).expect("空路由图混音计划有效");
    let mut source_block = vec![0.0f32; 960];
    let mut output_block = vec![0.0f32; 960];
    let silence_block = [0.0f32; 960];
    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds.max(1));
    let mut packets = 0u64;
    let mut captured_frames = 0u64;
    let mut fifo_overflows = 0u64;
    let mut render_writes = 0u64;
    let mut render_frames = 0u64;
    let mut discontinuities = 0u64;
    let mut timestamp_errors = 0u64;

    while Instant::now() < deadline {
        let drain = match capture.drain_packets(|packet, data| {
            packets += 1;
            captured_frames += u64::from(packet.frames);
            discontinuities += u64::from(packet.discontinuity);
            timestamp_errors += u64::from(packet.timestamp_error);
            if packet.silent {
                let samples = packet.frames as usize * 2;
                let mut written = 0;
                while written < samples {
                    let end = (written + silence_block.len()).min(samples);
                    let pushed = producer
                        .push_interleaved(&silence_block[..end - written])
                        .map(|result| result.frames() * 2)
                        .unwrap_or(0);
                    written += pushed;
                    if pushed == 0 {
                        break;
                    }
                }
                if written < samples {
                    fifo_overflows += 1;
                }
            } else if let Some(samples) = data {
                if producer
                    .push_interleaved(samples)
                    .map(|result| result.frames())
                    .unwrap_or(0)
                    < packet.frames as usize
                {
                    fifo_overflows += 1;
                }
            }
        }) {
            Ok(result) => result,
            Err(error) => exit_with_error("排空 capture packet 失败", error),
        };
        if drain.packets == 0 {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        source_block.fill(0.0);
        let read = consumer
            .pop_interleaved(&mut source_block)
            .unwrap()
            .frames();
        if read == 0 {
            continue;
        }
        let source_refs = [source_block.as_slice()];
        let mut sink_refs = [output_block.as_mut_slice()];
        mixer
            .process(&source_refs, &mut sink_refs)
            .expect("混音 block 形状固定");
        if let Ok(loopmaster_audio_windows::RenderWriteResult::Written { frames }) =
            render.write_f32_block(&output_block)
        {
            render_writes += 1;
            render_frames += u64::from(frames);
        }
    }

    println!("LoopMaster capture-render 验收");
    println!("运行时间: {} 秒", seconds.max(1));
    println!("capture packet: {packets}");
    println!("capture frames: {captured_frames}");
    println!("render writes: {render_writes}");
    println!("render frames: {render_frames}");
    println!("FIFO overflow events: {fifo_overflows}");
    println!("data discontinuity: {discontinuities}");
    println!("timestamp errors: {timestamp_errors}");
    std::process::exit(if packets > 0 && render_frames > 0 {
        0
    } else {
        2
    });
}

fn print_endpoint(index: usize, endpoint: &EndpointInfo) {
    println!();
    println!(
        "[{index}] {} ({})",
        endpoint.name,
        endpoint.flow.display_name()
    );
    println!("  ID: {}", endpoint.id.0);
    println!("  Flow: {}", endpoint.flow.as_str());
    match endpoint.endpoint_format() {
        Some(format) => {
            println!(
                "  Mix format: {} Hz, {} bit, {} channels{}",
                format.sample_rate,
                format.bits_per_sample,
                format.channels,
                if format.is_float { ", float" } else { "" }
            );
            println!("  Channel mask: 0x{:08X}", format.channel_mask);
            println!("  可用性: {}", availability_label(endpoint.flow, format));
        }
        None => {
            println!("  Mix format: unavailable");
            println!("  可用性: 未知（无法读取格式）");
        }
    }
}

fn availability_label(flow: EndpointFlow, format: EndpointFormat) -> &'static str {
    match flow {
        EndpointFlow::Capture => {
            if format.capture_compatible() {
                "可作为 capture source（48 kHz / 32-bit float / 2 声道）"
            } else {
                "不满足 capture 契约（需 48 kHz / 32-bit float / 2 声道）"
            }
        }
        EndpointFlow::Render => {
            if format.render_compatible() {
                "可作为 render sink（32-bit float / 2 声道，采样率自动重采样）"
            } else {
                "不满足 render 契约（需 32-bit float / 2 声道）"
            }
        }
    }
}

/// 峰值幅度转 dBFS 显示；静音或无效峰值显示 "silence"。
fn peak_dbfs(peak: f32) -> String {
    if peak.is_finite() && peak > 0.0 {
        format!("{:.1}", 20.0 * peak.log10())
    } else {
        "silence".to_owned()
    }
}

fn exit_with_error(context: &str, error: loopmaster_audio_windows::WindowsAudioError) -> ! {
    eprintln!("{context}: {error}");
    if let Some(hresult) = error.hresult() {
        eprintln!("HRESULT: 0x{:08X}", hresult as u32);
    }
    if let Some(endpoint_id) = error.endpoint_id() {
        eprintln!("Endpoint ID: {endpoint_id}");
    }
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_observation_counts_only_state_changes() {
        let mut observation = StateObservation::new(AudioEngineState::Running);
        observation.observe(AudioEngineState::Running);
        observation.observe(AudioEngineState::Degraded);
        observation.observe(AudioEngineState::Reconnecting);
        observation.observe(AudioEngineState::Running);
        observation.observe(AudioEngineState::Failed);
        observation.observe(AudioEngineState::Failed);

        assert_eq!(observation.first, Some(AudioEngineState::Running));
        assert_eq!(observation.last, Some(AudioEngineState::Failed));
        assert_eq!(observation.transition_count(AudioEngineState::Running), 1);
        assert_eq!(observation.transition_count(AudioEngineState::Degraded), 1);
        assert_eq!(
            observation.transition_count(AudioEngineState::Reconnecting),
            1
        );
        assert_eq!(observation.transition_count(AudioEngineState::Failed), 1);
    }
}
