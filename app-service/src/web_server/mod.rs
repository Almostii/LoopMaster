//! 桌面内嵌 Web 控制台（Phase 2 子任务 1 骨架）。
//!
//! 架构约束（方案 2 §2.1）：
//! - 运行在**独立 Tokio runtime**（专用线程，`worker_threads=2`），与 WASAPI
//!   实时线程零共享；与 Tauri async runtime 的取舍在子任务 2 原型报告中
//!   比较后冻结，本阶段以独立 runtime 优先保证关闭顺序与故障隔离；
//! - 对 StateHub 只做只读快照与 revision 订阅，绝不写入权威状态；
//! - **默认 HTTP**（`tls: false`）：浏览器控制台直接打开、无需安装证书
//!   （产品决策 2026-08-31）；`tls: true` 时提供 HTTPS/WSS（rustls），
//!   供手机 App（Phase 3，固定本机 CA）与高级场景使用；
//! - `/ws` 双向通道属子任务 2。

pub mod routes;
pub mod tls;

pub use tls::{local_ca_status, tls_dir_for, CaTrustStatus};

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use thiserror::Error;

/// 服务端类型（HTTP 与 TLS 的 acceptor 泛型不同，需分装后统一 serve）。
enum ServeKind {
    Http(axum_server::Server<axum_server::accept::DefaultAcceptor>),
    Tls(axum_server::Server<axum_server::tls_rustls::RustlsAcceptor>),
}

use crate::state::StateHub;

/// Web 控制台端口默认值（与 mDNS TXT `web_port` 默认一致，方案 1 §6）。
pub const DEFAULT_WEB_PORT: u16 = 8920;

/// Web 服务配置。
#[derive(Clone, Debug)]
pub struct WebServerConfig {
    /// 监听端口；绑定地址固定 `0.0.0.0`。
    pub port: u16,
    /// 是否启用 TLS（HTTPS/WSS）。默认 `false`（纯 HTTP）：
    ///
    /// - `false`：浏览器控制台默认走 `http://<本机IP>:<端口>`，直接打开
    ///   无需安装证书（产品决策 2026-08-31：无感体验优先，控制指令明文
    ///   风险在家庭局域网可接受）；
    /// - `true`：HTTPS/WSS，供手机 App（Phase 3，App 固定本机 CA）与高级
    ///   场景使用，需本机 CA（见 `tls.rs`）。
    pub tls: bool,
}

impl WebServerConfig {
    pub fn bind_addr(&self) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, self.port))
    }
}

/// Web 服务错误。
#[derive(Debug, Error)]
pub enum WebServerError {
    #[error("配置目录无效，无法定位 TLS 材料目录")]
    NoConfigDir,
    #[error("TLS 材料错误: {0}")]
    Tls(String),
    #[error("Web 服务启动失败: {0}")]
    Start(String),
}

impl From<io::Error> for WebServerError {
    fn from(error: io::Error) -> Self {
        WebServerError::Tls(format!("文件读写失败: {error}"))
    }
}

/// 运行中的 Web 服务句柄。
///
/// `Drop` 发送关闭信号但不 join（与桥接/mDNS 关闭路径一致，避免阻塞 UI 线程）；
/// 需要等待退出的调用方使用 [`WebServerHandle::shutdown`]。
pub struct WebServerHandle {
    /// 实际监听地址（端口 0 时为系统分配的结果）。
    addr: SocketAddr,
    shutdown: mpsc::Sender<()>,
    join: Option<JoinHandle<()>>,
}

impl WebServerHandle {
    /// 实际监听地址。
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 请求优雅关闭并等待服务线程退出（长超时兜底，不无限阻塞）。
    pub fn shutdown(mut self) {
        let _ = self.shutdown.send(());
        if let Some(join) = self.join.take() {
            // 服务线程自身有 graceful shutdown 超时，join 不会永久阻塞。
            let _ = join.join();
        }
    }
}

impl Drop for WebServerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
    }
}

