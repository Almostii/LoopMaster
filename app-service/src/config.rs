//! 应用配置：持久化路由图与 UI 状态（阶段 C.2）。
//!
//! 设计约束：
//! - 只持久化稳定 endpoint ID，不持久化设备列表索引或设备名称；
//! - 设备缺失时保留路由并标记 `missing_endpoints`，不按名称自动替换；
//! - 写入使用"临时文件 + 原子替换"，写入中断不会破坏旧文件；
//! - 加载只返回配置，**不自动启动音频**，是否启动由调用方（UI）在用户
//!   确认后决定；
//! - `schema_version` 显式校验：当前仅接受 `CURRENT_SCHEMA_VERSION`，
//!   旧版本返回 `UnsupportedSchemaVersion`，后续 schema 演进时在
//!   `from_json` 中加入迁移分支。

use loopmaster_audio_core::{EndpointId, RouteGraph, RouteGraphError};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// 当前配置 schema 版本。
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// 应用配置：schema 有版本，加载时按版本校验。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    pub schema_version: u32,
    pub graph: RouteGraph,
    /// 预设附加的 UI 选择；**不保存设备列表索引**。
    #[serde(default)]
    pub ui_state: UiState,
}

impl AppConfig {
    /// 以当前 schema 版本创建配置。
    pub fn new(graph: RouteGraph) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            graph,
            ui_state: UiState::default(),
        }
    }

    /// 从 JSON 字节反序列化并校验路由图。
    ///
    /// schema 版本必须等于 `CURRENT_SCHEMA_VERSION`；旧版本在此预留迁移
    /// 入口（当前直接拒绝，见 `ConfigError::UnsupportedSchemaVersion`）。
    pub fn from_json(bytes: &[u8]) -> Result<Self, ConfigError> {
        let config: AppConfig = serde_json::from_slice(bytes)?;
        if config.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchemaVersion(config.schema_version));
        }
        config.graph.validate()?;
        Ok(config)
    }

    /// 序列化为格式化 JSON。
    pub fn to_json(&self) -> Result<Vec<u8>, ConfigError> {
        Ok(serde_json::to_vec_pretty(self)?)
    }

    /// 从文件加载配置。
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        let bytes = fs::read(path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                ConfigError::NotFound(path.to_path_buf())
            } else {
                ConfigError::Io(error)
            }
        })?;
        Self::from_json(&bytes)
    }

    /// 保存到文件：先写同目录临时文件并 `sync_all`，再原子替换目标文件。
    ///
    /// 写入中断只会留下一个不完整的 `.tmp` 残留，目标文件保持上一次
    /// 完整内容；下一次保存会覆盖残留的 `.tmp`。`sync_all` 保证数据在
    /// 进程崩溃/断电场景下已落盘后再替换。
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        let bytes = self.to_json()?;
        let tmp = temp_path_for(path);
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// 按可用设备集合标记缺失 endpoint：保留路由，仅在 `ui_state` 中
    /// 记录缺失项（去重并排序），不按名称自动替换设备。
    pub fn mark_missing_endpoints(&mut self, available: &[EndpointId]) {
        let available: HashSet<&EndpointId> = available.iter().collect();
        let mut missing: Vec<EndpointId> = Vec::new();
        for source in &self.graph.sources {
            if let Some(endpoint) = &source.endpoint_id {
                if !available.contains(endpoint) {
                    missing.push(endpoint.clone());
                }
            }
        }
        for sink in &self.graph.sinks {
            if !available.contains(&sink.endpoint_id) {
                missing.push(sink.endpoint_id.clone());
            }
        }
        missing.sort_by(|a, b| a.0.cmp(&b.0));
        missing.dedup();
        self.ui_state.missing_endpoints = missing;
    }
}

/// 预设附加的 UI 选择。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UiState {
    /// 加载时按 endpoint ID 匹配，缺失设备标记为 unavailable。
    #[serde(default)]
    pub missing_endpoints: Vec<EndpointId>,
}

