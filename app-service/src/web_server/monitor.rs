//! 网页端无线监听管线（Phase 6.3，任务书 S1-3/S1-4）。
//!
//! 数据路径：引擎 MonitorTap（丢旧 FIFO）→ 本模块编码任务（20 ms Opus）
//! → webrtc-rs `TrackLocalStaticSample` → 浏览器 `RTCPeerConnection`。
//!
//! 生命周期（S1-4 / S2-2）：
//! - `monitor_start` 订阅抽头并分配 `peer_id`（未协商前不创建编码分支与 PC）；
//! - `webrtc_offer` 协商完成后启动编码任务，answer（含 host 候选）返回给调用方；
//! - `monitor_stop` / 控制连接断开 / 服务关闭时立即回收 PC 与编码任务；
//! - 全局并发上限 [`MAX_MONITOR_PEERS`]，超限返回结构化错误。
//!
//! 实时边界：本模块全部运行在 web runtime（非实时线程），从丢旧 FIFO
//! 单消费者读取，与引擎实时线程只通过预分配 FIFO 交互。

use loopmaster_audio_windows::MonitorTapReader;
use rand::Rng;
use rtc::interceptor::Registry;
use rtc::media::Sample as RtcSample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::MediaEngine;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::RTCIceCandidateInit;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};
use webrtc::runtime::{channel, Sender};

/// Opus 静态 payload type（媒体引擎注册值，SDP 协商沿用）。
pub const PAYLOAD_TYPE_OPUS: u8 = 111;
/// 默认 Opus 码率（任务书 §4 候选值：96 kbps stereo）。
pub const DEFAULT_OPUS_BITRATE_BPS: u32 = 96_000;
/// 全局并发监听上限（任务书 §4 候选值：2）。
pub const MAX_MONITOR_PEERS: usize = 2;
/// 等 ICE gathering 完成的上限（host-only 通常 < 1s；超时直接用当前 SDP）。
const GATHER_TIMEOUT: Duration = Duration::from_secs(3);

/// 监听管线错误（结构化，映射为 `/ws` 错误码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorError {
    /// 请求的 bus 不存在（可能路由图已变更或引擎未运行）。
    UnknownBus,
    /// 并发监听已达上限。
    LimitReached,
    /// peer 不存在或不属于当前连接。
    UnknownPeer,
    /// WebRTC/编码内部错误。
    Internal(String),
}

impl std::fmt::Display for MonitorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MonitorError::UnknownBus => write!(f, "请求的输出通道不存在"),
            MonitorError::LimitReached => write!(f, "监听连接数已达上限"),
            MonitorError::UnknownPeer => write!(f, "监听会话不存在"),
            MonitorError::Internal(message) => write!(f, "监听内部错误: {message}"),
        }
    }
}

impl MonitorError {
    /// 机器可读错误码（`/ws` error 响应的 `code` 字段）。
    pub const fn code(&self) -> &'static str {
        match self {
            MonitorError::UnknownBus => "unknown_bus",
            MonitorError::LimitReached => "monitor_limit_reached",
            MonitorError::UnknownPeer => "unknown_peer",
            MonitorError::Internal(_) => "monitor_internal",
        }
    }
}

// ---------------------------------------------------------------------------
// Opus 编码器（48 kHz / stereo / 20 ms，audiopus/libopus 1.3）
// ---------------------------------------------------------------------------

/// 每帧 20 ms 的单声道样本数（48 kHz）。
const SAMPLES_PER_CHANNEL_20MS: usize = 960;

/// Opus 编码器封装。DTX：libopus 默认关闭，与任务书"DTX 关闭"一致。
struct OpusEncoder {
    core: audiopus::coder::Encoder,
    frame_samples: usize,
    out_buf: Vec<u8>,
}

impl OpusEncoder {
    fn new(bitrate_bps: u32) -> Result<Self, audiopus::Error> {
        let mut core = audiopus::coder::Encoder::new(
            audiopus::SampleRate::Hz48000,
            audiopus::Channels::Stereo,
            audiopus::Application::Audio,
        )?;
        core.set_bitrate(audiopus::Bitrate::BitsPerSecond(bitrate_bps as i32))?;
        Ok(Self {
            core,
            frame_samples: SAMPLES_PER_CHANNEL_20MS * 2,
            out_buf: vec![0u8; 4_000],
        })
    }

