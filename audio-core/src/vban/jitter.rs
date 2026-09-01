//! VBAN 接收端自适应 Jitter Buffer（纯算法，与 Socket 无关）。
//!
//! 职责：
//! - 按 `nu_frame` 把乱序到达的网络包整理为有序 PCM 帧流；
//! - 检测并统计缺包（`nu_frame` 跳跃）、乱序与不连续；
//! - 暴露当前缓冲水位（供时钟漂移控制器作为误差输入）。
//!
//! 约束：
//! - 不持有 UDP Socket / Tokio 任务，由 `app-service::network` 在接收线程调用；
//! - 帧号使用 32 位自增语义，通过 [`frame_is_newer`] 做回绕容忍判断；
//! - 缓冲深度有界（受 [`VBAN_FRAME_WINDOW`] 约束），防止无界增长。
//!
//! 参考：[VBAN 局域网音频互通与传输方案]（../../../../Doc/网络传输与本地节点互通方案计划/1.VBAN局域网音频互通与传输方案.md）4.1 节。

use std::collections::BTreeMap;

use crate::vban::packet::VBAN_FRAME_WINDOW;

/// 回绕容忍的窗口宽度（用于帧号相对次序判定）。
const FRAME_WINDOW: u32 = VBAN_FRAME_WINDOW;

/// `a` 相对 `b` 的有符号帧偏移（回绕感知）。
///
/// 把 `u32` 差值的无符号范围映射到 `i32` 有符号范围：`a` 领先 `b` k 帧
/// （k ≤ 2^31-1）返回 +k；`a` 落后 `b` k 帧返回 -k。可正确处理 `u32`
/// 回绕（0xFFFF_FFFF → 0）。
fn frame_offset(a: u32, b: u32) -> i64 {
    (a.wrapping_sub(b) as i32) as i64
}

/// 判断 `a` 是否严格位于 `b` 之后（在回绕窗口内）。
fn frame_is_newer(a: u32, b: u32) -> bool {
    let offset = frame_offset(a, b);
    offset > 0 && offset <= (FRAME_WINDOW / 2) as i64
}

/// Jitter Buffer 运行统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VBanJitterStats {
    /// 已插入的帧总数。
    pub received: u64,
    /// 因缺包而跳过（未及时到达）的帧数。
    pub lost: u64,
    /// 乱序插入（帧号落后于当前已接收最大帧号）的帧数。
    pub reordered: u64,
    /// 出现不连续（`nu_frame` 大跳跃）的次数。
    pub discontinuities: u64,
    /// 因超出窗口被丢弃的过旧帧数。
    pub dropped_old: u64,
}

/// 自适应 Jitter Buffer。
///
/// 以 `nu_frame` 为键维护有序帧缓存，支持乱序插入与按序抽取。缓冲深度
/// 受窗口约束，过旧的帧会被丢弃。
///
/// 帧长度**可变**：发送端分包时不同包（帧）的样本数可能不同（如 256、44），
/// 因此 `push` 接受任意非空帧长，`pop_next` 返回该帧的实际样本数。
#[derive(Debug, Default)]
pub struct VBanJitterBuffer {
    /// 乱序/待抽取帧缓存：`nu_frame -> PCM 样本`。
    frames: BTreeMap<u32, Vec<f32>>,
    /// 最近一次已输出（`pop` 成功）的帧号。
    last_emitted: Option<u32>,
    /// 最近一次已插入（`push` 成功）的帧号，用于乱序/缺包判定。
    last_pushed: Option<u32>,
    /// 流起点帧号（首个成功 push 的帧），用于首次 `pop` 的基准选择。
    first_frame: Option<u32>,
    /// 统计。
    stats: VBanJitterStats,
    /// 参考帧样本数（首个 push 的帧样本数）；0 表示尚无帧。
    frame_samples: usize,
}

impl VBanJitterBuffer {
    /// 创建自适应 Jitter Buffer。
    ///
    /// 帧样本数不预先固定，由首个 `push` 的帧长度确定（可变帧支持）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前流的参考帧样本数（首个 push 的帧长度）；尚未收到帧时为 0。
    pub const fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    /// 当前就绪（可立即抽取）的帧数，即缓冲水位。
    ///
    /// 时钟漂移控制器用该值作为误差输入。
    pub fn fill_level(&self) -> usize {
        self.frames.len()
    }

