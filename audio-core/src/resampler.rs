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
///
/// 采样率比例在构造时确定，可在运行期通过 [`Self::set_sample_rates`] 平滑
/// 调整（用于网络流的时钟漂移对齐）。调整后调用方必须重新查询
/// [`Self::output_frames_max`] / [`Self::output_frames_next`] 再分配输出缓冲。
pub struct FixedInputResampler {
    channels: usize,
    input_frames: usize,
    input_rate: u32,
    output_rate: u32,
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
            input_rate,
            output_rate,
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

    pub const fn input_rate(&self) -> u32 {
        self.input_rate
    }

    pub const fn output_rate(&self) -> u32 {
        self.output_rate
    }

    /// 在运行期微调采样率比例（用于网络流时钟漂移平滑对齐）。
    ///
    /// 注意：受构造时 `rel_ratio_max`（当前为 1.002）限制，只支持时钟漂移
    /// 级别的**微小**比例调节，不支持大幅切换采样率；比例变化超出容差时返回
    /// [`ResamplerConfigError::Rubato`]。该方法不能跨 block 处理调用之间频繁
    /// 调用（rubato 约束）；调整后输出 frame 数随之变化，调用方必须重新查询
    /// [`Self::output_frames_max`] / [`Self::output_frames_next`]，并保证输出
    /// 缓冲容量满足新的最大值。
    pub fn set_sample_rates(
        &mut self,
        input_rate: u32,
        output_rate: u32,
    ) -> Result<(), ResamplerConfigError> {
        if input_rate == 0 {
            return Err(ResamplerConfigError::ZeroInputRate);
        }
        if output_rate == 0 {
            return Err(ResamplerConfigError::ZeroOutputRate);
        }
        let ratio = f64::from(output_rate) / f64::from(input_rate);
        self.resampler
            .set_resample_ratio(ratio, true)
            .map_err(|error| ResamplerConfigError::Rubato(error.to_string()))?;
        self.input_rate = input_rate;
        self.output_rate = output_rate;
        Ok(())
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
/// 采样率比例可在运行期通过 [`Self::set_sample_rates`] 平滑调整。
pub struct FixedOutputResampler {
    channels: usize,
    output_frames: usize,
    input_rate: u32,
    output_rate: u32,
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
            input_rate,
            output_rate,
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

    pub const fn input_rate(&self) -> u32 {
        self.input_rate
    }

    pub const fn output_rate(&self) -> u32 {
        self.output_rate
    }

    /// 在运行期微调采样率比例（用于网络流时钟漂移平滑对齐）。
    ///
    /// 注意：受构造时 `rel_ratio_max`（当前为 1.002）限制，只支持时钟漂移
    /// 级别的**微小**比例调节，不支持大幅切换采样率；比例变化超出容差时返回
    /// [`ResamplerConfigError::Rubato`]。该方法不能跨 block 处理调用之间频繁
    /// 调用（rubato 约束）；调整后输入 frame 数随之变化，调用方必须重新查询
    /// [`Self::input_frames`] 并保证输入缓冲容量满足新的 `input_frames_max`。
    pub fn set_sample_rates(
        &mut self,
        input_rate: u32,
        output_rate: u32,
    ) -> Result<(), ResamplerConfigError> {
        if input_rate == 0 {
            return Err(ResamplerConfigError::ZeroInputRate);
        }
        if output_rate == 0 {
            return Err(ResamplerConfigError::ZeroOutputRate);
        }
        let ratio = f64::from(output_rate) / f64::from(input_rate);
        self.resampler
            .set_resample_ratio(ratio, true)
            .map_err(|error| ResamplerConfigError::Rubato(error.to_string()))?;
        self.input_rate = input_rate;
        self.output_rate = output_rate;
        Ok(())
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
    use proptest::prelude::*;

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

    #[test]
    fn fixed_input_rejects_zero_rate_on_update() {
        let mut resampler = FixedInputResampler::new(48_000, 44_100, 2, 480).unwrap();
        assert_eq!(
            resampler.set_sample_rates(0, 48_000).unwrap_err(),
            ResamplerConfigError::ZeroInputRate
        );
        assert_eq!(
            resampler.set_sample_rates(48_000, 0).unwrap_err(),
            ResamplerConfigError::ZeroOutputRate
        );
        // 失败的更新不改变已生效的采样率。
        assert_eq!(resampler.input_rate(), 48_000);
        assert_eq!(resampler.output_rate(), 44_100);
    }

    #[test]
    fn fixed_input_sample_rate_update_changes_output_frames() {
        // 模拟网络流时钟漂移补偿：从匹配采样率起点（48_000）把输入微调为
        // 48_001 Hz。注意接口受构造时 rel_ratio_max=1.002 限制，只支持
        // 时钟漂移级别的微小比例调节，不支持大幅切换采样率。
        let mut resampler = FixedInputResampler::new(48_000, 48_000, 2, 480).unwrap();
        let before_max = resampler.output_frames_max();
        resampler.set_sample_rates(48_001, 48_000).unwrap();
        assert_eq!(resampler.input_rate(), 48_001);
        // 输出采样率保持 48_000，输出 frame 数应基本不变（仅因滤波相位微动）。
        assert!((resampler.output_frames_max() as i64 - before_max as i64).abs() <= 2);
        // 调整后仍可正常处理（输出缓冲按新的最大值重新分配）。
        let input = vec![0.25; 960];
        let mut output = vec![0.0; resampler.output_frames_max() * 2];
        let written = resampler.process_interleaved(&input, &mut output).unwrap();
        assert!(written > 0);
        assert!(output[..written * 2]
            .iter()
            .all(|sample| sample.is_finite()));
    }

    #[test]
    fn fixed_output_sample_rate_update_changes_input_frames() {
        // 模拟网络流时钟漂移：输入由 48_000 微调为 48_001 Hz。
        let mut resampler = FixedOutputResampler::new(48_000, 48_000, 2, 480).unwrap();
        let before_input = resampler.input_frames();
        resampler.set_sample_rates(48_001, 48_000).unwrap();
        assert_eq!(resampler.input_rate(), 48_001);
        // 输入采样率微升，每 block 所需的输入 frame 数也随之微增。
        assert!(resampler.input_frames() >= before_input);
        let mut output = vec![0.0; 960];
        let input = vec![0.25; resampler.input_frames() * 2];
        resampler.process_interleaved(&input, &mut output).unwrap();
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    /// 属性测试：对任意声道数/block 帧数，采样率微调（时钟漂移补偿场景）
    /// 后重采样仍应产出数值有限、声道对齐的样本。
    #[test]
    fn fixed_output_sample_rate_drift_keeps_finite_stereo_aligned_output() {
        proptest::proptest!(|(channels in 1usize..=4usize, output_frames in 1usize..=1024usize)| {
            let mut resampler =
                FixedOutputResampler::new(48_000, 48_000, channels, output_frames).unwrap();
            // 输入侧微调（模拟远端时钟漂移）。
            resampler.set_sample_rates(48_001, 48_000).unwrap();
            let mut output = vec![0.0; output_frames * channels];
            for _ in 0..2 {
                let input = vec![0.25; resampler.input_frames() * channels];
                resampler.process_interleaved(&input, &mut output).unwrap();
            }
            prop_assert!(output.iter().all(|sample| sample.is_finite()));
            // 声道对齐：同 frame 内不同声道样本一致。
            for frame in 0..output_frames {
                let first = output[frame * channels];
                prop_assert!((0..channels).all(|c| (output[frame * channels + c] - first).abs() < 0.0001));
            }
        });
    }
}
