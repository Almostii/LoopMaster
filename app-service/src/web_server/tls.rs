//! 局域网 TLS 材料：本地根 CA + 派生服务器证书（冻结方案见
//! `Plan/2026-08-31-Web控制台DTO与可信设备模型冻结.md` §3）。
//!
//! - 根 CA（10 年）持久化在 `<config_dir>/tls/ca.crt` / `ca.key`；手机端一次
//!   显式安装信任根后无需再动；CA 私钥不进 Git、不进日志、不进网络；
//! - 服务器证书由本地 CA 签发，SAN 含 `localhost`、`127.0.0.1` 与当前全部
//!   本机 IPv4（IP SAN）；短有效期（30 天）；
//! - 重签触发：绑定 IP 集合变化、证书剩余有效期不足 7 天；重签只换服务器
//!   证书，客户端无需任何操作；
//! - 仅 HTTPS/WSS（rustls），不提供明文 HTTP 回退。

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, Ia5String, IsCa, KeyPair, SanType,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::state::local_ipv4_addresses;

/// CA 有效期（候选值：10 年）。
const CA_VALIDITY: Duration = Duration::from_secs(10 * 365 * 24 * 3600);
/// 服务器证书有效期（候选值：30 天）。
const SERVER_CERT_VALIDITY: Duration = Duration::from_secs(30 * 24 * 3600);
/// 剩余有效期低于该阈值时触发重签（候选值：7 天）。
const REISSUE_THRESHOLD: Duration = Duration::from_secs(7 * 24 * 3600);

/// TLS 材料落盘位置与服务器证书元数据文件名。
const CA_CERT_FILE: &str = "ca.crt";
const CA_KEY_FILE: &str = "ca.key";
const SERVER_CERT_FILE: &str = "server.crt";
const SERVER_KEY_FILE: &str = "server.key";
const SERVER_META_FILE: &str = "server.meta.json";

/// 服务器证书签发元数据（用于判断 IP 集合变化与临近过期）。
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ServerCertMeta {
    /// 签发时的本机 IPv4 地址集合（排序去重）。
    ips: Vec<String>,
    /// 证书过期时刻（Unix 秒）。
    not_after_unix: i64,
}