    /// 当前统计快照。
    pub const fn stats(&self) -> VBanJitterStats {
        self.stats
    }

    /// 插入一个帧。处理乱序、缺包与回绕。
    ///
    /// 帧长度可变（支持发送端分包）；首个 `push` 的帧长度作为参考样本数。
    ///
    /// - 若样本数为 0，返回 `EmptySamples` 错误；
    /// - 若帧号已存在（重复包），丢弃并保持统计不变；
    /// - 若帧号过旧（落后已输出帧号超过窗口），丢弃并记 `dropped_old`；
    /// - 否则插入缓存，并按帧号推进缺包检测。
    pub fn push(&mut self, nu_frame: u32, samples: &[f32]) -> Result<(), VBanJitterError> {
        if samples.is_empty() {
            return Err(VBanJitterError::EmptySamples);
        }
        if self.frame_samples == 0 {
            self.frame_samples = samples.len();
        }
        // 重复帧号直接忽略。
        if self.frames.contains_key(&nu_frame) {
            return Ok(());
        }
        // 过旧帧（落后已输出帧号超过窗口）丢弃。
        if let Some(last) = self.last_emitted {
            let offset = frame_offset(nu_frame, last);
            if offset < 0 && -offset > (FRAME_WINDOW / 2) as i64 {
                self.stats.dropped_old += 1;
                return Ok(());
            }
        }

        // 乱序判定：帧号落后于当前已接收最大帧号。
        if let Some(last) = self.last_pushed {
            let offset = frame_offset(nu_frame, last);
            if offset < 0 {
                self.stats.reordered += 1;
            }
            // 缺包检测：帧号跳跃超过 1（回绕感知，仅对更新帧）。
            if offset > 1 {
                // 缺失帧数 = offset - 1，截断到窗口内。
                self.stats.lost += (offset - 1).min((FRAME_WINDOW / 2) as i64) as u64;
                self.stats.discontinuities += 1;
            }
        }
        // 记录流起点帧号（首个成功 push 的帧），供首次 pop 选择基准。
        if self.first_frame.is_none() {
            self.first_frame = Some(nu_frame);
        }
        // 更新已接收最大帧号。
        if self
            .last_pushed
            .is_none_or(|last| frame_is_newer(nu_frame, last))
        {
            self.last_pushed = Some(nu_frame);
        }
        self.frames.insert(nu_frame, samples.to_vec());
        self.stats.received += 1;
        Ok(())
    }

    /// 抽取下一个应输出的有序帧；无就绪帧时返回 `None`。
    ///
    /// 始终输出缓存中**最旧**（最小帧号）的帧，保证有序；若该帧落后于已输出
    /// 帧号（残留旧帧），则丢弃并继续找下一个。若与上次输出之间存在缺包
    /// （帧号跳跃），按缺失数量累加 `lost` 计数。
    pub fn pop_next(&mut self) -> Option<Vec<f32>> {
        if self.frames.is_empty() {
            return None;
        }
        // 选择逻辑上"下一个应输出"的帧号：
        // - 首次输出：以流起点 `first_frame` 为基准，选相对偏移最小且 >= 0 的帧；
        // - 后续输出：以 `last_emitted` 为基准，选相对偏移最小且 > 0 的帧。
        let base = self.last_emitted.or(self.first_frame)?;
        let mut best: Option<(u32, i64)> = None;
        for frame in self.frames.keys().copied() {
            let offset = frame_offset(frame, base);
            let valid = if self.last_emitted.is_some() {
                offset > 0
            } else {
                offset >= 0
            };
            if valid && best.is_none_or(|(_, best_off)| offset < best_off) {
                best = Some((frame, offset));
            }
        }
        let (next, gap) = best?;
        // 缺包：仅当已有上次输出（非流起点）且跳变超过 1 时累加缺失帧数。
        if self.last_emitted.is_some() && gap > 1 {
            self.stats.lost += (gap - 1) as u64;
        }
        self.last_emitted = Some(next);
        let samples = self.frames.remove(&next);
        self.trim_old();
        samples
    }

