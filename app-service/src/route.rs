//! 路由编辑：UI 暂存配置，提交后整体校验并冻结为不可变快照。

use loopmaster_audio_core::{
    BusId, BusSpec, RouteGraph, RouteGraphError, RouteGraphSnapshot, SendId, SendSpec, SinkId,
    SinkSpec, SourceId, SourceSpec,
};

/// 路由编辑操作：应用后整体校验，任何一步失败都不落盘。
#[derive(Clone, Debug, PartialEq)]
pub enum RouteEdit {
    AddSource(SourceSpec),
    RemoveSource(SourceId),
    AddBus(BusSpec),
    RemoveBus(BusId),
    AddSink(SinkSpec),
    RemoveSink(SinkId),
    /// 新增或覆盖同一稳定 ID 的连接（含 gain/muted/channel_map）。
    SetSend(SendSpec),
    RemoveSend(SendId),
    SetSendGain {
        send_id: SendId,
        gain_db: f32,
    },
    SetSendMuted {
        send_id: SendId,
        muted: bool,
    },
    /// 启用/禁用一条 send。`enabled=false` 保留增益/静音/通道映射配置，
    /// 但整条 send 从混音计划跳过；与 `SetSendMuted`（混音增益静音）语义不同。
    SetSendEnabled {
        send_id: SendId,
        enabled: bool,
    },
    SetSendChannelMap {
        send_id: SendId,
        channel_map: Vec<(u16, u16)>,
    },
    /// 更新 ProcessLoopback 声源的 PID（进程重启后按可执行路径重新匹配）。
    /// 用于服务层把失效 PID 自动重绑到新 PID；触发拓扑变化需引擎重启。
    SetSourceProcessId {
        source_id: SourceId,
        process_id: Option<u32>,
    },
}

/// 路由编辑会话：UI 编辑暂存配置，提交后整体校验并冻结为快照。
#[derive(Clone, Debug)]
pub struct RouteEditor {
    draft: RouteGraph,
}

impl RouteEditor {
    pub fn new(draft: RouteGraph) -> Self {
        Self { draft }
    }

    /// 当前暂存路由图（UI 渲染依据）。
    pub fn draft(&self) -> &RouteGraph {
        &self.draft
    }

    /// 应用一次原子编辑；非法编辑立即返回错误且 draft 不变。
    pub fn apply(&mut self, edit: RouteEdit) -> Result<(), RouteGraphError> {
        let previous = self.draft.clone();
        match edit {
            RouteEdit::AddSource(source) => self.draft.sources.push(source),
            RouteEdit::RemoveSource(id) => {
                if !self.draft.sources.iter().any(|s| s.id == id) {
                    return Err(RouteGraphError::MissingSource(id.0.clone()));
                }
                self.draft.sources.retain(|s| s.id != id);
                self.draft.sends.retain(|send| {
                    !matches!(send, SendSpec::SourceToBus { source_id, .. } if *source_id == id)
                });
            }
            RouteEdit::AddBus(bus) => self.draft.buses.push(bus),
            RouteEdit::RemoveBus(id) => {
                if !self.draft.buses.iter().any(|bus| bus.id == id) {
                    return Err(RouteGraphError::MissingBus(id.0.clone()));
                }
                self.draft.buses.retain(|bus| bus.id != id);
                self.draft.sends.retain(|send| {
                    !matches!(send,
                        SendSpec::SourceToBus { bus_id, .. } | SendSpec::BusToSink { bus_id, .. }
                            if *bus_id == id
                    )
                });
            }
            RouteEdit::AddSink(sink) => self.draft.sinks.push(sink),
            RouteEdit::RemoveSink(id) => {
                if !self.draft.sinks.iter().any(|s| s.id == id) {
                    return Err(RouteGraphError::MissingSink(id.0.clone()));
                }
                self.draft.sinks.retain(|s| s.id != id);
                self.draft.sends.retain(
                    |send| !matches!(send, SendSpec::BusToSink { sink_id, .. } if *sink_id == id),
                );
            }
            RouteEdit::SetSend(send) => {
                if let Some(existing) = self
                    .draft
                    .sends
                    .iter_mut()
                    .find(|existing| existing.id() == send.id())
                {
                    *existing = send;
                } else {
                    self.draft.sends.push(send);
                }
            }
            RouteEdit::RemoveSend(send_id) => {
                self.draft.sends.retain(|send| send.id() != &send_id);
            }
            RouteEdit::SetSendGain { send_id, gain_db } => {
                let send = self
                    .draft
                    .sends
                    .iter_mut()
                    .find(|send| send.id() == &send_id)
                    .ok_or_else(|| RouteGraphError::MissingSend(send_id.0.clone()))?;
                match send {
                    SendSpec::SourceToBus { gain_db: value, .. }
                    | SendSpec::BusToSink { gain_db: value, .. } => *value = gain_db,
                }
            }
            RouteEdit::SetSendMuted { send_id, muted } => {
                let send = self
                    .draft
                    .sends
                    .iter_mut()
                    .find(|send| send.id() == &send_id)
                    .ok_or_else(|| RouteGraphError::MissingSend(send_id.0.clone()))?;
                match send {
                    SendSpec::SourceToBus { muted: value, .. }
                    | SendSpec::BusToSink { muted: value, .. } => *value = muted,
                }
            }
            RouteEdit::SetSendEnabled { send_id, enabled } => {
                let send = self
                    .draft
                    .sends
                    .iter_mut()
                    .find(|send| send.id() == &send_id)
                    .ok_or_else(|| RouteGraphError::MissingSend(send_id.0.clone()))?;
                match send {
                    SendSpec::SourceToBus { enabled: value, .. }
                    | SendSpec::BusToSink { enabled: value, .. } => *value = enabled,
                }
            }
            RouteEdit::SetSendChannelMap {
                send_id,
                channel_map,
            } => {
                let send = self
                    .draft
                    .sends
                    .iter_mut()
                    .find(|send| send.id() == &send_id)
                    .ok_or_else(|| RouteGraphError::MissingSend(send_id.0.clone()))?;
                match send {
                    SendSpec::SourceToBus {
                        channel_map: value, ..
                    }
                    | SendSpec::BusToSink {
                        channel_map: value, ..
                    } => *value = channel_map,
                }
            }
            RouteEdit::SetSourceProcessId {
                source_id,
                process_id,
            } => {
                let source = self
                    .draft
                    .sources
                    .iter_mut()
                    .find(|source| source.id == source_id)
                    .ok_or_else(|| RouteGraphError::MissingSource(source_id.0.clone()))?;
                source.process_id = process_id;
            }
        }
        if let Err(error) = self.draft.validate() {
            self.draft = previous;
            return Err(error);
        }
        Ok(())
    }

