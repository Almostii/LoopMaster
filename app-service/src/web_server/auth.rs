//! 局域网配对与可信设备（Phase 2 子任务 4）。
//!
//! 模型冻结见 `Doc/Web控制台/2026-08-31-Web控制台DTO与可信设备模型冻结.md` §2：
//! - 首次配对、长期记住、显式撤销；配对窗口 5 分钟（候选）；
//! - pairing_secret ≥128bit（实际 256bit）+ 6 位 PIN，仅内存，兑换即作废；
//! - 首次配对成功签发 256bit opaque device credential，服务端只存 SHA-256
//!   哈希 + 设备名 + 权限 + 最后活动时间，持久化到 `<config_dir>/trusted-devices.json`；
//! - 只有「忘记设备 / 重置全部局域网信任」删除凭证；下线、关网络、退出均保留；
//! - 限流只作用于配对接口（同 IP 60 秒 5 次失败，候选）；
//! - 设备被忘记/重置时，立即关闭其已建立的 `/ws` 连接（连接吊销）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 配对窗口时长（候选值，方案 5 §5.1）。
const PAIR_WINDOW: Duration = Duration::from_secs(5 * 60);
/// 配对失败限流窗口（秒）。
const RATE_WINDOW_SECS: i64 = 60;
/// 同一 IP 在窗口内允许的最大失败次数（候选值）。
const RATE_MAX_FAILURES: usize = 5;
/// 凭证随机字节数（256bit）。
const CREDENTIAL_BYTES: usize = 32;
/// 持久化 Cookie 名称。
pub const COOKIE_NAME: &str = "lm_device";
/// 持久化有效期（约 400 天）。
pub const COOKIE_MAX_AGE: u64 = 400 * 24 * 3600;

/// 可信设备 DTO（持久化 + 桌面列表展示）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrustedDevice {
    pub id: String,
    /// 凭证 SHA-256 十六进制（服务端不存明文）。
    pub credential_hash: String,
    pub name: String,
    pub permission: String,
    /// 最后活动时间（Unix 秒）。
    pub last_seen_unix: i64,
}

/// 面向桌面/会话查询的公开设备概要（不暴露哈希）。
#[derive(Clone, Debug, Serialize)]
pub struct DeviceSummary {
    pub id: String,
    pub name: String,
    pub permission: String,
    pub last_seen_unix: i64,
}

/// 配对信息（桌面端渲染二维码/PIN）。
#[derive(Clone, Debug, Serialize)]
pub struct PairingInfo {
    /// `#secret=...` 中的 fragment 值（页面读取，仅内存）。
    pub secret: String,
    pub pin: String,
    /// 剩余有效秒数。
    pub expires_in_secs: u64,
}

/// 配对/会话错误。
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("配对窗口未开启或已过期")]
    PairingClosed,
    #[error("配对凭据不匹配")]
    InvalidCredential,
    #[error("配对失败次数过多，请 60 秒后重试")]
    RateLimited,
    #[error("未认证")]
    Unauthorized,
    #[error("设备不存在")]
    DeviceNotFound,
    #[error("持久化失败: {0}")]
    Persist(String),
}

struct Pairing {
    secret: String,
    pin: String,
    expires_unix: i64,
}

#[derive(Default)]
struct RateLimit {
    /// ip → 失败时间戳列表（Unix 秒）。
    failures: HashMap<String, Vec<i64>>,
}

/// 配对与可信设备权威状态（`Arc<AuthState>` 由 StateHub 持有）。
pub struct AuthState {
    config_path: PathBuf,
    pairing: Mutex<Option<Pairing>>,
    devices: Mutex<Vec<TrustedDevice>>,
    rate: Mutex<RateLimit>,
    /// device_id → 已建立的 /ws 连接关闭通知（吊销时 try_send）。
    connections: Mutex<HashMap<String, Vec<tokio::sync::mpsc::Sender<()>>>>,
}

impl AuthState {
    pub fn new(config_path: PathBuf) -> Self {
        let devices = load_devices(&config_path).unwrap_or_default();
        Self {
            config_path,
            pairing: Mutex::new(None),
            devices: Mutex::new(devices),
            rate: Mutex::new(RateLimit::default()),
            connections: Mutex::new(HashMap::new()),
        }
    }

    // ------------------------------------------------------------------
    // 配对窗口
    // ------------------------------------------------------------------

