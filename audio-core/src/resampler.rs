//! 固定输出 block 的平台无关重采样器。
//!
//! 该封装以 interleaved `f32` 与音频边界交互，在构造阶段分配 rubato 和临时
//! planar buffer。`process_interleaved` 只在调用方提供的固定切片中读写，不分配。

use rubato::{FastFixedIn, FastFixedOut, PolynomialDegree, Resampler};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ResamplerConfigError {
    #[error("输入采样率必须大于 0")]
    ZeroInputRate,
    #[error("输出采样率必须大于 0")]
    ZeroOutputRate,
    #[error("声道数必须大于 0")]
    ZeroChannels,
    #[error("输出 block frame 数必须大于 0")]
    ZeroOutputFrames,
    #[error("重采样器配置失败: {0}")]
    Rubato(String),
    #[error("重采样样本容量溢出")]
    SampleCountOverflow,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ResamplerProcessError {
    #[error("输入样本数 {actual}，期望 {expected}")]
    InputLength { expected: usize, actual: usize },
    #[error("输出样本数 {actual}，期望 {expected}")]
    OutputLength { expected: usize, actual: usize },
    #[error("重采样处理失败: {0}")]
    Rubato(String),
    #[error("重采样器输出 frame 数 {actual}，期望 {expected}")]
    OutputFrames { expected: usize, actual: usize },
}

/// 固定输入帧数、输出帧数随采样率比例变化的重采样器。
pub struct FixedInputResampler {
    channels: usize,
    input_frames: usize,
    resampler: FastFixedIn<f32>,
    input: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
}

impl FixedInputResampler {
    pub fn new(
        input_rate: u32,
        output_rate: u32,
        channels: usize,
        input_frames: usize,
    ) -> Result<Self, ResamplerConfigError> {
        if input_rate == 0 {
            return Err(ResamplerConfigError::ZeroInputRate);
        }
        if output_rate == 0 {
            return Err(ResamplerConfigError::ZeroOutputRate);
        }
        if channels == 0 {
            return Err(ResamplerConfigError::ZeroChannels);
        }
        if input_frames == 0 {
            return Err(ResamplerConfigError::ZeroOutputFrames);
        }
        input_frames
            .checked_mul(channels)
            .ok_or(ResamplerConfigError::SampleCountOverflow)?;
        let ratio = f64::from(output_rate) / f64::from(input_rate);
        let resampler = FastFixedIn::new(
            ratio,
            1.002,
            PolynomialDegree::Septic,
            input_frames,
            channels,
        )
        .map_err(|error| ResamplerConfigError::Rubato(error.to_string()))?;
        let input = resampler.input_buffer_allocate(true);
        let output = resampler.output_buffer_allocate(true);
        Ok(Self {
            channels,
            input_frames,
            resampler,
            input,
            output,
        })
    }

    pub const fn channels(&self) -> usize {
        self.channels
    }

    pub const fn input_frames(&self) -> usize {
        self.input_frames
    }

    pub fn output_frames_max(&self) -> usize {
        self.resampler.output_frames_max()
    }

    /// 下一次处理预计写出的 frame 数。该值不会小于实际写出数。
    pub fn output_frames_next(&self) -> usize {
        self.resampler.output_frames_next()
    }

    pub fn process_interleaved(
        &mut self,
        input: &[f32],
        output: &mut [f32],
    ) -> Result<usize, ResamplerProcessError> {
        let expected_input = self
            .input_frames
            .checked_mul(self.channels)
            .ok_or(ResamplerProcessError::Rubato("输入样本容量溢出".to_owned()))?;
        if input.len() != expected_input {
            return Err(ResamplerProcessError::InputLength {
                expected: expected_input,
                actual: input.len(),
            });
        }
        let max_output_samples = self
            .output_frames_max()
            .checked_mul(self.channels)
            .ok_or(ResamplerProcessError::Rubato("输出样本容量溢出".to_owned()))?;
        if output.len() < max_output_samples {
            return Err(ResamplerProcessError::OutputLength {
                expected: max_output_samples,
                actual: output.len(),
            });
        }
        for (channel, planar) in self.input.iter_mut().enumerate() {
            for frame in 0..self.input_frames {
                planar[frame] = input[frame * self.channels + channel];
            }
        }
        let (_, written_frames) = self
            .resampler
            .process_into_buffer(&self.input, &mut self.output, None)
            .map_err(|error| ResamplerProcessError::Rubato(error.to_string()))?;
        for (channel, planar) in self.output.iter().enumerate() {
            for frame in 0..written_frames {
                output[frame * self.channels + channel] = planar[frame];
            }
        }
        Ok(written_frames)
    }
}

/// 以固定输出帧数工作的异步重采样器。
///
/// 输入 block 帧数可通过 [`Self::input_frames`] 获取。由于滤波器历史和采样率
/// 比例，该数值不一定等于简单的比例换算；调用方应在每次处理前读取它。
pub struct FixedOutputResampler {
    channels: usize,
    output_frames: usize,
    resampler: FastFixedOut<f32>,
    input: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
}

impl FixedOutputResampler {
    pub fn new(
        input_rate: u32,
        output_rate: u32,
        channels: usize,
        output_frames: usize,
    ) -> Result<Self, ResamplerConfigError> {
        if input_rate == 0 {
            return Err(ResamplerConfigError::ZeroInputRate);
        }
        if output_rate == 0 {
            return Err(ResamplerConfigError::ZeroOutputRate);
        }
        if channels == 0 {
            return Err(ResamplerConfigError::ZeroChannels);
        }
        if output_frames == 0 {
            return Err(ResamplerConfigError::ZeroOutputFrames);
        }
        output_frames
            .checked_mul(channels)
            .ok_or(ResamplerConfigError::SampleCountOverflow)?;
        let ratio = f64::from(output_rate) / f64::from(input_rate);
        let resampler = FastFixedOut::new(
            ratio,
            1.002,
            PolynomialDegree::Septic,
            output_frames,
            channels,
        )
        .map_err(|error| ResamplerConfigError::Rubato(error.to_string()))?;
        resampler
            .input_frames_max()
            .checked_mul(channels)
            .ok_or(ResamplerConfigError::SampleCountOverflow)?;
        let input = resampler.input_buffer_allocate(true);
        let output = resampler.output_buffer_allocate(true);
        Ok(Self {
            channels,
            output_frames,
            resampler,
            input,
            output,
        })
    }

    pub const fn channels(&self) -> usize {
        self.channels
    }

    pub const fn output_frames(&self) -> usize {
        self.output_frames
    }

    pub fn input_frames(&self) -> usize {
        self.resampler.input_frames_next()
    }

    pub fn process_interleaved(
        &mut self,
        input: &[f32],
        output: &mut [f32],
    ) -> Result<(), ResamplerProcessError> {
        let input_frames = self.input_frames();
        let expected_input = input_frames
            .checked_mul(self.channels)
            .ok_or(ResamplerProcessError::Rubato("输入样本容量溢出".to_owned()))?;
        if input.len() != expected_input {
            return Err(ResamplerProcessError::InputLength {
                expected: expected_input,
                actual: input.len(),
            });
        }
        let expected_output = self
            .output_frames
            .checked_mul(self.channels)
            .ok_or(ResamplerProcessError::Rubato("输出样本容量溢出".to_owned()))?;
        if output.len() != expected_output {
            return Err(ResamplerProcessError::OutputLength {
                expected: expected_output,
                actual: output.len(),
            });
        }
        for (channel, planar) in self.input.iter_mut().enumerate() {
            for frame in 0..input_frames {
                planar[frame] = input[frame * self.channels + channel];
            }
        }
        let (_, written_frames) = self
            .resampler
            .process_into_buffer(&self.input, &mut self.output, None)
            .map_err(|error| ResamplerProcessError::Rubato(error.to_string()))?;
        if written_frames != self.output_frames {
            return Err(ResamplerProcessError::OutputFrames {
                expected: self.output_frames,
                actual: written_frames,
            });
        }
        for (channel, planar) in self.output.iter().enumerate() {
            for frame in 0..written_frames {
                output[frame * self.channels + channel] = planar[frame];
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_configuration() {
        assert_eq!(
            FixedOutputResampler::new(0, 48_000, 2, 480).err(),
            Some(ResamplerConfigError::ZeroInputRate)
        );
        assert_eq!(
            FixedOutputResampler::new(44_100, 48_000, 0, 480).err(),
            Some(ResamplerConfigError::ZeroChannels)
        );
    }

    #[test]
    fn validates_interleaved_block_lengths() {
        let mut resampler = FixedOutputResampler::new(44_100, 48_000, 2, 480).unwrap();
        let expected_input = resampler.input_frames() * 2;
        let mut output = vec![0.0; 960];
        assert_eq!(
            resampler
                .process_interleaved(&vec![0.0; expected_input - 1], &mut output)
                .unwrap_err(),
            ResamplerProcessError::InputLength {
                expected: expected_input,
                actual: expected_input - 1,
            }
        );
        assert_eq!(
            resampler
                .process_interleaved(&vec![0.0; expected_input], &mut output[..959])
                .unwrap_err(),
            ResamplerProcessError::OutputLength {
                expected: 960,
                actual: 959,
            }
        );
    }

    #[test]
    fn resamples_stereo_signal_to_fixed_output_block() {
        let mut resampler = FixedOutputResampler::new(44_100, 48_000, 2, 480).unwrap();
        let mut output = vec![0.0; 960];
        for _ in 0..3 {
            let input = vec![0.25; resampler.input_frames() * 2];
            resampler.process_interleaved(&input, &mut output).unwrap();
        }
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(output.iter().any(|sample| *sample > 0.2));
        for frame in 0..480 {
            assert!((output[frame * 2] - output[frame * 2 + 1]).abs() < 0.0001);
        }
    }

    #[test]
    fn fixed_input_resampler_produces_rate_adjusted_frames() {
        let mut resampler = FixedInputResampler::new(48_000, 44_100, 2, 480).unwrap();
        let input = vec![0.25; 960];
        let mut output = vec![0.0; resampler.output_frames_max() * 2];
        let mut total_written = 0;
        for _ in 0..4 {
            let written = resampler.process_interleaved(&input, &mut output).unwrap();
            total_written += written;
            assert!(output[..written * 2]
                .iter()
                .all(|sample| sample.is_finite()));
        }
        assert!((total_written as f64 - 4.0 * 441.0).abs() <= 12.0);
    }
}
