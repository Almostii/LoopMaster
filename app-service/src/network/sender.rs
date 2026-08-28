//! VBAN 音频发送链路。
//!
//! 把 interleaved `f32` PCM 帧封包为 VBAN Audio 数据包并经 UDP 发送到目标节点。
//! 纯网络服务层，不触碰音频引擎实时线程。字节序遵循 VBAN 规范（小端）。
//!
//! 参考：[VBAN 局域网音频互通与传输方案]
//! （../../../../Doc/网络传输与本地节点互通方案计划/1.VBAN局域网音频互通与传输方案.md）5 节。

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};

use loopmaster_audio_core::vban::packet::{
    max_samples_per_channel, sample_rate_to_index, VBanBitFormat, VBanHeader, VBAN_HEADER_SIZE,
    VBAN_STREAM_NAME_SIZE,
};

/// 发送错误。
#[derive(Debug, thiserror::Error)]
pub enum VBanSendError {
    #[error("采样率不受支持: {0}")]
    UnsupportedSampleRate(u32),
    #[error("输入样本数无效：期望为声道数的整数倍，实际 {0}")]
    InvalidSampleCount(usize),
    #[error("声道数为 0")]
    ZeroChannels,
    #[error("每包采样数无法计算（声道数/位深非法）")]
    InvalidPacketCapacity,
    #[error("流名无效：{0}")]
    InvalidStreamName(String),
    #[error("UDP 发送失败: {0}")]
    Udp(std::io::Error),
}

/// VBAN 音频发送器。
///
/// 使用一个未绑定的发送 `UdpSocket`（`send_to` 指定目标，共享发送 Socket），
/// 为每个流维护独立的 `nu_frame` 自增计数。
pub struct VBanSender {
    socket: UdpSocket,
    /// `stream_name -> nu_frame` 自增计数器（按流隔离）。
    frame_counters: HashMap<String, u32>,
    /// 预分配的封包缓冲（`VBAN_HEADER_SIZE + payload` 上限），避免每包分配。
    packet: Vec<u8>,
}

impl VBanSender {
    /// 创建发送器。
    pub fn new() -> Result<Self, VBanSendError> {
        // 未绑定 Socket：仅用于 send_to，不接收。
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(VBanSendError::Udp)?;
        socket.set_nonblocking(true).map_err(VBanSendError::Udp)?;
        let packet = vec![0u8; VBAN_HEADER_SIZE + 1436];
        Ok(Self {
            socket,
            frame_counters: HashMap::new(),
            packet,
        })
    }

    /// 发送一帧 PCM（interleaved `f32`，范围约 -1.0..=1.0）。
    ///
    /// `samples.len()` 必须为 `channels` 的整数倍。若帧长超过单包上限，会
    /// 自动分包为多个 VBAN 包连续发送；`nu_frame` 按流自增。
    ///
    /// 返回发送的包数。
    pub fn send_frame(
        &mut self,
        target: SocketAddr,
        stream_name: &str,
        sample_rate: u32,
        channels: usize,
        bit_format: VBanBitFormat,
        samples: &[f32],
    ) -> Result<usize, VBanSendError> {
        if channels == 0 {
            return Err(VBanSendError::ZeroChannels);
        }
        if !samples.len().is_multiple_of(channels) || samples.is_empty() {
            return Err(VBanSendError::InvalidSampleCount(samples.len()));
        }
        if stream_name.is_empty() || stream_name.len() > VBAN_STREAM_NAME_SIZE {
            return Err(VBanSendError::InvalidStreamName(stream_name.to_owned()));
        }
        let sample_rate_index = sample_rate_to_index(sample_rate)
            .ok_or(VBanSendError::UnsupportedSampleRate(sample_rate))?;
        let samples_per_packet = max_samples_per_channel(channels, bit_format.bytes_per_sample())
            .filter(|n| *n > 0)
            .ok_or(VBanSendError::InvalidPacketCapacity)?;

        let total_frames = samples.len() / channels;
        let frame = self
            .frame_counters
            .entry(stream_name.to_owned())
            .or_insert(0);
        let mut packets_sent = 0;
        let mut offset = 0usize;

        // 逐包发送。
        while offset < total_frames {
            let frames_this_packet = (total_frames - offset).min(samples_per_packet);
            let frame_count = frames_this_packet.min(u8::MAX as usize + 1);

            let header = VBanHeader {
                // Audio PCM 协议类型高 3 位为 0，故 format_sr 低 5 位即采样率索引。
                format_sr: sample_rate_index,
                format_nbs: (frame_count as u8).wrapping_sub(1),
                format_nbc: (channels as u8).wrapping_sub(1),
                format_bit: bit_format.index(),
                stream_name: {
                    let mut name = [0u8; VBAN_STREAM_NAME_SIZE];
                    name[..stream_name.len()].copy_from_slice(stream_name.as_bytes());
                    name
                },
                nu_frame: *frame,
            };
            // 把头部编码到独立的 28 字节数组，再拷贝进发送缓冲。
            let mut header_bytes = [0u8; VBAN_HEADER_SIZE];
            header.encode_into(&mut header_bytes);
            self.packet[..VBAN_HEADER_SIZE].copy_from_slice(&header_bytes);

            // 转换并写入 payload。
            encode_samples(
                &samples[offset * channels..(offset + frame_count) * channels],
                channels,
                bit_format,
                &mut self.packet[VBAN_HEADER_SIZE..],
            );

            let packet_len = VBAN_HEADER_SIZE + payload_bytes(bit_format, frame_count * channels);
            self.socket
                .send_to(&self.packet[..packet_len], target)
                .map_err(VBanSendError::Udp)?;

            *frame = frame.wrapping_add(1);
            offset += frame_count;
            packets_sent += 1;
        }
        Ok(packets_sent)
    }
}