/// 配置加载/保存错误。
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("配置文件不存在: {0}")]
    NotFound(PathBuf),
    #[error("配置文件不可读: {0}")]
    Io(#[from] io::Error),
    #[error("配置文件 JSON 解析失败: {0}")]
    Json(#[from] serde_json::Error),
    #[error("配置文件 schema 版本不支持: {0}（当前支持版本 {CURRENT_SCHEMA_VERSION}）")]
    UnsupportedSchemaVersion(u32),
    #[error("路由图校验失败: {0}")]
    Graph(#[from] RouteGraphError),
}

/// 与目标文件同目录的临时文件路径，保证 `fs::rename` 在同一文件系统内
/// 原子替换。
fn temp_path_for(path: &Path) -> PathBuf {
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    path.with_file_name(tmp_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use loopmaster_audio_core::{SendSpec, SinkId, SinkSpec, SourceId, SourceKind, SourceSpec};

    fn source(id: &str) -> SourceSpec {
        SourceSpec {
            id: SourceId(id.into()),
            kind: SourceKind::DeviceCapture,
            endpoint_id: Some(EndpointId(format!("endpoint-{id}"))),
            process_id: None,
            display_name: id.into(),
        }
    }

    fn sink(id: &str) -> SinkSpec {
        SinkSpec {
            id: SinkId(id.into()),
            endpoint_id: EndpointId(format!("endpoint-{id}")),
            display_name: id.into(),
        }
    }

    fn send(source_id: &str, sink_id: &str) -> SendSpec {
        SendSpec {
            source_id: SourceId(source_id.into()),
            sink_id: SinkId(sink_id.into()),
            gain_db: 0.0,
            muted: false,
            enabled: true,
            channel_map: Vec::new(),
        }
    }

    fn graph() -> RouteGraph {
        RouteGraph {
            sources: vec![source("a")],
            sinks: vec![sink("out")],
            sends: vec![send("a", "out")],
        }
    }

    #[test]
    fn json_round_trip_preserves_config() {
        let config = AppConfig::new(graph());
        let json = config.to_json().unwrap();
        let loaded = AppConfig::from_json(&json).unwrap();
        assert_eq!(loaded, config);
        assert_eq!(loaded.schema_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn file_save_load_round_trip() {
        let dir =
            std::env::temp_dir().join(format!("loopmaster-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let _ = std::fs::remove_file(&path);

        let config = AppConfig::new(graph());
        config.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded, config);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_reports_not_found() {
        let dir =
            std::env::temp_dir().join(format!("loopmaster-config-missing-{}", std::process::id()));
        let path = dir.join("absent.json");
        let error = AppConfig::load_from(&path).unwrap_err();
        assert!(matches!(error, ConfigError::NotFound(_)));
    }

    #[test]
    fn malformed_json_is_rejected() {
        let error = AppConfig::from_json(b"{\"broken").unwrap_err();
        assert!(matches!(error, ConfigError::Json(_)));
    }

    #[test]
    fn unknown_schema_version_is_rejected() {
        let mut config = AppConfig::new(graph());
        config.schema_version = 999;
        let json = config.to_json().unwrap();
        let error = AppConfig::from_json(&json).unwrap_err();
        assert!(matches!(error, ConfigError::UnsupportedSchemaVersion(999)));
    }

    #[test]
    fn duplicate_graph_ids_are_rejected_on_load() {
        let mut duplicate = graph();
        duplicate.sources.push(source("a")); // 与已有 source 重复 ID
        let config = AppConfig::new(duplicate);
        let json = config.to_json().unwrap();
        let error = AppConfig::from_json(&json).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::Graph(RouteGraphError::DuplicateSource(ref id)) if id == "a"
        ));
    }

    #[test]
    fn invalid_gain_is_rejected_on_load() {
        let mut graph = graph();
        graph.sends[0].gain_db = 99.0; // 超出 -60..=12 dB 范围
        let config = AppConfig::new(graph);
        let json = config.to_json().unwrap();
        let error = AppConfig::from_json(&json).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::Graph(RouteGraphError::InvalidGain(99.0))
        ));
    }

    #[test]
    fn send_referencing_missing_source_is_rejected_on_load() {
        let mut graph = graph();
        graph.sends[0].source_id = SourceId("ghost".into());
        let config = AppConfig::new(graph);
        let json = config.to_json().unwrap();
        let error = AppConfig::from_json(&json).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::Graph(RouteGraphError::MissingSource(ref id)) if id == "ghost"
        ));
    }

    #[test]
    fn config_without_ui_state_field_parses_with_defaults() {
        // 模拟旧 JSON：没有 ui_state 字段，应回退到默认空状态。
        let json = br#"{
            "schema_version": 1,
            "graph": {
                "sources": [{
                    "id": "a",
                    "kind": "DeviceCapture",
                    "endpoint_id": "endpoint-a",
                    "process_id": null,
                    "display_name": "a"
                }],
                "sinks": [{
                    "id": "out",
                    "endpoint_id": "endpoint-out",
                    "display_name": "out"
                }],
                "sends": [{
                    "source_id": "a",
                    "sink_id": "out",
                    "gain_db": 0.0,
                    "muted": false,
                    "enabled": true,
                    "channel_map": []
                }]
            }
        }"#;
        let config = AppConfig::from_json(json).unwrap();
        assert_eq!(config.ui_state, UiState::default());
        assert!(config.ui_state.missing_endpoints.is_empty());
    }

    #[test]
    fn mark_missing_endpoints_keeps_route_and_dedups() {
        let mut config = AppConfig::new(graph());
        config.mark_missing_endpoints(&[]); // 所有设备缺失
        assert_eq!(
            config.ui_state.missing_endpoints,
            vec![
                EndpointId("endpoint-a".into()),
                EndpointId("endpoint-out".into())
            ]
        );
        // 路由保留，不按名称替换
        assert_eq!(config.graph.sources.len(), 1);
        assert_eq!(config.graph.sinks.len(), 1);

        // 全部可用后缺失列表清空
        config.mark_missing_endpoints(&[
            EndpointId("endpoint-a".into()),
            EndpointId("endpoint-out".into()),
        ]);
        assert!(config.ui_state.missing_endpoints.is_empty());
    }

    #[test]
    fn save_does_not_corrupt_previous_file_when_tmp_is_left_behind() {
        let dir =
            std::env::temp_dir().join(format!("loopmaster-config-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let first = AppConfig::new(graph());
        first.save_to(&path).unwrap();

        // 模拟上一次写入中断：同目录残留半截临时文件。
        std::fs::write(temp_path_for(&path), b"{\"broken").unwrap();

        // 新配置保存成功，目标文件为完整新内容，且可正常加载。
        let mut second = first.clone();
        second.graph.sends[0].gain_db = -6.0;
        second.save_to(&path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded, second);
        assert!(!temp_path_for(&path).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
