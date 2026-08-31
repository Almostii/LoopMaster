//! StateHub：应用权威状态单一事实源（Phase 2 Web 控制台前置）。
//!
//! 设计约束（方案 2 §2.1）：
//! - 权威状态（路由编辑器、引擎服务、应用设置、网络身份、发现/桥接句柄）
//!   全部持有在应用服务层；Tauri 壳层与未来的 WebSocket 通道只是投影；
//! - 任何权威状态变更必须使单调递增 [`StateHub::revision`] +1 并通知订阅者；
//!   订阅方检测到 revision 跳变后必须重新拉取全量快照（broadcast 允许慢
//!   消费者丢消息，因此不做事件队列，只保留最新 revision）；
//! - 并发模型沿用 Phase 1 的 `std::sync::Mutex` 细粒度锁：Tauri 命令线程、
//!   后台线程（桥接等待/进程监控）与未来的 Tokio runtime 都只通过短临界区
//!   访问，绝不在 WASAPI 实时线程上触碰本模块。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use loopmaster_audio_core::RouteGraph;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use loopmaster_audio_windows::AudioEngineState;

use crate::command::EngineCommand;
use crate::config::{AppConfig, ConfigError};
use crate::engine::EngineService;
use crate::error::ServiceError;
use crate::network::{NetworkBridge, NetworkDiscovery};
use crate::route::{RouteEdit, RouteEditor};
use crate::web_server::{auth::AuthState, WebServerHandle};

/// 应用设置 DTO（前端设置页持久化的内容）。
///
/// 从 Tauri 壳层下沉到应用服务层：设置属于权威状态，由 StateHub 持有，
/// 壳层命令只是读写投影。字段与序列化格式保持不变，前端零改动。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppSettings {
    pub theme: String,
    pub start_on_boot: bool,
    pub launch_hidden: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: "light".into(),
            start_on_boot: false,
            launch_hidden: false,
        }
    }
}

impl AppSettings {
    /// 从配置文件的 `ui_state` 提取设置，读取失败或文件缺失时回退默认。
    pub fn load_from_config(config_path: &Path) -> Self {
        match AppConfig::load_from(config_path) {
            Ok(config) => Self {
                theme: config.ui_state.theme().to_string(),
                start_on_boot: config.ui_state.start_on_boot,
                launch_hidden: config.ui_state.launch_hidden,
            },
            Err(_) => Self::default(),
        }
    }

    /// 将设置合并回现有配置（保留路由图与缺失设备标记）并写盘。
    pub fn save_to_config(&self, config_path: &Path) -> Result<(), ConfigError> {
        let mut config = match AppConfig::load_from(config_path) {
            Ok(config) => config,
            Err(ConfigError::NotFound(_)) => {
                // 尚无配置文件：以空图构造，再由路由保存流程补充图内容。
                AppConfig::new(RouteGraph::default())
            }
            Err(e) => return Err(e),
        };
        config.ui_state.theme = self.theme.clone();
        config.ui_state.start_on_boot = self.start_on_boot;
        config.ui_state.launch_hidden = self.launch_hidden;
        config.save_to(config_path)
    }
}

/// 本机网络身份概要（StateHub 持有的权威身份缓存）。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NodeIdentityBrief {
    pub node_id: String,
    pub device_name: String,
    pub network_enabled: bool,
    pub web_port: u16,
    /// 本机 IPv4 地址列表（多网卡时多个），便于用户跨机连接时查看。
    #[serde(default)]
    pub addresses: Vec<String>,
}

impl NodeIdentityBrief {
    /// 全空身份（尚未初始化时的占位）。
    pub fn empty() -> Self {
        Self {
            node_id: String::new(),
            device_name: String::new(),
            network_enabled: false,
            web_port: 0,
            addresses: Vec::new(),
        }
    }
}

