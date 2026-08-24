//! 平台无关的固定 block 音频混音计划。
//!
//! [`MixerPlan`] 在非实时线程由 [`RouteGraph`] 编译得到。编译阶段完成 ID 查找、
//! 增益换算和通道边界校验；实时 [`MixerPlan::process`] 只使用调用方提供的切片，
//! 不分配内存、不加锁、不等待。

use crate::{RouteGraph, RouteGraphError};
use thiserror::Error;

/// 固定 block 混音计划构建或执行时的错误。
#[derive(Debug, Error, PartialEq)]
pub enum MixerError {
    /// block frame 数必须大于 0。
    #[error("block frame 数必须大于 0")]
    ZeroBlockFrames,
    /// 输入 source 声道数必须大于 0。
    #[error("输入声道数必须大于 0")]
    ZeroSourceChannels,
    /// 输出 sink 声道数必须大于 0。
    #[error("输出声道数必须大于 0")]
    ZeroSinkChannels,
    /// 路由图中的 source 不存在。
    #[error("路由图错误: {0}")]
    Graph(#[from] RouteGraphError),
    /// 显式映射中的 source 声道越界。
    #[error("send {send_index} 的 source channel {channel} 超出范围 {channels}")]
    SourceChannelOutOfRange {
        send_index: usize,
        channel: u16,
        channels: usize,
    },
    /// 显式映射中的 sink 声道越界。
    #[error("send {send_index} 的 sink channel {channel} 超出范围 {channels}")]
    SinkChannelOutOfRange {
        send_index: usize,
        channel: u16,
        channels: usize,
    },
    /// process 收到的 source 数量与计划不一致。
    #[error("source block 数量 {actual} 与计划要求 {expected} 不一致")]
    SourceCountMismatch { expected: usize, actual: usize },
    /// process 收到的 sink 数量与计划不一致。
    #[error("sink block 数量 {actual} 与计划要求 {expected} 不一致")]
    SinkCountMismatch { expected: usize, actual: usize },
    /// source block 不是完整的固定长度 interleaved block。
    #[error("source {index} 样本长度 {actual}，期望 {expected}")]
    SourceBlockLength {
        index: usize,
        expected: usize,
        actual: usize,
    },
    /// source block 包含不完整的 interleaved frame。
    #[error("source {index} 样本长度 {samples} 不能按 {channels} 声道对齐")]
    SourceBlockUnaligned {
        index: usize,
        samples: usize,
        channels: usize,
    },
    /// sink block 不是完整的固定长度 interleaved block。
    #[error("sink {index} 样本长度 {actual}，期望 {expected}")]
    SinkBlockLength {
        index: usize,
        expected: usize,
        actual: usize,
    },
    /// 固定 block 的 frame 数和声道数相乘超出 `usize`。
    #[error("block 样本容量溢出")]
    BlockSampleCountOverflow,
}

#[derive(Clone, Debug)]
struct CompiledSend {
    source_index: usize,
    sink_index: usize,
    gain_linear: f32,
    muted: bool,
    /// `enabled=false` 的 send 保留配置但从不参与混音（跳过）。
    enabled: bool,
    /// 空切片表示 identity 映射；非空切片保存 source -> sink 映射。
    channel_map: Vec<(usize, usize)>,
}

/// 非实时构建、实时复用的固定 block 混音计划。
///
/// 一个计划固定 source/sink 数量、每个 block 的 frame 数以及所有 block 的声道数。
/// `process` 要求 source 和 sink 切片分别按 `RouteGraph.sources` 与
/// `RouteGraph.sinks` 的顺序传入。source 可以少于固定 block 长度，缺少的尾部帧
/// 按静音处理；sink 必须始终提供完整固定长度。
#[derive(Clone, Debug)]
pub struct MixerPlan {
    block_frames: usize,
    source_channels: usize,
    sink_channels: usize,
    source_count: usize,
    sink_count: usize,
    sends: Vec<CompiledSend>,
}

impl MixerPlan {
    /// 从路由图编译混音计划。
    ///
    /// 此方法允许分配，用于配置变更或启动阶段；生成的计划应在实时线程中长期复用。
    /// `channel_map` 的每一项解释为 `(source_channel, sink_channel)`。空映射表示
    /// 按相同声道序号连接，超出任一端声道数的部分不参与混音。
    pub fn new(
        graph: &RouteGraph,
        block_frames: usize,
        source_channels: usize,
        sink_channels: usize,
    ) -> Result<Self, MixerError> {
        if block_frames == 0 {
            return Err(MixerError::ZeroBlockFrames);
        }
        if source_channels == 0 {
            return Err(MixerError::ZeroSourceChannels);
        }
        if sink_channels == 0 {
            return Err(MixerError::ZeroSinkChannels);
        }
        block_frames
            .checked_mul(source_channels)
            .ok_or(MixerError::BlockSampleCountOverflow)?;
        block_frames
            .checked_mul(sink_channels)
            .ok_or(MixerError::BlockSampleCountOverflow)?;
        graph.validate()?;

        let mut sends = Vec::with_capacity(graph.sends.len());
        for (send_index, send) in graph.sends.iter().enumerate() {
            let source_index = graph
                .sources
                .iter()
                .position(|source| source.id == send.source_id)
                .ok_or_else(|| {
                    MixerError::Graph(RouteGraphError::MissingSource(send.source_id.0.clone()))
                })?;
            let sink_index = graph
                .sinks
                .iter()
                .position(|sink| sink.id == send.sink_id)
                .ok_or_else(|| {
                    MixerError::Graph(RouteGraphError::MissingSink(send.sink_id.0.clone()))
                })?;

            let mut channel_map = Vec::with_capacity(send.channel_map.len());
            for &(source_channel, sink_channel) in &send.channel_map {
                if usize::from(source_channel) >= source_channels {
                    return Err(MixerError::SourceChannelOutOfRange {
                        send_index,
                        channel: source_channel,
                        channels: source_channels,
                    });
                }
                if usize::from(sink_channel) >= sink_channels {
                    return Err(MixerError::SinkChannelOutOfRange {
                        send_index,
                        channel: sink_channel,
                        channels: sink_channels,
                    });
                }
                channel_map.push((usize::from(source_channel), usize::from(sink_channel)));
            }

            sends.push(CompiledSend {
                source_index,
                sink_index,
                gain_linear: db_to_linear(send.gain_db),
                muted: send.muted,
                enabled: send.enabled,
                channel_map,
            });
        }

        Ok(Self {
            block_frames,
            source_channels,
            sink_channels,
            source_count: graph.sources.len(),
            sink_count: graph.sinks.len(),
            sends,
        })
    }