    /// 开启配对窗口（幂等：已开启则重新生成）。返回 secret/PIN 与剩余秒数。
    pub fn start_pairing(&self) -> PairingInfo {
        let secret = random_hex(CREDENTIAL_BYTES);
        let pin = random_pin();
        let expires_unix = now_unix() + PAIR_WINDOW.as_secs() as i64;
        let mut slot = self.pairing.lock().expect("配对锁未中毒");
        *slot = Some(Pairing {
            secret: secret.clone(),
            pin: pin.clone(),
            expires_unix,
        });
        PairingInfo {
            secret,
            pin,
            expires_in_secs: PAIR_WINDOW.as_secs(),
        }
    }

    pub fn stop_pairing(&self) {
        *self.pairing.lock().expect("配对锁未中毒") = None;
    }

    /// 当前配对窗口信息（已过期返回 None）。
    pub fn pairing_status(&self) -> Option<PairingInfo> {
        let slot = self.pairing.lock().expect("配对锁未中毒");
        let pairing = slot.as_ref()?;
        let remaining = pairing.expires_unix - now_unix();
        if remaining <= 0 {
            return None;
        }
        Some(PairingInfo {
            secret: pairing.secret.clone(),
            pin: pairing.pin.clone(),
            expires_in_secs: remaining as u64,
        })
    }

    // ------------------------------------------------------------------
    // 首次配对
    // ------------------------------------------------------------------

    /// 处理首次配对：校验 secret 或 pin → 签发凭证 → 持久化。
    ///
    /// `client_ip` 用于限流；返回凭证原文（写入 Set-Cookie）。
    pub fn pair(
        &self,
        secret: Option<&str>,
        pin: Option<&str>,
        client_name: &str,
        client_ip: &str,
    ) -> Result<String, AuthError> {
        self.rate.lock().expect("限流锁未中毒").prune(now_unix());
        if self
            .rate
            .lock()
            .expect("限流锁未中毒")
            .failures
            .get(client_ip)
            .map(|items| items.len() >= RATE_MAX_FAILURES)
            .unwrap_or(false)
        {
            return Err(AuthError::RateLimited);
        }

        // 窗口未开启或已过期 → PairingClosed；窗口内凭据不匹配 → InvalidCredential。
        let window_open = self
            .pairing
            .lock()
            .expect("配对锁未中毒")
            .as_ref()
            .is_some_and(|pairing| pairing.expires_unix > now_unix());
        if !window_open {
            return Err(AuthError::PairingClosed);
        }
        let valid = {
            let slot = self.pairing.lock().expect("配对锁未中毒");
            let pairing = slot.as_ref().expect("窗口已确认开启");
            let secret_ok = secret.is_some_and(|s| constant_time_eq(s, &pairing.secret));
            let pin_ok = pin.is_some_and(|p| constant_time_eq(p, &pairing.pin));
            secret_ok || pin_ok
        };
        if !valid {
            self.rate
                .lock()
                .expect("限流锁未中毒")
                .record_failure(client_ip, now_unix());
            return Err(AuthError::InvalidCredential);
        }

        // 兑换成功：凭证作废 + 签发持久化 credential。
        self.stop_pairing();
        let credential = random_hex(CREDENTIAL_BYTES);
        let credential_hash = sha256_hex(&credential);
        let device = TrustedDevice {
            id: uuid_v4(),
            credential_hash,
            name: client_name.to_owned(),
            permission: "control".to_owned(),
            last_seen_unix: now_unix(),
        };
        {
            let mut devices = self.devices.lock().expect("设备锁未中毒");
            devices.push(device);
        }
        self.persist()?;
        Ok(credential)
    }

    // ------------------------------------------------------------------
    // 会话鉴权
    // ------------------------------------------------------------------

    /// 校验 Cookie 凭证，命中返回设备（并更新最后活动时间）。
    pub fn verify_credential(&self, cookie: Option<&str>) -> Option<DeviceSummary> {
        let cookie = cookie?;
        let hash = sha256_hex(cookie);
        let mut devices = self.devices.lock().expect("设备锁未中毒");
        let device = devices.iter_mut().find(|d| d.credential_hash == hash)?;
        device.last_seen_unix = now_unix();
        Some(DeviceSummary {
            id: device.id.clone(),
            name: device.name.clone(),
            permission: device.permission.clone(),
            last_seen_unix: device.last_seen_unix,
        })
    }

    // ------------------------------------------------------------------
    // 忘记 / 重置
    // ------------------------------------------------------------------

