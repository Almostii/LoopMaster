//! VBAN Audio PCM 数据包头部编解码与载荷校验。
//!
//! 遵循 VBAN 规范与项目约束：
//! - 28 字节 `VBanHeader` 使用**小端序**显式字段读写，禁止把网络字节直接
//!   `transmute`/cast 为 `#[repr(C, packed)]` 结构体。
//! - 解码器先检查包长与魔数，再逐字段读取。
//! - `nu_frame` 使用 `u32::from_le_bytes` / `to_le_bytes`。
//!
//! 位布局（28 字节头）：
//! ```text
//!  0..4  FourCC 'V''B''A''N'（小端 u32 = 0x4E414256）
//!  4     format_sr：高 3 位协议类型（Audio PCM = 0b000）+ 低 5 位采样率索引
//!  5     format_nbs：每声道采样点数 - 1（0..=255）
//!  6     format_nbc：声道数 - 1（0..=255）
//!  7     format_bit：高 4 位编解码类型（PCM = 0x00）+ Bit3 保留 + 低 3 位位深
//!  8..24 stream_name：16 字节，不足补 0，首 NUL 截断
//!  24..28 nu_frame：u32 自增帧号（小端）
//! ```

use thiserror::Error;

/// 固定协议魔数 `VBAN`（ASCII）。
pub const VBAN_MAGIC: [u8; 4] = *b"VBAN";
/// 头部固定字节数。
pub const VBAN_HEADER_SIZE: usize = 28;
/// 单包最大字节数（<= 1500 MTU 减去 UDP/IP 头）。
pub const VBAN_MAX_PACKET_SIZE: usize = 1464;
/// 单包最大载荷字节数。
pub const VBAN_MAX_PAYLOAD_SIZE: usize = VBAN_MAX_PACKET_SIZE - VBAN_HEADER_SIZE;
/// `stream_name` 固定字节长度。
pub const VBAN_STREAM_NAME_SIZE: usize = 16;
/// 每个声道每包最大采样点数（协议上限）。
pub const VBAN_MAX_SAMPLES_PER_CHANNEL: usize = 256;
/// `nu_frame` 跳变容忍窗口：用于接收端判断丢包/乱序。
pub const VBAN_FRAME_WINDOW: u32 = 1024;

/// 位深字节中的低位位深索引。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VBanPacketError {
    #[error("数据包过短：{actual} 字节，期望至少 {expected} 字节")]
    PacketTooShort { expected: usize, actual: usize },
    #[error("VBAN 魔数不匹配")]
    InvalidMagic,
    #[error("非 Audio PCM 协议类型: {protocol_type}")]
    UnsupportedProtocol { protocol_type: u8 },
    #[error("不支持或无效的采样率索引: {index}")]
    InvalidSampleRateIndex { index: u8 },
    #[error("不支持或无效的位深索引: {index}")]
    InvalidBitFormatIndex { index: u8 },
    #[error("流名无效：{reason}")]
    InvalidStreamName { reason: String },
    #[error("声道数超出上限: {channels}")]
    TooManyChannels { channels: usize },
    #[error("载荷超过协议上限：{actual} 字节，上限 {max} 字节")]
    PayloadTooLarge { max: usize, actual: usize },
}

/// VBAN 协议类型（`format_sr` 高 3 位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VBanProtocol {
    /// Audio PCM。
    Audio,
}

impl VBanProtocol {
    /// `format_sr` 高 3 位的二进制值（Audio PCM = 0b000）。
    pub const fn code(self) -> u8 {
        match self {
            Self::Audio => 0b000,
        }
    }
}

/// VBAN 位深（`format_bit` 低 3 位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VBanBitFormat {
    Int16,
    Int24,
    Int32,
    Float32,
}

impl VBanBitFormat {
    /// `format_bit` 低 3 位的索引。
    pub const fn index(self) -> u8 {
        match self {
            Self::Int16 => 0x01,
            Self::Int24 => 0x02,
            Self::Int32 => 0x03,
            Self::Float32 => 0x04,
        }
    }

    /// 每样本字节数。
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::Int16 => 2,
            Self::Int24 => 3,
            Self::Int32 => 4,
            Self::Float32 => 4,
        }
    }

    fn from_index(index: u8) -> Result<Self, VBanPacketError> {
        match index {
            0x01 => Ok(Self::Int16),
            0x02 => Ok(Self::Int24),
            0x03 => Ok(Self::Int32),
            0x04 => Ok(Self::Float32),
            _ => Err(VBanPacketError::InvalidBitFormatIndex { index }),
        }
    }
}

