//! 平台无关的固定 block 两阶段混音计划。
//!
//! [`MixerPlan`] 在非实时线程由 [`RouteGraph`] 编译得到。编译阶段完成 ID 查找、
//! 增益换算和通道边界校验；实时 [`MixerPlan::process`] 只使用调用方提供的切片与
//! 计划内预分配的 Bus 缓冲，不分配内存、不加锁、不等待。

use crate::{RouteGraph, RouteGraphError, SendId, SendSpec};
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum MixerError {
    #[error("block frame 数必须大于 0")]
    ZeroBlockFrames,
    #[error("输入 source 声道数必须大于 0")]
    ZeroSourceChannels,
    #[error("输出 sink 声道数必须大于 0")]
    ZeroSinkChannels,
    #[error("路由图错误: {0}")]
    Graph(#[from] RouteGraphError),
    #[error("send {send_index} 的 source channel {channel} 超出范围 {channels}")]
    SourceChannelOutOfRange {
        send_index: usize,
        channel: u16,
        channels: usize,
    },
    #[error("send {send_index} 的 bus channel {channel} 超出范围 {channels}")]
    BusChannelOutOfRange {
        send_index: usize,
        channel: u16,
        channels: usize,
    },
    #[error("send {send_index} 的 sink channel {channel} 超出范围 {channels}")]
    SinkChannelOutOfRange {
        send_index: usize,
        channel: u16,
        channels: usize,
    },
    #[error("source block 数量 {actual} 与计划要求 {expected} 不一致")]
    SourceCountMismatch { expected: usize, actual: usize },
    #[error("sink block 数量 {actual} 与计划要求 {expected} 不一致")]
    SinkCountMismatch { expected: usize, actual: usize },
    #[error("source {index} 样本长度 {actual}，期望不超过 {expected}")]
    SourceBlockLength {
        index: usize,
        expected: usize,
        actual: usize,
    },
    #[error("source {index} 样本长度 {samples} 不能按 {channels} 声道对齐")]
    SourceBlockUnaligned {
        index: usize,
        samples: usize,
        channels: usize,
    },
    #[error("sink {index} 样本长度 {actual}，期望 {expected}")]
    SinkBlockLength {
        index: usize,
        expected: usize,
        actual: usize,
    },
    #[error("block 样本容量溢出")]
    BlockSampleCountOverflow,
}

#[derive(Clone, Debug)]
struct CompiledSend {
    /// 该 send 的稳定标识，用于把逐通道峰值回传给调用方按 send 聚合。
    id: SendId,
    input_index: usize,
    output_index: usize,
    gain_linear: f32,
    muted: bool,
    enabled: bool,
    /// 空映射表示 identity 映射；非空切片保存 input -> output 映射。
    channel_map: Vec<(usize, usize)>,
}

/// 非实时构建、实时复用的固定 block 两阶段混音计划。
///
/// 图中的路由固定为 `source -> bus -> sink`：首先所有 Source send 累加到内部 Bus
/// block，再将每个 Bus send 混入 Sink block。Bus block 在创建计划时一次性分配，
/// `process` 不会执行堆分配。
#[derive(Clone, Debug)]
pub struct MixerPlan {
    block_frames: usize,
    source_channels: usize,
    bus_channels: usize,
    sink_channels: usize,
    source_count: usize,
    bus_count: usize,
    sink_count: usize,
    source_sends: Vec<CompiledSend>,
    bus_sends: Vec<CompiledSend>,
    bus_blocks: Vec<f32>,
    /// 每条 send 在最近一次 `process` 后输出的逐通道（L/R）峰值幅度（0.0~1.0）。
    /// 顺序为 `source_sends` 全体后接 `bus_sends` 全体，与 `send_ids` 一一对应。
    send_peaks: Vec<(SendId, [f32; 2])>,
}

impl MixerPlan {
    /// 从路由图编译混音计划。
    ///
    /// `channel_map` 的每一项解释为 `(input_channel, output_channel)`。空映射表示
    /// 按相同声道序号连接，超出任一端声道数的部分不参与混音。Bus 的内部声道数
    /// 取 source/sink 声道数中的较大值，因此同一个计划可保持端点的完整通道信息。
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

        let bus_channels = source_channels.max(sink_channels);
        block_frames
            .checked_mul(source_channels)
            .ok_or(MixerError::BlockSampleCountOverflow)?;
        let bus_block_samples = block_frames
            .checked_mul(bus_channels)
            .ok_or(MixerError::BlockSampleCountOverflow)?;
        block_frames
            .checked_mul(sink_channels)
            .ok_or(MixerError::BlockSampleCountOverflow)?;
        let bus_block_total = graph
            .buses
            .len()
            .checked_mul(bus_block_samples)
            .ok_or(MixerError::BlockSampleCountOverflow)?;
        graph.validate()?;

        let mut source_sends = Vec::new();
        let mut bus_sends = Vec::new();
        for (send_index, send) in graph.sends.iter().enumerate() {
            match send {
                SendSpec::SourceToBus {
                    source_id, bus_id, ..
                } => {
                    let source_index = graph
                        .sources
                        .iter()
                        .position(|source| source.id == *source_id)
                        .ok_or_else(|| {
                            MixerError::Graph(RouteGraphError::MissingSource(source_id.0.clone()))
                        })?;
                    let bus_index = graph
                        .buses
                        .iter()
                        .position(|bus| bus.id == *bus_id)
                        .ok_or_else(|| {
                            MixerError::Graph(RouteGraphError::MissingBus(bus_id.0.clone()))
                        })?;
                    let channel_map = compile_channel_map(
                        send.channel_map(),
                        send_index,
                        source_channels,
                        bus_channels,
                        ChannelSide::Source,
                        ChannelSide::Bus,
                    )?;
                    source_sends.push(CompiledSend {
                        id: send.id().clone(),
                        input_index: source_index,
                        output_index: bus_index,
                        gain_linear: db_to_linear(send.gain_db()),
                        muted: send.muted(),
                        enabled: send.enabled(),
                        channel_map,
                    });
                }
                SendSpec::BusToSink {
                    bus_id, sink_id, ..
                } => {
                    let bus_index = graph
                        .buses
                        .iter()
                        .position(|bus| bus.id == *bus_id)
                        .ok_or_else(|| {
                            MixerError::Graph(RouteGraphError::MissingBus(bus_id.0.clone()))
                        })?;
                    let sink_index = graph
                        .sinks
                        .iter()
                        .position(|sink| sink.id == *sink_id)
                        .ok_or_else(|| {
                            MixerError::Graph(RouteGraphError::MissingSink(sink_id.0.clone()))
                        })?;
                    let channel_map = compile_channel_map(
                        send.channel_map(),
                        send_index,
                        bus_channels,
                        sink_channels,
                        ChannelSide::Bus,
                        ChannelSide::Sink,
                    )?;
                    bus_sends.push(CompiledSend {
                        id: send.id().clone(),
                        input_index: bus_index,
                        output_index: sink_index,
                        gain_linear: db_to_linear(send.gain_db()),
                        muted: send.muted(),
                        enabled: send.enabled(),
                        channel_map,
                    });
                }
            }
        }

        // 每条 send 占一项，初始峰值为 0。顺序与 process 中统计顺序一致。
        let mut send_peaks = Vec::with_capacity(source_sends.len() + bus_sends.len());
        for send in source_sends.iter().chain(bus_sends.iter()) {
            send_peaks.push((send.id.clone(), [0.0f32; 2]));
        }

        Ok(Self {
            block_frames,
            source_channels,
            bus_channels,
            sink_channels,
            source_count: graph.sources.len(),
            bus_count: graph.buses.len(),
            sink_count: graph.sinks.len(),
            source_sends,
            bus_sends,
            bus_blocks: vec![0.0; bus_block_total],
            send_peaks,
        })
    }

    pub const fn block_frames(&self) -> usize {
        self.block_frames
    }
    pub const fn source_channels(&self) -> usize {
        self.source_channels
    }
    pub const fn bus_channels(&self) -> usize {
        self.bus_channels
    }
    pub const fn sink_channels(&self) -> usize {
        self.sink_channels
    }
    pub const fn source_count(&self) -> usize {
        self.source_count
    }
    pub const fn bus_count(&self) -> usize {
        self.bus_count
    }
    pub const fn sink_count(&self) -> usize {
        self.sink_count
    }
    /// 最近一次 [`process`](Self::process) 后各条 send 的逐通道（L/R）输出峰值。
    ///
    /// 每个元素为 `(send_id, [left_peak, right_peak])`，峰值幅度 0.0~1.0。
    /// 顺序与内部 `source_sends` 接 `bus_sends` 一致，调用方据此按 send 聚合。
    pub fn send_peaks(&self) -> &[(SendId, [f32; 2])] {
        &self.send_peaks
    }

    /// 将 source block 经内部 Bus 混音到 sink block。
    ///
    /// source 可短于固定 block 长度，缺少的尾帧按静音处理；sink 必须提供完整
    /// block。输出与内部 Bus block 均会先清零。该方法不做削波。
    pub fn process(
        &mut self,
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

        self.bus_blocks.fill(0.0);
        for sink in sink_blocks.iter_mut() {
            sink.fill(0.0);
        }

        let bus_block_samples = self.block_frames * self.bus_channels;
        // 统计每条 send 的逐通道（L/R）峰值，静音/禁用的 send 峰值记为 0。
        // 顺序与 `send_peaks` 初始化一致：先 source_sends 后 bus_sends。
        let mut peak_index = 0;
        for send in &self.source_sends {
            if send.muted || !send.enabled {
                self.send_peaks[peak_index].1 = [0.0f32; 2];
                peak_index += 1;
                continue;
            }
            let source = source_blocks[send.input_index];
            let bus_offset = send.output_index * bus_block_samples;
            let bus = &mut self.bus_blocks[bus_offset..bus_offset + bus_block_samples];
            let peaks = mix_block(source, self.source_channels, bus, self.bus_channels, send);
            self.send_peaks[peak_index].1 = peaks;
            peak_index += 1;
        }
        for send in &self.bus_sends {
            if send.muted || !send.enabled {
                self.send_peaks[peak_index].1 = [0.0f32; 2];
                peak_index += 1;
                continue;
            }
            let bus_offset = send.input_index * bus_block_samples;
            let bus = &self.bus_blocks[bus_offset..bus_offset + bus_block_samples];
            let sink = &mut sink_blocks[send.output_index];
            let peaks = mix_block(bus, self.bus_channels, sink, self.sink_channels, send);
            self.send_peaks[peak_index].1 = peaks;
            peak_index += 1;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum ChannelSide {
    Source,
    Bus,
    Sink,
}

fn compile_channel_map(
    map: &[(u16, u16)],
    send_index: usize,
    input_channels: usize,
    output_channels: usize,
    input_side: ChannelSide,
    output_side: ChannelSide,
) -> Result<Vec<(usize, usize)>, MixerError> {
    let mut compiled = Vec::with_capacity(map.len());
    for &(input_channel, output_channel) in map {
        if usize::from(input_channel) >= input_channels {
            return Err(channel_out_of_range(
                input_side,
                send_index,
                input_channel,
                input_channels,
            ));
        }
        if usize::from(output_channel) >= output_channels {
            return Err(channel_out_of_range(
                output_side,
                send_index,
                output_channel,
                output_channels,
            ));
        }
        compiled.push((usize::from(input_channel), usize::from(output_channel)));
    }
    Ok(compiled)
}

fn channel_out_of_range(
    side: ChannelSide,
    send_index: usize,
    channel: u16,
    channels: usize,
) -> MixerError {
    match side {
        ChannelSide::Source => MixerError::SourceChannelOutOfRange {
            send_index,
            channel,
            channels,
        },
        ChannelSide::Bus => MixerError::BusChannelOutOfRange {
            send_index,
            channel,
            channels,
        },
        ChannelSide::Sink => MixerError::SinkChannelOutOfRange {
            send_index,
            channel,
            channels,
        },
    }
}

/// 将单条 send 的 `input` 混入 `output`，返回该 send 在输出块上的逐通道
/// （L/R，即第 0/1 声道）峰值幅度（0.0~1.0，取绝对值最大）。
///
/// 峰值在增益应用后统计，反映该 send 实际送入下游的电平，供逐通道电平表使用。
fn mix_block(
    input: &[f32],
    input_channels: usize,
    output: &mut [f32],
    output_channels: usize,
    send: &CompiledSend,
) -> [f32; 2] {
    let available_frames = input.len() / input_channels;
    let mut peaks = [0.0f32; 2];
    if send.channel_map.is_empty() {
        let mapped_channels = input_channels.min(output_channels);
        for frame in 0..available_frames {
            let input_base = frame * input_channels;
            let output_base = frame * output_channels;
            for channel in 0..mapped_channels {
                let sample = input[input_base + channel] * send.gain_linear;
                output[output_base + channel] += sample;
                let mag = sample.abs();
                if channel < 2 && mag > peaks[channel] {
                    peaks[channel] = mag;
                }
            }
        }
    } else {
        for frame in 0..available_frames {
            let input_base = frame * input_channels;
            let output_base = frame * output_channels;
            for &(input_channel, output_channel) in &send.channel_map {
                let sample = input[input_base + input_channel] * send.gain_linear;
                output[output_base + output_channel] += sample;
                if output_channel < 2 {
                    let mag = sample.abs();
                    if mag > peaks[output_channel] {
                        peaks[output_channel] = mag;
                    }
                }
            }
        }
    }
    peaks
}

fn db_to_linear(gain_db: f32) -> f32 {
    10.0_f32.powf(gain_db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BusId, BusSpec, EndpointId, SendId, SinkId, SinkKind, SinkSpec, SourceId, SourceKind,
        SourceSpec,
    };

    fn source(id: &str) -> SourceSpec {
        SourceSpec {
            id: SourceId(id.into()),
            kind: SourceKind::DeviceCapture,
            endpoint_id: None,
            process_id: None,
            executable_path: None,
            display_name: id.into(),
        }
    }
    fn bus(id: &str) -> BusSpec {
        BusSpec {
            id: BusId(id.into()),
            display_name: id.into(),
        }
    }
    fn sink(id: &str) -> SinkSpec {
        SinkSpec {
            id: SinkId(id.into()),
            endpoint_id: EndpointId(id.into()),
            display_name: id.into(),
            kind: SinkKind::Device,
            stream_name: None,
        }
    }
    fn source_send(id: &str, source_id: &str, bus_id: &str) -> SendSpec {
        SendSpec::SourceToBus {
            id: SendId(id.into()),
            source_id: SourceId(source_id.into()),
            bus_id: BusId(bus_id.into()),
            gain_db: 0.0,
            muted: false,
            enabled: true,
            channel_map: Vec::new(),
        }
    }
    fn bus_send(id: &str, bus_id: &str, sink_id: &str) -> SendSpec {
        SendSpec::BusToSink {
            id: SendId(id.into()),
            bus_id: BusId(bus_id.into()),
            sink_id: SinkId(sink_id.into()),
            gain_db: 0.0,
            muted: false,
            enabled: true,
            channel_map: Vec::new(),
        }
    }
    fn graph(sources: &[&str], buses: &[&str], sinks: &[&str], sends: Vec<SendSpec>) -> RouteGraph {
        RouteGraph {
            sources: sources.iter().map(|id| source(id)).collect(),
            buses: buses.iter().map(|id| bus(id)).collect(),
            sinks: sinks.iter().map(|id| sink(id)).collect(),
            sends,
        }
    }
    fn process(plan: &mut MixerPlan, sources: &[Vec<f32>]) -> Vec<Vec<f32>> {
        let source_refs: Vec<&[f32]> = sources.iter().map(Vec::as_slice).collect();
        let mut outputs = (0..plan.sink_count())
            .map(|_| vec![99.0; plan.block_frames() * plan.sink_channels()])
            .collect::<Vec<_>>();
        let mut sink_refs: Vec<&mut [f32]> = outputs.iter_mut().map(Vec::as_mut_slice).collect();
        plan.process(&source_refs, &mut sink_refs).unwrap();
        outputs
    }

    #[test]
    fn source_to_bus_then_bus_to_sink() {
        let mapped_graph = graph(
            &["source"],
            &["mix"],
            &["sink"],
            vec![
                source_send("source-mix", "source", "mix"),
                bus_send("mix-sink", "mix", "sink"),
            ],
        );
        let mut plan = MixerPlan::new(&mapped_graph, 2, 2, 2).unwrap();
        assert_eq!(
            process(&mut plan, &[vec![1.0, -2.0, 3.0, -4.0]]),
            vec![vec![1.0, -2.0, 3.0, -4.0]]
        );
    }

    #[test]
    fn accumulates_multiple_sources_in_one_bus() {
        let graph = graph(
            &["a", "b"],
            &["mix"],
            &["sink"],
            vec![
                source_send("a-mix", "a", "mix"),
                source_send("b-mix", "b", "mix"),
                bus_send("mix-sink", "mix", "sink"),
            ],
        );
        let mut plan = MixerPlan::new(&graph, 1, 1, 1).unwrap();
        assert_eq!(process(&mut plan, &[vec![0.5], vec![0.5]]), vec![vec![1.0]]);
    }

    #[test]
    fn fans_one_bus_out_to_multiple_sinks() {
        let graph = graph(
            &["source"],
            &["mix"],
            &["a", "b"],
            vec![
                source_send("source-mix", "source", "mix"),
                bus_send("mix-a", "mix", "a"),
                bus_send("mix-b", "mix", "b"),
            ],
        );
        let mut plan = MixerPlan::new(&graph, 1, 1, 1).unwrap();
        assert_eq!(
            process(&mut plan, &[vec![0.75]]),
            vec![vec![0.75], vec![0.75]]
        );
    }

    #[test]
    fn keeps_buses_isolated() {
        let graph = graph(
            &["a", "b"],
            &["mix-a", "mix-b"],
            &["sink-a", "sink-b"],
            vec![
                source_send("a-mix-a", "a", "mix-a"),
                source_send("b-mix-b", "b", "mix-b"),
                bus_send("mix-a-sink-a", "mix-a", "sink-a"),
                bus_send("mix-b-sink-b", "mix-b", "sink-b"),
            ],
        );
        let mut plan = MixerPlan::new(&graph, 1, 1, 1).unwrap();
        assert_eq!(
            process(&mut plan, &[vec![0.25], vec![0.75]]),
            vec![vec![0.25], vec![0.75]]
        );
    }

    #[test]
    fn gain_mute_enable_and_channel_map_apply_to_each_stage() {
        let mut first = source_send("source-mix", "source", "mix");
        if let SendSpec::SourceToBus {
            gain_db,
            channel_map,
            ..
        } = &mut first
        {
            *gain_db = 6.0206;
            *channel_map = vec![(0, 1), (1, 0)];
        }
        let mapped_graph = graph(
            &["source"],
            &["mix"],
            &["sink"],
            vec![first, bus_send("mix-sink", "mix", "sink")],
        );
        let mut plan = MixerPlan::new(&mapped_graph, 1, 2, 2).unwrap();
        let output = process(&mut plan, &[vec![1.0, 2.0]]);
        assert!((output[0][0] - 4.0).abs() < 0.0002);
        assert!((output[0][1] - 2.0).abs() < 0.0002);

        let mut muted = source_send("source-mix", "source", "mix");
        if let SendSpec::SourceToBus { muted, .. } = &mut muted {
            *muted = true;
        }
        let muted_graph = graph(
            &["source"],
            &["mix"],
            &["sink"],
            vec![muted, bus_send("mix-sink", "mix", "sink")],
        );
        let mut muted_plan = MixerPlan::new(&muted_graph, 1, 1, 1).unwrap();
        assert_eq!(process(&mut muted_plan, &[vec![1.0]]), vec![vec![0.0]]);

        let mut disabled = bus_send("mix-sink", "mix", "sink");
        if let SendSpec::BusToSink { enabled, .. } = &mut disabled {
            *enabled = false;
        }
        let disabled_graph = graph(
            &["source"],
            &["mix"],
            &["sink"],
            vec![source_send("source-mix", "source", "mix"), disabled],
        );
        let mut disabled_plan = MixerPlan::new(&disabled_graph, 1, 1, 1).unwrap();
        assert_eq!(process(&mut disabled_plan, &[vec![1.0]]), vec![vec![0.0]]);
    }

    #[test]
    fn rejects_invalid_mapping_and_block_boundaries() {
        let mut invalid = source_send("source-mix", "source", "mix");
        if let SendSpec::SourceToBus { channel_map, .. } = &mut invalid {
            *channel_map = vec![(2, 0)];
        }
        let invalid_graph = graph(&["source"], &["mix"], &["sink"], vec![invalid]);
        assert_eq!(
            MixerPlan::new(&invalid_graph, 1, 2, 2).unwrap_err(),
            MixerError::SourceChannelOutOfRange {
                send_index: 0,
                channel: 2,
                channels: 2
            }
        );

        let mut invalid_sink = bus_send("mix-sink", "mix", "sink");
        if let SendSpec::BusToSink { channel_map, .. } = &mut invalid_sink {
            *channel_map = vec![(0, 2)];
        }
        let invalid_sink_graph = graph(&["source"], &["mix"], &["sink"], vec![invalid_sink]);
        assert_eq!(
            MixerPlan::new(&invalid_sink_graph, 1, 2, 2).unwrap_err(),
            MixerError::SinkChannelOutOfRange {
                send_index: 0,
                channel: 2,
                channels: 2
            }
        );

        let graph = graph(
            &["source"],
            &["mix"],
            &["sink"],
            vec![
                source_send("source-mix", "source", "mix"),
                bus_send("mix-sink", "mix", "sink"),
            ],
        );
        let mut plan = MixerPlan::new(&graph, 2, 2, 2).unwrap();
        let source = vec![1.0, 2.0, 3.0];
        let source_refs = vec![source.as_slice()];
        let mut output = vec![0.0; 4];
        let mut sink_refs = vec![output.as_mut_slice()];
        assert_eq!(
            plan.process(&source_refs, &mut sink_refs).unwrap_err(),
            MixerError::SourceBlockUnaligned {
                index: 0,
                samples: 3,
                channels: 2
            }
        );
    }

    #[test]
    fn rejects_sample_count_overflow() {
        let graph = graph(&[], &[], &[], vec![]);
        assert_eq!(
            MixerPlan::new(&graph, usize::MAX, 2, 2).unwrap_err(),
            MixerError::BlockSampleCountOverflow
        );
    }

    #[test]
    fn treats_short_source_as_silence_and_does_not_clip() {
        let graph = graph(
            &["a", "b"],
            &["mix"],
            &["sink"],
            vec![
                source_send("a-mix", "a", "mix"),
                source_send("b-mix", "b", "mix"),
                bus_send("mix-sink", "mix", "sink"),
            ],
        );
        let mut plan = MixerPlan::new(&graph, 2, 1, 1).unwrap();
        assert_eq!(
            process(&mut plan, &[vec![4.0], vec![1.0, 1.0]]),
            vec![vec![5.0, 1.0]]
        );
    }
}