/// 枚举本机网络接口的 IPv4 地址（排除 loopback 与链路本地地址）。
///
/// 用于设备页展示"本机 IP"、Web 控制台证书 IP SAN 与配对二维码。
/// 排序偏好：**RFC1918 私网地址（10/8、172.16/12、192.168/16）优先**，其余可路由
/// 地址在后；并排除虚拟网卡常用段（198.18/15 的 TUN/代理适配器、100.64/10 的
/// CGNAT），避免配对二维码取到只有本机可达的虚拟网卡地址（真机实测教训：
/// 桌面有 198.18.0.1 虚拟网卡时二维码取到了它，手机连不上）。
pub fn local_ipv4_addresses() -> Vec<String> {
    let addrs = match if_addrs::get_if_addrs() {
        Ok(list) => list,
        Err(_) => return Vec::new(),
    };
    let mut candidates: Vec<(u8, String)> = Vec::new();
    for iface in addrs {
        if let std::net::IpAddr::V4(v4) = iface.ip() {
            // 排除回环、链路本地（169.254/16）、未指定地址。
            if v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() {
                continue;
            }
            let octets = v4.octets();
            // 排除虚拟网卡/代理常用段：198.18/15（TUN）、100.64/10（CGNAT）。
            if (octets[0] == 198 && octets[1] & 0xfe == 18) || (octets[0] == 100 && octets[1] & 0xc0 == 0x40)
            {
                continue;
            }
            let text = v4.to_string();
            if !candidates.iter().any(|(_, existing)| *existing == text) {
                let priority = if is_rfc1918(&v4) { 0 } else { 1 };
                candidates.push((priority, text));
            }
        }
    }
    candidates.sort_by_key(|(priority, _)| *priority);
    candidates.into_iter().map(|(_, text)| text).collect()
}

/// 是否为 RFC1918 私有局域网地址。
fn is_rfc1918(v4: &std::net::Ipv4Addr) -> bool {
    let octets = v4.octets();
    octets[0] == 10
        || (octets[0] == 172 && (16..=31).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 168)
}

/// 应用权威状态：路由编辑器、引擎服务、设置、网络身份与运行时资源句柄。
///
/// 变更纪律：
/// - 通过 [`StateHub::bump`] 或带 bump 的高层变更方法（`replace_editor` /
///   `insert_engine` / `store_settings` / `store_identity` / `set_discovery` /
///   `set_bridge` / `take_engine` / `take_bridge_recovering`）修改状态；
/// - 组合操作（锁内改字段后再做后续动作）先 drop 守卫再调用 [`StateHub::bump`]。
pub struct StateHub {
    config_path: PathBuf,
    /// 拓扑操作互斥（串行化 start_engine / apply_route_edit 等多步流程）。
    route_operation: Mutex<()>,
    editor: Mutex<RouteEditor>,
    engine: Mutex<Option<EngineService>>,
    settings: Mutex<AppSettings>,
    /// 本机网络身份缓存（首次启动时生成 node_id 并持久化）。
    identity: Mutex<NodeIdentityBrief>,
    /// 后台局域网节点监听服务。
    discovery: Mutex<Option<NetworkDiscovery>>,
    /// VBAN 网络桥接（网络 FIFO 与 UDP 收发对接）。
    bridge: Mutex<Option<NetworkBridge>>,
    /// 内嵌 Web 控制台句柄（随网络开关启停）。
    web: Mutex<Option<WebServerHandle>>,
    /// 局域网配对与可信设备（首次配对/长期记住/显式撤销）。
    auth: std::sync::Arc<AuthState>,
    revision: AtomicU64,
    /// revision 变更通知：只保留最新值，订阅者只关心"变了"。
    notify: watch::Sender<u64>,
    /// 常驻 receiver：保证 watch 通道永不关闭（否则全部 receiver 被 drop 后
    /// `send` 会失败且不存储新值，后续订阅者将读到过期 revision）。
    _notify_rx: watch::Receiver<u64>,
}

impl StateHub {
    pub fn new(config_path: PathBuf) -> Self {
        let (notify, notify_rx) = watch::channel(0);
        Self {
            config_path: config_path.clone(),
            route_operation: Mutex::new(()),
            editor: Mutex::new(RouteEditor::new(RouteGraph::default())),
            engine: Mutex::new(None),
            settings: Mutex::new(AppSettings::default()),
            identity: Mutex::new(NodeIdentityBrief::empty()),
            discovery: Mutex::new(None),
            bridge: Mutex::new(None),
            web: Mutex::new(None),
            auth: std::sync::Arc::new(AuthState::new(config_path.clone())),
            revision: AtomicU64::new(0),
            notify,
            _notify_rx: notify_rx,
        }
    }

