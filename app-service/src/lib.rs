//! 应用服务层：UI 与音频引擎之间的唯一入口（阶段 C / M1+）。
//!
//! UI 只允许调用本层：`DeviceRepository` / `ProcessRepository` /
//! `RouteEditor` / `EngineService`，以及 `DeviceModel` / `ProcessModel`
//! 视图模型。本层只依赖 audio-core 的模型与 audio-windows 的公开能力，
//! 不暴露 WASAPI 对象、`AudioEngine` worker 或实时线程数据。
//!
//! M1 范围：设备/进程枚举投影、路由编辑（增删 source/sink/send、增益、
//! 静音、启停、通道映射）、引擎命令（`EngineCommand`）、状态/统计事件
//! 订阅（`ServiceEvent`）、手动重连（`request_reconnect`）。
//! M2 范围：配置与预设（`AppConfig`/schema version/原子写入）。

mod command;
mod config;
mod engine;
mod error;
mod event;
mod model;
pub mod network;
mod route;

pub use command::EngineCommand;
pub use config::{AppConfig, ConfigError, NetworkConfig, UiState, CURRENT_SCHEMA_VERSION};
pub use engine::EngineService;
pub use error::ServiceError;
pub use event::{ServiceEvent, ServiceEventReceiver};
pub use model::{
    DeviceCategory, DeviceCompatibility, DeviceFlow, DeviceFormatSupport, DeviceModel,
    DeviceRepository, DeviceStatus, ProcessModel, ProcessRepository,
};
pub use network::{MdnsAdvertiser, MdnsBrowser, MdnsError, NetworkEvent, NodeInfo};
pub use route::{RouteEdit, RouteEditor};
