//! WASAPI 原生样本格式与 LoopMaster 内部格式之间的无分配转换。

use super::EndpointFormat;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleEncoding {
    Float32,
    Pcm16,
}

pub fn encoding(format: EndpointFormat) -> Option<SampleEncoding> {
    if format.is_float && !format.is_pcm && format.bits_per_sample == 32 {
        Some(SampleEncoding::Float32)
    } else if format.is_pcm && !format.is_float && format.bits_per_sample == 16 {
        Some(SampleEncoding::Pcm16)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConversionError {
    UnsupportedFormat,
    InputLength,
    OutputLength,
}

/// 将原生交错样本解码并映射为内部双声道 f32。`output` 至少应容纳
/// `frames * 2` 个样本；函数不分配。
pub fn decode_to_stereo(
    format: EndpointFormat,
    input: &[u8],
    frames: usize,
    output: &mut [f32],
) -> Result<(), ConversionError> {
    let channels = usize::from(format.channels);
    if channels == 0 || output.len() < frames.saturating_mul(2) {
        return Err(ConversionError::OutputLength);
    }
    let bytes_per_sample = match encoding(format) {
        Some(SampleEncoding::Float32) => 4,
        Some(SampleEncoding::Pcm16) => 2,
        None => return Err(ConversionError::UnsupportedFormat),
    };
    let expected = frames
        .checked_mul(channels)
        .and_then(|n| n.checked_mul(bytes_per_sample))
        .ok_or(ConversionError::InputLength)?;
    if input.len() < expected {
        return Err(ConversionError::InputLength);
    }
    for frame in 0..frames {
        let base = frame * channels;
        let mut left = 0.0;
        let mut right = 0.0;
        let mut left_weight = 0.0;
        let mut right_weight = 0.0;
        if channels == 1 {
            let sample = read_sample(format, input, base, bytes_per_sample);
            left = sample;
            right = sample;
            left_weight = 1.0;
            right_weight = 1.0;
        } else {
            let fl = channel_index(format.channel_mask, 0x1, channels).unwrap_or(0);
            let fr = channel_index(format.channel_mask, 0x2, channels).unwrap_or(1);
            left += read_sample(format, input, base + fl, bytes_per_sample);
            right += read_sample(format, input, base + fr, bytes_per_sample);
            left_weight += 1.0;
            right_weight += 1.0;
            if let Some(fc) = channel_index(format.channel_mask, 0x4, channels) {
                let sample = read_sample(format, input, base + fc, bytes_per_sample) * 0.70710677;
                left += sample;
                right += sample;
                left_weight += 0.70710677;
                right_weight += 0.70710677;
            }
            for (mask, weight, to_left) in [
                (0x10, 0.5, true),
                (0x200, 0.5, true),
                (0x20, 0.5, false),
                (0x400, 0.5, false),
            ] {
                if let Some(index) = channel_index(format.channel_mask, mask, channels) {
                    let sample =
                        read_sample(format, input, base + index, bytes_per_sample) * weight;
                    if to_left {
                        left += sample;
                        left_weight += weight;
                    } else {
                        right += sample;
                        right_weight += weight;
                    }
                }
            }
        }
        output[frame * 2] = left / left_weight.max(1.0);
        output[frame * 2 + 1] = right / right_weight.max(1.0);
    }
    Ok(())
}

/// 将内部双声道 f32 映射并编码为目标 endpoint 的原生格式。
pub fn encode_from_stereo(
    format: EndpointFormat,
    input: &[f32],
    frames: usize,
    output: &mut [u8],
) -> Result<(), ConversionError> {
    if input.len() < frames.saturating_mul(2) {
        return Err(ConversionError::InputLength);
    }
    let channels = usize::from(format.channels);
    let bytes_per_sample = match encoding(format) {
        Some(SampleEncoding::Float32) => 4,
        Some(SampleEncoding::Pcm16) => 2,
        None => return Err(ConversionError::UnsupportedFormat),
    };
    let expected = frames
        .checked_mul(channels)
        .and_then(|n| n.checked_mul(bytes_per_sample))
        .ok_or(ConversionError::OutputLength)?;
    if output.len() < expected {
        return Err(ConversionError::OutputLength);
    }
    for frame in 0..frames {
        let left = input[frame * 2];
        let right = input[frame * 2 + 1];
        for channel in 0..channels {
            let sample = output_sample(format.channel_mask, channels, channel, left, right);
            write_sample(
                format,
                output,
                frame * channels + channel,
                bytes_per_sample,
                sample,
            );
        }
    }
    Ok(())
}

fn channel_index(mask: u32, wanted: u32, channels: usize) -> Option<usize> {
    if mask & wanted == 0 {
        return None;
    }
    let mut index = 0;
    for bit in [
        0x1u32, 0x2, 0x4, 0x8, 0x10, 0x20, 0x40, 0x80, 0x100, 0x200, 0x400, 0x800, 0x1000, 0x2000,
        0x4000, 0x8000,
    ] {
        if mask & bit != 0 {
            if bit == wanted {
                return (index < channels).then_some(index);
            }
            index += 1;
        }
    }
    None
}

fn read_sample(format: EndpointFormat, input: &[u8], index: usize, bytes: usize) -> f32 {
    let offset = index * bytes;
    match encoding(format) {
        Some(SampleEncoding::Float32) => {
            let sample = f32::from_le_bytes(input[offset..offset + 4].try_into().unwrap());
            if sample.is_finite() {
                sample
            } else {
                0.0
            }
        }
        Some(SampleEncoding::Pcm16) => {
            f32::from(i16::from_le_bytes(
                input[offset..offset + 2].try_into().unwrap(),
            )) / 32768.0
        }
        None => 0.0,
    }
}

fn output_sample(mask: u32, channels: usize, index: usize, left: f32, right: f32) -> f32 {
    if channels == 1 {
        return (left + right) * 0.5;
    }
    let mut current = 0;
    for bit in [
        0x1u32, 0x2, 0x4, 0x8, 0x10, 0x20, 0x40, 0x80, 0x100, 0x200, 0x400, 0x800, 0x1000, 0x2000,
        0x4000, 0x8000,
    ] {
        if mask & bit != 0 {
            if current == index {
                return match bit {
                    0x1 => left,
                    0x2 => right,
                    // 本轮只做内部 stereo 到 endpoint 前置左右声道的边界映射。
                    // center/LFE/surround 保持静音，不能冒充动态多声道路由。
                    _ => 0.0,
                };
            }
            current += 1;
        }
    }
    if mask == 0 {
        return match index {
            0 => left,
            1 => right,
            _ => 0.0,
        };
    }
    0.0
}

fn write_sample(
    format: EndpointFormat,
    output: &mut [u8],
    index: usize,
    bytes: usize,
    sample: f32,
) {
    let offset = index * bytes;
    let sample = if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    match encoding(format) {
        Some(SampleEncoding::Float32) => {
            output[offset..offset + 4].copy_from_slice(&sample.to_le_bytes())
        }
        Some(SampleEncoding::Pcm16) => {
            let value = (sample.clamp(-1.0, 0.9999695) * 32768.0).round() as i16;
            output[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(bits: u16, channels: u16, is_float: bool, is_pcm: bool, mask: u32) -> EndpointFormat {
        EndpointFormat {
            sample_rate: 48_000,
            bits_per_sample: bits,
            channels,
            channel_mask: mask,
            is_float,
            is_pcm,
        }
    }

    #[test]
    fn pcm16_mono_decodes_to_stereo() {
        let input = [0x00, 0x40, 0x00, 0xC0];
        let mut output = [0.0; 4];
        decode_to_stereo(format(16, 1, false, true, 0), &input, 2, &mut output).unwrap();
        assert!((output[0] - 0.5).abs() < 0.001);
        assert!((output[3] + 0.5).abs() < 0.001);
    }

    #[test]
    fn stereo_round_trip_pcm16() {
        let source = [0.25, -0.5];
        let mut bytes = [0u8; 4];
        encode_from_stereo(format(16, 2, false, true, 3), &source, 1, &mut bytes).unwrap();
        let mut output = [0.0; 2];
        decode_to_stereo(format(16, 2, false, true, 3), &bytes, 1, &mut output).unwrap();
        assert!((output[0] - 0.25).abs() < 0.001);
        assert!((output[1] + 0.5).abs() < 0.001);
    }

    #[test]
    fn maskless_multichannel_input_uses_first_two_channels() {
        let native = format(32, 4, true, false, 0);
        let input: Vec<u8> = [0.25f32, -0.5, 0.9, 0.8]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        let mut output = [0.0; 2];
        decode_to_stereo(native, &input, 1, &mut output).unwrap();
        assert_eq!(output, [0.25, -0.5]);
    }

    #[test]
    fn five_point_one_input_downmixes_without_lfe() {
        let native = format(32, 6, true, false, 0x3f);
        // FL, FR, FC, LFE, BL, BR
        let input: Vec<u8> = [0.5f32, -0.5, 0.25, 1.0, 0.25, -0.25]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        let mut output = [0.0; 2];
        decode_to_stereo(native, &input, 1, &mut output).unwrap();
        assert!(output[0] > 0.35 && output[0] < 0.5);
        assert!(output[1] < -0.2 && output[1] > -0.5);
    }

    #[test]
    fn five_point_one_output_only_populates_front_left_right() {
        let native = format(32, 6, true, false, 0x3f);
        let mut bytes = [0u8; 24];
        encode_from_stereo(native, &[0.25, -0.5], 1, &mut bytes).unwrap();
        let samples: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
            .collect();
        assert_eq!(samples, [0.25, -0.5, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn non_finite_samples_are_silenced_and_pcm_is_clipped() {
        let float = format(32, 2, true, false, 3);
        let input: Vec<u8> = [f32::NAN, f32::INFINITY]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        let mut decoded = [1.0; 2];
        decode_to_stereo(float, &input, 1, &mut decoded).unwrap();
        assert_eq!(decoded, [0.0, 0.0]);

        let pcm = format(16, 2, false, true, 3);
        let mut encoded = [0u8; 4];
        encode_from_stereo(pcm, &[2.0, -2.0], 1, &mut encoded).unwrap();
        assert_eq!(
            i16::from_le_bytes(encoded[0..2].try_into().unwrap()),
            i16::MAX
        );
        assert_eq!(
            i16::from_le_bytes(encoded[2..4].try_into().unwrap()),
            i16::MIN
        );
    }

    #[test]
    fn mutually_conflicting_encoding_flags_are_rejected() {
        let invalid = format(32, 2, true, true, 3);
        assert_eq!(encoding(invalid), None);
        assert_eq!(
            decode_to_stereo(invalid, &[0; 8], 1, &mut [0.0; 2]),
            Err(ConversionError::UnsupportedFormat)
        );
    }
}