    /// 自动保存目标配置文件路径（构造时解析一次）。
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    // ------------------------------------------------------------------
    // 锁访问（短临界区；中毒时 panic，与既有壳层行为一致）
    // ------------------------------------------------------------------

    /// 拓扑操作互斥哨兵。
    pub fn route_operation(&self) -> MutexGuard<'_, ()> {
        self.route_operation.lock().expect("拓扑操作锁未中毒")
    }

    pub fn editor(&self) -> MutexGuard<'_, RouteEditor> {
        self.editor.lock().expect("路由锁未中毒")
    }

    pub fn engine(&self) -> MutexGuard<'_, Option<EngineService>> {
        self.engine.lock().expect("引擎锁未中毒")
    }

    pub fn settings(&self) -> MutexGuard<'_, AppSettings> {
        self.settings.lock().expect("设置锁未中毒")
    }

    pub fn identity(&self) -> MutexGuard<'_, NodeIdentityBrief> {
        self.identity.lock().expect("身份锁未中毒")
    }

    pub fn discovery(&self) -> MutexGuard<'_, Option<NetworkDiscovery>> {
        self.discovery.lock().expect("监听锁未中毒")
    }

    pub fn bridge(&self) -> MutexGuard<'_, Option<NetworkBridge>> {
        self.bridge.lock().expect("桥接锁未中毒")
    }

    // ------------------------------------------------------------------
    // state_revision
    // ------------------------------------------------------------------

    /// 当前权威状态版本（单调递增，从 1 开始计第一次变更）。
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    /// 订阅 revision 变更（watch：只保留最新值，不回放历史）。
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self._notify_rx.clone()
    }

    /// 标记权威状态已变化：revision +1 并通知订阅者。
    pub fn bump(&self) {
        let revision = self.revision.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.notify.send(revision);
    }

    // ------------------------------------------------------------------
    // 高层变更入口（自动 bump）
    // ------------------------------------------------------------------

    /// 当前路由草稿快照（权威状态读投影）。
    pub fn route_snapshot(&self) -> RouteGraph {
        self.editor().draft().clone()
    }

    /// 把一次路由编辑应用到权威状态（含运行中引擎的 send 级热更新转发）。
    ///
    /// 语义与壳层 `apply_route_edit` 一致：克隆草稿应用编辑，失败不改状态；
    /// 运行中的引擎收到 send 级热更新后立即生效；成功后整体替换草稿并 bump。
    /// `/ws` 上行控制（子任务 2）走此入口，与桌面 Tauri 命令共用同一事实源。
    pub fn apply_route_edit(&self, edit: RouteEdit) -> Result<(), ServiceError> {
        let _operation = self.route_operation();
        let mut next = self.editor().clone();
        next.apply(edit.clone()).map_err(ServiceError::from)?;
        self.forward_edit_to_engine(&edit)?;
        self.replace_editor(next);
        Ok(())
    }

    /// 把 send 级编辑热更新转发给运行中的引擎；未运行或无匹配时跳过。
    ///
    /// 仅 `SetSendGain` / `SetSendMuted` / `SetSendEnabled` 有热更新命令；
    /// 其余编辑（拓扑变化）在下次启动引擎时生效。
    pub fn forward_edit_to_engine(&self, edit: &RouteEdit) -> Result<(), ServiceError> {
        let command = match edit {
            RouteEdit::SetSendGain { send_id, gain_db } => Some(EngineCommand::SetGain {
                send_id: send_id.clone(),
                gain_db: *gain_db,
            }),
            RouteEdit::SetSendMuted { send_id, muted } => Some(EngineCommand::SetMuted {
                send_id: send_id.clone(),
                muted: *muted,
            }),
            RouteEdit::SetSendEnabled { send_id, enabled } => Some(EngineCommand::SetSendEnabled {
                send_id: send_id.clone(),
                enabled: *enabled,
            }),
            _ => None,
        };
        let Some(command) = command else {
            return Ok(());
        };
        let engine_slot = self.engine();
        let engine = match engine_slot.as_ref() {
            Some(engine) => engine,
            None => return Ok(()), // 引擎尚未创建
        };
        if engine.status().state != AudioEngineState::Running {
            return Ok(()); // 未运行：草稿已更新，下次启动生效
        }
        engine.command(command)?;
        drop(engine_slot);
        self.bump(); // 引擎内部状态已变（revision 应递增）
        Ok(())
    }

    /// 整体替换路由编辑器（`load_config`、重建编辑图后调用）。
    pub fn replace_editor(&self, editor: RouteEditor) {
        *self.editor() = editor;
        self.bump();
    }

    /// 插入新创建的引擎服务（惰性创建首次成功时调用）。
    pub fn insert_engine(&self, service: EngineService) {
        *self.engine() = Some(service);
        self.bump();
    }

    /// 取出并清空引擎服务槽位（重启引擎丢弃旧实例时调用）。
    pub fn take_engine(&self) -> Option<EngineService> {
        let taken = self.engine().take();
        if taken.is_some() {
            self.bump();
        }
        taken
    }

    /// 存储设置快照（`update_settings` 成功落盘后调用）。
    pub fn store_settings(&self, settings: AppSettings) {
        *self.settings() = settings;
        self.bump();
    }

    /// 存储本机网络身份快照。
    pub fn store_identity(&self, identity: NodeIdentityBrief) {
        *self.identity() = identity;
        self.bump();
    }

    /// 设置局域网监听服务槽位。
    pub fn set_discovery(&self, discovery: Option<NetworkDiscovery>) {
        *self.discovery() = discovery;
        self.bump();
    }

    /// 设置 VBAN 桥接句柄。
    pub fn set_bridge(&self, bridge: Option<NetworkBridge>) {
        *self.bridge() = bridge;
        self.bump();
    }

    /// 取出 VBAN 桥接（幂等停止路径）。
    ///
    /// 锁中毒时不 panic（异常路径下崩溃进程比安全释放桥接更糟），直接取
    /// 中毒内部数据；与既有壳层 `stop_network_bridge` 的防御行为一致。
    pub fn take_bridge_recovering(&self) -> Option<NetworkBridge> {
        let taken = match self.bridge.lock() {
            Ok(mut slot) => slot.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if taken.is_some() {
            self.bump();
        }
        taken
    }

    /// 内嵌 Web 控制台句柄访问（启停由壳层网络开关驱动）。
    pub fn web(&self) -> MutexGuard<'_, Option<WebServerHandle>> {
        self.web.lock().expect("Web 服务锁未中毒")
    }

    /// 存储内嵌 Web 控制台句柄。
    pub fn set_web(&self, web: Option<WebServerHandle>) {
        *self.web() = web;
        self.bump();
    }

    /// 局域网配对与可信设备状态（首次配对/长期记住/显式撤销）。
    pub fn auth(&self) -> &std::sync::Arc<AuthState> {
        &self.auth
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "loopmaster-statehub-{tag}-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn revision_is_monotonic_and_notifies_subscribers() {
        let hub = StateHub::new(temp_config_path("revision"));
        let mut rx = hub.subscribe();
        assert_eq!(hub.revision(), 0);
        assert_eq!(*rx.borrow(), 0);

        hub.bump();
        hub.bump();
        assert_eq!(hub.revision(), 2);
        assert_eq!(*rx.borrow_and_update(), 2);
    }

    #[test]
    fn settings_round_trip_through_config_file() {
        let path = temp_config_path("settings");
        let settings = AppSettings {
            theme: "dark".into(),
            start_on_boot: true,
            launch_hidden: false,
        };
        settings.save_to_config(&path).unwrap();
        let loaded = AppSettings::load_from_config(&path);
        assert_eq!(loaded, settings);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn settings_save_creates_missing_config_file() {
        let path = temp_config_path("settings-missing");
        let _ = std::fs::remove_file(&path);
        AppSettings::default()
            .save_to_config(&path)
            .expect("缺失配置文件时以空图构造并写盘");
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn high_level_mutations_bump_revision() {
        let hub = StateHub::new(temp_config_path("mutations"));
        hub.store_settings(AppSettings::default());
        hub.store_identity(NodeIdentityBrief::empty());
        hub.set_discovery(None);
        hub.set_bridge(None);
        assert!(hub.take_bridge_recovering().is_none());
        assert!(hub.take_engine().is_none());
        assert!(hub.revision() >= 4);
        assert_eq!(*hub.subscribe().borrow(), hub.revision());
    }

    #[test]
    fn local_ipv4_addresses_are_unique_and_valid() {
        for address in local_ipv4_addresses() {
            address.parse::<std::net::Ipv4Addr>().unwrap();
        }
    }
}
