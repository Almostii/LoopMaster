//! LoopMaster 的平台无关音频模型和路由图接口。
//!
//! 本 crate 不依赖 Windows、WASAPI 或 Slint，也不负责打开设备。

use serde::{Deserialize, Serialize};
use thiserror::Error;

mod fifo;
mod mixer;
mod resampler;
mod route_snapshot;
mod test_tone;

pub use fifo::{
    AudioFifo, AudioFifoConsumer, AudioFifoProducer, FifoConfigError, PopResult, PushResult,
    UnalignedSamples,
};
pub use mixer::{MixerError, MixerPlan};
pub use resampler::{
    FixedInputResampler, FixedOutputResampler, ResamplerConfigError, ResamplerProcessError,
};
pub use route_snapshot::RouteGraphSnapshot;
pub use test_tone::{fill_block, TestToneConfig, TestToneKind, TonePhase};

pub const INTERNAL_SAMPLE_RATE: u32 = 48_000;
pub const INTERNAL_CHANNELS: usize = 2;
pub const DEFAULT_BLOCK_FRAMES: usize = 480;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioFormat {
    pub const INTERNAL: Self = Self {
        sample_rate: INTERNAL_SAMPLE_RATE,
        channels: INTERNAL_CHANNELS as u16,
    };
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EndpointId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BusId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SinkId(pub String);

/// 路由连接的稳定标识。
///
/// 同一对节点之间允许存在多条连接，因此参数更新与删除必须通过 `SendId` 定位，
/// 不能只依赖两端节点 ID。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SendId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    DeviceCapture,
    DeviceLoopback,
    ProcessLoopback,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpec {
    pub id: SourceId,
    pub kind: SourceKind,
    pub endpoint_id: Option<EndpointId>,
    pub process_id: Option<u32>,
    pub display_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkSpec {
    pub id: SinkId,
    pub endpoint_id: EndpointId,
    pub display_name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SendSpec {
    /// 将真实输入源送入软件内部混音节点。
    SourceToBus {
        id: SendId,
        source_id: SourceId,
        bus_id: BusId,
        gain_db: f32,
        muted: bool,
        enabled: bool,
        channel_map: Vec<(u16, u16)>,
    },
    /// 将软件内部混音节点送往真实 render endpoint。
    BusToSink {
        id: SendId,
        bus_id: BusId,
        sink_id: SinkId,
        gain_db: f32,
        muted: bool,
        enabled: bool,
        channel_map: Vec<(u16, u16)>,
    },
}

impl SendSpec {
    pub fn id(&self) -> &SendId {
        match self {
            Self::SourceToBus { id, .. } | Self::BusToSink { id, .. } => id,
        }
    }

    pub fn gain_db(&self) -> f32 {
        match self {
            Self::SourceToBus { gain_db, .. } | Self::BusToSink { gain_db, .. } => *gain_db,
        }
    }

    pub fn muted(&self) -> bool {
        match self {
            Self::SourceToBus { muted, .. } | Self::BusToSink { muted, .. } => *muted,
        }
    }

    pub fn enabled(&self) -> bool {
        match self {
            Self::SourceToBus { enabled, .. } | Self::BusToSink { enabled, .. } => *enabled,
        }
    }

    pub fn channel_map(&self) -> &[(u16, u16)] {
        match self {
            Self::SourceToBus { channel_map, .. } | Self::BusToSink { channel_map, .. } => {
                channel_map
            }
        }
    }
}

/// 软件内部混音节点。Bus 永远不是 Windows endpoint，也不包含设备标识。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusSpec {
    pub id: BusId,
    pub display_name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RouteGraph {
    pub sources: Vec<SourceSpec>,
    pub buses: Vec<BusSpec>,
    pub sinks: Vec<SinkSpec>,
    pub sends: Vec<SendSpec>,
}

#[derive(Debug, Error, PartialEq)]
pub enum RouteGraphError {
    #[error("source ID 重复: {0}")]
    DuplicateSource(String),
    #[error("sink ID 重复: {0}")]
    DuplicateSink(String),
    #[error("bus ID 重复: {0}")]
    DuplicateBus(String),
    #[error("send ID 重复: {0}")]
    DuplicateSend(String),
    #[error("source endpoint ID 重复: {0}")]
    DuplicateSourceEndpoint(String),
    #[error("sink endpoint ID 重复: {0}")]
    DuplicateSinkEndpoint(String),
    #[error("source 不存在: {0}")]
    MissingSource(String),
    #[error("bus 不存在: {0}")]
    MissingBus(String),
    #[error("sink 不存在: {0}")]
    MissingSink(String),
    #[error("send 不存在: {0}")]
    MissingSend(String),
    #[error("增益超出范围: {0} dB")]
    InvalidGain(f32),
}

impl RouteGraph {
    pub fn validate(&self) -> Result<(), RouteGraphError> {
        let mut source_ids = std::collections::HashSet::new();
        let mut source_endpoints = std::collections::HashSet::new();
        for source in &self.sources {
            if !source_ids.insert(source.id.clone()) {
                return Err(RouteGraphError::DuplicateSource(source.id.0.clone()));
            }
            if let Some(endpoint_id) = &source.endpoint_id {
                if !source_endpoints.insert(endpoint_id.clone()) {
                    return Err(RouteGraphError::DuplicateSourceEndpoint(
                        endpoint_id.0.clone(),
                    ));
                }
            }
        }
        let mut sink_ids = std::collections::HashSet::new();
        let mut sink_endpoints = std::collections::HashSet::new();
        for sink in &self.sinks {
            if !sink_ids.insert(sink.id.clone()) {
                return Err(RouteGraphError::DuplicateSink(sink.id.0.clone()));
            }
            if !sink_endpoints.insert(sink.endpoint_id.clone()) {
                return Err(RouteGraphError::DuplicateSinkEndpoint(
                    sink.endpoint_id.0.clone(),
                ));
            }
        }
        let mut bus_ids = std::collections::HashSet::new();
        for bus in &self.buses {
            if !bus_ids.insert(bus.id.clone()) {
                return Err(RouteGraphError::DuplicateBus(bus.id.0.clone()));
            }
        }
        let mut send_ids = std::collections::HashSet::new();
        for send in &self.sends {
            if !send_ids.insert(send.id().clone()) {
                return Err(RouteGraphError::DuplicateSend(send.id().0.clone()));
            }
            match send {
                SendSpec::SourceToBus {
                    source_id, bus_id, ..
                } => {
                    if !self.sources.iter().any(|source| source.id == *source_id) {
                        return Err(RouteGraphError::MissingSource(source_id.0.clone()));
                    }
                    if !self.buses.iter().any(|bus| bus.id == *bus_id) {
                        return Err(RouteGraphError::MissingBus(bus_id.0.clone()));
                    }
                }
                SendSpec::BusToSink {
                    bus_id, sink_id, ..
                } => {
                    if !self.buses.iter().any(|bus| bus.id == *bus_id) {
                        return Err(RouteGraphError::MissingBus(bus_id.0.clone()));
                    }
                    if !self.sinks.iter().any(|sink| sink.id == *sink_id) {
                        return Err(RouteGraphError::MissingSink(sink_id.0.clone()));
                    }
                }
            }
            if !send.gain_db().is_finite() || !(-60.0..=12.0).contains(&send.gain_db()) {
                return Err(RouteGraphError::InvalidGain(send.gain_db()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod route_graph_tests {
    use super::*;

    fn source(id: &str, endpoint: Option<&str>) -> SourceSpec {
        SourceSpec {
            id: SourceId(id.into()),
            kind: SourceKind::DeviceCapture,
            endpoint_id: endpoint.map(|value| EndpointId(value.into())),
            process_id: None,
            display_name: id.into(),
        }
    }

    fn sink(id: &str, endpoint: &str) -> SinkSpec {
        SinkSpec {
            id: SinkId(id.into()),
            endpoint_id: EndpointId(endpoint.into()),
            display_name: id.into(),
        }
    }

    fn bus(id: &str) -> BusSpec {
        BusSpec {
            id: BusId(id.into()),
            display_name: id.into(),
        }
    }

    fn source_to_bus_send(id: &str, source_id: &str, bus_id: &str) -> SendSpec {
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

    #[test]
    fn rejects_duplicate_graph_ids_and_endpoints() {
        let duplicate_source = RouteGraph {
            sources: vec![source("a", None), source("a", None)],
            ..RouteGraph::default()
        };
        assert_eq!(
            duplicate_source.validate().unwrap_err(),
            RouteGraphError::DuplicateSource("a".into())
        );

        let duplicate_sink_endpoint = RouteGraph {
            sinks: vec![sink("a", "endpoint"), sink("b", "endpoint")],
            ..RouteGraph::default()
        };
        assert_eq!(
            duplicate_sink_endpoint.validate().unwrap_err(),
            RouteGraphError::DuplicateSinkEndpoint("endpoint".into())
        );

        let duplicate_bus = RouteGraph {
            buses: vec![bus("mix"), bus("mix")],
            ..RouteGraph::default()
        };
        assert_eq!(
            duplicate_bus.validate().unwrap_err(),
            RouteGraphError::DuplicateBus("mix".into())
        );
    }

    #[test]
    fn validates_bus_connections_and_stable_send_ids() {
        let missing_bus = RouteGraph {
            sources: vec![source("source", None)],
            sends: vec![source_to_bus_send("source-mix", "source", "mix")],
            ..RouteGraph::default()
        };
        assert_eq!(
            missing_bus.validate().unwrap_err(),
            RouteGraphError::MissingBus("mix".into())
        );

        let duplicate_send = RouteGraph {
            sources: vec![source("source", None)],
            buses: vec![bus("mix")],
            sends: vec![
                source_to_bus_send("send", "source", "mix"),
                source_to_bus_send("send", "source", "mix"),
            ],
            ..RouteGraph::default()
        };
        assert_eq!(
            duplicate_send.validate().unwrap_err(),
            RouteGraphError::DuplicateSend("send".into())
        );
    }
}
