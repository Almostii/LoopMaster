use loopmaster_audio_core::{
    AudioFifo, EndpointId, MixerPlan, RouteGraph, SendSpec, SinkId, SinkSpec, SourceId, SourceKind,
    SourceSpec,
};
use loopmaster_audio_windows::{EndpointInfo, WindowsAudioBackend};
use std::env;
use std::time::{Duration, Instant};

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
    if args.len() >= 3 {
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
