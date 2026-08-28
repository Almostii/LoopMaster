//! VBAN 音频接收链路。
//!
//! 从 UDP 接收 VBAN Audio 数据包，校验头部、按位深解码 PCM，送入
//! [`VBanJitterBuffer`] 去抖，再按序抽取 interleaved `f32` 帧。
//! 纯网络服务层，不触碰音频引擎实时线程。
//!
//! 参考：[VBAN 局域网音频互通与传输方案]
//! （../../../../Doc/网络传输与本地节点互通方案计划/1.VBAN局域网音频互通与传输方案.md）4 节。

use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use loopmaster_audio_core::vban::jitter::{VBanJitterBuffer, VBanJitterError};
use loopmaster_audio_core::vban::packet::{
    VBanBitFormat, VBanHeader, VBanPacketError, VBAN_HEADER_SIZE, VBAN_MAX_PACKET_SIZE,
};

/// 接收错误。
#[derive(Debug, thiserror::Error)]
pub enum VBanReceiveError {
    #[error("UDP 接收失败: {0}")]
    Udp(std::io::Error),
    #[error("数据包解析失败: {0}")]
    Packet(#[from] VBanPacketError),
    #[error("包长非法：实际 {actual}，期望 {expected}")]
    InvalidLength { expected: usize, actual: usize },
    #[error("Jitter Buffer 错误: {0}")]
    Jitter(#[from] VBanJitterError),
    #[error("接收缓冲建立失败: {0}")]
    JitterInit(String),
}

/// 接收统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VBanReceiveStats {
    /// 成功解析并送入 Jitter Buffer 的包数。
    pub received: u64,
    /// 魔数错误的包数。
    pub bad_magic: u64,
    /// 过短（不足头部）的包数。
    pub packet_too_short: u64,
    /// 其它解析失败的包数。
    pub parse_error: u64,
    /// 从 Jitter Buffer 成功抽取的帧数。
    pub frames_out: u64,
}

/// VBAN 音频接收器。
///
/// 绑定单个接收端口，维护一个 Jitter Buffer 与统计。`recv_and_push` 接收并
/// 解析一个包（非阻塞），`pop_next` 抽取下一个有序帧。由上层（后台线程）循环驱动。
pub struct VBanReceiver {
    socket: UdpSocket,
    /// 按 nu_frame 去抖的帧缓冲。
    jitter: VBanJitterBuffer,
    /// 预分配的接收缓冲（单包上限）。
    recv_buf: Vec<u8>,
    /// 统计。
    stats: VBanReceiveStats,
}

impl VBanReceiver {
    /// 创建接收器并绑定端口。
    ///
    /// `frame_samples` 为期望的每帧样本数（`samples_per_channel × channels`），
    /// 用于构造 Jitter Buffer。Jitter Buffer 拒绝样本数不匹配的包。
    pub fn bind(bind_addr: SocketAddr, frame_samples: usize) -> Result<Self, VBanReceiveError> {
        let socket = UdpSocket::bind(bind_addr).map_err(VBanReceiveError::Udp)?;
        socket
            .set_nonblocking(true)
            .map_err(VBanReceiveError::Udp)?;
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .map_err(VBanReceiveError::Udp)?;
        let jitter = VBanJitterBuffer::new(frame_samples)
            .map_err(|e| VBanReceiveError::JitterInit(e.to_string()))?;
        Ok(Self {
            socket,
            jitter,
            recv_buf: vec![0u8; VBAN_MAX_PACKET_SIZE],
            stats: VBanReceiveStats::default(),
        })
    }

    /// 接收 socket 实际绑定的地址（端口 0 时由系统分配）。
    pub fn local_addr(&self) -> Result<SocketAddr, VBanReceiveError> {
        self.socket.local_addr().map_err(VBanReceiveError::Udp)
    }