    /// 编码一帧 20 ms interleaved stereo f32，包写入 `out`，返回字节数。
    fn encode_f32(&mut self, pcm: &[f32], out: &mut Vec<u8>) -> Result<usize, audiopus::Error> {
        debug_assert_eq!(pcm.len(), self.frame_samples);
        let mut scratch: Vec<i16> = Vec::with_capacity(self.frame_samples);
        for src in pcm {
            let v = (*src * 32_767.0).round();
            scratch.push(v.clamp(-32_768.0, 32_767.0) as i16);
        }
        let n = self.core.encode(&scratch, &mut self.out_buf)?;
        out.clear();
        out.extend_from_slice(&self.out_buf[..n]);
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// 单 peer 编码任务
// ---------------------------------------------------------------------------

/// 编码任务句柄：Drop 即 abort，随会话回收。
struct PeerPipeline {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for PeerPipeline {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn spawn_pipeline(
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    reader: MonitorTapReader,
    bitrate_bps: u32,
) -> PeerPipeline {
    let task = tokio::spawn(async move {
        let mut enc = match OpusEncoder::new(bitrate_bps) {
            Ok(enc) => enc,
            Err(e) => {
                eprintln!("[monitor] Opus 编码器创建失败：{e}");
                return;
            }
        };
        let mut frame = vec![0f32; enc.frame_samples];
        let mut packet: Vec<u8> = Vec::with_capacity(1_500);
        let mut filled = 0usize;
        let mut encoded_frames: u64 = 0;
        loop {
            if filled < frame.len() {
                filled += reader.pop(&mut frame[filled..]);
                if filled < frame.len() {
                    // FIFO 数据不足：短暂等待（丢旧 FIFO 保证永不阻塞）
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    continue;
                }
            }
            filled = 0;
            match enc.encode_f32(&frame, &mut packet) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[monitor] Opus 编码失败：{e}");
                    return;
                }
            }
            let sample = RtcSample {
                data: bytes::Bytes::copy_from_slice(&packet),
                duration: Duration::from_millis(20),
                ..Default::default()
            };
            if let Err(e) = track
                .write_sample(ssrc, PAYLOAD_TYPE_OPUS, &sample, &[])
                .await
            {
                eprintln!("[monitor] 写轨失败（连接已关闭）：{e}");
                return;
            }
            encoded_frames += 1;
            if encoded_frames.is_multiple_of(500) {
                eprintln!("[monitor] peer 已编码 {encoded_frames} 帧（10s）");
            }
        }
    });
    PeerPipeline { task }
}

// ---------------------------------------------------------------------------
// PeerConnection 事件 handler（转发候选 + 等 gathering 完成）
// ---------------------------------------------------------------------------

struct PeerHandler {
    /// 下行 ICE 候选出口（webrtc_answer 之后浏览器仍可收到 trickle 候选）。
    ice_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<RTCIceCandidateInit>>>,
    gather_complete_tx: Sender<()>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for PeerHandler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gather_complete_tx.try_send(());
        }
    }

    async fn on_ice_candidate(
        &self,
        event: rtc::peer_connection::event::RTCPeerConnectionIceEvent,
    ) {
        let candidate = event.candidate;
        // 日志脱敏（S2-5）：只记录候选类型，不记录候选内容。
        eprintln!("[monitor] 本地候选类型: {:?}", candidate.typ);
        let Ok(init) = candidate.to_json() else {
            return;
        };
        if let Some(tx) = self.ice_tx.lock().expect("ice 出口锁未中毒").as_ref() {
            let _ = tx.send(init);
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        eprintln!("[monitor] PC 状态: {state:?}");
    }
}

// ---------------------------------------------------------------------------
// 会话与服务
// ---------------------------------------------------------------------------

struct PeerState {
    owner: u64,
    #[allow(dead_code)]
    bus_id: String,
    /// `monitor_start` 即订阅抽头（读端持有期间引擎侧保持拷贝激活）；
    /// 协商完成时移交编码任务。
    reader: Option<MonitorTapReader>,
    pc: Option<Arc<dyn PeerConnection>>,
    pipeline: Option<PeerPipeline>,
}

impl PeerState {
    /// 回收 PC（编码任务随 pipeline Drop abort、订阅随 reader Drop 递减）。
    async fn close(&mut self) {
        self.pipeline.take();
        if let Some(pc) = self.pc.take() {
            let _ = pc.close().await;
        }
    }
}

/// 监听服务：全局 peer 表（含归属连接校验）。
///
/// 抽头注册表托管在 [`StateHub`](super::super::StateHub)（由壳层的
/// monitor-tap pump 线程随引擎 session 重建写入），本服务按 bus_id 实时查。
pub struct MonitorService {
    hub: Arc<crate::state::StateHub>,
    inner: Mutex<Inner>,
    next_peer_id: AtomicU64,
    next_owner_id: AtomicU64,
    bitrate_bps: u32,
}

struct Inner {
    peers: HashMap<u64, PeerState>,
}

impl MonitorService {
    pub fn new(hub: Arc<crate::state::StateHub>) -> Arc<Self> {
        Arc::new(Self {
            hub,
            inner: Mutex::new(Inner {
                peers: HashMap::new(),
            }),
            next_peer_id: AtomicU64::new(1),
            next_owner_id: AtomicU64::new(1),
            bitrate_bps: DEFAULT_OPUS_BITRATE_BPS,
        })
    }