    /// 校验并通过不可变快照交给引擎；成功后 draft 与快照一致。
    pub fn commit(&self) -> Result<RouteGraphSnapshot, RouteGraphError> {
        RouteGraphSnapshot::new(self.draft.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loopmaster_audio_core::SourceKind;

    fn source(id: &str) -> SourceSpec {
        SourceSpec {
            id: SourceId(id.into()),
            kind: SourceKind::ProcessLoopback,
            endpoint_id: None,
            process_id: Some(1),
            executable_path: None,
            stream_name: None,
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
            endpoint_id: loopmaster_audio_core::EndpointId(format!("endpoint-{id}")),
            display_name: id.into(),
            kind: loopmaster_audio_core::SinkKind::Device,
            stream_name: None,
            remote_addr: None,
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

    fn graph() -> RouteGraph {
        RouteGraph {
            sources: vec![source("a"), source("b")],
            buses: vec![bus("mix")],
            sinks: vec![sink("out")],
            sends: vec![
                source_send("a-mix", "a", "mix"),
                source_send("b-mix", "b", "mix"),
                bus_send("mix-out", "mix", "out"),
            ],
        }
    }

    #[test]
    fn send_parameters_are_updated_by_stable_send_id() {
        let mut editor = RouteEditor::new(graph());
        editor
            .apply(RouteEdit::SetSendGain {
                send_id: SendId("a-mix".into()),
                gain_db: -6.0,
            })
            .unwrap();
        editor
            .apply(RouteEdit::SetSendMuted {
                send_id: SendId("mix-out".into()),
                muted: true,
            })
            .unwrap();
        editor
            .apply(RouteEdit::SetSendEnabled {
                send_id: SendId("b-mix".into()),
                enabled: false,
            })
            .unwrap();

        assert_eq!(editor.draft().sends[0].gain_db(), -6.0);
        assert!(editor.draft().sends[2].muted());
        assert!(!editor.draft().sends[1].enabled());
        editor.commit().unwrap();
    }

    #[test]
    fn replacing_a_send_uses_its_id_not_its_endpoints() {
        let mut editor = RouteEditor::new(graph());
        let mut replacement = source_send("a-mix", "a", "mix");
        if let SendSpec::SourceToBus { gain_db, .. } = &mut replacement {
            *gain_db = -3.0;
        }
        editor.apply(RouteEdit::SetSend(replacement)).unwrap();
        assert_eq!(editor.draft().sends.len(), 3);
        assert_eq!(editor.draft().sends[0].gain_db(), -3.0);
    }

    #[test]
    fn missing_send_id_does_not_mutate_draft() {
        let mut editor = RouteEditor::new(graph());
        let before = editor.draft().clone();
        let error = editor
            .apply(RouteEdit::SetSendChannelMap {
                send_id: SendId("missing".into()),
                channel_map: vec![(0, 0)],
            })
            .unwrap_err();
        assert_eq!(error, RouteGraphError::MissingSend("missing".into()));
        assert_eq!(editor.draft(), &before);
    }

    #[test]
    fn removing_nodes_cascades_their_incident_sends() {
        let mut editor = RouteEditor::new(graph());
        editor
            .apply(RouteEdit::RemoveSource(SourceId("a".into())))
            .unwrap();
        assert_eq!(editor.draft().sends.len(), 2);

        editor
            .apply(RouteEdit::RemoveBus(BusId("mix".into())))
            .unwrap();
        assert!(editor.draft().sends.is_empty());
        assert!(editor.draft().buses.is_empty());

        editor
            .apply(RouteEdit::RemoveSink(SinkId("out".into())))
            .unwrap();
        assert!(editor.draft().sinks.is_empty());
    }

    #[test]
    fn add_bus_is_validated_atomically() {
        let mut editor = RouteEditor::new(graph());
        let before = editor.draft().clone();
        let error = editor.apply(RouteEdit::AddBus(bus("mix"))).unwrap_err();
        assert_eq!(error, RouteGraphError::DuplicateBus("mix".into()));
        assert_eq!(editor.draft(), &before);
    }
}