/// 计算给定样本数在指定位深下的 payload 字节数。
fn payload_bytes(bit_format: VBanBitFormat, sample_count: usize) -> usize {
    sample_count * bit_format.bytes_per_sample()
}

/// 把 interleaved `f32` 样本编码为指定位深的字节（小端序），写入 `output`。
fn encode_samples(samples: &[f32], _channels: usize, bit_format: VBanBitFormat, output: &mut [u8]) {
    match bit_format {
        VBanBitFormat::Float32 => {
            for (i, sample) in samples.iter().enumerate() {
                let bytes = sample.to_le_bytes();
                output[i * 4..i * 4 + 4].copy_from_slice(&bytes);
            }
        }
        VBanBitFormat::Int32 => {
            for (i, sample) in samples.iter().enumerate() {
                let v = f32_to_i32(*sample);
                let bytes = v.to_le_bytes();
                output[i * 4..i * 4 + 4].copy_from_slice(&bytes);
            }
        }
        VBanBitFormat::Int24 => {
            for (i, sample) in samples.iter().enumerate() {
                let v = f32_to_i24(*sample);
                let bytes = v.to_le_bytes();
                // Int24：取低 3 字节（小端），最高字节为符号扩展。
                output[i * 3] = bytes[0];
                output[i * 3 + 1] = bytes[1];
                output[i * 3 + 2] = bytes[2];
            }
        }
        VBanBitFormat::Int16 => {
            for (i, sample) in samples.iter().enumerate() {
                let v = f32_to_i16(*sample);
                let bytes = v.to_le_bytes();
                output[i * 2..i * 2 + 2].copy_from_slice(&bytes);
            }
        }
    }
}

/// 把 `f32`（-1..=1）映射到 `i16`（-32768..=32767）。
fn f32_to_i16(sample: f32) -> i16 {
    let clamped = sample.clamp(-1.0, 1.0);
    (clamped * 32767.0) as i16
}

/// 把 `f32`（-1..=1）映射到 `i32`（-2^31..=2^31-1）。
fn f32_to_i32(sample: f32) -> i32 {
    let clamped = sample.clamp(-1.0, 1.0);
    (clamped * 2_147_483_647.0) as i32
}

/// 把 `f32`（-1..=1）映射到 24 位有符号整数（-2^23..=2^23-1）。
fn f32_to_i24(sample: f32) -> i32 {
    let clamped = sample.clamp(-1.0, 1.0);
    (clamped * 8_388_607.0) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use loopmaster_audio_core::vban::packet::{VBAN_HEADER_SIZE, VBAN_MAGIC};

    fn random_port() -> u16 {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    #[test]
    fn rejects_invalid_inputs() {
        let mut sender = VBanSender::new().unwrap();
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
        // 样本数不是声道数整数倍。
        assert!(sender
            .send_frame(addr, "S", 48_000, 2, VBanBitFormat::Int16, &[0.0, 0.0, 0.0])
            .is_err());
        // 空流名。
        assert!(sender
            .send_frame(addr, "", 48_000, 2, VBanBitFormat::Int16, &[0.0, 0.0])
            .is_err());
        // 声道数为 0。
        assert!(sender
            .send_frame(addr, "S", 48_000, 0, VBanBitFormat::Int16, &[0.0])
            .is_err());
        // 不受支持的采样率。
        assert!(sender
            .send_frame(addr, "S", 12345, 2, VBanBitFormat::Int16, &[0.0, 0.0])
            .is_err());
    }

    #[test]
    fn rejects_excessive_channels_without_looping() {
        // 超大 channels 使 bytes_per_frame > 1436，max_samples_per_channel 返回
        // 0；应返回 InvalidPacketCapacity 错误而非死循环。
        let mut sender = VBanSender::new().unwrap();
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let samples = vec![0.0f32; 500];
        let result = sender.send_frame(addr, "S", 48_000, 500, VBanBitFormat::Float32, &samples);
        assert!(matches!(result, Err(VBanSendError::InvalidPacketCapacity)));
    }

    #[test]
    fn splits_large_frame_and_increments_frame_number() {
        // Int16 双声道单包上限 256 帧；发送 300 帧应拆为 2 包。
        let port = random_port();
        let bind_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let recv = std::net::UdpSocket::bind(bind_addr).unwrap();
        recv.set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();

        let mut sender = VBanSender::new().unwrap();
        let samples = vec![0.25f32; 300 * 2];
        let packets = sender
            .send_frame(
                bind_addr,
                "Split",
                48_000,
                2,
                VBanBitFormat::Int16,
                &samples,
            )
            .unwrap();
        assert_eq!(packets, 2, "300 帧 Int16 双声道应拆为 2 包");

        // 收集两包，验证魔数与 nu_frame 递增。
        let mut received = Vec::new();
        let mut buf = [0u8; 1500];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while received.len() < packets && std::time::Instant::now() < deadline {
            match recv.recv_from(&mut buf) {
                Ok((len, _)) => received.push(buf[..len].to_vec()),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        assert_eq!(received.len(), 2);
        for (i, pkt) in received.iter().enumerate() {
            assert_eq!(&pkt[0..4], &VBAN_MAGIC, "包 {i} 魔数应正确");
            assert!(pkt.len() >= VBAN_HEADER_SIZE);
        }
        // 解析 header 验证 nu_frame 递增。
        let h0 = VBanHeader::decode(&received[0]).unwrap();
        let h1 = VBanHeader::decode(&received[1]).unwrap();
        assert_eq!(h0.nu_frame.wrapping_add(1), h1.nu_frame);
        assert_eq!(h0.samples_per_channel(), 256);
        assert_eq!(h1.samples_per_channel(), 44); // 300 - 256
    }
}