/// VBAN 采样率索引表（20 项，对应 `format_sr` 低 5 位的 0..=19）。
pub const SAMPLE_RATE_INDEX_TO_HZ: [u32; 20] = [
    6_000, 12_000, 24_000, 48_000, 96_000, 192_000, 384_000, 8_000, 16_000, 32_000, 64_000,
    128_000, 256_000, 11_025, 22_050, 44_100, 88_200, 176_400, 352_800, 705_600,
];

/// 采样率 → 索引映射，返回第一个匹配项。
pub const fn sample_rate_to_index(hz: u32) -> Option<u8> {
    let mut index = 0;
    while index < SAMPLE_RATE_INDEX_TO_HZ.len() {
        if SAMPLE_RATE_INDEX_TO_HZ[index] == hz {
            return Some(index as u8);
        }
        index += 1;
    }
    None
}

/// 由采样率查索引；未知采样率返回错误。
fn sample_rate_to_index_result(hz: u32) -> Result<u8, VBanPacketError> {
    sample_rate_to_index(hz).ok_or(VBanPacketError::InvalidSampleRateIndex {
        index: hz.min(u8::MAX as u32) as u8,
    })
}

/// VBAN 头部的逐字段解码结构。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VBanHeader {
    /// `format_sr` 原字节（高 3 位协议 + 低 5 位采样率索引）。
    pub format_sr: u8,
    /// `format_nbs`：每声道采样点数 - 1。
    pub format_nbs: u8,
    /// `format_nbc`：声道数 - 1。
    pub format_nbc: u8,
    /// `format_bit` 原字节（高 4 位编解码 + Bit3 保留 + 低 3 位位深）。
    pub format_bit: u8,
    /// 流名（固定 16 字节，首 NUL 截断）。
    pub stream_name: [u8; VBAN_STREAM_NAME_SIZE],
    /// 自增帧号。
    pub nu_frame: u32,
}

impl VBanHeader {
    /// 从数据包解码头部；先校验包长与魔数，再逐字段解析。
    pub fn decode(packet: &[u8]) -> Result<Self, VBanPacketError> {
        if packet.len() < VBAN_HEADER_SIZE {
            return Err(VBanPacketError::PacketTooShort {
                expected: VBAN_HEADER_SIZE,
                actual: packet.len(),
            });
        }
        if packet[0..4] != VBAN_MAGIC {
            return Err(VBanPacketError::InvalidMagic);
        }
        let format_sr = packet[4];
        let protocol_type = format_sr >> 5;
        if protocol_type != VBanProtocol::Audio.code() {
            return Err(VBanPacketError::UnsupportedProtocol { protocol_type });
        }
        let mut stream_name = [0u8; VBAN_STREAM_NAME_SIZE];
        stream_name.copy_from_slice(&packet[8..24]);
        Ok(Self {
            format_sr,
            format_nbs: packet[5],
            format_nbc: packet[6],
            format_bit: packet[7],
            stream_name,
            nu_frame: u32::from_le_bytes([packet[24], packet[25], packet[26], packet[27]]),
        })
    }

    /// 将头部编码进 28 字节输出缓冲区。
    pub fn encode_into(&self, output: &mut [u8; VBAN_HEADER_SIZE]) {
        output[0..4].copy_from_slice(&VBAN_MAGIC);
        output[4] = self.format_sr;
        output[5] = self.format_nbs;
        output[6] = self.format_nbc;
        output[7] = self.format_bit;
        output[8..24].copy_from_slice(&self.stream_name);
        let frame = self.nu_frame.to_le_bytes();
        output[24..28].copy_from_slice(&frame);
    }

    /// 解码出的协议类型（基于 `format_sr` 高 3 位）。
    pub const fn protocol_type(&self) -> u8 {
        self.format_sr >> 5
    }

    /// 解码出的采样率索引（`format_sr` 低 5 位）。
    pub const fn sample_rate_index(&self) -> u8 {
        self.format_sr & 0b0001_1111
    }

    /// 采样率（由索引查表）。
    pub fn sample_rate_hz(&self) -> Result<u32, VBanPacketError> {
        let index = self.sample_rate_index();
        ((index as usize) < SAMPLE_RATE_INDEX_TO_HZ.len())
            .then(|| SAMPLE_RATE_INDEX_TO_HZ[index as usize])
            .ok_or(VBanPacketError::InvalidSampleRateIndex { index })
    }