/// 启动内嵌 Web 服务（非阻塞；就绪后返回句柄）。
///
/// 启动流程在专用线程内完成：创建独立 Tokio runtime →（TLS 模式）生成/恢复
/// TLS 材料 → 绑定端口 → 上报就绪 → serve。任一步骤失败通过就绪通道回传错误。
pub fn start(
    config: WebServerConfig,
    hub: std::sync::Arc<StateHub>,
) -> Result<WebServerHandle, WebServerError> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<SocketAddr, String>>();
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
    let join = std::thread::Builder::new()
        .name("loopmaster-web-server".into())
        .spawn(move || run_server(config, hub, ready_tx, shutdown_rx))
        .map_err(|e| WebServerError::Start(format!("创建 Web 服务线程失败: {e}")))?;

    match ready_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(addr)) => Ok(WebServerHandle {
            addr,
            shutdown: shutdown_tx,
            join: Some(join),
        }),
        Ok(Err(message)) => Err(WebServerError::Start(message)),
        Err(_) => {
            // 就绪超时：请求关闭并回收线程，不留悬挂服务。
            let _ = shutdown_tx.send(());
            let _ = join.join();
            Err(WebServerError::Start(
                "Web 服务启动超时（30 秒）".to_owned(),
            ))
        }
    }
}

/// 服务线程主体：创建 runtime 并阻塞执行。
fn run_server(
    config: WebServerConfig,
    hub: std::sync::Arc<StateHub>,
    ready_tx: mpsc::Sender<Result<SocketAddr, String>>,
    shutdown_rx: mpsc::Receiver<()>,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("loopmaster-web-runtime")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready_tx.send(Err(format!("创建 Tokio runtime 失败: {error}")));
            return;
        }
    };
    runtime.block_on(serve(config, hub, ready_tx, shutdown_rx));
    runtime.shutdown_timeout(Duration::from_secs(5));
}

