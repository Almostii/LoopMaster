//! 平台无关的测试音生成（阶段 B.6）。
//!
//! 用于引擎自测、声道识别与硬件验收：正弦、周期脉冲、静音与多声道
//! 识别音（左 440 Hz / 右 880 Hz / 其余 1320 Hz）。生成逻辑只依赖
//! [`crate::INTERNAL_SAMPLE_RATE`]，不接触设备。

/// 测试音类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestToneKind {
    /// 正弦连续音（默认 440 Hz）。
    Sine,
    /// 周期脉冲（默认 2 Hz，用于延迟与时钟测量）。
    Impulse,
    /// 全静音（基线/电平验证）。
    Silence,
    /// 声道识别：左声道 440 Hz、右声道 1300 Hz、其余声道 1760 Hz。
    ChannelId,
}

/// 测试音参数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TestToneConfig {
    pub kind: TestToneKind,
    /// Sine/Impulse 的基频（Hz）；ChannelId 固定 440/1300/1760。
    pub frequency_hz: f32,
    /// 幅度（0.0~1.0）。
    pub amplitude: f32,
}

impl Default for TestToneConfig {
    fn default() -> Self {
        Self {
            kind: TestToneKind::Sine,
            frequency_hz: 440.0,
            amplitude: 0.5,
        }
    }
}

/// 相位状态：跨 block 保持，保证 block 间波形连续。
#[derive(Clone, Copy, Debug, Default)]
pub struct TonePhase {
    frame: u64,
}

/// 填充一个交错 `f32` block。`block.len()` 必须能被 `channels` 整除。
/// 同一 frame 的所有声道共享同一相位（时间点），保证交错声道对齐。
pub fn fill_block(
    block: &mut [f32],
    channels: usize,
    config: &TestToneConfig,
    phase: &mut TonePhase,
) {
    debug_assert!(channels > 0);
    debug_assert!(block.len().is_multiple_of(channels));
    let frames = block.len() / channels;
    let sample_rate = crate::INTERNAL_SAMPLE_RATE as f64;
    for frame in 0..frames {
        let n = phase.frame as f64;
        for channel in 0..channels {
            let sample = match config.kind {
                TestToneKind::Silence => 0.0,
                TestToneKind::Sine => {
                    let frequency = config.frequency_hz.max(1.0) as f64;
                    (2.0 * std::f64::consts::PI * frequency * n / sample_rate).sin() as f32
                        * config.amplitude
                }
                TestToneKind::Impulse => {
                    let period =
                        (sample_rate / config.frequency_hz.max(1.0) as f64).max(1.0) as u64;
                    if (n as u64).is_multiple_of(period) {
                        config.amplitude
                    } else {
                        0.0
                    }
                }
                TestToneKind::ChannelId => {
                    let frequency = match channel {
                        0 => 440.0,
                        1 => 1300.0,
                        _ => 1760.0,
                    };
                    (2.0 * std::f64::consts::PI * frequency * n / sample_rate).sin() as f32
                        * config.amplitude
                }
            };
            block[frame * channels + channel] = sample;
        }
        phase.frame += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(kind: TestToneKind, frames: usize, channels: usize) -> Vec<f32> {
        let mut block = vec![0.0f32; frames * channels];
        let mut phase = TonePhase::default();
        fill_block(
            &mut block,
            channels,
            &TestToneConfig {
                kind,
                ..TestToneConfig::default()
            },
            &mut phase,
        );
        block
    }

    #[test]
    fn silence_is_all_zero() {
        assert_eq!(fill(TestToneKind::Silence, 480, 2), vec![0.0; 960]);
    }

    #[test]
    fn sine_starts_at_zero_and_oscillates() {
        let block = fill(TestToneKind::Sine, 480, 2);
        // sin(0)=0，两个声道起始样本为 0。
        assert_eq!(block[0], 0.0);
        assert_eq!(block[1], 0.0);
        // 峰值为幅度 0.5（在周期内应出现接近 ±0.5 的样本）。
        let peak = block.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.02);
    }

    #[test]
    fn impulse_is_periodic() {
        let mut block = vec![0.0f32; 480];
        let mut phase = TonePhase::default();
        fill_block(
            &mut block,
            1,
            &TestToneConfig {
                kind: TestToneKind::Impulse,
                frequency_hz: 2.0,
                ..TestToneConfig::default()
            },
            &mut phase,
        );
        assert_eq!(block[0], 0.5);
        // 2 Hz 周期 = 24000 样本，480 帧内除首样本外全 0。
        assert!(block[1..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn channel_id_uses_distinct_frequencies() {
        let block = fill(TestToneKind::ChannelId, 480, 2);
        // 第 100 帧：左右声道同一时刻的样本值应不同（440 vs 880 Hz 相位不同）。
        let left = block[100 * 2];
        let right = block[100 * 2 + 1];
        assert!((left - right).abs() > 0.01);
    }

    #[test]
    fn phase_keeps_waveform_continuous_across_blocks() {
        let mut phase = TonePhase::default();
        let mut first = vec![0.0f32; 960];
        let mut second = vec![0.0f32; 960];
        let config = TestToneConfig::default();
        fill_block(&mut first, 2, &config, &mut phase);
        fill_block(&mut second, 2, &config, &mut phase);
        // 若相位连续，第二块的起始样本应与第一块末尾相位衔接（sin 连续）。
        let expected_next =
            (2.0 * std::f64::consts::PI * 440.0 * 480.0 / 48_000.0).sin() as f32 * 0.5;
        assert!((second[0] - expected_next).abs() < 1e-3);
    }
}