/// 就绪可用的 TLS 材料路径。
#[derive(Clone, Debug)]
pub struct TlsMaterial {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

/// 本机根证书信任状态（`Cert:\CurrentUser\Root`）。
///
/// Chrome / Edge 读取 Windows 证书存储，安装后访问 `https://<本机IP>:<端口>`
/// 不再有证书告警；**Firefox 使用自带证书库**，不读 Windows 存储，需在
/// `about:config` 打开 `security.enterprise_roots.enabled`（一次性）或手动导入。
#[derive(Clone, Debug, Serialize)]
pub struct CaTrustStatus {
    /// 当前 CA 是否已安装到当前用户的受信任根证书存储。
    pub installed: bool,
    /// 状态是否成功检测（Windows 且 PowerShell 可用）。
    pub checked: bool,
    /// CA 证书文件路径（供用户在 Firefox 等场景手动导入）。
    pub ca_path: Option<String>,
    /// 面向用户的说明。
    pub message: String,
}

/// 由配置文件路径推导 TLS 材料目录（`<config_dir>/tls`）。
pub fn tls_dir_for(config_path: &Path) -> Option<PathBuf> {
    config_path.parent().map(|dir| dir.join("tls"))
}

/// 查询本机根证书信任状态（只读，不修改证书存储）。
pub fn local_ca_status(tls_dir: &Path) -> CaTrustStatus {
    let ca_path = tls_dir.join(CA_CERT_FILE);
    #[cfg(windows)]
    {
        if !ca_path.is_file() {
            return CaTrustStatus {
                installed: false,
                checked: true,
                ca_path: None,
                message: "尚未生成本地根证书，请先开启网络功能。".to_owned(),
            };
        }
        let script = store_script(
            "ReadOnly",
            &[
                "$present = @($store.Certificates | Where-Object { $_.Thumbprint -eq $cert.Thumbprint }).Count",
                "if ($present -gt 0) { 'installed' } else { 'not_installed' }",
            ],
            &ca_path,
        );
        match run_powershell(&script) {
            Ok(stdout) if stdout.trim() == "installed" => CaTrustStatus {
                installed: true,
                checked: true,
                ca_path: Some(ca_path.display().to_string()),
                message: "本机已信任该根证书，Chrome / Edge 访问不再告警。".to_owned(),
            },
            Ok(_) => CaTrustStatus {
                installed: false,
                checked: true,
                ca_path: Some(ca_path.display().to_string()),
                message: "本机尚未信任该根证书，浏览器会提示不安全。".to_owned(),
            },
            Err(error) => CaTrustStatus {
                installed: false,
                checked: false,
                ca_path: Some(ca_path.display().to_string()),
                message: format!("检测根证书信任状态失败: {error}"),
            },
        }
    }
    #[cfg(not(windows))]
    {
        let _ = tls_dir;
        CaTrustStatus {
            installed: false,
            checked: false,
            ca_path: Some(ca_path.display().to_string()),
            message: "当前平台不支持自动安装根证书，请手动导入 CA。".to_owned(),
        }
    }
}

/// 把本机根证书安装到当前用户的受信任根证书存储（**无需管理员**）。
///
/// 幂等：已安装则不重复添加；同时清理旧的 LoopMaster 根证书（CA 重新生成后的
/// 残留），避免信任库里堆积失效的信任锚。
pub fn install_local_ca(tls_dir: &Path) -> Result<CaTrustStatus, super::WebServerError> {
    let ca_path = tls_dir.join(CA_CERT_FILE);
    if !ca_path.is_file() {
        // CA 尚未生成（Web 控制台没启动过）：先生成材料。
        ensure_tls_material(tls_dir, None)?;
    }
    #[cfg(windows)]
    {
        let script = store_script(
            "ReadWrite",
            &[
                "$stale = @($store.Certificates | Where-Object { $_.Subject -like '*LoopMaster*' -and $_.Thumbprint -ne $cert.Thumbprint })",
                "foreach ($item in $stale) { $store.Remove($item) }",
                "$present = @($store.Certificates | Where-Object { $_.Thumbprint -eq $cert.Thumbprint }).Count",
                "if ($present -eq 0) { $store.Add($cert) }",
                "'installed'",
            ],
            &ca_path,
        );
        run_powershell(&script)
            .map_err(|e| super::WebServerError::Tls(format!("安装根证书失败: {e}")))?;
        Ok(local_ca_status(tls_dir))
    }
    #[cfg(not(windows))]
    {
        Err(super::WebServerError::Tls(
            "当前平台不支持自动安装根证书".to_owned(),
        ))
    }
}

/// 从当前用户的受信任根证书存储中移除 LoopMaster 根证书。
pub fn remove_local_ca(tls_dir: &Path) -> Result<CaTrustStatus, super::WebServerError> {
    #[cfg(windows)]
    {
        let ca_path = tls_dir.join(CA_CERT_FILE);
        let script = store_script(
            "ReadWrite",
            &[
                "$matches = @($store.Certificates | Where-Object { $_.Subject -like '*LoopMaster*' })",
                "foreach ($item in $matches) { $store.Remove($item) }",
                "'removed'",
            ],
            &ca_path,
        );
        if ca_path.is_file() {
            run_powershell(&script)
                .map_err(|e| super::WebServerError::Tls(format!("移除根证书失败: {e}")))?;
        }
        Ok(local_ca_status(tls_dir))
    }
    #[cfg(not(windows))]
    {
        let _ = tls_dir;
        Err(super::WebServerError::Tls(
            "当前平台不支持移除根证书".to_owned(),
        ))
    }
}

/// 组装操作 `Cert:\CurrentUser\Root` 的 PowerShell 脚本。
///
/// `body` 为已打开存储后可执行的语句（脚本自带 `$cert` / `$store` 与 try/finally）。
#[cfg(windows)]
fn store_script(open_mode: &str, body: &[&str], ca_path: &Path) -> String {
    let mut lines: Vec<String> = vec![
        "$ErrorActionPreference = 'Stop'".to_owned(),
        format!(
            "$cert = [System.Security.Cryptography.X509Certificates.X509Certificate2]::new('{}')",
            ca_path.display().to_string().replace('\'', "''")
        ),
        format!(
            "$store = [System.Security.Cryptography.X509Certificates.X509Store]::new('Root','CurrentUser')"
        ),
        format!("$store.Open('{open_mode}')"),
        "try {".to_owned(),
    ];
    for line in body {
        lines.push(format!("  {line}"));
    }
    lines.push("} finally { $store.Close() }".to_owned());
    lines.join("\r\n")
}

/// 执行 PowerShell 脚本（脚本写入临时文件，避免多层引号转义）。
#[cfg(windows)]
fn run_powershell(script: &str) -> Result<String, String> {
    let script_path = std::env::temp_dir().join("loopmaster-ca-store.ps1");
    std::fs::write(&script_path, script).map_err(|e| format!("写入脚本失败: {e}"))?;
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            &script_path.display().to_string(),
        ])
        .output()
        .map_err(|e| format!("执行 PowerShell 失败: {e}"))?;
    let _ = std::fs::remove_file(&script_path);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("PowerShell 返回失败: {}", stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// 确保本地 CA 与服务器证书就绪；需要时生成/重签。
///
/// 返回可直接交给 rustls 的证书与私钥路径。`host_ips` 为当前需要写入 IP SAN
/// 的地址集合；`None` 表示自动探测本机 IPv4（另加 `127.0.0.1`）。
pub fn ensure_tls_material(
    tls_dir: &Path,
    host_ips: Option<Vec<IpAddr>>,
) -> Result<TlsMaterial, super::WebServerError> {
    std::fs::create_dir_all(tls_dir)
        .map_err(|e| super::WebServerError::Tls(format!("创建 TLS 目录失败: {e}")))?;

    let ca = ensure_ca(tls_dir)?;
    let ips = host_ips.unwrap_or_else(default_host_ips);
    ensure_server_cert(tls_dir, ca, &ips)?;

    Ok(TlsMaterial {
        cert_path: tls_dir.join(SERVER_CERT_FILE),
        key_path: tls_dir.join(SERVER_KEY_FILE),
    })
}

/// 默认主机地址集合：本机全部 IPv4 + 回环。
fn default_host_ips() -> Vec<IpAddr> {
    let mut ips: Vec<IpAddr> = local_ipv4_addresses()
        .into_iter()
        .filter_map(|text| text.parse::<IpAddr>().ok())
        .collect();
    ips.push(IpAddr::from([127, 0, 0, 1]));
    ips.sort();
    ips.dedup();
    ips
}

/// 确保本地根 CA 存在；缺失时生成并落盘，存在时从 PEM 恢复。
fn ensure_ca(tls_dir: &Path) -> Result<Ca, super::WebServerError> {
    let cert_path = tls_dir.join(CA_CERT_FILE);
    let key_path = tls_dir.join(CA_KEY_FILE);
    match (
        std::fs::read_to_string(&cert_path),
        std::fs::read_to_string(&key_path),
    ) {
        (Ok(cert_pem), Ok(key_pem)) => {
            let key = KeyPair::from_pem(&key_pem)
                .map_err(|e| super::WebServerError::Tls(format!("恢复 CA 私钥失败: {e}")))?;
            // 从 PEM 恢复既有 CA 参数（含原序列号与有效期）+ 既有私钥重建签发器；
            // 同参数同密钥重签结果与原证书一致。
            let params = CertificateParams::from_ca_cert_pem(&cert_pem)
                .map_err(|e| super::WebServerError::Tls(format!("解析 CA 证书失败: {e}")))?;
            let cert = params
                .self_signed(&key)
                .map_err(|e| super::WebServerError::Tls(format!("恢复 CA 证书失败: {e}")))?;
            Ok(Ca { cert, key })
        }
        _ => {
            let key = KeyPair::generate()
                .map_err(|e| super::WebServerError::Tls(format!("生成 CA 私钥失败: {e}")))?;
            let mut params = CertificateParams::default();
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params
                .distinguished_name
                .push(DnType::CommonName, "LoopMaster Local CA");
            let now = OffsetDateTime::now_utc();
            params.not_before = now - time::Duration::days(1);
            params.not_after = now + to_time_duration(CA_VALIDITY);
            let cert = params
                .self_signed(&key)
                .map_err(|e| super::WebServerError::Tls(format!("自签 CA 失败: {e}")))?;
            write_private(&cert_path, cert.pem())?;
            write_private(&key_path, key.serialize_pem())?;
            Ok(Ca { cert, key })
        }
    }
}

/// 本地根 CA（签发器）。
struct Ca {
    cert: Certificate,
    key: KeyPair,
}

/// 确保服务器证书就绪：缺失、IP 集合变化或临近过期时重签。
fn ensure_server_cert(tls_dir: &Path, ca: Ca, ips: &[IpAddr]) -> Result<(), super::WebServerError> {
    let cert_path = tls_dir.join(SERVER_CERT_FILE);
    let key_path = tls_dir.join(SERVER_KEY_FILE);
    let meta_path = tls_dir.join(SERVER_META_FILE);

    let ips_text: Vec<String> = ips.iter().map(|ip| ip.to_string()).collect();
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let meta = std::fs::read_to_string(&meta_path)
        .ok()
        .and_then(|text| serde_json::from_str::<ServerCertMeta>(&text).ok());
    let needs_reissue = match meta {
        Some(meta) => {
            meta.ips != ips_text
                || now_unix + REISSUE_THRESHOLD.as_secs() as i64 >= meta.not_after_unix
        }
        None => true,
    };
    if !needs_reissue && cert_path.is_file() && key_path.is_file() {
        return Ok(());
    }

    let key = KeyPair::generate()
        .map_err(|e| super::WebServerError::Tls(format!("生成服务器私钥失败: {e}")))?;
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::NoCa;
    params
        .distinguished_name
        .push(DnType::CommonName, "LoopMaster Web Console");
    let localhost = Ia5String::try_from("localhost")
        .map_err(|e| super::WebServerError::Tls(format!("localhost SAN 无效: {e}")))?;
    params.subject_alt_names.push(SanType::DnsName(localhost));
    for ip in ips {
        params.subject_alt_names.push(SanType::IpAddress(*ip));
    }
    let now = OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + to_time_duration(SERVER_CERT_VALIDITY);
    let cert = params
        .signed_by(&key, &ca.cert, &ca.key)
        .map_err(|e| super::WebServerError::Tls(format!("签发服务器证书失败: {e}")))?;

    write_private(&cert_path, cert.pem())?;
    write_private(&key_path, key.serialize_pem())?;
    let meta = ServerCertMeta {
        ips: ips_text,
        not_after_unix: (now + to_time_duration(SERVER_CERT_VALIDITY)).unix_timestamp(),
    };
    std::fs::write(
        &meta_path,
        serde_json::to_vec_pretty(&meta)
            .map_err(|e| super::WebServerError::Tls(format!("序列化证书元数据失败: {e}")))?,
    )
    .map_err(|e| super::WebServerError::Tls(format!("写入证书元数据失败: {e}")))?;
    Ok(())
}

/// `std::time::Duration` 转 `time::Duration`（rcgen 使用 time crate）。
fn to_time_duration(duration: Duration) -> time::Duration {
    time::Duration::seconds(duration.as_secs() as i64)
}

/// 写入秘密材料（CA/服务器私钥与证书），失败即报错，不允许静默半成品。
fn write_private(path: &Path, contents: String) -> Result<(), super::WebServerError> {
    std::fs::write(path, contents)
        .map_err(|e| super::WebServerError::Tls(format!("写入 {} 失败: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_tls_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "loopmaster-tls-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn fixed_ips() -> Vec<IpAddr> {
        vec![
            IpAddr::from([127, 0, 0, 1]),
            IpAddr::from([192, 168, 1, 10]),
        ]
    }

    #[test]
    fn ca_and_server_cert_are_generated_and_reused() {
        let dir = temp_tls_dir("gen");
        let material = ensure_tls_material(&dir, Some(fixed_ips())).unwrap();
        assert!(material.cert_path.is_file());
        assert!(material.key_path.is_file());
        assert!(dir.join(CA_CERT_FILE).is_file());
        assert!(dir.join(CA_KEY_FILE).is_file());

        let cert_pem = std::fs::read_to_string(&material.cert_path).unwrap();
        assert!(cert_pem.contains("BEGIN CERTIFICATE"));

        // 相同 IP 集合重复调用：复用现有材料（不重签）。
        let before = std::fs::read(&material.cert_path).unwrap();
        let again = ensure_tls_material(&dir, Some(fixed_ips())).unwrap();
        let after = std::fs::read(&again.cert_path).unwrap();
        assert_eq!(before, after);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ip_set_change_triggers_reissue() {
        let dir = temp_tls_dir("reissue");
        ensure_tls_material(&dir, Some(vec![IpAddr::from([192, 168, 1, 10])])).unwrap();
        let before = std::fs::read(dir.join(SERVER_CERT_FILE)).unwrap();

        ensure_tls_material(&dir, Some(vec![IpAddr::from([192, 168, 1, 20])])).unwrap();
        let after = std::fs::read(dir.join(SERVER_CERT_FILE)).unwrap();
        assert_ne!(before, after, "IP 集合变化必须重签服务器证书");

        // CA 不变：重签只换服务器证书，手机端信任根后无需重复安装。
        assert!(dir.join(CA_CERT_FILE).is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 只读验证：新生成的 CA 未被信任（不修改真实证书存储，安装/移除由真机验证）。
    #[test]
    fn fresh_ca_is_reported_as_not_trusted() {
        let dir = temp_tls_dir("trust-status");
        ensure_tls_material(&dir, Some(fixed_ips())).unwrap();
        let status = local_ca_status(&dir);
        // 检测本身应成功（PowerShell 可用）；信任结果是环境相关的，只断言不 panic
        // 且 CA 路径被回填。
        assert!(!status.installed, "临时目录新建的 CA 不应已在本机信任库");
        assert_eq!(
            status.ca_path.as_deref(),
            Some(dir.join(CA_CERT_FILE).display().to_string().as_str())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tls_dir_is_derived_from_config_path() {
        let config = std::path::Path::new("C:/config/LoopMaster/config.json");
        assert_eq!(
            tls_dir_for(config).unwrap(),
            std::path::PathBuf::from("C:/config/LoopMaster/tls")
        );
    }

    #[test]
    fn ca_survives_process_restart_via_pem_reload() {
        let dir = temp_tls_dir("reload");
        ensure_tls_material(&dir, Some(fixed_ips())).unwrap();
        let ca_before = std::fs::read(dir.join(CA_CERT_FILE)).unwrap();
        let server_before = std::fs::read(dir.join(SERVER_CERT_FILE)).unwrap();

        // 删除服务器材料（模拟过期清理），CA 保留；重签后新服务器证书必须仍
        // 能由同一 CA 签出。
        std::fs::remove_file(dir.join(SERVER_CERT_FILE)).unwrap();
        std::fs::remove_file(dir.join(SERVER_KEY_FILE)).unwrap();
        std::fs::remove_file(dir.join(SERVER_META_FILE)).unwrap();
        ensure_tls_material(&dir, Some(fixed_ips())).unwrap();

        assert_eq!(ca_before, std::fs::read(dir.join(CA_CERT_FILE)).unwrap());
        assert_ne!(
            server_before,
            std::fs::read(dir.join(SERVER_CERT_FILE)).unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