    pub fn list_devices(&self) -> Vec<DeviceSummary> {
        self.devices
            .lock()
            .expect("设备锁未中毒")
            .iter()
            .map(|device| DeviceSummary {
                id: device.id.clone(),
                name: device.name.clone(),
                permission: device.permission.clone(),
                last_seen_unix: device.last_seen_unix,
            })
            .collect()
    }

    /// 忘记单个设备：删除凭证并关闭其 /ws 连接。
    pub fn forget(&self, device_id: &str) -> Result<(), AuthError> {
        let mut devices = self.devices.lock().expect("设备锁未中毒");
        let before = devices.len();
        devices.retain(|device| device.id != device_id);
        if devices.len() == before {
            return Err(AuthError::DeviceNotFound);
        }
        drop(devices);
        self.revoke_connections(device_id);
        self.persist()
    }

    /// 重置全部局域网信任。
    pub fn reset(&self) -> Result<(), AuthError> {
        let ids: Vec<String> = self
            .devices
            .lock()
            .expect("设备锁未中毒")
            .iter()
            .map(|device| device.id.clone())
            .collect();
        self.devices.lock().expect("设备锁未中毒").clear();
        for id in ids {
            self.revoke_connections(&id);
        }
        self.persist()
    }

    // ------------------------------------------------------------------
    // /ws 连接吊销
    // ------------------------------------------------------------------

    /// 注册一条 /ws 连接的关闭通知（设备凭证已被验证后）。
    pub fn register_connection(&self, device_id: &str, close: tokio::sync::mpsc::Sender<()>) {
        self.connections
            .lock()
            .expect("连接表锁未中毒")
            .entry(device_id.to_owned())
            .or_default()
            .push(close);
    }

    fn revoke_connections(&self, device_id: &str) {
        let senders = self
            .connections
            .lock()
            .expect("连接表锁未中毒")
            .remove(device_id);
        if let Some(senders) = senders {
            for sender in senders {
                // try_send 非阻塞：连接已关闭时忽略。
                let _ = sender.try_send(());
            }
        }
    }

    // ------------------------------------------------------------------
    // 持久化
    // ------------------------------------------------------------------

    fn persist(&self) -> Result<(), AuthError> {
        let devices = self.devices.lock().expect("设备锁未中毒");
        let payload = serde_json::to_vec_pretty(&PersistedDevices {
            schema_version: 1,
            devices: &devices,
        })
        .map_err(|e| AuthError::Persist(e.to_string()))?;
        let path = trusted_devices_path(&self.config_path);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, payload).map_err(|e| AuthError::Persist(e.to_string()))?;
        std::fs::rename(&tmp, &path).map_err(|e| AuthError::Persist(e.to_string()))?;
        Ok(())
    }
}

#[derive(Serialize)]
struct PersistedDevices<'a> {
    schema_version: u32,
    devices: &'a Vec<TrustedDevice>,
}

fn trusted_devices_path(config_path: &std::path::Path) -> PathBuf {
    config_path
        .parent()
        .map(|dir| dir.join("trusted-devices.json"))
        .unwrap_or_else(|| PathBuf::from("trusted-devices.json"))
}

fn load_devices(config_path: &std::path::Path) -> Result<Vec<TrustedDevice>, String> {
    let path = trusted_devices_path(config_path);
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    #[derive(Deserialize)]
    struct Loaded {
        devices: Vec<TrustedDevice>,
    }
    let loaded: Loaded = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    Ok(loaded.devices)
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn random_hex(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    getrandom::getrandom(&mut bytes).expect("CSPRNG 可用");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn random_pin() -> String {
    let mut bytes = [0u8; 3];
    getrandom::getrandom(&mut bytes).expect("CSPRNG 可用");
    let value = u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]) % 1_000_000;
    format!("{value:06}")
}

fn sha256_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// 常数时间字符串比较（避免凭据时序侧信道）。
fn constant_time_eq(left: &str, right: &str) -> bool {
    let a = left.as_bytes();
    let b = right.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("CSPRNG 可用");
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 10
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// 从 Cookie 头解析指定键的值。
pub fn extract_cookie(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    let cookie = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|part| {
        let part = part.trim();
        let (key, value) = part.split_once('=')?;
        if key.trim() == name {
            Some(value.trim().to_owned())
        } else {
            None
        }
    })
}