    /// 分配新的连接归属 id（ws 连接建立时调用）。
    pub fn new_owner(&self) -> u64 {
        self.next_owner_id.fetch_add(1, Ordering::Relaxed)
    }

    /// 当前可监听的 bus id 列表（initial_state 的 monitor_available）。
    pub fn available_buses(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .hub
            .monitor_taps()
            .into_iter()
            .map(|(bus, _)| bus.0)
            .collect();
        ids.sort();
        ids
    }

    /// S2-1 `monitor_start`：校验 bus 与并发上限，订阅抽头，返回 peer_id。
    pub fn start_peer(&self, owner: u64, bus_id: &str) -> Result<u64, MonitorError> {
        let mut inner = self.lock();
        if inner.peers.len() >= MAX_MONITOR_PEERS {
            return Err(MonitorError::LimitReached);
        }
        let endpoint = self
            .hub
            .monitor_tap(bus_id)
            .ok_or(MonitorError::UnknownBus)?;
        let reader = endpoint.subscribe();
        let peer_id = self.next_peer_id.fetch_add(1, Ordering::Relaxed);
        inner.peers.insert(
            peer_id,
            PeerState {
                owner,
                bus_id: bus_id.to_owned(),
                reader: Some(reader),
                pc: None,
                pipeline: None,
            },
        );
        Ok(peer_id)
    }