    /// 接收并解析下一个 VBAN 包（非阻塞）。
    ///
    /// - `Ok(true)`：成功解析并送入 Jitter Buffer；
    /// - `Ok(false)`：当前无数据（超时/非阻塞空）；
    /// - `Err`：UDP 错误。
    pub fn recv_and_push(&mut self) -> Result<bool, VBanReceiveError> {
        let (len, _peer) = match self.socket.recv_from(&mut self.recv_buf) {
            Ok(v) => v,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Ok(false);
            }
            Err(e) => return Err(VBanReceiveError::Udp(e)),
        };
        // 拷贝出接收数据，避免 `&mut self` 与 `&self.recv_buf` 借用冲突。
        let packet = self.recv_buf[..len].to_vec();
        match self.parse_and_push(&packet) {
            Ok(()) => self.stats.received += 1,
            Err(VBanReceiveError::Packet(VBanPacketError::InvalidMagic)) => {
                self.stats.bad_magic += 1;
            }
            Err(VBanReceiveError::Packet(VBanPacketError::PacketTooShort { .. })) => {
                self.stats.packet_too_short += 1;
            }
            Err(_) => self.stats.parse_error += 1,
        }
        Ok(true)
    }

    /// 解析一个完整 VBAN 包并把解码出的帧送入 Jitter Buffer。
    fn parse_and_push(&mut self, packet: &[u8]) -> Result<(), VBanReceiveError> {
        let header = VBanHeader::decode(packet)?;
        let samples_per_channel = header.samples_per_channel();
        let channels = header.channels();
        let expected_payload =
            samples_per_channel * channels * header.bit_format()?.bytes_per_sample();
        let payload = &packet[VBAN_HEADER_SIZE..];
        if payload.len() < expected_payload {
            return Err(VBanReceiveError::InvalidLength {
                expected: expected_payload,
                actual: payload.len(),
            });
        }
        // 解码 PCM 为 interleaved f32。
        let mut samples = vec![0.0f32; samples_per_channel * channels];
        decode_samples(payload, header.bit_format()?, &mut samples);
        self.jitter.push(header.nu_frame, &samples)?;
        Ok(())
    }

    /// 抽取下一个有序帧（interleaved `f32`）；无就绪帧时返回 `None`。
    pub fn pop_next(&mut self) -> Option<Vec<f32>> {
        let frame = self.jitter.pop_next();
        if frame.is_some() {
            self.stats.frames_out += 1;
        }
        frame
    }

    /// 当前 Jitter Buffer 水位（帧数）。
    pub fn fill_level(&self) -> usize {
        self.jitter.fill_level()
    }

    /// 当前统计快照。
    pub const fn stats(&self) -> VBanReceiveStats {
        self.stats
    }
}