    /// 计划固定的 block frame 数。
    pub const fn block_frames(&self) -> usize {
        self.block_frames
    }

    /// 每个 source block 的固定声道数。
    pub const fn source_channels(&self) -> usize {
        self.source_channels
    }

    /// 每个 sink block 的固定声道数。
    pub const fn sink_channels(&self) -> usize {
        self.sink_channels
    }

    /// 计划要求的 source block 数量。
    pub const fn source_count(&self) -> usize {
        self.source_count
    }

    /// 计划要求的 sink block 数量。
    pub const fn sink_count(&self) -> usize {
        self.sink_count
    }

    /// 将一组固定长度 interleaved source block 混音到 sink block。
    ///
    /// 调用方必须保证 `source_blocks` 和 `sink_blocks` 的切片引用在调用期间有效。
    /// 输出 block 会先清零；短 source block 的缺失尾帧保持为静音。该方法不做削波，
    /// 因此多个 source 累加后可以超过 `[-1, 1]`。
    /// 该策略保留峰值信息，削波或 limiter 应在明确的后续 DSP 阶段实现。
    pub fn process(
        &self,
        source_blocks: &[&[f32]],
        sink_blocks: &mut [&mut [f32]],
    ) -> Result<(), MixerError> {
        if source_blocks.len() != self.source_count {
            return Err(MixerError::SourceCountMismatch {
                expected: self.source_count,
                actual: source_blocks.len(),
            });
        }
        if sink_blocks.len() != self.sink_count {
            return Err(MixerError::SinkCountMismatch {
                expected: self.sink_count,
                actual: sink_blocks.len(),
            });
        }

        let source_block_samples = self.block_frames * self.source_channels;
        let sink_block_samples = self.block_frames * self.sink_channels;
        for (index, source) in source_blocks.iter().enumerate() {
            if source.len() > source_block_samples {
                return Err(MixerError::SourceBlockLength {
                    index,
                    expected: source_block_samples,
                    actual: source.len(),
                });
            }
            if !source.len().is_multiple_of(self.source_channels) {
                return Err(MixerError::SourceBlockUnaligned {
                    index,
                    samples: source.len(),
                    channels: self.source_channels,
                });
            }
        }
        for (index, sink) in sink_blocks.iter().enumerate() {
            if sink.len() != sink_block_samples {
                return Err(MixerError::SinkBlockLength {
                    index,
                    expected: sink_block_samples,
                    actual: sink.len(),
                });
            }
        }

        for sink in sink_blocks.iter_mut() {
            sink.fill(0.0);
        }

        for send in &self.sends {
            if send.muted || !send.enabled {
                continue;
            }
            let source = source_blocks[send.source_index];
            let sink = &mut sink_blocks[send.sink_index];
            let available_frames = source.len() / self.source_channels;
            if send.channel_map.is_empty() {
                let mapped_channels = self.source_channels.min(self.sink_channels);
                for frame in 0..available_frames {
                    let source_base = frame * self.source_channels;
                    let sink_base = frame * self.sink_channels;
                    for channel in 0..mapped_channels {
                        sink[sink_base + channel] +=
                            source[source_base + channel] * send.gain_linear;
                    }
                }
            } else {
                for frame in 0..available_frames {
                    let source_base = frame * self.source_channels;
                    let sink_base = frame * self.sink_channels;
                    for &(source_channel, sink_channel) in &send.channel_map {
                        sink[sink_base + sink_channel] +=
                            source[source_base + source_channel] * send.gain_linear;
                    }
                }
            }
        }
        Ok(())
    }
}

fn db_to_linear(gain_db: f32) -> f32 {
    10.0_f32.powf(gain_db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EndpointId, SendSpec, SinkId, SinkSpec, SourceId, SourceKind, SourceSpec};

    fn source(id: &str) -> SourceSpec {
        SourceSpec {
            id: SourceId(id.to_owned()),
            kind: SourceKind::DeviceCapture,
            endpoint_id: None,
            process_id: None,
            display_name: id.to_owned(),
        }
    }

    fn sink(id: &str) -> SinkSpec {
        SinkSpec {
            id: SinkId(id.to_owned()),
            endpoint_id: EndpointId(id.to_owned()),
            display_name: id.to_owned(),
        }
    }

    fn graph(sources: &[&str], sends: Vec<SendSpec>) -> RouteGraph {
        RouteGraph {
            sources: sources.iter().map(|id| source(id)).collect(),
            sinks: vec![sink("sink")],
            sends,
        }
    }

    fn send(source_id: &str, gain_db: f32, muted: bool, channel_map: Vec<(u16, u16)>) -> SendSpec {
        SendSpec {
            source_id: SourceId(source_id.to_owned()),
            sink_id: SinkId("sink".to_owned()),
            gain_db,
            muted,
            enabled: true,
            channel_map,
        }
    }

    fn process(plan: &MixerPlan, sources: &[Vec<f32>]) -> Vec<f32> {
        let source_refs: Vec<&[f32]> = sources.iter().map(Vec::as_slice).collect();
        let mut output = vec![99.0; plan.block_frames() * plan.sink_channels()];
        let mut sink_refs = vec![output.as_mut_slice()];
        plan.process(&source_refs, &mut sink_refs).unwrap();
        output
    }

    #[test]
    fn zero_db_identity_and_muted_send() {
        let graph0 = graph(&["source"], vec![send("source", 0.0, false, Vec::new())]);
        let plan = MixerPlan::new(&graph0, 2, 2, 2).unwrap();
        assert_eq!(
            process(&plan, &[vec![1.0, -2.0, 3.0, -4.0]]),
            vec![1.0, -2.0, 3.0, -4.0]
        );

        let muted_graph = graph(&["source"], vec![send("source", 0.0, true, Vec::new())]);
        let muted_plan = MixerPlan::new(&muted_graph, 2, 2, 2).unwrap();
        assert_eq!(
            process(&muted_plan, &[vec![1.0, 2.0, 3.0, 4.0]]),
            vec![0.0; 4]
        );
    }

    #[test]
    fn disabled_send_is_skipped_but_re_enabled_restores_configuration() {
        // enabled=false 的 send 从混音计划跳过，输出为静音。
        let mut disabled = send("source", 0.0, false, Vec::new());
        disabled.enabled = false;
        let disabled_graph = graph(&["source"], vec![disabled]);
        let disabled_plan = MixerPlan::new(&disabled_graph, 2, 2, 2).unwrap();
        assert_eq!(
            process(&disabled_plan, &[vec![1.0, -2.0, 3.0, -4.0]]),
            vec![0.0; 4]
        );

        // 重新启用后增益与通道映射配置原样恢复（与 muted 的静音语义不同）。
        let enabled_graph = graph(&["source"], vec![send("source", 0.0, false, Vec::new())]);
        let enabled_plan = MixerPlan::new(&enabled_graph, 2, 2, 2).unwrap();
        assert_eq!(
            process(&enabled_plan, &[vec![1.0, -2.0, 3.0, -4.0]]),
            vec![1.0, -2.0, 3.0, -4.0]
        );
    }

    #[test]
    fn applies_gain_and_accumulates_sources() {
        let graph = graph(
            &["a", "b"],
            vec![
                send("a", 0.0, false, Vec::new()),
                send("b", 6.0206, false, Vec::new()),
            ],
        );
        let plan = MixerPlan::new(&graph, 1, 1, 1).unwrap();
        let output = process(&plan, &[vec![0.5], vec![0.5]]);
        assert!((output[0] - 1.5).abs() < 0.0002);
    }

    #[test]
    fn applies_explicit_channel_map() {
        let graph = graph(
            &["source"],
            vec![send("source", 0.0, false, vec![(0, 1), (1, 0)])],
        );
        let plan = MixerPlan::new(&graph, 1, 2, 2).unwrap();
        assert_eq!(process(&plan, &[vec![1.0, 2.0]]), vec![2.0, 1.0]);
    }

    #[test]
    fn rejects_invalid_mapping_and_block_boundaries() {
        let invalid = graph(&["source"], vec![send("source", 0.0, false, vec![(2, 0)])]);
        assert_eq!(
            MixerPlan::new(&invalid, 1, 2, 2).unwrap_err(),
            MixerError::SourceChannelOutOfRange {
                send_index: 0,
                channel: 2,
                channels: 2,
            }
        );

        let invalid_sink = graph(&["source"], vec![send("source", 0.0, false, vec![(0, 2)])]);
        assert_eq!(
            MixerPlan::new(&invalid_sink, 1, 2, 2).unwrap_err(),
            MixerError::SinkChannelOutOfRange {
                send_index: 0,
                channel: 2,
                channels: 2,
            }
        );

        let valid = graph(&["source"], vec![send("source", 0.0, false, Vec::new())]);
        let plan = MixerPlan::new(&valid, 2, 2, 2).unwrap();
        let source = vec![1.0, 2.0, 3.0];
        let source_refs = vec![source.as_slice()];
        let mut output = vec![0.0; 4];
        let mut sink_refs = vec![output.as_mut_slice()];
        assert_eq!(
            plan.process(&source_refs, &mut sink_refs).unwrap_err(),
            MixerError::SourceBlockUnaligned {
                index: 0,
                samples: 3,
                channels: 2,
            }
        );
    }

    #[test]
    fn rejects_sample_count_overflow() {
        let graph = graph(&["source"], vec![]);
        assert_eq!(
            MixerPlan::new(&graph, usize::MAX, 2, 2).unwrap_err(),
            MixerError::BlockSampleCountOverflow
        );
    }

    #[test]
    fn treats_short_source_as_silence() {
        let graph = graph(&["source"], vec![send("source", 0.0, false, Vec::new())]);
        let plan = MixerPlan::new(&graph, 2, 1, 1).unwrap();
        assert_eq!(process(&plan, &[vec![4.0]]), vec![4.0, 0.0]);
    }

    #[test]
    fn explicitly_does_not_clip_accumulated_output() {
        let graph = graph(
            &["a", "b"],
            vec![
                send("a", 0.0, false, Vec::new()),
                send("b", 0.0, false, Vec::new()),
            ],
        );
        let plan = MixerPlan::new(&graph, 1, 1, 1).unwrap();
        assert_eq!(process(&plan, &[vec![1.0], vec![1.0]]), vec![2.0]);
    }
}