    /// S2-1 `webrtc_offer`：协商并启动编码分支，返回 answer SDP（含 host 候选）。
    ///
    /// `ice_tx` 用于后续服务端 trickle 候选下发；协商前收到的浏览器候选
    /// 由调用方缓存并在协商完成后补投。
    pub async fn offer_peer(
        &self,
        owner: u64,
        peer_id: u64,
        offer_sdp: String,
        ice_tx: tokio::sync::mpsc::UnboundedSender<RTCIceCandidateInit>,
    ) -> Result<String, MonitorError> {
        let (reader, bitrate_bps) = {
            let mut inner = self.lock();
            let peer = inner
                .peers
                .get_mut(&peer_id)
                .filter(|peer| peer.owner == owner)
                .ok_or(MonitorError::UnknownPeer)?;
            if peer.pc.is_some() {
                return Err(MonitorError::Internal("peer 已协商".into()));
            }
            (
                peer.reader
                    .take()
                    .ok_or(MonitorError::Internal("peer 已协商".into()))?,
                self.bitrate_bps,
            )
        };

        let (gather_tx, mut gather_rx) = channel::<()>(1);
        let pc = build_peer_connection(ice_tx, gather_tx)
            .await
            .map_err(|e| MonitorError::Internal(format!("创建 PeerConnection 失败: {e}")))?;
        let ssrc: u32 = rand::rng().random();
        let track = Arc::new(
            TrackLocalStaticSample::new(MediaStreamTrack::new(
                "loopmaster-monitor".to_owned(),
                "loopmaster-audio".to_owned(),
                "loopmaster-audio".to_owned(),
                RtpCodecKind::Audio,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(ssrc),
                        ..Default::default()
                    },
                    codec: opus_codec(),
                    ..Default::default()
                }],
            ))
            .map_err(|e| MonitorError::Internal(format!("创建音轨失败: {e}")))?,
        );
        let track_dyn: Arc<dyn TrackLocal> = track.clone();
        pc.add_track(track_dyn)
            .await
            .map_err(|e| MonitorError::Internal(format!("add_track 失败: {e}")))?;

        let offer = RTCSessionDescription::offer(offer_sdp)
            .map_err(|e| MonitorError::Internal(format!("offer SDP 无效: {e}")))?;
        pc.set_remote_description(offer)
            .await
            .map_err(|e| MonitorError::Internal(format!("set_remote_description 失败: {e}")))?;
        let answer = pc
            .create_answer(None)
            .await
            .map_err(|e| MonitorError::Internal(format!("create_answer 失败: {e}")))?;
        pc.set_local_description(answer)
            .await
            .map_err(|e| MonitorError::Internal(format!("set_local_description 失败: {e}")))?;
        // 非 trickle：等 gathering 完成让 answer 携带全部 host 候选。
        // 超时则用当前 SDP（浏览器仍可经 trickle 收到后续候选）。
        let _ = tokio::time::timeout(GATHER_TIMEOUT, gather_rx.recv()).await;
        let local = pc
            .local_description()
            .await
            .ok_or_else(|| MonitorError::Internal("local description 缺失".into()))?;

        {
            let mut inner = self.lock();
            let peer = inner
                .peers
                .get_mut(&peer_id)
                .filter(|peer| peer.owner == owner)
                .ok_or(MonitorError::UnknownPeer)?;
            peer.pipeline = Some(spawn_pipeline(track, ssrc, reader, bitrate_bps));
            peer.pc = Some(Arc::clone(&pc));
        }
        Ok(local.sdp)
    }

    /// S2-1 `webrtc_ice_candidate`：投递浏览器候选。
    pub async fn add_ice(
        &self,
        owner: u64,
        peer_id: u64,
        candidate: RTCIceCandidateInit,
    ) -> Result<(), MonitorError> {
        let pc = {
            let inner = self.lock();
            let peer = inner
                .peers
                .get(&peer_id)
                .filter(|peer| peer.owner == owner)
                .ok_or(MonitorError::UnknownPeer)?;
            peer.pc.clone()
        };
        let Some(pc) = pc else {
            // 尚未协商：任务书允许候选先于 answer 到达的场景由调用方缓存；
            // 这里对"协商前候选"返回 UnknownPeer 的上层语义不合适，直接忽略。
            return Ok(());
        };
        pc.add_ice_candidate(candidate)
            .await
            .map_err(|e| MonitorError::Internal(format!("add_ice_candidate 失败: {e}")))
    }

    /// S2-1 `monitor_stop`：回收指定 peer（校验归属连接）。
    pub async fn stop_peer(&self, owner: u64, peer_id: u64) -> Result<(), MonitorError> {
        // 先校验归属再摘除（锁内不跨 await），锁外关闭 PC。
        // 注意：不能先 remove 后 filter——错误归属的调用不得删除 peer。
        let peer = {
            let mut inner = self.lock();
            match inner.peers.get(&peer_id) {
                Some(peer) if peer.owner == owner => inner.peers.remove(&peer_id),
                _ => return Err(MonitorError::UnknownPeer),
            }
        };
        let Some(mut peer) = peer else {
            return Err(MonitorError::UnknownPeer);
        };
        peer.close().await;
        Ok(())
    }

    /// 控制连接断开：回收该连接拥有的全部 peer（S2-2）。
    pub async fn close_owner(&self, owner: u64) {
        let ids: Vec<u64> = {
            let inner = self.lock();
            inner
                .peers
                .iter()
                .filter(|(_, peer)| peer.owner == owner)
                .map(|(id, _)| *id)
                .collect()
        };
        for id in ids {
            let _ = self.stop_peer(owner, id).await;
        }
    }

    /// 服务关闭（web server 停止 / 网络开关关闭）：回收全部 peer（S1-5）。
    pub async fn shutdown(&self) {
        let ids: Vec<u64> = { self.lock().peers.keys().cloned().collect() };
        for id in ids {
            // 锁内只摘除，锁外关闭（MutexGuard 不跨 await）。
            let peer = self.lock().peers.remove(&id);
            if let Some(mut peer) = peer {
                peer.close().await;
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("monitor 服务锁未中毒")
    }
}

fn opus_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: "audio/opus".to_owned(),
        clock_rate: 48_000,
        channels: 2,
        sdp_fmtp_line: "minptime=20;stereo=1;sprop-stereo=1".to_owned(),
        rtcp_feedback: vec![],
    }
}

