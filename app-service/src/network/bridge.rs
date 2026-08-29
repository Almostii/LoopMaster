//! VBAN 音频桥接：把音频引擎的网络 FIFO 与 VBAN UDP 收发链路对接。
//!
//! 引擎为 VBAN 源/目标创建 FIFO 后，本模块持有非 WASAPI 端句柄：
//! - **接收线程**：`VBanReceiver` 绑定端口接收远端音频，把有序帧写入对应
//!   网络 source 的 FIFO producer（混音从 consumer 读取，作为普通 source）；
//! - **发送线程**：从网络 sink 的 FIFO consumer 读取混音结果，经
//!   `VBanSender::send_frame` 发送到目标节点。
//!
//! 纯网络服务层，不在音频实时路径阻塞。参考专项文档 4/5 节。

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use loopmaster_audio_core::vban::clock_drift::ClockDriftCompensator;
use loopmaster_audio_core::vban::jitter::VBanJitterBuffer;
use loopmaster_audio_core::vban::packet::{
    VBanBitFormat, VBanHeader, VBAN_HEADER_SIZE, VBAN_MAX_PACKET_SIZE,
};
use loopmaster_audio_core::{
    AudioFifoConsumer, AudioFifoProducer, RouteGraph, SinkId, SinkKind, SourceId, SourceKind,
    DEFAULT_BLOCK_FRAMES, INTERNAL_CHANNELS, INTERNAL_SAMPLE_RATE,
};
use loopmaster_audio_windows::NetworkIoHandles;

use crate::network::sender::VBanSender;

