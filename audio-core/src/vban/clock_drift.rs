//! VBAN 时钟漂移 PI 补偿控制器（纯算法，与回调周期解耦）。
//!
//! 两台电脑的物理音频时钟存在微小频偏（如 48_001 Hz vs 48_000 Hz），长期运行
//! 会导致 Jitter Buffer 逐渐溢出（Overflow）或下溢（Underflow）。本控制器基于
//! 缓冲水位，输出一个采样率比例调节因子（`1.0 + correction`），供
//! [`crate::resampler`] 的 `set_sample_rates` 微调对齐。
//!
//! 纯比例（P）控制存在稳态误差，无法完全消除持续漂移，因此采用 PI 控制器，
//! 积分项负责消除长期漂移。**所有控制参数（`kp`/`ki`/限幅）为初始候选值，
//! 必须通过原型/真机测试报告后才能冻结为推荐值。**
//!
//! 参考：[VBAN 局域网音频互通与传输方案]（../../../../Doc/网络传输与本地节点互通方案计划/1.VBAN局域网音频互通与传输方案.md）4.2 节。

/// PI 控制器默认参数（**初始候选值，待真机/仿真调参，非发布承诺**）。
pub mod defaults {
    /// 比例增益。**初始候选值**。
    pub const KP: f64 = 0.002;
    /// 积分增益（每秒）。**初始候选值**。
    pub const KI_PER_SECOND: f64 = 0.0005;
    /// 积分项累积上限（秒），防止积分饱和。**初始候选值**。
    pub const INTEGRAL_LIMIT_SECONDS: f64 = 1.0;
    /// 比例调节因子最大偏移（如 0.002 = ±0.2%），与 resampler `rel_ratio_max`
    /// 的容差相匹配。**初始候选值**。
    pub const MAX_RATIO_OFFSET: f64 = 0.002;
}

/// 时钟漂移 PI 控制器。
///
/// `update` 接收当前缓冲水位与距上次调用的时间（秒），输出采样率比例因子
/// `1.0 + correction`。由于与调用周期解耦（使用 `dt_seconds`），可被任意频率
/// 的定时任务调用而行为一致。
#[derive(Debug, Clone)]
pub struct ClockDriftCompensator {
    /// 期望的目标水位（帧数）。**初始候选值**。
    target_fill: f64,
    /// 归一化比例误差（无单位）。
    kp: f64,
    /// 积分增益（每秒）。
    ki_per_second: f64,
    /// 积分项累积上限（秒）。
    integral_limit_seconds: f64,
    /// 输出比例因子最大偏移。
    max_ratio_offset: f64,
    /// 积分项（秒）。
    integral_seconds: f64,
}

impl ClockDriftCompensator {
    /// 创建控制器；`target_fill` 为期望水位（帧数，> 0）。
    pub fn new(target_fill: f64) -> Self {
        Self {
            target_fill,
            kp: defaults::KP,
            ki_per_second: defaults::KI_PER_SECOND,
            integral_limit_seconds: defaults::INTEGRAL_LIMIT_SECONDS,
            max_ratio_offset: defaults::MAX_RATIO_OFFSET,
            integral_seconds: 0.0,
        }
    }

    /// 创建控制器并覆盖全部参数（用于真机调参）。
    pub fn with_parameters(
        target_fill: f64,
        kp: f64,
        ki_per_second: f64,
        integral_limit_seconds: f64,
        max_ratio_offset: f64,
    ) -> Self {
        Self {
            target_fill,
            kp,
            ki_per_second,
            integral_limit_seconds,
            max_ratio_offset,
            integral_seconds: 0.0,
        }
    }

    /// 期望的目标水位（帧数）。
    pub const fn target_fill(&self) -> f64 {
        self.target_fill
    }

    /// 当前积分项（秒）。
    pub const fn integral_seconds(&self) -> f64 {
        self.integral_seconds
    }

