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
pub struct SinkId(pub String);

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
pub struct SendSpec {
    pub source_id: SourceId,
    pub sink_id: SinkId,
    pub gain_db: f32,
    pub muted: bool,
    pub channel_map: Vec<(u16, u16)>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RouteGraph {
    pub sources: Vec<SourceSpec>,
    pub sinks: Vec<SinkSpec>,
    pub sends: Vec<SendSpec>,
}

#[derive(Debug, Error, PartialEq)]
pub enum RouteGraphError {
    #[error("source ID 重复: {0}")]
    DuplicateSource(String),
    #[error("sink ID 重复: {0}")]
    DuplicateSink(String),
    #[error("source endpoint ID 重复: {0}")]
    DuplicateSourceEndpoint(String),
    #[error("sink endpoint ID 重复: {0}")]
    DuplicateSinkEndpoint(String),
    #[error("source 不存在: {0}")]
    MissingSource(String),
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
        for send in &self.sends {
            if !self.sources.iter().any(|s| s.id == send.source_id) {
                return Err(RouteGraphError::MissingSource(send.source_id.0.clone()));
            }
            if !self.sinks.iter().any(|s| s.id == send.sink_id) {
                return Err(RouteGraphError::MissingSink(send.sink_id.0.clone()));
            }
            if !send.gain_db.is_finite() || !(-60.0..=12.0).contains(&send.gain_db) {
                return Err(RouteGraphError::InvalidGain(send.gain_db));
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
    }
}