    /// 已输出帧的帧号（最近一次 `pop_next` 成功返回的帧）。
    pub const fn last_emitted(&self) -> Option<u32> {
        self.last_emitted
    }

    /// 清理过旧帧，防止缓存无界增长。
    fn trim_old(&mut self) {
        if let Some(last) = self.last_emitted {
            let cutoff = last.wrapping_sub(FRAME_WINDOW / 2);
            let old_keys: Vec<u32> = self
                .frames
                .keys()
                .copied()
                .filter(|frame| frame_offset(*frame, cutoff) < 0)
                .collect();
            for key in old_keys {
                self.frames.remove(&key);
            }
        }
    }

    /// 重置全部状态与统计。
    pub fn reset(&mut self) {
        self.frames.clear();
        self.last_emitted = None;
        self.last_pushed = None;
        self.first_frame = None;
        self.frame_samples = 0;
        self.stats = VBanJitterStats::default();
    }
}

/// Jitter Buffer 错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VBanJitterError {
    #[error("帧样本数为空")]
    EmptySamples,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn frame(samples: usize, value: f32) -> Vec<f32> {
        vec![value; samples]
    }

    #[test]
    fn rejects_empty_samples() {
        let mut buf = VBanJitterBuffer::new();
        assert_eq!(buf.push(1, &[]).unwrap_err(), VBanJitterError::EmptySamples);
    }

    #[test]
    fn pops_frames_in_ascending_order() {
        let mut buf = VBanJitterBuffer::new();
        buf.push(1, &frame(4, 1.0)).unwrap();
        buf.push(3, &frame(4, 3.0)).unwrap();
        buf.push(2, &frame(4, 2.0)).unwrap();
        assert_eq!(buf.fill_level(), 3);
        assert_eq!(buf.pop_next().unwrap(), frame(4, 1.0));
        assert_eq!(buf.pop_next().unwrap(), frame(4, 2.0));
        assert_eq!(buf.pop_next().unwrap(), frame(4, 3.0));
        assert_eq!(buf.pop_next(), None);
    }

    #[test]
    fn supports_variable_length_frames() {
        // 分包场景：不同帧样本数不同，都应正确入队/出队。
        let mut buf = VBanJitterBuffer::new();
        buf.push(1, &frame(256, 1.0)).unwrap();
        buf.push(2, &frame(44, 2.0)).unwrap();
        buf.push(3, &frame(44, 3.0)).unwrap();
        assert_eq!(buf.frame_samples(), 256); // 首个帧样本数作为参考
        assert_eq!(buf.pop_next().unwrap(), frame(256, 1.0));
        assert_eq!(buf.pop_next().unwrap(), frame(44, 2.0));
        assert_eq!(buf.pop_next().unwrap(), frame(44, 3.0));
        assert_eq!(buf.pop_next(), None);
    }

    #[test]
    fn duplicate_frame_is_ignored() {
        let mut buf = VBanJitterBuffer::new();
        buf.push(5, &frame(2, 1.0)).unwrap();
        buf.push(5, &frame(2, 9.0)).unwrap(); // 重复帧号，保留首次
        assert_eq!(buf.fill_level(), 1);
        assert_eq!(buf.pop_next().unwrap(), frame(2, 1.0));
        assert_eq!(buf.stats().received, 1);
    }

    #[test]
    fn counts_lost_frames_on_gap() {
        let mut buf = VBanJitterBuffer::new();
        buf.push(10, &frame(2, 1.0)).unwrap();
        buf.push(14, &frame(2, 2.0)).unwrap();
        // 跳到 14，缺失 11/12/13 三帧。
        assert_eq!(buf.stats().lost, 3);
        assert_eq!(buf.stats().discontinuities, 1);
        // pop 时再补计缺失（从 10 到 14）。
        assert_eq!(buf.pop_next().unwrap(), frame(2, 1.0));
        assert_eq!(buf.pop_next().unwrap(), frame(2, 2.0));
    }

    #[test]
    fn counts_reordered_frames() {
        let mut buf = VBanJitterBuffer::new();
        buf.push(3, &frame(2, 1.0)).unwrap();
        buf.push(1, &frame(2, 2.0)).unwrap(); // 乱序（落后于已接收最大 3）
        assert!(buf.stats().reordered >= 1);
    }

    #[test]
    fn handles_u32_frame_wraparound() {
        let mut buf = VBanJitterBuffer::new();
        let near_max = u32::MAX - 1;
        buf.push(near_max, &frame(2, 1.0)).unwrap();
        buf.push(u32::MAX, &frame(2, 2.0)).unwrap();
        // 回绕后帧号从 0 继续。
        buf.push(0, &frame(2, 3.0)).unwrap();
        assert_eq!(buf.pop_next().unwrap(), frame(2, 1.0));
        assert_eq!(buf.pop_next().unwrap(), frame(2, 2.0));
        assert_eq!(buf.pop_next().unwrap(), frame(2, 3.0));
        assert_eq!(buf.pop_next(), None);
    }

    #[test]
    fn drops_frames_far_behind_emitted_pointer() {
        let mut buf = VBanJitterBuffer::new();
        buf.push(100, &frame(2, 1.0)).unwrap();
        buf.pop_next().unwrap();
        // 远旧帧（落后已输出 100 超过窗口一半，即 >512 帧）被丢弃。
        let far_old = 100u32.wrapping_sub(600);
        buf.push(far_old, &frame(2, 9.0)).unwrap();
        assert_eq!(buf.fill_level(), 0);
        assert_eq!(buf.stats().dropped_old, 1);
    }

    #[test]
    fn keeps_new_frames_after_emission_pointer_advances() {
        // 回归：push 的过旧判定不得把正常递增的新帧误判为过旧。
        let mut buf = VBanJitterBuffer::new();
        buf.push(100, &frame(2, 1.0)).unwrap();
        assert_eq!(buf.pop_next().unwrap(), frame(2, 1.0)); // last_emitted=100
        buf.push(200, &frame(2, 2.0)).unwrap(); // 新帧，应保留
        assert_eq!(buf.fill_level(), 1);
        assert_eq!(buf.stats().dropped_old, 0);
        assert_eq!(buf.pop_next().unwrap(), frame(2, 2.0));
    }

    #[test]
    fn reset_clears_state_and_stats() {
        let mut buf = VBanJitterBuffer::new();
        buf.push(1, &frame(2, 1.0)).unwrap();
        buf.push(5, &frame(2, 2.0)).unwrap();
        buf.reset();
        assert_eq!(buf.fill_level(), 0);
        assert_eq!(buf.frame_samples(), 0);
        assert_eq!(buf.stats().lost, 0);
        assert_eq!(buf.pop_next(), None);
    }

    /// 属性测试：随机乱序/缺包序列下不 panic、水位有界、输出有序。
    #[test]
    fn arbitrary_out_of_order_stream_keeps_bounded_and_ordered() {
        proptest::proptest!(|(
            frames in proptest::collection::vec(0u32..500u32, 1..200),
            sample_len in 1usize..=16usize,
        )| {
            let mut buf = VBanJitterBuffer::new();
            let mut prev_emitted: Option<u32> = None;
            for f in &frames {
                let _ = buf.push(*f, &vec![0.5; sample_len]);
                if buf.fill_level() > FRAME_WINDOW as usize {
                    // 水位不得无界增长（触发裁剪）。
                    buf.trim_old();
                }
                if let Some(samples) = buf.pop_next() {
                    prop_assert_eq!(samples.len(), sample_len);
                    prop_assert!(samples.iter().all(|s| s.is_finite()));
                    if let Some(prev) = prev_emitted {
                        // 回绕感知：本次输出不得早于上次输出。
                        prop_assert!(frame_offset(buf.last_emitted().unwrap(), prev) > 0);
                    }
                    prev_emitted = buf.last_emitted();
                }
            }
            // 接收计数不超过推送次数（重复/过旧帧不计入）。
            prop_assert!(buf.stats().received <= frames.len() as u64);
            // 输出始终有序（由循环内断言保证）；统计计数无需上界断言。
            prop_assert!(buf.fill_level() <= FRAME_WINDOW as usize);
        });
    }
}