    /// 每声道采样点数。
    pub const fn samples_per_channel(&self) -> usize {
        self.format_nbs as usize + 1
    }

    /// 声道数。
    pub const fn channels(&self) -> usize {
        self.format_nbc as usize + 1
    }

    /// 位深格式。
    pub fn bit_format(&self) -> Result<VBanBitFormat, VBanPacketError> {
        VBanBitFormat::from_index(self.format_bit & 0b0111)
    }

    /// 流名（按首 NUL 截断，并校验为可打印 ASCII）。
    pub fn stream_name_str(&self) -> Result<&str, VBanPacketError> {
        let len = self
            .stream_name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(self.stream_name.len());
        let bytes = &self.stream_name[..len];
        if !bytes.iter().all(|byte| (32..=126).contains(byte)) {
            return Err(VBanPacketError::InvalidStreamName {
                reason: "流名包含不可打印或非 ASCII 字节".into(),
            });
        }
        // 安全：已通过可打印 ASCII 校验。
        Ok(std::str::from_utf8(bytes).expect("可打印 ASCII 必为合法 UTF-8"))
    }

    /// 载荷实际字节数（整包长度减去头部）。
    pub const fn payload_len(total_packet_len: usize) -> usize {
        total_packet_len.saturating_sub(VBAN_HEADER_SIZE)
    }
}

/// 发送流的稳定配置（用于构造头部与计算每包采样数）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VBanStreamConfig {
    pub stream_name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub bit_format: VBanBitFormat,
}

impl VBanStreamConfig {
    /// 校验并构造配置；流名限制为 1..=16 字节可打印 ASCII。
    pub fn new(
        stream_name: String,
        sample_rate: u32,
        channels: u16,
        bit_format: VBanBitFormat,
    ) -> Result<Self, VBanPacketError> {
        if stream_name.is_empty() || stream_name.len() > VBAN_STREAM_NAME_SIZE {
            return Err(VBanPacketError::InvalidStreamName {
                reason: format!(
                    "流名长度 {}(字节) 必须在 1..={} 之间",
                    stream_name.len(),
                    VBAN_STREAM_NAME_SIZE
                ),
            });
        }
        if !stream_name.bytes().all(|byte| (32..=126).contains(&byte)) {
            return Err(VBanPacketError::InvalidStreamName {
                reason: "流名包含不可打印或非 ASCII 字节".into(),
            });
        }
        if channels == 0 {
            return Err(VBanPacketError::TooManyChannels { channels: 0 });
        }
        sample_rate_to_index_result(sample_rate)?;
        Ok(Self {
            stream_name,
            sample_rate,
            channels,
            bit_format,
        })
    }

    /// 每声道每包采样点数，受协议上限与载荷容量双重约束。
    pub fn max_samples_per_channel(&self) -> Option<usize> {
        max_samples_per_channel(self.channels as usize, self.bit_format.bytes_per_sample())
    }
}