/// 构建 ice_servers 为空（仅 host candidates）的 PeerConnection，
/// 绑定本机全部接口（UDP 0.0.0.0:0）。
async fn build_peer_connection(
    ice_tx: tokio::sync::mpsc::UnboundedSender<RTCIceCandidateInit>,
    gather_tx: Sender<()>,
) -> Result<Arc<dyn PeerConnection>, Box<dyn std::error::Error + Send + Sync>> {
    let mut media_engine = MediaEngine::default();
    media_engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: opus_codec(),
            payload_type: PAYLOAD_TYPE_OPUS,
        },
        RtpCodecKind::Audio,
    )?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;
    let config = rtc::peer_connection::configuration::RTCConfigurationBuilder::new()
        .with_ice_servers(vec![])
        .build();
    let handler = Arc::new(PeerHandler {
        ice_tx: Mutex::new(Some(ice_tx)),
        gather_complete_tx: gather_tx,
    });
    let pc: Arc<dyn PeerConnection> = Arc::new(
        PeerConnectionBuilder::new()
            .with_configuration(config)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_handler(handler)
            .with_udp_addrs(vec!["0.0.0.0:0"])
            .build()
            .await?,
    );
    Ok(pc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateHub;
    use std::path::PathBuf;

    fn test_service() -> (Arc<StateHub>, Arc<MonitorService>) {
        let dir = std::env::temp_dir().join("lm-monitor-test");
        std::fs::create_dir_all(&dir).unwrap();
        let hub = Arc::new(StateHub::new(dir.join("config.json")));
        let service = MonitorService::new(Arc::clone(&hub));
        (hub, service)
    }

    #[test]
    fn encoder_produces_packets_and_dtx_stays_off() {
        let mut enc = OpusEncoder::new(DEFAULT_OPUS_BITRATE_BPS).unwrap();
        let frame = vec![0.25f32; enc.frame_samples];
        let mut out = Vec::new();
        let n = enc.encode_f32(&frame, &mut out).unwrap();
        assert!(n > 0 && n < 1_300);
        let silence = vec![0.0f32; enc.frame_samples];
        assert!(enc.encode_f32(&silence, &mut out).unwrap() > 0);
    }

    #[test]
    fn monitor_error_codes_are_stable() {
        assert_eq!(MonitorError::UnknownBus.code(), "unknown_bus");
        assert_eq!(MonitorError::LimitReached.code(), "monitor_limit_reached");
        assert_eq!(MonitorError::UnknownPeer.code(), "unknown_peer");
        assert_eq!(
            MonitorError::Internal("x".into()).code(),
            "monitor_internal"
        );
    }

    #[tokio::test]
    async fn start_peer_validates_bus_and_limit() {
        let (hub, service) = test_service();
        assert_eq!(
            service.start_peer(1, "bus-a"),
            Err(MonitorError::UnknownBus)
        );
        hub.set_monitor_taps(vec![(
            loopmaster_audio_core::BusId("bus-a".into()),
            loopmaster_audio_windows::MonitorTapEndpoint::new().unwrap(),
        )]);
        let id1 = service.start_peer(1, "bus-a").unwrap();
        let id2 = service.start_peer(2, "bus-a").unwrap();
        assert_eq!(
            service.start_peer(3, "bus-a"),
            Err(MonitorError::LimitReached)
        );
        assert_eq!(service.available_buses(), vec!["bus-a".to_string()]);
        // 归属校验：他人不能停别人的 peer
        assert_eq!(
            service.stop_peer(9, id1).await,
            Err(MonitorError::UnknownPeer)
        );
        service.stop_peer(1, id1).await.unwrap();
        service.stop_peer(2, id2).await.unwrap();
        assert_eq!(service.available_buses(), vec!["bus-a".to_string()]);
    }

    #[tokio::test]
    async fn tap_subscription_activates_copy_and_reader_receives() {
        let endpoint = loopmaster_audio_windows::MonitorTapEndpoint::new().unwrap();
        assert_eq!(endpoint.subscriber_count(), 0);
        // 无订阅时零拷贝（写入直接跳过）
        assert_eq!(endpoint.write_bus_block(&[0.0f32; 960]), 0);
        let reader = endpoint.subscribe();
        assert_eq!(endpoint.subscriber_count(), 1);
        // 有订阅时写入并读回
        let block: Vec<f32> = (0..960).map(|i| i as f32).collect();
        assert_eq!(endpoint.write_bus_block(&block), 0);
        assert_eq!(reader.available_samples(), 960);
        let mut out = vec![0.0f32; 960];
        assert_eq!(reader.pop(&mut out), 960);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[959], 959.0);
        drop(reader);
        assert_eq!(endpoint.subscriber_count(), 0);
        let _ = PathBuf::new(); // 保持导入
    }
}
