//! VBAN 音频桥接：把音频引擎的网络 FIFO 与 VBAN UDP 收发链路对接。
//!
//! 引擎为 VBAN 源/目标创建 FIFO 后，本模块持有非 WASAPI 端句柄：
//! - **接收线程**：`VBanReceiver` 绑定端口接收远端音频，把有序帧写入对应
//!   网络 source 的 FIFO producer（混音从 consumer 读取，作为普通 source）；
//! - **发送线程**：从网络 sink 的 FIFO consumer 读取混音结果，经
//!   `VBanSender::send_frame` 发送到目标节点。
//!
//! 纯网络服务层，不在音频实时路径阻塞。参考专项文档 4/5 节。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use loopmaster_audio_core::vban::packet::VBanBitFormat;
use loopmaster_audio_core::{
    AudioFifoConsumer, RouteGraph, SinkId, SinkKind, SourceId, SourceKind, DEFAULT_BLOCK_FRAMES,
    INTERNAL_CHANNELS, INTERNAL_SAMPLE_RATE,
};
use loopmaster_audio_windows::NetworkIoHandles;

use crate::network::receiver::VBanReceiver;
use crate::network::sender::VBanSender;

/// 桥接错误。
#[derive(Debug, thiserror::Error)]
pub enum NetworkBridgeError {
    #[error("接收端绑定失败: {0}")]
    Bind(#[from] crate::network::receiver::VBanReceiveError),
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
    /// 每帧样本数（samples_per_channel × channels）。
    pub frame_samples: usize,
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
        let frame_samples = DEFAULT_BLOCK_FRAMES * INTERNAL_CHANNELS;
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
                    frame_samples,
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

        // 接收线程：每个源一个独立接收器（按源绑定）。
        for source in sources {
            let mut receiver = VBanReceiver::bind(receiver_bind, source.frame_samples)?;
            let mut producer = source.producer;
            let stop_clone = Arc::clone(&stop);
            threads.push(
                thread::Builder::new()
                    .name(format!("loopmaster-vban-recv-{}", source.stream_name))
                    .spawn(move || {
                        let mut out = vec![0.0f32; source.frame_samples];
                        while !stop_clone.load(Ordering::Acquire) {
                            let _ = receiver.recv_and_push();
                            while let Some(frame) = receiver.pop_next() {
                                // 写入混音 source FIFO；不满帧补静音。
                                out.fill(0.0);
                                let write_len = frame.len().min(out.len());
                                out[..write_len].copy_from_slice(&frame[..write_len]);
                                let _ = producer.push_interleaved(&out);
                            }
                            if receiver.fill_level() == 0 {
                                thread::sleep(std::time::Duration::from_millis(1));
                            }
                        }
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
                frame_samples: BLOCK_SAMPLES,
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
        let mut back_receiver = VBanReceiver::bind(send_addr, BLOCK_SAMPLES).unwrap();
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
