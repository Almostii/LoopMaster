use crate::{RouteGraph, RouteGraphError};
use std::sync::Arc;

/// 已完成校验、可在实时线程只读的路由图快照。
#[derive(Clone, Debug, PartialEq)]
pub struct RouteGraphSnapshot(Arc<RouteGraph>);

impl RouteGraphSnapshot {
    pub fn new(graph: RouteGraph) -> Result<Self, RouteGraphError> {
        graph.validate()?;
        Ok(Self(Arc::new(graph)))
    }

    pub fn graph(&self) -> &RouteGraph {
        &self.0
    }

    pub fn into_graph(self) -> RouteGraph {
        match Arc::try_unwrap(self.0) {
            Ok(graph) => graph,
            Err(shared) => (*shared).clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EndpointId, SinkId, SinkKind, SinkSpec, SourceId, SourceKind, SourceSpec};

    fn source(id: &str) -> SourceSpec {
        SourceSpec {
            id: SourceId(id.into()),
            kind: SourceKind::DeviceCapture,
            endpoint_id: None,
            process_id: None,
            executable_path: None,
            stream_name: None,
            display_name: id.into(),
        }
    }

    #[test]
    fn validates_snapshot_before_sharing() {
        let graph = RouteGraph {
            sources: vec![source("a")],
            buses: Vec::new(),
            sinks: vec![SinkSpec {
                id: SinkId("s".into()),
                endpoint_id: EndpointId("endpoint".into()),
                display_name: "sink".into(),
                kind: SinkKind::Device,
                stream_name: None,
                remote_addr: None,
            }],
            sends: Vec::new(),
        };
        let snapshot = RouteGraphSnapshot::new(graph).unwrap();
        let clone = snapshot.clone();
        assert_eq!(snapshot.graph(), clone.graph());
    }
}
