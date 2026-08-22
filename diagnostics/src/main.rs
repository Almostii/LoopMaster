use loopmaster_audio_windows::{EndpointInfo, WindowsAudioBackend};

fn main() {
    let backend = match WindowsAudioBackend::new() {
        Ok(backend) => backend,
        Err(error) => exit_with_error("初始化 Windows 音频后端失败", error),
    };

    let endpoints = match backend.enumerate_endpoints() {
        Ok(endpoints) => endpoints,
        Err(error) => exit_with_error("枚举 Windows 音频 endpoint 失败", error),
    };

    println!("LoopMaster Windows endpoint diagnostics");
    println!("发现 {} 个 active endpoint", endpoints.len());
    for (index, endpoint) in endpoints.iter().enumerate() {
        print_endpoint(index + 1, endpoint);
    }
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
