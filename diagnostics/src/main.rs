use loopmaster_audio_core::{
    AudioFifo, EndpointId, MixerPlan, RouteGraph, RouteGraphSnapshot, SendSpec, SinkId, SinkSpec,
    SourceId, SourceKind, SourceSpec,
};
use loopmaster_audio_windows::{
    AudioEngine, AudioEngineConfig, AudioEngineState, EndpointInfo, WindowsAudioBackend,
};
use std::env;
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
    if args.get(1).map(String::as_str) == Some("--engine") && args.len() >= 4 {
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
                "实时统计: state={} | capture packet={} | render writes={} | FIFO underflow={} | discontinuity={} | reconnect attempts={}",
                status.state.as_str(),
                status.stats.capture_packets,
                status.stats.render_writes,
                status.stats.fifo_underflows,
                status.stats.discontinuities,
                status.stats.reconnect_attempts
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
                "  Mix format: {} Hz, {} bit, {} channels",
                format.sample_rate, format.bits_per_sample, format.channels
            );
            println!("  Channel mask: 0x{:08X}", format.channel_mask);
        }
        None => println!("  Mix format: unavailable"),
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