/// 桥接错误。
#[derive(Debug, thiserror::Error)]
pub enum NetworkBridgeError {
    #[error("接收端绑定失败: {0}")]
    Bind(#[from] crate::network::receiver::VBanReceiveError),
    #[error("接收 UDP 失败: {0}")]
    Udp(#[from] std::io::Error),
    #[error("发送器初始化失败: {0}")]
    SenderInit(#[from] crate::network::sender::VBanSendError),
    #[error("无网络源/目标句柄")]
    NoSources,
}

/// 一个 VBAN 接收源（网络接收 → 混音输入）。
pub struct VbanSourceBridge {
    /// 该源对应的 FIFO producer（网络接收帧写入）。
    pub producer: loopmaster_audio_core::AudioFifoProducer,
    /// 接收流名（用于从混音图匹配）。
    pub stream_name: String,
}

/// 一个 VBAN 发送目标（混音输出 → 网络发送）。
pub struct VbanSinkBridge {
    /// 该目标对应的 FIFO consumer（网络发送读取）。
    pub consumer: AudioFifoConsumer,
    /// 发送流名。
    pub stream_name: String,
    /// 目标节点地址。
    pub target: SocketAddr,
    /// 采样率。
    pub sample_rate: u32,
    /// 声道数。
    pub channels: usize,
    /// 位深。
    pub bit_format: VBanBitFormat,
    /// 每 block 帧数（发送采样点数）。
    pub block_frames: usize,
}

/// 网络桥接：启动接收/发送线程，把引擎的网络 FIFO 与 VBAN UDP 对接。
pub struct NetworkBridge {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl NetworkBridge {
    /// 从引擎暴露的网络句柄 + 路由图推导网络桥接的源/目标参数，并启动桥接。
    ///
    /// `receiver_bind` 为本机 VBAN 接收 UDP 端口。遍历句柄中的 Vban source/
    /// sink FIFO，与路由图中对应节点（按 SourceId/SinkId 匹配）的 stream_name、
    /// remote_addr 组装成 `VbanSourceBridge`/`VbanSinkBridge`，再调用
    /// [`Self::start`] 启动收发线程。
    pub fn from_handles(
        receiver_bind: SocketAddr,
        graph: &RouteGraph,
        handles: NetworkIoHandles,
    ) -> Result<Self, NetworkBridgeError> {
        let sources: Vec<VbanSourceBridge> = handles
            .vban_source_producers
            .into_iter()
            .filter_map(|(SourceId(source_id), producer)| {
                let spec = graph.sources.iter().find(|s| s.id.0 == source_id)?;
                if spec.kind != SourceKind::Vban {
                    return None;
                }
                Some(VbanSourceBridge {
                    producer,
                    stream_name: spec.stream_name.clone().unwrap_or_default(),
                })
            })
            .collect();
        let sinks: Vec<VbanSinkBridge> = handles
            .vban_sink_consumers
            .into_iter()
            .filter_map(|(SinkId(sink_id), consumer)| {
                let spec = graph.sinks.iter().find(|s| s.id.0 == sink_id)?;
                if spec.kind != SinkKind::Vban {
                    return None;
                }
                let target = spec.remote_addr.as_deref()?.parse().ok()?;
                Some(VbanSinkBridge {
                    consumer,
                    stream_name: spec.stream_name.clone().unwrap_or_default(),
                    target,
                    sample_rate: INTERNAL_SAMPLE_RATE,
                    channels: INTERNAL_CHANNELS,
                    bit_format: VBanBitFormat::Float32,
                    block_frames: DEFAULT_BLOCK_FRAMES,
                })
            })
            .collect();
        Self::start(receiver_bind, sources, sinks)
    }

    /// 创建桥接并启动接收/发送线程。
    ///
    /// `receiver_bind` 为接收 UDP 端口，`sources` 为网络源（接收帧写入），
    /// `sinks` 为网络目标（发送帧读取）。
    pub fn start(
        receiver_bind: SocketAddr,
        sources: Vec<VbanSourceBridge>,
        sinks: Vec<VbanSinkBridge>,
    ) -> Result<Self, NetworkBridgeError> {
        // 允许单边为空（如"网络输入→本机扬声器"或"本机麦克风→网络目标"的
        // 不对称拓扑），但至少一边要有节点，否则无需桥接。
        if sources.is_empty() && sinks.is_empty() {
            return Err(NetworkBridgeError::NoSources);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();

        // 接收：单个共享 UDP socket（绑定 6980），按流名分发到各流的 jitter buffer。
        if !sources.is_empty() {
            let socket = std::net::UdpSocket::bind(receiver_bind)?;
            socket.set_nonblocking(true)?;
            let stop_clone = Arc::clone(&stop);
            let streams: Vec<(String, AudioFifoProducer)> = sources
                .into_iter()
                .map(|s| (s.stream_name, s.producer))
                .collect();
            threads.push(
                thread::Builder::new()
                    .name("loopmaster-vban-recv".into())
                    .spawn(move || {
                        recv_loop(socket, streams, stop_clone);
                    })
                    .expect("创建 VBAN 接收线程失败"),
            );
        }

        // 发送线程：从每个 sink consumer 读帧并发送。
        for sink in sinks {
            let mut consumer = sink.consumer;
            let mut sender = VBanSender::new()?;
            let stop_clone = Arc::clone(&stop);
            let target = sink.target;
            let stream_name = sink.stream_name;
            let sample_rate = sink.sample_rate;
            let channels = sink.channels;
            let bit_format = sink.bit_format;
            let block_frames = sink.block_frames;
            threads.push(
                thread::Builder::new()
                    .name(format!("loopmaster-vban-send-{stream_name}"))
                    .spawn(move || {
                        let mut out = vec![0.0f32; block_frames * channels];
                        while !stop_clone.load(Ordering::Acquire) {
                            if consumer.available_frames() >= block_frames {
                                let popped = consumer
                                    .pop_interleaved(&mut out)
                                    .map(|r| r.frames())
                                    .unwrap_or(0);
                                if popped > 0 {
                                    let _ = sender.send_frame(
                                        target,
                                        &stream_name,
                                        sample_rate,
                                        channels,
                                        bit_format,
                                        &out[..popped * channels],
                                    );
                                }
                            } else {
                                thread::sleep(std::time::Duration::from_millis(1));
                            }
                        }
                    })
                    .expect("创建 VBAN 发送线程失败"),
            );
        }

        Ok(Self { stop, threads })
    }

    /// 停止所有收发线程。
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl Drop for NetworkBridge {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 共享 UDP socket 的多流接收循环。
///
/// 每个流（按 `stream_name` 匹配）持有一个独立 jitter buffer 与其混音 FIFO
/// producer。循环 `recv_from` → 解析 VBAN 头 → 按流名分发 → 入 jitter →
/// 抽帧写入对应 producer。
fn recv_loop(socket: UdpSocket, streams: Vec<(String, AudioFifoProducer)>, stop: Arc<AtomicBool>) {
    // 流名 -> (jitter, producer, 时钟漂移补偿器)。
    let mut streams: HashMap<String, (VBanJitterBuffer, AudioFifoProducer, ClockDriftCompensator)> =
        streams
            .into_iter()
            .map(|(name, producer)| {
                // 目标水位：2 个包（初始候选值，真机联调时冻结）。
                let comp = ClockDriftCompensator::new(2.0);
                (name, (VBanJitterBuffer::new(), producer, comp))
            })
            .collect();
    let mut recv_buf = vec![0u8; VBAN_MAX_PACKET_SIZE];
    let mut block = vec![0.0f32; DEFAULT_BLOCK_FRAMES * INTERNAL_CHANNELS];

    while !stop.load(Ordering::Acquire) {
        let (len, _peer) = match socket.recv_from(&mut recv_buf) {
            Ok(v) => v,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // 无数据：顺带把各流已就绪的帧写走。
                drain_all(&mut streams, &mut block);
                thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            Err(_) => {
                thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
        };
        if len < VBAN_HEADER_SIZE {
            continue;
        }
        let Ok(header) = VBanHeader::decode(&recv_buf[..len]) else {
            continue;
        };
        let Ok(stream_name) = header.stream_name_str() else {
            continue;
        };
        // 分发到已配置的接收流：
        // - 优先按流名精确匹配；
        // - 若没有精确匹配，且本机只配置了一个接收流，则降级投递给它
        //   （发送端的流名默认是"目标设备名"，而接收端期待"源设备名"，
        //    两者天然不一致；单流场景下不应因此丢弃全部音频包）；
        // - 多流且无精确匹配时才丢弃（避免串流）。
        let stream_key: Option<String> = if streams.contains_key(stream_name) {
            Some(stream_name.to_owned())
        } else if streams.len() == 1 {
            streams.keys().next().cloned()
        } else {
            None
        };
        let Some(stream_key) = stream_key else {
            continue;
        };
        if let Some((jitter, _, _)) = streams.get_mut(&stream_key) {
            // 按位深解码 payload 为 f32。
            let Ok(bit_format) = header.bit_format() else {
                continue;
            };
            let samples_per_channel = header.samples_per_channel();
            let channels = header.channels();
            let sample_count = samples_per_channel * channels;
            let expected_payload = sample_count * bit_format.bytes_per_sample();
            let payload = &recv_buf[VBAN_HEADER_SIZE..len];
            // 校验 payload 长度：恶意/损坏包头不得触发超大分配。
            if payload.len() < expected_payload {
                continue;
            }
            let mut samples = vec![0.0f32; sample_count];
            decode_vban_payload(payload, bit_format, &mut samples);
            let _ = jitter.push(header.nu_frame, &samples);
        }
        drain_all(&mut streams, &mut block);
    }
}

/// 把各流 jitter 已就绪的帧写入对应 producer（不满帧补静音到 block 长度）。
///
/// 时钟漂移：基于 jitter 当前水位（fill_level）驱动 `ClockDriftCompensator`
/// 计算采样率比例因子，输出到 stderr 便于诊断；真实的重采样对齐
/// （`FixedOutputResampler::set_sample_rates` 微调）在真机联调阶段接入，
/// 因为漂移比例需按设备实测冻结。
fn drain_all(
    streams: &mut HashMap<String, (VBanJitterBuffer, AudioFifoProducer, ClockDriftCompensator)>,
    block: &mut [f32],
) {
    for (name, (jitter, producer, comp)) in streams.iter_mut() {
        // 时钟漂移：仅当流有数据（水位 > 0）时更新补偿器并诊断，
        // 避免空闲流在每个 drain 周期刷屏；比例因子仅供诊断，不改变输出。
        if jitter.fill_level() > 0 {
            let ratio = comp.update(jitter.fill_level(), 0.1);
            if (ratio - 1.0).abs() > 1e-3 {
                eprintln!(
                    "[vban:{name}] 时钟漂移比例 {:.6} (水位 {})",
                    ratio,
                    jitter.fill_level()
                );
            }
        }
        while let Some(frame) = jitter.pop_next() {
            block.fill(0.0);
            let write_len = frame.len().min(block.len());
            block[..write_len].copy_from_slice(&frame[..write_len]);
            let _ = producer.push_interleaved(block);
        }
    }
}

/// 把 VBAN payload 按位深解码为 interleaved `f32`（-1..=1）。
fn decode_vban_payload(payload: &[u8], bit_format: VBanBitFormat, output: &mut [f32]) {
    match bit_format {
        VBanBitFormat::Float32 => {
            for (i, sample) in output.iter_mut().enumerate() {
                let bytes = payload
                    .get(i * 4..i * 4 + 4)
                    .and_then(|s| s.try_into().ok())
                    .unwrap_or(&[0u8; 4]);
                *sample = f32::from_le_bytes(*bytes);
            }
        }
        VBanBitFormat::Int32 => {
            for (i, sample) in output.iter_mut().enumerate() {
                let bytes = payload
                    .get(i * 4..i * 4 + 4)
                    .and_then(|s| s.try_into().ok())
                    .unwrap_or(&[0u8; 4]);
                *sample = i32::from_le_bytes(*bytes) as f32 / 2_147_483_647.0;
            }
        }
        VBanBitFormat::Int24 => {
            for (i, sample) in output.iter_mut().enumerate() {
                let b0 = payload.get(i * 3).copied().unwrap_or(0) as i32;
                let b1 = payload.get(i * 3 + 1).copied().unwrap_or(0) as i32;
                let b2 = payload.get(i * 3 + 2).copied().unwrap_or(0) as i32;
                let raw = b0 | (b1 << 8) | (b2 << 16);
                let signed = if b2 & 0x80 != 0 { raw - (1 << 24) } else { raw };
                *sample = signed as f32 / 8_388_607.0;
            }
        }
        VBanBitFormat::Int16 => {
            for (i, sample) in output.iter_mut().enumerate() {
                let bytes = payload
                    .get(i * 2..i * 2 + 2)
                    .and_then(|s| s.try_into().ok())
                    .unwrap_or(&[0u8; 2]);
                *sample = i16::from_le_bytes(*bytes) as f32 / 32_767.0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::receiver::VBanReceiver;
    use loopmaster_audio_core::AudioFifo;

    fn random_port() -> u16 {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    const CHANNELS: usize = 2;
    const FRAMES: usize = 64; // 每 block 帧数
    const BLOCK_SAMPLES: usize = FRAMES * CHANNELS;

    /// 自环集成测试：验证 bridge 的接收路径（网络帧 → source FIFO）与
    /// 发送路径（sink FIFO → 网络发送）数据流通畅，无需 WASAPI/真实设备。
    #[test]
    fn bridge_relays_audio_between_network_and_fifo() {
        // 1) source FIFO：bridge 接收线程写入 producer，测试读 consumer。
        let (source_producer, mut source_consumer) =
            AudioFifo::split(FRAMES * 8, CHANNELS).unwrap();
        // 2) sink FIFO：测试写 producer，bridge 发送线程读 consumer。
        let (mut sink_producer, sink_consumer) = AudioFifo::split(FRAMES * 8, CHANNELS).unwrap();

        let recv_port = random_port();
        let recv_addr: SocketAddr = format!("127.0.0.1:{recv_port}").parse().unwrap();
        let send_port = random_port();
        let send_addr: SocketAddr = format!("127.0.0.1:{send_port}").parse().unwrap();

        // 3) 启动 bridge。
        let mut bridge = NetworkBridge::start(
            recv_addr,
            vec![VbanSourceBridge {
                producer: source_producer,
                stream_name: "In".to_owned(),
            }],
            vec![VbanSinkBridge {
                consumer: sink_consumer,
                stream_name: "Out".to_owned(),
                target: send_addr,
                sample_rate: 48_000,
                channels: CHANNELS,
                bit_format: VBanBitFormat::Float32,
                block_frames: FRAMES,
            }],
        )
        .unwrap();

        // 4) 发送合成音到接收端口，验证 bridge 接收写入 source FIFO。
        let mut sender = VBanSender::new().unwrap();
        let test_samples: Vec<f32> = (0..BLOCK_SAMPLES)
            .map(|i| ((i % 17) as f32 / 17.0) - 0.5)
            .collect();
        let _ = sender
            .send_frame(
                recv_addr,
                "In",
                48_000,
                CHANNELS,
                VBanBitFormat::Float32,
                &test_samples,
            )
            .unwrap();

        // 等待 bridge 接收并把帧写入 source FIFO。
        let mut received: Vec<f32> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while received.len() < BLOCK_SAMPLES && std::time::Instant::now() < deadline {
            let mut buf = vec![0.0f32; BLOCK_SAMPLES];
            if source_consumer
                .pop_interleaved(&mut buf)
                .map(|r| r.frames())
                .unwrap_or(0)
                > 0
            {
                received.extend_from_slice(&buf);
            } else {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        assert!(
            received.len() >= BLOCK_SAMPLES,
            "bridge 应把接收的网络帧写入 source FIFO"
        );
        // Float32 往返应一致。
        assert!(received[..BLOCK_SAMPLES]
            .iter()
            .zip(&test_samples)
            .all(|(a, b)| (a - b).abs() < 1e-5));

        // 5) 写 sink FIFO，验证 bridge 发送线程回发到 send_addr。
        let outbound = test_samples.clone();
        sink_producer.push_interleaved(&outbound).unwrap();

        // 用一个接收器收 bridge 回发的音频。
        let mut back_receiver = VBanReceiver::bind(send_addr).unwrap();
        let mut echoed: Vec<f32> = Vec::new();
        let deadline2 = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while echoed.len() < BLOCK_SAMPLES && std::time::Instant::now() < deadline2 {
            let _ = back_receiver.recv_and_push();
            while let Some(frame) = back_receiver.pop_next() {
                echoed.extend_from_slice(&frame);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            echoed.len() >= BLOCK_SAMPLES,
            "bridge 应把 sink FIFO 的帧经网络发送出去"
        );
        assert!(echoed[..BLOCK_SAMPLES]
            .iter()
            .zip(&outbound)
            .all(|(a, b)| (a - b).abs() < 1e-5));

        bridge.shutdown();
    }

    /// 从 NetworkIoHandles + 路由图推导并启动桥接（含 Vban 源/目标）。
    #[test]
    fn from_handles_builds_bridge_from_graph() {
        use loopmaster_audio_core::{
            BusId, BusSpec, EndpointId, RouteGraph, SendId, SendSpec, SinkId, SinkSpec, SourceId,
            SourceKind, SourceSpec,
        };

        // 构造含一个 Vban source + 一个 Vban sink 的路由图。
        let graph = RouteGraph {
            sources: vec![SourceSpec {
                id: SourceId("net-in".into()),
                kind: SourceKind::Vban,
                endpoint_id: None,
                process_id: None,
                executable_path: None,
                stream_name: Some("In".into()),
                display_name: "网络输入".into(),
            }],
            buses: vec![BusSpec {
                id: BusId("mix".into()),
                display_name: "Mix".into(),
            }],
            sinks: vec![SinkSpec {
                id: SinkId("net-out".into()),
                endpoint_id: EndpointId("vban".into()),
                display_name: "网络输出".into(),
                kind: loopmaster_audio_core::SinkKind::Vban,
                stream_name: Some("Out".into()),
                remote_addr: Some("127.0.0.1:6999".into()),
            }],
            sends: vec![
                SendSpec::SourceToBus {
                    id: SendId("s1".into()),
                    source_id: SourceId("net-in".into()),
                    bus_id: BusId("mix".into()),
                    gain_db: 0.0,
                    muted: false,
                    enabled: true,
                    channel_map: Vec::new(),
                },
                SendSpec::BusToSink {
                    id: SendId("s2".into()),
                    bus_id: BusId("mix".into()),
                    sink_id: SinkId("net-out".into()),
                    gain_db: 0.0,
                    muted: false,
                    enabled: true,
                    channel_map: Vec::new(),
                },
            ],
        };
        graph.validate().unwrap();

        // 构造网络句柄：一个 Vban source producer + 一个 Vban sink consumer。
        let (source_producer, _source_consumer) = AudioFifo::split(480 * 8, 2).unwrap();
        let (_sink_producer, sink_consumer) = AudioFifo::split(480 * 8, 2).unwrap();
        let handles = NetworkIoHandles {
            vban_source_producers: vec![(SourceId("net-in".into()), source_producer)],
            vban_sink_consumers: vec![(SinkId("net-out".into()), sink_consumer)],
        };

        let recv_port = random_port();
        let receiver_bind: SocketAddr = format!("127.0.0.1:{recv_port}").parse().unwrap();
        let mut bridge = NetworkBridge::from_handles(receiver_bind, &graph, handles).unwrap();
        // 成功启动后清理。
        bridge.shutdown();
    }
}