    /// 根据当前缓冲水位与距上次调用的时间，输出采样率比例因子 `1.0 + correction`。
    ///
    /// - 误差 = (当前水位 - 目标水位) / 目标水位（归一化）；
    /// - 积分项用 `dt_seconds` 累积并限幅；
    /// - 输出夹在 `[1.0 - max_ratio_offset, 1.0 + max_ratio_offset]`。
    pub fn update(&mut self, current_fill: usize, dt_seconds: f64) -> f64 {
        let target = self.target_fill.max(1.0);
        let error = (current_fill as f64 - target) / target;
        self.integral_seconds = (self.integral_seconds + error * dt_seconds)
            .clamp(-self.integral_limit_seconds, self.integral_limit_seconds);
        let correction = (self.kp * error + self.ki_per_second * self.integral_seconds)
            .clamp(-self.max_ratio_offset, self.max_ratio_offset);
        1.0 + correction
    }

    /// 重置积分项，使控制器从零漂移假设重新开始。
    pub fn reset(&mut self) {
        self.integral_seconds = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn returns_ratio_one_at_target_fill() {
        let mut comp = ClockDriftCompensator::new(8.0);
        let ratio = comp.update(8, 0.01);
        assert!((ratio - 1.0).abs() < 1e-9);
        assert_eq!(comp.integral_seconds(), 0.0);
    }

    #[test]
    fn overfill_pushes_ratio_above_one() {
        // 水位高于目标 → 误差为正 → correction 为正 → ratio > 1。
        let mut comp = ClockDriftCompensator::new(8.0);
        let ratio = comp.update(16, 0.01);
        assert!(ratio > 1.0);
    }

    #[test]
    fn underfill_pulls_ratio_below_one() {
        // 水位低于目标 → ratio < 1。
        let mut comp = ClockDriftCompensator::new(8.0);
        let ratio = comp.update(4, 0.01);
        assert!(ratio < 1.0);
    }

    #[test]
    fn output_is_clamped_to_max_ratio_offset() {
        // 极端水位持续输入，输出应被限幅在 [1-offset, 1+offset]。
        let mut comp = ClockDriftCompensator::with_parameters(8.0, 0.01, 0.01, 10.0, 0.002);
        for _ in 0..1000 {
            let ratio = comp.update(1000, 0.1);
            assert!((ratio - 1.0).abs() <= 0.002 + 1e-9);
        }
    }

    #[test]
    fn integral_accumulates_to_eliminate_steady_state_error() {
        // 持续偏高水位，积分项应单调累积（消除稳态误差）。
        let mut comp = ClockDriftCompensator::with_parameters(8.0, 0.0, 0.001, 10.0, 0.002);
        let mut last_integral = 0.0;
        for _ in 0..50 {
            let ratio = comp.update(12, 0.1);
            assert!(ratio > 1.0);
            assert!(comp.integral_seconds() >= last_integral);
            last_integral = comp.integral_seconds();
        }
        assert!(comp.integral_seconds() > 0.0);
    }

    #[test]
    fn update_is_decoupled_from_callback_period() {
        // 相同累积时间下，不同 dt 拆分得到的积分项应近似一致（解耦调用周期）。
        let mut a = ClockDriftCompensator::new(8.0);
        let mut b = ClockDriftCompensator::new(8.0);
        for _ in 0..100 {
            let _ = a.update(12, 0.1);
        }
        for _ in 0..10 {
            let _ = b.update(12, 1.0);
        }
        assert!((a.integral_seconds() - b.integral_seconds()).abs() < 1e-6);
    }

    #[test]
    fn reset_clears_integral() {
        let mut comp = ClockDriftCompensator::new(8.0);
        for _ in 0..20 {
            let _ = comp.update(20, 0.1);
        }
        assert!(comp.integral_seconds() > 0.0);
        comp.reset();
        assert_eq!(comp.integral_seconds(), 0.0);
        assert!((comp.update(8, 0.1) - 1.0).abs() < 1e-9);
    }

    /// 属性测试：任意水位与 dt 下输出有限且始终在限幅范围内。
    #[test]
    fn arbitrary_inputs_remain_bounded_and_finite() {
        proptest::proptest!(|(
            fill in 0usize..=4096usize,
            dt in 0.0f64..0.2f64,
            target in 1.0f64..256.0f64,
        )| {
            let mut comp = ClockDriftCompensator::new(target);
            let ratio = comp.update(fill, dt);
            prop_assert!(ratio.is_finite());
            prop_assert!((ratio - 1.0).abs() <= defaults::MAX_RATIO_OFFSET + 1e-9);
        });
    }
}
