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

pub mod auth;
pub mod routes;
pub mod tls;
pub mod ws;

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
/// 电平广播频率默认值（候选，子任务 2 原型对比 30/60Hz 后冻结）。
pub const DEFAULT_METER_HZ: u16 = 30;

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
    /// 电平广播频率（Hz）。子任务 2 原型对比 30/60Hz 后冻结默认值。
    pub meter_hz: u16,
}

impl Default for WebServerConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_WEB_PORT,
            tls: false,
            meter_hz: DEFAULT_METER_HZ,
        }
    }
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

    // meter 广播任务 + /ws 路由（子任务 2）+ 配对/可信设备（子任务 4）。
    let meter_tx = ws::spawn_meter_task(hub.clone(), config.meter_hz);
    let app = ws::ws_router()
        .with_state(ws::WsState {
            hub: hub.clone(),
            meter_tx,
            auth: hub.auth().clone(),
            is_https: config.tls,
            require_pairing: hub.require_pairing_flag(),
        })
        .merge(routes::router())
        .into_make_service_with_connect_info::<SocketAddr>();
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
            ..Default::default()
        };
        assert_eq!(config.bind_addr().to_string(), "0.0.0.0:12345");
        assert_eq!(DEFAULT_WEB_PORT, 8920);
        assert_eq!(DEFAULT_METER_HZ, 30);
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
                ..Default::default()
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
                ..Default::default()
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
        let handle = start(
            WebServerConfig {
                port: 0,
                tls: true,
                ..Default::default()
            },
            hub,
        )
        .expect("启动成功");

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

    /// 端到端（WebSocket）：连接即下发 `initial_state`，控制指令回 `ack` 且
    /// 写入权威状态，随后收到二进制 meter 帧（30Hz 广播）。
    #[tokio::test]
    async fn ws_round_trip_initial_state_ack_and_meter() {
        use futures_util::SinkExt as _;
        use futures_util::StreamExt as _;

        let config_dir = std::env::temp_dir().join(format!(
            "loopmaster-ws-e2e-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&config_dir).unwrap();
        let hub = std::sync::Arc::new(StateHub::new(config_dir.join("config.json")));
        // 准备一张图：source → bus → sink，send `send_mic_to_master`。
        use crate::route::RouteEdit;
        use loopmaster_audio_core::{
            BusSpec, EndpointId, SendSpec, SinkId, SinkKind, SinkSpec, SourceId, SourceKind,
            SourceSpec,
        };
        hub.apply_route_edit(RouteEdit::AddSource(SourceSpec {
            id: SourceId("src_mic_1".into()),
            kind: SourceKind::DeviceCapture,
            endpoint_id: Some(EndpointId("endpoint-1".into())),
            process_id: None,
            executable_path: None,
            stream_name: None,
            display_name: "麦克风".into(),
        }))
        .unwrap();
        hub.apply_route_edit(RouteEdit::AddBus(BusSpec {
            id: loopmaster_audio_core::BusId("bus_master_1".into()),
            display_name: "主通道".into(),
        }))
        .unwrap();
        hub.apply_route_edit(RouteEdit::AddSink(SinkSpec {
            id: SinkId("out_spk_1".into()),
            endpoint_id: EndpointId("endpoint-out".into()),
            display_name: "扬声器".into(),
            kind: SinkKind::Device,
            stream_name: None,
            remote_addr: None,
        }))
        .unwrap();
        hub.apply_route_edit(RouteEdit::SetSend(SendSpec::SourceToBus {
            id: loopmaster_audio_core::SendId("send_mic_to_master".into()),
            source_id: SourceId("src_mic_1".into()),
            bus_id: loopmaster_audio_core::BusId("bus_master_1".into()),
            gain_db: 0.0,
            muted: false,
            enabled: true,
            channel_map: Vec::new(),
        }))
        .unwrap();

        let handle = start(
            WebServerConfig {
                port: 0,
                tls: false,
                meter_hz: 30,
            },
            hub.clone(),
        )
        .expect("启动成功");

        // 配对并取得凭证（/ws 现在要求可信设备 Cookie）。
        let pairing = hub.auth().start_pairing();
        let credential = hub
            .auth()
            .pair(Some(&pairing.secret), None, "TestPhone", "127.0.0.1")
            .unwrap();
        let url = format!("ws://127.0.0.1:{}/ws", handle.addr().port());
        let request = ws_request(
            &url,
            Some(&format!("lm_device={credential}")),
            &format!("http://127.0.0.1:{}", handle.addr().port()),
        );
        let (mut ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("连接 /ws");

        // 1) 连接即收 initial_state。
        let first = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("initial_state 超时")
            .expect("流结束")
            .expect("协议错误");
        let first = first.into_text().unwrap();
        assert!(first.contains(r#""event":"initial_state""#), "{first}");
        assert!(first.contains(r#""state_revision""#), "{first}");
        assert!(first.contains("send_mic_to_master"), "{first}");

        // 2) 控制指令 → ack，且权威状态被修改。
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({
                "seq": 101,
                "action": "set_send_gain",
                "data": { "send_id": "send_mic_to_master", "gain_db": -3.0 }
            })
            .to_string(),
        ))
        .await
        .unwrap();
        // 控制命令会使 revision 变化并触发 initial_state 重推，可能先于 ack 到达；
        // 客户端本就应容忍任意消息顺序，这里循环跳过直到收到 ack。
        let mut ack: Option<String> = None;
        for _ in 0..10 {
            let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("ack 超时")
                .expect("流结束")
                .expect("协议错误");
            let text = message.into_text().unwrap();
            if text.contains(r#""ack":"set_send_gain""#) {
                ack = Some(text);
                break;
            }
        }
        let ack = ack.expect("应收到 set_send_gain 的 ack");
        assert!(ack.contains(r#""seq":101"#), "{ack}");
        assert_eq!(
            hub.route_snapshot().sends[0].gain_db(),
            -3.0,
            "增益应写入权威状态"
        );

        // 3) 30Hz meter 帧（二进制）应在 1 秒内到达。
        let mut got_meter = false;
        for _ in 0..40 {
            match tokio::time::timeout(Duration::from_millis(50), ws.next()).await {
                Ok(Some(Ok(message))) if message.is_binary() => {
                    got_meter = true;
                    break;
                }
                Ok(Some(Ok(_))) => {}
                _ => break,
            }
        }
        assert!(got_meter, "应在超时前收到二进制 meter 帧");

        handle.shutdown();
        let _ = std::fs::remove_dir_all(&config_dir);
    }

    /// 30/60Hz 电平广播实测（原型对比，`#[ignore]`：手动运行并读输出）。
    ///
    /// 统计 2 秒内收到的二进制 meter 帧数与期望帧数之比，用于排期 §2.2
    /// 「电平刷新率（待冻结）」验收：两档均不应出现持续积压/丢帧。
    #[tokio::test]
    #[ignore]
    async fn measure_meter_hz_30_vs_60() {
        use futures_util::StreamExt as _;
        for hz in [30u16, 60] {
            let hub = std::sync::Arc::new(StateHub::new(std::env::temp_dir().join(format!(
                "loopmaster-meter-measure-{hz}-{}-{:?}.json",
                std::process::id(),
                std::thread::current().id()
            ))));
            let handle = start(
                WebServerConfig {
                    port: 0,
                    tls: false,
                    meter_hz: hz,
                },
                hub.clone(),
            )
            .expect("启动成功");
            let pairing = hub.auth().start_pairing();
            let credential = hub
                .auth()
                .pair(Some(&pairing.secret), None, "TestPhone", "127.0.0.1")
                .unwrap();
            let url = format!("ws://127.0.0.1:{}/ws", handle.addr().port());
            let request = ws_request(
                &url,
                Some(&format!("lm_device={credential}")),
                &format!("http://127.0.0.1:{}", handle.addr().port()),
            );
            let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
            // 丢弃 initial_state。
            let _ = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();

            let window = Duration::from_secs(2);
            let start = std::time::Instant::now();
            let mut frames = 0u32;
            while start.elapsed() < window {
                match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
                    Ok(Some(Ok(message))) if message.is_binary() => frames += 1,
                    Ok(Some(Ok(_))) => {}
                    _ => break,
                }
            }
            let expected = hz as u32 * 2;
            println!(
                "[meter-measure] {hz}Hz：2 秒实收 {frames} 帧（期望 {expected}，约 {:.0}% 送达）",
                frames as f64 / expected as f64 * 100.0
            );
            handle.shutdown();
        }
    }

    /// 构造带自定义 Cookie/Origin 的 WS 握手请求（自动生成合法握手头）。
    fn ws_request(url: &str, cookie: Option<&str>, origin: &str) -> axum::http::Request<()> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = url.into_client_request().unwrap();
        if let Some(cookie) = cookie {
            request
                .headers_mut()
                .insert("Cookie", cookie.parse().unwrap());
        }
        request
            .headers_mut()
            .insert("Origin", origin.parse().unwrap());
        request
    }

    /// 极简 HTTP 客户端（测试用）：发送请求并返回状态码、响应文本与 Set-Cookie。
    async fn http_request(
        addr: SocketAddr,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (u16, String, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut request =
            format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
        for (key, value) in headers {
            request.push_str(&format!("{key}: {value}\r\n"));
        }
        if !body.is_empty() {
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");
        if !body.is_empty() {
            request.push_str(body);
        }
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buffer = Vec::new();
        stream.read_to_end(&mut buffer).await.unwrap();
        let text = String::from_utf8_lossy(&buffer).to_string();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let set_cookie = text
            .split("\r\n")
            .find(|line| line.to_ascii_lowercase().starts_with("set-cookie:"))
            .map(|line| {
                line.trim_start_matches("set-cookie:")
                    .trim_start_matches("Set-Cookie:")
                    .trim()
                    .to_string()
            })
            .unwrap_or_default();
        (status, text, set_cookie)
    }

    /// 配对全流程 + /ws 鉴权与 Origin 拒绝（任务书 §4.4 验收项）。
    #[tokio::test]
    async fn pair_flow_and_ws_auth_and_origin_rejection() {
        use futures_util::StreamExt as _;

        let config_dir = std::env::temp_dir().join(format!(
            "loopmaster-auth-e2e-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.json");
        // 本测试验证配对/鉴权路径，需显式开启 require_pairing。
        let mut config =
            crate::config::AppConfig::new(loopmaster_audio_core::RouteGraph::default());
        config.network.require_pairing = true;
        config.save_to(&config_path).unwrap();
        let hub = std::sync::Arc::new(StateHub::new(config_path));
        let handle = start(
            WebServerConfig {
                port: 0,
                tls: false,
                ..Default::default()
            },
            hub.clone(),
        )
        .expect("启动成功");
        let addr = handle.addr();

        // 1) 未开启配对窗口 → 403。
        let (status, _, _) = http_request(
            addr,
            "POST",
            "/api/auth/pair",
            &[("Content-Type", "application/json")],
            r#"{"client_name":"Phone"}"#,
        )
        .await;
        assert_eq!(status, 403, "未开启配对窗口应 403");

        // 2) 开启窗口后配对成功 → 200 + Set-Cookie 持久化凭证。
        let pairing = hub.auth().start_pairing();
        let payload = format!(r#"{{"secret":"{}","client_name":"Phone"}}"#, pairing.secret);
        let (status, body, set_cookie) = http_request(
            addr,
            "POST",
            "/api/auth/pair",
            &[("Content-Type", "application/json")],
            &payload,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(
            set_cookie.contains("lm_device="),
            "应签发凭证 Cookie: {set_cookie}"
        );
        assert!(
            set_cookie.to_ascii_lowercase().contains("httponly"),
            "凭证 Cookie 必须 HttpOnly: {set_cookie}"
        );
        let cookie = set_cookie.split(';').next().unwrap().trim().to_string();

        // 3) 带凭证 GET /api/auth/session → ok。
        let (status, body, _) =
            http_request(addr, "GET", "/api/auth/session", &[("Cookie", &cookie)], "").await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains(r#""ok":true"#), "{body}");

        // 4) /ws：无凭证 → 拒绝；有凭证但 Origin 不匹配 → 403。
        let url = format!("ws://127.0.0.1:{}/ws", addr.port());
        let origin = format!("http://127.0.0.1:{}", addr.port());
        let no_cookie = ws_request(&url, None, &origin);
        assert!(
            tokio_tungstenite::connect_async(no_cookie).await.is_err(),
            "无凭证应被拒绝"
        );
        let bad_origin = ws_request(&url, Some(&cookie), "http://evil.example.com");
        assert!(
            tokio_tungstenite::connect_async(bad_origin).await.is_err(),
            "Origin 不匹配应 403"
        );

        // 5) 有效凭证 + 匹配 Origin → 连接成功并收到 initial_state。
        let good = ws_request(&url, Some(&cookie), &origin);
        let (mut ws, _) = tokio_tungstenite::connect_async(good)
            .await
            .expect("应连接成功");
        let first = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("initial_state 超时")
            .expect("流结束")
            .expect("协议错误");
        assert!(
            first.into_text().unwrap().contains("initial_state"),
            "连接应下发 initial_state"
        );

        handle.shutdown();
        let _ = std::fs::remove_dir_all(&config_dir);
    }

    // ---------------------------------------------------------------------------
    // 6.2 收尾验收（排期 §2.2）
    // ---------------------------------------------------------------------------

    /// 在 hub 上准备 2 个 send（send_mic / send_music → bus_main），供并发/隔离/延迟测试。
    fn populate_two_sends(hub: &StateHub) {
        use crate::route::RouteEdit;
        use loopmaster_audio_core::{
            BusId, BusSpec, EndpointId, SendId, SendSpec, SinkId, SinkKind, SinkSpec, SourceId,
            SourceKind, SourceSpec,
        };
        for (source_id, name) in [("src_mic", "麦克风"), ("src_music", "音乐")] {
            hub.apply_route_edit(RouteEdit::AddSource(SourceSpec {
                id: SourceId(source_id.into()),
                kind: SourceKind::DeviceCapture,
                endpoint_id: Some(EndpointId(format!("ep-in-{source_id}"))),
                process_id: None,
                executable_path: None,
                stream_name: None,
                display_name: name.into(),
            }))
            .unwrap();
        }
        hub.apply_route_edit(RouteEdit::AddBus(BusSpec {
            id: BusId("bus_main".into()),
            display_name: "主通道".into(),
        }))
        .unwrap();
        hub.apply_route_edit(RouteEdit::AddSink(SinkSpec {
            id: SinkId("out_main".into()),
            endpoint_id: EndpointId("ep-out-1".into()),
            display_name: "扬声器".into(),
            kind: SinkKind::Device,
            stream_name: None,
            remote_addr: None,
        }))
        .unwrap();
        for send_id in ["send_mic", "send_music"] {
            hub.apply_route_edit(RouteEdit::SetSend(SendSpec::SourceToBus {
                id: SendId(send_id.into()),
                source_id: SourceId(format!("src_{}", send_id.trim_start_matches("send_"))),
                bus_id: BusId("bus_main".into()),
                gain_db: 0.0,
                muted: false,
                enabled: true,
                channel_map: Vec::new(),
            }))
            .unwrap();
        }
    }

    /// 读取 WS 流直到某条**文本**消息满足谓词（自动跳过 meter 二进制帧）。
    async fn ws_read_text<S>(stream: &mut S, pred: impl Fn(&str) -> bool) -> String
    where
        S: futures_util::Stream<
                Item = Result<
                    tokio_tungstenite::tungstenite::Message,
                    tokio_tungstenite::tungstenite::Error,
                >,
            > + Unpin,
    {
        use futures_util::StreamExt as _;
        loop {
            let message = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("读取 WS 消息超时")
                .expect("WS 流结束")
                .expect("WS 协议错误");
            if let tokio_tungstenite::tungstenite::Message::Text(text) = message {
                if pred(&text) {
                    return text;
                }
            }
        }
    }

    /// 等待一次 `initial_state` 重推（增益 -3.0）；`expect_ack` 时同时确认收到过 ack。
    async fn await_revision_resync<S>(stream: &mut S, expect_ack: bool) -> (String, bool)
    where
        S: futures_util::Stream<
                Item = Result<
                    tokio_tungstenite::tungstenite::Message,
                    tokio_tungstenite::tungstenite::Error,
                >,
            > + Unpin,
    {
        let mut ack_seen = false;
        let mut resync: Option<String> = None;
        loop {
            let text = ws_read_text(stream, |_| true).await;
            if text.contains(r#""ack""#) {
                ack_seen = true;
            }
            if resync.is_none() && text.contains("initial_state") && text.contains("-3.0") {
                resync = Some(text);
            }
            if resync.is_some() && (!expect_ack || ack_seen) {
                return (resync.take().unwrap(), ack_seen);
            }
        }
    }

    /// 读取直到收到 Close 帧或流结束，返回 (close_code, reason)。
    async fn ws_read_close<S>(stream: &mut S) -> (u16, String)
    where
        S: futures_util::Stream<
                Item = Result<
                    tokio_tungstenite::tungstenite::Message,
                    tokio_tungstenite::tungstenite::Error,
                >,
            > + Unpin,
    {
        use futures_util::StreamExt as _;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
                Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Close(Some(frame))))) => {
                    let code: u16 = frame.code.into();
                    return (code, frame.reason.to_string());
                }
                Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Close(None)))) => {
                    return (1000, String::new())
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(_))) | Ok(None) => return (0, "stream-ended".into()),
                Err(_) => panic!("等待吊销 close 超时"),
            }
        }
    }

    /// §2.2 并发连接（待冻结）：8 客户端收 initial_state、变更后一致重推、控制互不串扰。
    #[tokio::test]
    async fn concurrent_clients_receive_consistent_revision_resync() {
        use futures_util::SinkExt as _;

        let config_dir = std::env::temp_dir().join(format!(
            "loopmaster-conc-e2e-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&config_dir).unwrap();
        let hub = std::sync::Arc::new(StateHub::new(config_dir.join("config.json")));
        populate_two_sends(&hub);

        let handle = start(
            WebServerConfig {
                port: 0,
                tls: false,
                meter_hz: 30,
            },
            hub.clone(),
        )
        .expect("启动成功");
        let url = format!("ws://127.0.0.1:{}/ws", handle.addr().port());
        let origin = format!("http://127.0.0.1:{}", handle.addr().port());

        // 8 个客户端同时建立连接，各自收到 initial_state。
        const CLIENT_COUNT: usize = 8;
        let mut clients = Vec::with_capacity(CLIENT_COUNT);
        for index in 0..CLIENT_COUNT {
            let request = ws_request(&url, None, &origin);
            let connected = tokio::time::timeout(
                Duration::from_secs(5),
                tokio_tungstenite::connect_async(request),
            )
            .await
            .unwrap_or_else(|_| panic!("客户端 {index} connect_async 超时（5s）"));
            let (mut ws, _) = connected.expect("客户端应连接成功");
            let first = ws_read_text(&mut ws, |text| text.contains("initial_state")).await;
            assert!(
                first.contains("send_mic"),
                "客户端 {index} 应收到初始快照: {first}"
            );
            clients.push(ws);
        }

        // 客户端 0 修改 send_mic 增益。
        tokio::time::timeout(
            Duration::from_secs(5),
            clients[0].send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::json!({
                    "seq": 42,
                    "action": "set_send_gain",
                    "data": { "send_id": "send_mic", "gain_db": -3.0 }
                })
                .to_string(),
            )),
        )
        .await
        .expect("客户端 0 发送控制超时（5s）")
        .unwrap();

        // 所有客户端都应收到一致的重推：revision 相同、send_mic=-3.0、send_music=0.0。
        let mut expected_revision: Option<u64> = None;
        for (index, client) in clients.iter_mut().enumerate() {
            let (text, ack_seen) = await_revision_resync(client, index == 0).await;
            assert!(
                ack_seen || index != 0,
                "客户端 0 应收到 set_send_gain 的 ack"
            );
            let value: serde_json::Value = serde_json::from_str(&text).expect("重推应为合法 JSON");
            let revision = value["data"]["state_revision"]
                .as_u64()
                .expect("应有 state_revision");
            match expected_revision {
                None => expected_revision = Some(revision),
                Some(prev) => assert_eq!(
                    prev, revision,
                    "客户端 {index} 的 revision 应与其余客户端一致"
                ),
            }
            let sends = value["data"]["sends"].as_array().expect("应有 sends 数组");
            let mic_gain = sends
                .iter()
                .find(|send| send["send_id"] == "send_mic")
                .expect("应找到 send_mic")["gain_db"]
                .as_f64()
                .unwrap();
            let music_gain = sends
                .iter()
                .find(|send| send["send_id"] == "send_music")
                .expect("应找到 send_music")["gain_db"]
                .as_f64()
                .unwrap();
            assert_eq!(mic_gain, -3.0, "客户端 {index}: send_mic 增益应更新");
            assert_eq!(
                music_gain, 0.0,
                "客户端 {index}: send_music 增益应保持不变（互不串扰）"
            );
        }

        handle.shutdown();
        let _ = std::fs::remove_dir_all(&config_dir);
    }

    /// §2.2 配对凭证隔离：健康检查/静态页/配对响应/会话/持久化文件均不得泄露明文。
    #[tokio::test]
    async fn credential_isolation_no_secret_leak_on_public_surfaces() {
        let config_dir = std::env::temp_dir().join(format!(
            "loopmaster-leak-e2e-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.json");
        let mut config =
            crate::config::AppConfig::new(loopmaster_audio_core::RouteGraph::default());
        config.network.require_pairing = true;
        config.save_to(&config_path).unwrap();
        let hub = std::sync::Arc::new(StateHub::new(config_path));
        let handle = start(
            WebServerConfig {
                port: 0,
                tls: false,
                ..Default::default()
            },
            hub.clone(),
        )
        .expect("启动成功");
        let addr = handle.addr();

        // 完成一次真实配对，拿到 secret/pin/credential 三组敏感值。
        let pairing = hub.auth().start_pairing();
        let payload = format!(r#"{{"secret":"{}","client_name":"Phone"}}"#, pairing.secret);
        let (status, pair_body, set_cookie) = http_request(
            addr,
            "POST",
            "/api/auth/pair",
            &[("Content-Type", "application/json")],
            &payload,
        )
        .await;
        assert_eq!(status, 200, "{pair_body}");
        let cookie = set_cookie.split(';').next().unwrap().trim().to_string();
        let credential = cookie.trim_start_matches("lm_device=").to_string();
        assert!(!credential.is_empty(), "应取得凭证");

        let secrets = [
            pairing.secret.clone(),
            pairing.pin.clone(),
            credential.clone(),
        ];
        let assert_no_leak = |label: &str, text: &str| {
            for secret in &secrets {
                assert!(
                    !text.contains(secret.as_str()),
                    "{label} 泄露了敏感信息（{} 字符）",
                    secret.len()
                );
            }
        };

        // 公开表面逐一扫描（凭证经 Set-Cookie 下发是设计机制，这里只查响应体）。
        let body_only = |full: &str| full.split("\r\n\r\n").nth(1).unwrap_or(full).to_string();
        let (status, body, _) = http_request(addr, "GET", "/api/health", &[], "").await;
        assert_eq!(status, 200, "{body}");
        assert_no_leak("/api/health", &body);
        assert_no_leak("/api/auth/pair 响应体", &body_only(&pair_body));
        let (status, body, _) = http_request(addr, "GET", "/", &[], "").await;
        assert_eq!(status, 200, "{body}");
        assert_no_leak("index.html", &body);
        let (status, body, _) = http_request(addr, "GET", "/pair", &[], "").await;
        assert_eq!(status, 200, "{body}");
        assert_no_leak("SPA fallback (/pair)", &body);
        let (status, body, _) =
            http_request(addr, "GET", "/api/auth/session", &[("Cookie", &cookie)], "").await;
        assert_eq!(status, 200, "{body}");
        assert_no_leak("已授权 session", &body);
        let (status, body, _) = http_request(addr, "GET", "/api/auth/session", &[], "").await;
        assert_eq!(status, 401, "{body}");
        assert_no_leak("未授权 session", &body);

        // 持久化文件只存哈希。
        let disk =
            std::fs::read_to_string(config_dir.join("trusted-devices.json")).expect("应已持久化");
        assert_no_leak("trusted-devices.json", &disk);
        assert!(disk.contains("credential_hash"), "{disk}");
        assert!(
            disk.split('"')
                .any(|part| part.len() == 64 && part.chars().all(|c| c.is_ascii_hexdigit())),
            "持久化文件应含 64 位十六进制凭证哈希: {disk}"
        );

        handle.shutdown();
        let _ = std::fs::remove_dir_all(&config_dir);
    }

    /// §2.2 控制响应延迟（待冻结基线）：200 次 set_send_gain→ack 往返，输出 p50/p95。
    #[tokio::test]
    async fn control_rpc_round_trip_latency_baseline() {
        use futures_util::SinkExt as _;

        let config_dir = std::env::temp_dir().join(format!(
            "loopmaster-lat-e2e-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&config_dir).unwrap();
        let hub = std::sync::Arc::new(StateHub::new(config_dir.join("config.json")));
        populate_two_sends(&hub);
        let handle = start(
            WebServerConfig {
                port: 0,
                tls: false,
                meter_hz: 30,
            },
            hub,
        )
        .expect("启动成功");
        let url = format!("ws://127.0.0.1:{}/ws", handle.addr().port());
        let origin = format!("http://127.0.0.1:{}", handle.addr().port());
        let request = ws_request(&url, None, &origin);
        let (mut ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("连接 /ws");
        let _ = ws_read_text(&mut ws, |text| text.contains("initial_state")).await;

        const SAMPLES: u32 = 200;
        let mut latencies = Vec::with_capacity(SAMPLES as usize);
        for seq in 1..=SAMPLES {
            let payload = serde_json::json!({
                "seq": seq,
                "action": "set_send_gain",
                "data": { "send_id": "send_mic", "gain_db": -3.0 }
            })
            .to_string();
            let start = std::time::Instant::now();
            ws.send(tokio_tungstenite::tungstenite::Message::Text(payload))
                .await
                .unwrap();
            let ack = ws_read_text(&mut ws, |text| {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
                    value.get("seq").and_then(|s| s.as_u64()) == Some(seq as u64)
                        && value.get("ack").is_some()
                } else {
                    false
                }
            })
            .await;
            assert!(ack.contains(r#""ack":"set_send_gain""#), "{ack}");
            latencies.push(start.elapsed().as_secs_f64() * 1000.0);
        }

        latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p50 = latencies[((latencies.len() as f64) * 0.50) as usize];
        let p95 = latencies[((latencies.len() as f64) * 0.95) as usize];
        println!(
            "[rpc-latency] {SAMPLES} 次 set_send_gain→ack 往返：p50={p50:.2}ms p95={p95:.2}ms（待冻结基线）"
        );
        assert!(p95 < 200.0, "p95 {p95:.2}ms 应低于宽松上限 200ms（防回归）");

        handle.shutdown();
        let _ = std::fs::remove_dir_all(&config_dir);
    }

    /// §2.2 可信设备与重连 + 忘记设备：断线重连免重配、重启后凭据恢复、
    /// 忘记后原连接 close 4001、旧凭证拒连、其他设备不受影响。
    #[tokio::test]
    async fn trusted_device_reconnect_and_revoke_on_forget() {
        use futures_util::SinkExt as _;

        let config_dir = std::env::temp_dir().join(format!(
            "loopmaster-recv-e2e-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.json");
        let mut config =
            crate::config::AppConfig::new(loopmaster_audio_core::RouteGraph::default());
        config.network.require_pairing = true;
        config.save_to(&config_path).unwrap();

        let hub = std::sync::Arc::new(StateHub::new(config_path.clone()));
        populate_two_sends(&hub);
        let handle = start(
            WebServerConfig {
                port: 0,
                tls: false,
                ..Default::default()
            },
            hub.clone(),
        )
        .expect("启动成功");
        let url = format!("ws://127.0.0.1:{}/ws", handle.addr().port());
        let origin = format!("http://127.0.0.1:{}", handle.addr().port());

        // 配对两台设备。
        let pairing_a = hub.auth().start_pairing();
        let credential_a = hub
            .auth()
            .pair(Some(&pairing_a.secret), None, "PhoneA", "127.0.0.1")
            .unwrap();
        let pairing_b = hub.auth().start_pairing();
        let credential_b = hub
            .auth()
            .pair(Some(&pairing_b.secret), None, "PhoneB", "127.0.0.1")
            .unwrap();

        let connect = |cookie: &str| {
            let request = ws_request(&url, Some(&format!("lm_device={cookie}")), &origin);
            tokio_tungstenite::connect_async(request)
        };

        // A 首次连接 → 正常关闭 → 同凭证重连（无需重新配对）。
        let (mut ws_a, _) = connect(&credential_a).await.expect("A 首次连接成功");
        let _ = ws_read_text(&mut ws_a, |text| text.contains("initial_state")).await;
        ws_a.send(tokio_tungstenite::tungstenite::Message::Close(None))
            .await
            .unwrap();
        drop(ws_a);

        let (mut ws_a2, _) = connect(&credential_a)
            .await
            .expect("A 断线后同凭证重连成功");
        let _ = ws_read_text(&mut ws_a2, |text| text.contains("initial_state")).await;
        let (status, body, _) = http_request(
            handle.addr(),
            "GET",
            "/api/auth/session",
            &[("Cookie", &format!("lm_device={credential_a}"))],
            "",
        )
        .await;
        assert_eq!(status, 200, "重连后会话应仍有效: {body}");
        drop(ws_a2);

        // 模拟应用重启：同一 config 目录重建 StateHub/AuthState，凭据应从磁盘恢复。
        let hub2 = std::sync::Arc::new(StateHub::new(config_path.clone()));
        assert!(
            hub2.auth().verify_credential(Some(&credential_a)).is_some(),
            "重启后 A 的凭证应仍有效"
        );
        assert!(
            hub2.auth().verify_credential(Some(&credential_b)).is_some(),
            "重启后 B 的凭证应仍有效"
        );

        // 吊销：A、B 同时在线，忘记 A → A 收到 close 4001，B 不受影响。
        let (mut ws_a3, _) = connect(&credential_a).await.expect("A 重连成功");
        let _ = ws_read_text(&mut ws_a3, |text| text.contains("initial_state")).await;
        let (mut ws_b, _) = connect(&credential_b).await.expect("B 连接成功");
        let _ = ws_read_text(&mut ws_b, |text| text.contains("initial_state")).await;

        let device_a = hub
            .auth()
            .list_devices()
            .into_iter()
            .find(|device| device.name == "PhoneA")
            .expect("应找到 PhoneA");
        hub.auth().forget(&device_a.id).unwrap();

        let (code, reason) = ws_read_close(&mut ws_a3).await;
        assert_eq!(code, 4001, "A 的吊销关闭码应为 4001: {reason}");
        ws_b.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({
                "seq": 7,
                "action": "set_send_gain",
                "data": { "send_id": "send_mic", "gain_db": -3.0 }
            })
            .to_string(),
        ))
        .await
        .unwrap();
        let ack_b = ws_read_text(&mut ws_b, |text| {
            text.contains(r#""ack":"set_send_gain""#) && text.contains(r#""seq":7"#)
        })
        .await;
        assert!(ack_b.contains(r#""ack""#), "B 不应受忘记 A 影响: {ack_b}");

        // A 旧凭证再连 → 401 拒绝。
        assert!(
            connect(&credential_a).await.is_err(),
            "忘记后 A 旧凭证应被拒绝"
        );

        handle.shutdown();
        let _ = std::fs::remove_dir_all(&config_dir);
    }
}