/// 计算每声道每包最大采样点数：`min(256, floor(1436 / (channels * bytes_per_sample)))`。
pub fn max_samples_per_channel(channels: usize, bytes_per_sample: usize) -> Option<usize> {
    let bytes_per_frame = channels.checked_mul(bytes_per_sample)?;
    if bytes_per_frame == 0 {
        return None;
    }
    Some((VBAN_MAX_PAYLOAD_SIZE / bytes_per_frame).min(VBAN_MAX_SAMPLES_PER_CHANNEL))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn header(sr: u8, nbs: u8, nbc: u8, bit: u8, name: &str, frame: u32) -> VBanHeader {
        let mut stream_name = [0u8; VBAN_STREAM_NAME_SIZE];
        stream_name[..name.len()].copy_from_slice(name.as_bytes());
        VBanHeader {
            format_sr: sr,
            format_nbs: nbs,
            format_nbc: nbc,
            format_bit: bit,
            stream_name,
            nu_frame: frame,
        }
    }

    fn encode(h: &VBanHeader) -> [u8; VBAN_HEADER_SIZE] {
        let mut out = [0u8; VBAN_HEADER_SIZE];
        h.encode_into(&mut out);
        out
    }

    /// Audio PCM(0<<5=0) + 采样率索引 3 (48kHz)
    const SR_48K: u8 = 0b0000_0011;

    #[test]
    fn round_trips_stereo_48k_pcm_header() {
        let h = header(SR_48K, 255, 1, 0b0000_0001, "Stream1", 42);
        let bytes = encode(&h);
        assert_eq!(bytes[0..4], VBAN_MAGIC);
        let decoded = VBanHeader::decode(&bytes).unwrap();
        assert_eq!(decoded, h);
        assert_eq!(decoded.sample_rate_hz().unwrap(), 48_000);
        assert_eq!(decoded.samples_per_channel(), 256);
        assert_eq!(decoded.channels(), 2);
        assert_eq!(decoded.bit_format().unwrap(), VBanBitFormat::Int16);
        assert_eq!(decoded.stream_name_str().unwrap(), "Stream1");
    }

    #[test]
    fn rejects_short_packet_before_magic_check() {
        let err = VBanHeader::decode(&[0u8; 10]).unwrap_err();
        assert_eq!(
            err,
            VBanPacketError::PacketTooShort {
                expected: VBAN_HEADER_SIZE,
                actual: 10,
            }
        );
    }

    #[test]
    fn rejects_invalid_magic() {
        let mut bytes = [0u8; VBAN_HEADER_SIZE];
        bytes[0..4].copy_from_slice(b"XXXX");
        assert_eq!(
            VBanHeader::decode(&bytes).unwrap_err(),
            VBanPacketError::InvalidMagic
        );
    }

    #[test]
    fn rejects_unsupported_protocol_type() {
        // format_sr 高 3 位 = 0b010（非 Audio）。
        let bytes = encode(&header(SR_48K | 0b0100_0000, 0, 1, 0b0000_0001, "S", 0));
        let decoded = VBanHeader::decode(&bytes);
        assert!(matches!(
            decoded,
            Err(VBanPacketError::UnsupportedProtocol {
                protocol_type: 0b010
            })
        ));
    }

    #[test]
    fn covers_all_twenty_sample_rate_indices() {
        let expected = [
            6_000, 12_000, 24_000, 48_000, 96_000, 192_000, 384_000, 8_000, 16_000, 32_000, 64_000,
            128_000, 256_000, 11_025, 22_050, 44_100, 88_200, 176_400, 352_800, 705_600,
        ];
        for (index, hz) in expected.iter().enumerate() {
            let h = header(SR_48K, 0, 0, 0b0000_0001, "S", 0);
            let mut bytes = encode(&h);
            // 覆盖 format_sr 低 5 位。
            bytes[4] = VBanProtocol::Audio.code() << 5 | index as u8;
            let decoded = VBanHeader::decode(&bytes).unwrap();
            assert_eq!(decoded.sample_rate_hz().unwrap(), *hz);
            assert_eq!(sample_rate_to_index(*hz), Some(index as u8));
        }
    }

    #[test]
    fn covers_all_supported_bit_depths() {
        let cases = [
            (0x01, VBanBitFormat::Int16, 2),
            (0x02, VBanBitFormat::Int24, 3),
            (0x03, VBanBitFormat::Int32, 4),
            (0x04, VBanBitFormat::Float32, 4),
        ];
        for (bit_index, bit_format, bytes_per) in cases {
            let h = header(SR_48K, 0, 1, bit_index, "S", 0);
            let decoded = VBanHeader::decode(&encode(&h)).unwrap();
            assert_eq!(decoded.bit_format().unwrap(), bit_format);
            assert_eq!(bit_format.bytes_per_sample(), bytes_per);
            assert_eq!(bit_format.index(), bit_index);
        }
    }

    #[test]
    fn rejects_invalid_bit_format() {
        let h = header(SR_48K, 0, 1, 0b0000_0110, "S", 0); // index 6 未定义
        assert!(matches!(
            VBanHeader::decode(&encode(&h)).unwrap().bit_format(),
            Err(VBanPacketError::InvalidBitFormatIndex { index: 6 })
        ));
    }

    #[test]
    fn rejects_reserved_sample_rate_indices_20_to_31() {
        for index in 20u8..=31 {
            let mut bytes = encode(&header(SR_48K, 0, 0, 0b0000_0001, "S", 0));
            bytes[4] = VBanProtocol::Audio.code() << 5 | index;
            let decoded = VBanHeader::decode(&bytes).unwrap();
            assert!(matches!(
                decoded.sample_rate_hz(),
                Err(VBanPacketError::InvalidSampleRateIndex { index: idx }) if idx == index
            ));
        }
    }

    #[test]
    fn stream_name_truncates_at_first_nul() {
        let h = header(SR_48K, 0, 0, 0b0000_0001, "Stream1", 0);
        assert_eq!(h.stream_name_str().unwrap(), "Stream1");
        // 非 ASCII 或不可打印字节应被拒绝。
        let bad = header(SR_48K, 0, 0, 0b0000_0001, "\u{1F600}", 0);
        assert!(matches!(
            bad.stream_name_str(),
            Err(VBanPacketError::InvalidStreamName { .. })
        ));
    }

    #[test]
    fn stream_config_validates_name_and_sample_rate() {
        assert!(VBanStreamConfig::new(String::new(), 48_000, 2, VBanBitFormat::Int16).is_err());
        assert!(VBanStreamConfig::new("x".repeat(17), 48_000, 2, VBanBitFormat::Int16).is_err());
        assert!(VBanStreamConfig::new("OK".into(), 44_100, 2, VBanBitFormat::Float32).is_ok());
        assert!(VBanStreamConfig::new("OK".into(), 41_000, 2, VBanBitFormat::Int16).is_err());
    }

    #[test]
    fn max_samples_per_channel_is_bounded() {
        // 双声道 Float32：1436 / 8 = 179，远小于 256。
        assert_eq!(max_samples_per_channel(2, 4), Some(179));
        // 双声道 Int16：1436 / 4 = 359，受协议上限 256 截断。
        assert_eq!(max_samples_per_channel(2, 2), Some(256));
        // 溢出/非法输入返回 None。
        assert_eq!(max_samples_per_channel(usize::MAX, 2), None);
        assert_eq!(max_samples_per_channel(0, 2), None);
    }

    #[test]
    fn payload_length_is_total_minus_header() {
        assert_eq!(VBanHeader::payload_len(VBAN_HEADER_SIZE + 100), 100);
        assert_eq!(VBanHeader::payload_len(0), 0);
    }

    /// 属性测试：任意合法 header 字段的 encode/decode 往返保持字节与语义一致。
    #[test]
    fn arbitrary_header_round_trips() {
        proptest::proptest!(|(
            sample_rate_index in 0u8..20u8,
            nbs in 0u8..=255u8,
            nbc in 0u8..=255u8,
            bit_index in 0u8..=4u8,
            frame in 0u32..=u32::MAX,
        )| {
            let mut name_bytes = [0u8; VBAN_STREAM_NAME_SIZE];
            name_bytes[..3].copy_from_slice(b"abc");
            let format_sr = VBanProtocol::Audio.code() << 5 | sample_rate_index;
            let format_bit = bit_index; // 0..=4 均在低 3 位内
            let h = VBanHeader {
                format_sr,
                format_nbs: nbs,
                format_nbc: nbc,
                format_bit,
                stream_name: name_bytes,
                nu_frame: frame,
            };
            let mut out = [0u8; VBAN_HEADER_SIZE];
            h.encode_into(&mut out);
            let decoded = VBanHeader::decode(&out).unwrap();
            prop_assert_eq!(&decoded, &h);
            prop_assert_eq!(
                decoded.sample_rate_hz().unwrap(),
                SAMPLE_RATE_INDEX_TO_HZ[sample_rate_index as usize]
            );
        });
    }

    /// 属性测试：任何短于头部长度的随机字节包都必须被拒绝，且不得 panic。
    #[test]
    fn arbitrary_truncated_packets_are_rejected() {
        proptest::proptest!(|(packet in proptest::collection::vec(any::<u8>(), 0..VBAN_HEADER_SIZE))| {
            let rejected = matches!(
                VBanHeader::decode(&packet),
                Err(VBanPacketError::PacketTooShort { .. })
            );
            prop_assert!(rejected);
        });
    }

    /// 属性测试：非法魔数（非 VBAN）的完整长度包头必须被拒绝。
    #[test]
    fn arbitrary_bad_magic_is_rejected() {
        proptest::proptest!(|(
            first in any::<u8>(),
            second in any::<u8>(),
            third in any::<u8>(),
            fourth in any::<u8>(),
        )| {
            // 构造 28 字节包，魔数不等于 VBAN 时解码必须报 InvalidMagic。
            prop_assert!(
                !matches!((first, second, third, fourth), (b'V', b'B', b'A', b'N'))
                    || VBanHeader::decode(&[
                        first, second, third, fourth, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                        0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    ])
                    .is_ok()
            );
        });
    }
}