/// 把 VBAN payload 按位深解码为 interleaved `f32`（-1..=1）。
fn decode_samples(payload: &[u8], bit_format: VBanBitFormat, output: &mut [f32]) {
    match bit_format {
        VBanBitFormat::Float32 => {
            for (i, sample) in output.iter_mut().enumerate() {
                let bytes = payload[i * 4..i * 4 + 4]
                    .try_into()
                    .expect("Float32 payload 长度校验");
                *sample = f32::from_le_bytes(bytes);
            }
        }
        VBanBitFormat::Int32 => {
            for (i, sample) in output.iter_mut().enumerate() {
                let bytes = payload[i * 4..i * 4 + 4]
                    .try_into()
                    .expect("Int32 payload 长度校验");
                *sample = i32::from_le_bytes(bytes) as f32 / 2_147_483_647.0;
            }
        }
        VBanBitFormat::Int24 => {
            for (i, sample) in output.iter_mut().enumerate() {
                let b0 = payload[i * 3] as i32;
                let b1 = payload[i * 3 + 1] as i32;
                let b2 = payload[i * 3 + 2] as i32;
                // 符号扩展 24 位有符号。
                let raw = (b0) | (b1 << 8) | (b2 << 16);
                let signed = if b2 & 0x80 != 0 { raw - (1 << 24) } else { raw };
                *sample = signed as f32 / 8_388_607.0;
            }
        }
        VBanBitFormat::Int16 => {
            for (i, sample) in output.iter_mut().enumerate() {
                let bytes = payload[i * 2..i * 2 + 2]
                    .try_into()
                    .expect("Int16 payload 长度校验");
                *sample = i16::from_le_bytes(bytes) as f32 / 32_767.0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::sender::VBanSender;
    use loopmaster_audio_core::vban::packet::VBanBitFormat;
    use proptest::prelude::*;

    fn random_port() -> u16 {
        // 用 UDP socket 探测可用端口（与后续 UDP 绑定一致），获取后立即释放。
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);
        port
    }

    /// 自环往返：VBanSender 发送 → VBanReceiver 接收抽取 → 校验 PCM 一致。
    fn round_trip(bit_format: VBanBitFormat, channels: usize, sample_rate: u32, frames: usize) {
        let port = random_port();
        let bind_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let frame_samples = frames * channels;
        let mut receiver = VBanReceiver::bind(bind_addr, frame_samples).unwrap();

        let samples: Vec<f32> = (0..frame_samples)
            .map(|i| (i as f32 / frame_samples as f32) * 0.8 - 0.4)
            .collect();
        let mut sender = VBanSender::new().unwrap();
        let packets = sender
            .send_frame(
                bind_addr,
                "Test",
                sample_rate,
                channels,
                bit_format,
                &samples,
            )
            .unwrap();
        assert!(packets >= 1);

        // 等待并接收所有包。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut received_count = 0;
        while std::time::Instant::now() < deadline && received_count < packets {
            if receiver.recv_and_push().unwrap() {
                received_count += 1;
            } else {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        assert_eq!(received_count, packets, "应收到所有 UDP 包");

        // 抽取帧，重建 samples。
        let mut out: Vec<f32> = Vec::new();
        while let Some(frame) = receiver.pop_next() {
            out.extend_from_slice(&frame);
        }
        assert_eq!(out.len(), samples.len(), "往返样本数应一致");

        // Float32 精确一致；整数位深允许量化误差。
        match bit_format {
            VBanBitFormat::Float32 => {
                assert!(
                    out.iter().zip(&samples).all(|(a, b)| (a - b).abs() < 1e-6),
                    "Float32 往返应精确一致"
                );
            }
            _ => {
                let tolerance = 2.0 / (1u64 << (bit_format.bytes_per_sample() * 8 - 1)) as f64;
                assert!(
                    out.iter()
                        .zip(&samples)
                        .all(|(a, b)| (a - b).abs() < tolerance as f32),
                    "整数位深往返应在量化容差内"
                );
            }
        }
    }

    #[test]
    fn round_trip_float32_stereo_48k() {
        round_trip(VBanBitFormat::Float32, 2, 48_000, 128);
    }

    #[test]
    fn round_trip_int16_stereo_48k() {
        round_trip(VBanBitFormat::Int16, 2, 48_000, 128);
    }

    #[test]
    fn round_trip_int24_stereo_48k() {
        round_trip(VBanBitFormat::Int24, 2, 48_000, 128);
    }

    #[test]
    fn rejects_invalid_magic_counts_stat() {
        let port = random_port();
        let bind_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut receiver = VBanReceiver::bind(bind_addr, 4).unwrap();
        // 发送非法魔数的包。
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        let garbage = [0x58u8; 64]; // 魔数非 VBAN
        socket.send_to(&garbage, bind_addr).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while std::time::Instant::now() < deadline {
            let _ = receiver.recv_and_push().unwrap();
            if receiver.stats().bad_magic > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(receiver.stats().bad_magic >= 1, "应统计到非法魔数包");
        assert_eq!(receiver.stats().received, 0);
    }

    /// 属性测试：随机声道数下 Float32 自环往返数值一致（单包帧长，避免分包）。
    #[test]
    fn round_trip_float32_random_dims() {
        proptest::proptest!(|(channels in 1usize..=4usize, frames in 1usize..=64usize)| {
            // 绑定端口 0，由系统分配，避免随机端口冲突。
            let frame_samples = frames * channels;
            let mut receiver = VBanReceiver::bind("127.0.0.1:0".parse().unwrap(), frame_samples).unwrap();
            let bind_addr = receiver.local_addr().unwrap();
            let samples: Vec<f32> = (0..frame_samples)
                .map(|i| ((i % 97) as f32 / 97.0) - 0.5)
                .collect();
            let mut sender = VBanSender::new().unwrap();
            let packets = sender
                .send_frame(bind_addr, "T", 48_000, channels, VBanBitFormat::Float32, &samples)
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            let mut received_count = 0;
            while std::time::Instant::now() < deadline && received_count < packets {
                if receiver.recv_and_push().unwrap() {
                    received_count += 1;
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
            prop_assert_eq!(received_count, packets);
            let mut out = Vec::new();
            while let Some(frame) = receiver.pop_next() {
                out.extend_from_slice(&frame);
            }
            prop_assert_eq!(out.len(), samples.len());
            prop_assert!(
                out.iter().zip(&samples).all(|(a, b)| (a - b).abs() < 1e-5)
            );
        });
    }
}