impl RateLimit {
    fn prune(&mut self, now: i64) {
        self.failures.retain(|_, timestamps| {
            timestamps
                .last()
                .is_some_and(|t| now - *t <= RATE_WINDOW_SECS)
        });
    }

    fn record_failure(&mut self, ip: &str, now: i64) {
        let timestamps = self.failures.entry(ip.to_owned()).or_default();
        timestamps.push(now);
        timestamps.retain(|t| now - *t <= RATE_WINDOW_SECS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "loopmaster-auth-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.json")
    }

    #[test]
    fn pairing_secret_is_256_bit_and_pin_six_digits() {
        let config = temp_config("pairing");
        let auth = AuthState::new(config);
        let info = auth.start_pairing();
        assert_eq!(info.secret.len(), 64, "32 字节 = 64 个 hex 字符");
        assert_eq!(info.pin.len(), 6);
        assert!(info.pin.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn pair_with_secret_issues_persistent_credential() {
        let config = temp_config("pair-secret");
        let auth = AuthState::new(config.clone());
        let info = auth.start_pairing();
        let credential = auth
            .pair(Some(&info.secret), None, "iPhone", "192.168.1.5")
            .expect("secret 应配对成功");
        assert_eq!(credential.len(), 64);
        // 兑换后窗口关闭，pin 不再有效。
        assert!(matches!(
            auth.pair(None, Some(&info.pin), "iPhone", "192.168.1.5"),
            Err(AuthError::PairingClosed)
        ));
        // 凭证可校验。
        let session = auth
            .verify_credential(Some(&credential))
            .expect("凭证应有效");
        assert_eq!(session.name, "iPhone");
        assert_eq!(session.permission, "control");
        // 持久化后可恢复。
        let reloaded = AuthState::new(config.clone());
        assert!(reloaded.verify_credential(Some(&credential)).is_some());
        let _ = std::fs::remove_dir_all(config.parent().unwrap());
    }

    #[test]
    fn pair_with_pin_works_and_wrong_credential_is_rejected() {
        let config = temp_config("pair-pin");
        let auth = AuthState::new(config);
        let info = auth.start_pairing();
        assert!(auth.pair(None, Some(&info.pin), "iPad", "10.0.0.2").is_ok());
        // 新窗口
        auth.start_pairing();
        assert!(matches!(
            auth.pair(Some("wrong"), None, "iPad", "10.0.0.2"),
            Err(AuthError::InvalidCredential)
        ));
    }

    #[test]
    fn rate_limit_blocks_after_five_failures() {
        let config = temp_config("rate");
        let auth = AuthState::new(config);
        auth.start_pairing();
        for _ in 0..RATE_MAX_FAILURES {
            assert!(matches!(
                auth.pair(Some("wrong"), None, "x", "10.0.0.9"),
                Err(AuthError::InvalidCredential)
            ));
        }
        assert!(matches!(
            auth.pair(Some("wrong"), None, "x", "10.0.0.9"),
            Err(AuthError::RateLimited)
        ));
    }

    #[test]
    fn forget_revokes_and_reset_clears_all() {
        let config = temp_config("forget");
        let auth = AuthState::new(config.clone());
        let info = auth.start_pairing();
        let credential = auth
            .pair(Some(&info.secret), None, "Phone", "10.0.0.3")
            .unwrap();
        let device = auth.verify_credential(Some(&credential)).unwrap();

        let (close_tx, mut close_rx) = tokio::sync::mpsc::channel::<()>(1);
        auth.register_connection(&device.id, close_tx);
        auth.forget(&device.id).unwrap();
        assert!(close_rx.try_recv().is_ok(), "吊销应立即关闭连接");
        assert!(auth.verify_credential(Some(&credential)).is_none());

        let info = auth.start_pairing();
        let credential2 = auth
            .pair(Some(&info.secret), None, "Phone2", "10.0.0.4")
            .unwrap();
        assert!(auth.verify_credential(Some(&credential2)).is_some());
        auth.reset().unwrap();
        assert!(auth.verify_credential(Some(&credential2)).is_none());
        assert!(auth.list_devices().is_empty());
        let _ = std::fs::remove_dir_all(config.parent().unwrap());
    }

    #[test]
    fn cookie_extraction_handles_multiple_pairs() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            "other=1; lm_device=abc123; x=y".parse().unwrap(),
        );
        assert_eq!(
            extract_cookie(&headers, "lm_device").as_deref(),
            Some("abc123")
        );
    }
}