/// 异步主流程：绑定 →（TLS 模式）TLS 材料 → 就绪 → serve（优雅关闭）。
async fn serve(
    config: WebServerConfig,
    hub: std::sync::Arc<StateHub>,
    ready_tx: mpsc::Sender<Result<SocketAddr, String>>,
    shutdown_rx: mpsc::Receiver<()>,
) {
    // 自行绑定 TcpListener：绑定错误能通过就绪通道回传，端口 0 时也能拿到
    // 实际地址（axum-server 的 bind_rustls 延迟绑定，两者都拿不到）。
    let addr = config.bind_addr();
    let listener = match std::net::TcpListener::bind(addr) {
        Ok(listener) => listener,
        Err(error) => {
            let _ = ready_tx.send(Err(format!("绑定 {addr} 失败: {error}")));
            return;
        }
    };
    let bound_addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(error) => {
            let _ = ready_tx.send(Err(format!("读取监听地址失败: {error}")));
            return;
        }
    };
    // 通配绑定（0.0.0.0:0）时 local_addr 不可直连，回报用回环地址。
    let reported_addr = if bound_addr.ip().is_unspecified() {
        SocketAddr::from(([127, 0, 0, 1], bound_addr.port()))
    } else {
        bound_addr
    };

    let server = if config.tls {
        // HTTPS：本机 CA + rustls（供手机 App / 高级场景）。
        let tls_dir = hub
            .config_path()
            .parent()
            .map(|dir| dir.join("tls"))
            .ok_or_else(|| "配置目录无效，无法定位 TLS 材料目录".to_owned());
        let material = match tls_dir {
            Ok(tls_dir) => {
                match tokio::task::spawn_blocking(move || tls::ensure_tls_material(&tls_dir, None))
                    .await
                {
                    Ok(Ok(material)) => material,
                    Ok(Err(error)) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                        return;
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!("TLS 材料任务失败: {error}")));
                        return;
                    }
                }
            }
            Err(error) => {
                let _ = ready_tx.send(Err(error));
                return;
            }
        };
        let tls_config = match axum_server::tls_rustls::RustlsConfig::from_pem_file(
            &material.cert_path,
            &material.key_path,
        )
        .await
        {
            Ok(config) => config,
            Err(error) => {
                let _ = ready_tx.send(Err(format!("加载 TLS 配置失败: {error}")));
                return;
            }
        };
        ServeKind::Tls(axum_server::from_tcp_rustls(listener, tls_config))
    } else {
        // HTTP：浏览器控制台默认入口，无需证书。
        ServeKind::Http(axum_server::from_tcp(listener))
    };
    let _ = ready_tx.send(Ok(reported_addr));

    // 关闭信号桥接：std mpsc（UI 线程）→ 优雅关闭。
    let server_handle = axum_server::Handle::new();
    let shutdown_handle = server_handle.clone();
    let shutdown_task = tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || shutdown_rx.recv()).await;
        shutdown_handle.graceful_shutdown(Some(Duration::from_secs(5)));
    });

    // revision 投影：验证 StateHub watch 通知跨 runtime 边界可用（子任务 2
    // 的 /ws 广播以此为基线）；同时作为内嵌服务存活的诊断线索。
    let mut revision_rx = hub.subscribe();
    let revision_task = tokio::spawn(async move { while revision_rx.changed().await.is_ok() {} });

    let app = routes::router().into_make_service();
    match server {
        ServeKind::Http(server) => {
            let _ = server.handle(server_handle).serve(app).await;
        }
        ServeKind::Tls(server) => {
            let _ = server.handle(server_handle).serve(app).await;
        }
    }
    shutdown_task.abort();
    revision_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试二进制里 rustls 同时启用 ring（本 crate dev-dep）与 aws-lc-rs
    /// （axum-server 传递），进程级 provider 产生歧义；显式安装一次以消除。
    fn install_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[test]
    fn bind_addr_uses_configured_port() {
        let config = WebServerConfig {
            port: 12345,
            tls: false,
        };
        assert_eq!(config.bind_addr().to_string(), "0.0.0.0:12345");
        assert_eq!(DEFAULT_WEB_PORT, 8920);
    }

    /// 端到端：启动（端口 0）→ TCP 可连接 → 优雅关闭。
    ///
    /// HTTP 语义由 routes 测试覆盖；此处验证绑定、就绪回报与关闭路径。
    /// 浏览器侧 TLS 握手验证属真机验收（Plan 冻结文档 §3.2）。
    #[test]
    fn start_binds_and_shuts_down() {
        install_crypto_provider();
        let hub = std::sync::Arc::new(StateHub::new(std::env::temp_dir().join(format!(
            "loopmaster-web-e2e-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ))));
        let handle = start(
            WebServerConfig {
                port: 0,
                tls: false,
            },
            hub,
        )
        .expect("端口 0 应由系统分配且启动成功");

        // 监听已绑定：TCP 三次握手应成功。
        let stream = std::net::TcpStream::connect(handle.addr()).expect("监听应接受 TCP 连接");
        drop(stream);

        handle.shutdown();
    }

    /// 端到端（HTTP 默认模式）：请求 `GET /api/health` 返回 200 与 `{"ok":true}`。
    ///
    /// 对应产品决策：浏览器控制台默认 `http://<本机IP>:<端口>`，无需证书。
    #[tokio::test]
    async fn health_is_reachable_over_http() {
        let hub = std::sync::Arc::new(StateHub::new(std::env::temp_dir().join(format!(
            "loopmaster-web-http-e2e-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ))));
        let handle = start(
            WebServerConfig {
                port: 0,
                tls: false,
            },
            hub,
        )
        .expect("启动成功");

        let mut stream = tokio::net::TcpStream::connect(handle.addr()).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        stream
            .write_all(b"GET /api/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 200 OK"), "响应行: {text}");
        assert!(text.contains(r#"{"ok":true}"#), "响应体: {text}");

        handle.shutdown();
    }

    /// 端到端（TLS）：以本地 CA 为信任锚完成真实握手，请求 `GET /api/health`。
    ///
    /// 对应子任务 1 验收项"`/api/health` 可访问"；浏览器首次信任流程仍属
    /// 真机人工验收（Plan 冻结文档 §3.2）。
    #[tokio::test]
    async fn health_is_reachable_over_tls_with_local_ca() {
        install_crypto_provider();
        let config_dir = std::env::temp_dir().join(format!(
            "loopmaster-web-tls-e2e-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.json");

        let hub = std::sync::Arc::new(StateHub::new(config_path));
        let handle = start(WebServerConfig { port: 0, tls: true }, hub).expect("启动成功");

        // CA 证书即信任锚（手机端显式安装同一张 ca.crt）。
        let ca_pem = std::fs::read_to_string(config_dir.join("tls").join("ca.crt")).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut ca_pem.as_bytes()) {
            roots.add(cert.unwrap()).expect("CA 证书应可加入信任锚");
        }
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(client_config));

        let tcp = tokio::net::TcpStream::connect(handle.addr()).await.unwrap();
        let server_name = rustls::pki_types::ServerName::try_from("localhost".to_string())
            .expect("localhost 应为合法 ServerName");
        let mut tls = connector
            .connect(server_name, tcp)
            .await
            .expect("TLS 握手应成功");

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tls.write_all(b"GET /api/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tls.read_to_end(&mut response).await.unwrap();
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 200 OK"), "响应行: {text}");
        assert!(text.contains(r#"{"ok":true}"#), "响应体: {text}");

        handle.shutdown();
        let _ = std::fs::remove_dir_all(&config_dir);
    }
}
