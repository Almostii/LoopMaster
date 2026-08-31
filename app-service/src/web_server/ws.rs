//! WebSocket 实时通道（Phase 2 子任务 2）。
//!
//! 协议见 `Plan/2026-08-31-Web控制台DTO与可信设备模型冻结.md` §1：
//! - 上行：JSON 控制指令（`set_send_gain` / `set_send_muted` / `set_send_enabled` /
//!   `add_send`），每条必须收到含相同 `seq` 的 `ack`/`error`；同一连接内重复
//!   `seq` 幂等（直接回旧响应，不重复应用）；
//! - 下行：连接即下发 `initial_state` 全量快照（含 `state_revision`）；之后按
//!   原型频率（默认 30Hz）广播二进制 meter 帧（帧类型 `0x01`，见方案 2 §4.2）；
//! - 客户端检测 revision 跳变后必须重新拉取全量快照（broadcast 允许慢消费者丢消息）。

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{
    ws::{Message, WebSocket, WebSocketUpgrade},
    ConnectInfo, State,
};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{broadcast, mpsc};

use loopmaster_audio_core::{
    BusId, SendId, SendSpec, SinkKind, SourceId, SourceKind, INTERNAL_SAMPLE_RATE,
};

use crate::route::RouteEdit;
use crate::state::StateHub;
use crate::web_server::auth::{
    extract_cookie, AuthError, AuthState, DeviceSummary, COOKIE_MAX_AGE, COOKIE_NAME,
};

/// 控制消息 `seq` 幂等缓存上限（超出后淘汰最旧，防止无限增长）。
const MAX_SEQ_CACHE: usize = 512;

/// WebSocket 处理器所需共享状态。
#[derive(Clone)]
pub struct WsState {
    pub hub: Arc<StateHub>,
    pub meter_tx: broadcast::Sender<Vec<u8>>,
    /// 配对与可信设备。
    pub auth: Arc<AuthState>,
    /// 当前服务是否为 HTTPS/WSS（决定 `Secure` Cookie 与凭证策略）。
    pub is_https: bool,
    /// 是否要求配对/可信设备（`StateHub.require_pairing`，运行时可切换）。
    ///
    /// `false`（默认）：局域网内设备直接访问，`/ws` 不做凭证校验（仍校验 Origin）；
    /// `true`：启用 M4 配对流程，`/ws` 只接受已配对设备的凭证 Cookie。
    pub require_pairing: Arc<std::sync::atomic::AtomicBool>,
}

/// 实时 + 认证路由（带 `WsState`）。
pub fn ws_router() -> Router<WsState> {
    Router::new()
        .route("/ws", get(ws_handler))
        .route("/api/auth/pair", post(pair_handler))
        .route("/api/auth/session", get(session_handler))
        .route("/api/auth/forget", post(forget_handler))
}

/// WebSocket Upgrade 入口：
/// - 始终校验 Origin（与请求 Host 不匹配返回 403，防跨站 WebSocket 劫持）；
/// - `require_pairing=false`（默认）：不校验凭证，局域网设备直接连接；
/// - `require_pairing=true`：校验可信设备 Cookie（无凭证 401）。
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<WsState>,
    headers: HeaderMap,
) -> Response {
    if !origin_matches_request(&headers) {
        return (StatusCode::FORBIDDEN, "403 Forbidden").into_response();
    }
    let require_pairing = state
        .require_pairing
        .load(std::sync::atomic::Ordering::Relaxed);
    let device = if require_pairing {
        let cookie = extract_cookie(&headers, COOKIE_NAME);
        match state.auth.verify_credential(cookie.as_deref()) {
            Some(device) => Some(device),
            None => return (StatusCode::UNAUTHORIZED, "401 Unauthorized").into_response(),
        }
    } else {
        None
    };
    ws.on_upgrade(move |socket| handle_socket(socket, state, device))
}

/// 单个连接的主循环：先下发快照，再并行走「控制读 + 广播写」。
async fn handle_socket(socket: WebSocket, state: WsState, device: Option<DeviceSummary>) {
    let (mut sink, mut stream) = socket.split();

    // 吊销通知：仅配对模式下有设备；设备被忘记/重置时立即关闭本连接。
    let mut revoke_rx = device.as_ref().map(|device| {
        let (revoke_tx, revoke_rx) = mpsc::channel::<()>(1);
        state.auth.register_connection(&device.id, revoke_tx);
        revoke_rx
    });

    // 连接即下发全量快照。
    let initial = json!({
        "event": "initial_state",
        "data": build_initial_state(&state.hub),
    });
    if sink
        .send(Message::Text(initial.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    // 控制响应（ack/error）经该通道交给写端，避免读写抢占同一 sink。
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(64);
    let mut seq_cache = SeqCache::default();
    let mut meter_rx = state.meter_tx.subscribe();
    // 权威状态 revision 订阅：跳变时重推 initial_state（实现"revision 跳变 →
    // 重新拉全量快照"，保证多客户端一致）。
    let mut revision_rx = state.hub.subscribe();

    let reader = async {
        while let Some(message) = stream.next().await {
            let message = match message {
                Ok(message) => message,
                Err(_) => break,
            };
            match message {
                Message::Text(text) => {
                    if let Some(response) =
                        handle_control(&state.hub, text.as_str(), &mut seq_cache)
                    {
                        if out_tx.send(Message::Text(response.into())).await.is_err() {
                            break;
                        }
                    }
                }
                // Ping 由协议层自动回 Pong；二进制上行在 M2 不做处理。
                Message::Close(_) | Message::Ping(_) | Message::Pong(_) | Message::Binary(_) => {}
            }
        }
    };

    let writer = async {
        loop {
            tokio::select! {
                out = out_rx.recv() => match out {
                    Some(message) => {
                        if sink.send(message).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                frame = meter_rx.recv() => match frame {
                    Ok(data) => {
                        if sink.send(Message::Binary(data.into())).await.is_err() {
                            break;
                        }
                    }
                    // Lagged：广播被丢弃属预期，客户端按 revision 重拉快照。
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                changed = revision_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    // 权威状态变化：重推全量快照；标记 watch 已消费避免空转。
                    let snapshot = json!({
                        "event": "initial_state",
                        "data": build_initial_state(&state.hub),
                    });
                    if sink
                        .send(Message::Text(snapshot.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    let _ = revision_rx.borrow_and_update();
                },
                _revoked = async {
                    match revoke_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    // 设备被忘记/重置（配对模式）：立即关闭连接。
                    let _ = sink
                        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                            code: 4001,
                            reason: "revoked".into(),
                        })))
                        .await;
                    break;
                },
            }
        }
    };

    tokio::select! {
        _ = reader => {}
        _ = writer => {}
    }
}

// ---------------------------------------------------------------------------
// 配对与可信设备（子任务 4）
// ---------------------------------------------------------------------------

/// 配对请求体：`secret` 或 `pin` 二选一（冻结文档 §2）。
#[derive(Deserialize)]
struct PairRequest {
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    pin: Option<String>,
    client_name: String,
}

/// `POST /api/auth/pair`：配对成功后签发持久化凭证（Set-Cookie）。
async fn pair_handler(
    State(state): State<WsState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<PairRequest>,
) -> Response {
    let client_ip = addr.ip().to_string();
    match state.auth.pair(
        body.secret.as_deref(),
        body.pin.as_deref(),
        &body.client_name,
        &client_ip,
    ) {
        Ok(credential) => {
            let mut response = (
                StatusCode::OK,
                Json(json!({ "ok": true, "device_name": body.client_name })),
            )
                .into_response();
            response
                .headers_mut()
                .insert(header::SET_COOKIE, set_cookie(&credential, state.is_https));
            response
        }
        Err(error) => auth_error_response(&error),
    }
}

/// `GET /api/auth/session`：查询当前连接所属可信设备与权限。
async fn session_handler(State(state): State<WsState>, headers: HeaderMap) -> Response {
    let cookie = extract_cookie(&headers, COOKIE_NAME);
    match state.auth.verify_credential(cookie.as_deref()) {
        Some(device) => Json(json!({ "ok": true, "device": device })).into_response(),
        None => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized" })),
        )
            .into_response(),
    }
}

/// `POST /api/auth/forget`：忘记当前设备（删除凭证并关闭其连接）。
async fn forget_handler(State(state): State<WsState>, headers: HeaderMap) -> Response {
    let cookie = extract_cookie(&headers, COOKIE_NAME);
    let Some(device) = state.auth.verify_credential(cookie.as_deref()) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized" })),
        )
            .into_response();
    };
    match state.auth.forget(&device.id) {
        Ok(()) => {
            let mut response = (StatusCode::OK, Json(json!({ "ok": true }))).into_response();
            // 同时清掉本机 Cookie。
            response.headers_mut().insert(
                header::SET_COOKIE,
                HeaderValue::from_str(&format!(
                    "{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"
                ))
                .expect("Set-Cookie 头应合法"),
            );
            response
        }
        Err(error) => auth_error_response(&error),
    }
}

fn auth_error_response(error: &AuthError) -> Response {
    let status = match error {
        AuthError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        AuthError::PairingClosed => StatusCode::FORBIDDEN,
        AuthError::InvalidCredential => StatusCode::UNAUTHORIZED,
        AuthError::Unauthorized => StatusCode::UNAUTHORIZED,
        AuthError::DeviceNotFound => StatusCode::NOT_FOUND,
        AuthError::Persist(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(json!({ "error": "auth_failed", "message": error.to_string() })),
    )
        .into_response()
}

/// 构造持久化凭证 Cookie（HTTPS 模式附加 `Secure`；HTTP 模式浏览器会拒绝
/// `Secure` 属性，按 ADR-007 仅提供轻量配对）。
fn set_cookie(credential: &str, is_https: bool) -> HeaderValue {
    let secure = if is_https { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={credential}; Path=/; HttpOnly; SameSite=Strict; Max-Age={COOKIE_MAX_AGE}{secure}"
    ))
    .expect("Set-Cookie 头应合法")
}

/// WebSocket Origin 校验：Origin 的主机/端口必须与请求 Host 一致。
///
/// 无 Origin 头（非浏览器客户端）放行；不匹配返回 false（上层 403）。
fn origin_matches_request(headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    let Ok(origin_uri) = origin.parse::<axum::http::Uri>() else {
        return false;
    };
    let Some(origin_host) = origin_uri.host() else {
        return false;
    };
    let origin_port = origin_uri.port_u16();
    let request_host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let (host, port) = match request_host.split_once(':') {
        Some((host, port)) => (host, port.parse::<u16>().ok()),
        None => (request_host, None),
    };
    host == origin_host && port == origin_port
}

// ---------------------------------------------------------------------------
// 上行控制
// ---------------------------------------------------------------------------

/// 控制消息（协议 §1.3）。
#[derive(Deserialize)]
struct ControlMessage {
    seq: u64,
    action: String,
    #[serde(default)]
    data: serde_json::Value,
}

/// 同连接内按 `seq` 幂等：记录已响应内容，重复 seq 直接回旧响应。
#[derive(Default)]
struct SeqCache {
    responses: HashMap<u64, String>,
    order: VecDeque<u64>,
}

impl SeqCache {
    fn get(&self, seq: &u64) -> Option<&String> {
        self.responses.get(seq)
    }

    fn insert(&mut self, seq: u64, response: String) {
        if self.responses.contains_key(&seq) {
            return;
        }
        self.responses.insert(seq, response);
        self.order.push_back(seq);
        while self.order.len() > MAX_SEQ_CACHE {
            if let Some(oldest) = self.order.pop_front() {
                self.responses.remove(&oldest);
            }
        }
    }
}

/// 处理一条上行控制消息，返回要回复的 JSON 文本；非 JSON 消息返回错误响应。
fn handle_control(hub: &StateHub, text: &str, cache: &mut SeqCache) -> Option<String> {
    let message: ControlMessage = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(error) => {
            return Some(
                json!({
                    "error": "invalid_json",
                    "message": format!("消息解析失败: {error}")
                })
                .to_string(),
            );
        }
    };
    if let Some(cached) = cache.get(&message.seq) {
        return Some(cached.clone());
    }

    let result = control_to_edit(&message)
        .and_then(|edit| hub.apply_route_edit(edit).map_err(|e| e.to_string()));
    let response = match result {
        Ok(()) => json!({ "seq": message.seq, "ack": message.action }),
        Err(error) => json!({ "seq": message.seq, "error": "rejected", "message": error }),
    };
    let response = response.to_string();
    cache.insert(message.seq, response.clone());
    Some(response)
}

/// 协议 action → `RouteEdit` 映射（冻结文档 §1.3；非法命令返回 Err，不改变状态）。
fn control_to_edit(message: &ControlMessage) -> Result<RouteEdit, String> {
    let data = &message.data;
    match message.action.as_str() {
        "set_send_gain" => Ok(RouteEdit::SetSendGain {
            send_id: SendId(get_str(data, "send_id")?),
            gain_db: get_f32(data, "gain_db")?,
        }),
        "set_send_muted" => Ok(RouteEdit::SetSendMuted {
            send_id: SendId(get_str(data, "send_id")?),
            muted: get_bool(data, "muted")?,
        }),
        "set_send_enabled" => Ok(RouteEdit::SetSendEnabled {
            send_id: SendId(get_str(data, "send_id")?),
            enabled: get_bool(data, "enabled")?,
        }),
        "add_send" => {
            // 协议适配层补齐初始参数后映射为 SourceToBus（冻结文档 §1.3）。
            Ok(RouteEdit::SetSend(SendSpec::SourceToBus {
                id: SendId(get_str(data, "send_id")?),
                source_id: SourceId(get_str(data, "source_id")?),
                bus_id: BusId(get_str(data, "output_channel_id")?),
                gain_db: 0.0,
                muted: false,
                enabled: true,
                channel_map: Vec::new(),
            }))
        }
        other => Err(format!("未知 action: {other}")),
    }
}

fn get_str(value: &serde_json::Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("缺少字段 {key}"))
}

fn get_f32(value: &serde_json::Value, key: &str) -> Result<f32, String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_f64)
        .map(|v| v as f32)
        .ok_or_else(|| format!("缺少字段 {key}"))
}

fn get_bool(value: &serde_json::Value, key: &str) -> Result<bool, String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| format!("缺少字段 {key}"))
}

// ---------------------------------------------------------------------------
// 下行：initial_state 全量快照
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct InitialState {
    state_revision: u64,
    engine_status: String,
    sample_rate: u32,
    sources: Vec<SourceDto>,
    output_channels: Vec<OutputChannelDto>,
    external_outputs: Vec<ExternalOutputDto>,
    sends: Vec<SendDto>,
}

#[derive(Serialize)]
struct SourceDto {
    id: String,
    kind: String,
    display_name: String,
    endpoint_id: Option<String>,
    process_id: Option<u32>,
}

#[derive(Serialize)]
struct OutputChannelDto {
    id: String,
    display_name: String,
}

#[derive(Serialize)]
struct ExternalOutputDto {
    id: String,
    endpoint_id: String,
    display_name: String,
    kind: String,
}

#[derive(Serialize)]
struct SendDto {
    send_id: String,
    source: Option<String>,
    output_channel: Option<String>,
    external_output: Option<String>,
    gain_db: f32,
    muted: bool,
    enabled: bool,
    channel_map: Vec<[u16; 2]>,
}

fn source_kind_str(kind: &SourceKind) -> &'static str {
    match kind {
        SourceKind::DeviceCapture => "device_capture",
        SourceKind::DeviceLoopback => "device_loopback",
        SourceKind::ProcessLoopback => "process_loopback",
        SourceKind::Vban => "vban",
    }
}

fn sink_kind_str(kind: &SinkKind) -> &'static str {
    match kind {
        SinkKind::Device => "device",
        SinkKind::Vban => "vban",
    }
}

/// 构建全量快照（冻结文档 §1.2）。
fn build_initial_state(hub: &StateHub) -> InitialState {
    let graph = hub.route_snapshot();
    let engine = hub.engine();
    let status = engine.as_ref().map(|engine| engine.status());
    drop(engine);

    InitialState {
        state_revision: hub.revision(),
        engine_status: status
            .as_ref()
            .map(|status| status.state.as_str().to_string())
            .unwrap_or_else(|| "stopped".to_owned()),
        sample_rate: INTERNAL_SAMPLE_RATE,
        sources: graph
            .sources
            .iter()
            .map(|source| SourceDto {
                id: source.id.0.clone(),
                kind: source_kind_str(&source.kind).to_owned(),
                display_name: source.display_name.clone(),
                endpoint_id: source.endpoint_id.as_ref().map(|id| id.0.clone()),
                process_id: source.process_id,
            })
            .collect(),
        output_channels: graph
            .buses
            .iter()
            .map(|bus| OutputChannelDto {
                id: bus.id.0.clone(),
                display_name: bus.display_name.clone(),
            })
            .collect(),
        external_outputs: graph
            .sinks
            .iter()
            .map(|sink| ExternalOutputDto {
                id: sink.id.0.clone(),
                endpoint_id: sink.endpoint_id.0.clone(),
                display_name: sink.display_name.clone(),
                kind: sink_kind_str(&sink.kind).to_owned(),
            })
            .collect(),
        sends: graph
            .sends
            .iter()
            .map(|send| {
                let (source, output_channel, external_output) = match send {
                    SendSpec::SourceToBus {
                        source_id, bus_id, ..
                    } => (Some(source_id.0.clone()), Some(bus_id.0.clone()), None),
                    SendSpec::BusToSink {
                        bus_id, sink_id, ..
                    } => (None, Some(bus_id.0.clone()), Some(sink_id.0.clone())),
                };
                SendDto {
                    send_id: send.id().0.clone(),
                    source,
                    output_channel,
                    external_output,
                    gain_db: send.gain_db(),
                    muted: send.muted(),
                    enabled: send.enabled(),
                    channel_map: send
                        .channel_map()
                        .iter()
                        .map(|(input, output)| [*input, *output])
                        .collect(),
                }
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// 下行：二进制 meter 帧（方案 2 §4.2）
// ---------------------------------------------------------------------------

/// 幅度 → dBFS（下限 -120 dBFS）。
fn amp_to_dbfs(amp: f32) -> f32 {
    if amp <= 1e-9 {
        -120.0
    } else {
        (20.0 * amp.log10()).max(-120.0)
    }
}

fn timestamp_ms() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32
}

/// 编码二进制 meter 帧：
/// `0x01 | N(u8) | ts(u32 LE) | (len(u8) + id + peak(f32 LE) + rms(f32 LE)) × N`
fn encode_meter_frame(nodes: &[(String, f32, f32)], timestamp_ms: u32) -> Vec<u8> {
    let capacity = 6 + nodes
        .iter()
        .map(|(id, _, _)| 1 + id.len() + 8)
        .sum::<usize>();
    let mut buf = Vec::with_capacity(capacity);
    buf.push(0x01);
    buf.push(nodes.len().min(255) as u8);
    buf.extend_from_slice(&timestamp_ms.to_le_bytes());
    for (id, peak_db, rms_db) in nodes {
        buf.push(id.len().min(255) as u8);
        buf.extend_from_slice(id.as_bytes());
        buf.extend_from_slice(&peak_db.to_le_bytes());
        buf.extend_from_slice(&rms_db.to_le_bytes());
    }
    buf
}

/// 从引擎统计构建一帧 meter 数据（按 send id 排序，保证帧内顺序稳定）。
fn build_meter_frame(hub: &StateHub) -> Vec<u8> {
    let engine = hub.engine();
    let stats = engine.as_ref().map(|engine| engine.status().stats);
    drop(engine);
    let stats = match stats {
        Some(stats) => stats,
        None => return encode_meter_frame(&[], timestamp_ms()),
    };
    let mut nodes: Vec<(String, f32, f32)> = stats
        .send_peaks
        .iter()
        .map(|(id, peaks)| {
            let rms = stats.send_rms.get(id).copied().unwrap_or([0.0f32; 2]);
            let peak_db = amp_to_dbfs(peaks[0].max(peaks[1]));
            let rms_db = amp_to_dbfs(rms[0].max(rms[1]));
            (id.clone(), peak_db, rms_db)
        })
        .collect();
    nodes.sort_by(|a, b| a.0.cmp(&b.0));
    encode_meter_frame(&nodes, timestamp_ms())
}

/// 启动 meter 广播任务（`meter_hz` 为原型对比频率，默认 30）。
pub fn spawn_meter_task(hub: Arc<StateHub>, meter_hz: u16) -> broadcast::Sender<Vec<u8>> {
    let (tx, _) = broadcast::channel(64);
    let task_tx = tx.clone();
    let hz = meter_hz.max(1);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs_f64(1.0 / f64::from(hz)));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let frame = build_meter_frame(&hub);
            // 无订阅者时 send 失败是预期的（广播通道保持打开，新连接仍能订阅）。
            let _ = task_tx.send(frame);
        }
    });
    tx
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_hub() -> Arc<StateHub> {
        Arc::new(StateHub::new(std::env::temp_dir().join(format!(
            "loopmaster-ws-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ))))
    }

    /// 准备一张 source→bus→sink 的图。
    fn populate_graph(hub: &StateHub) {
        use loopmaster_audio_core::{BusSpec, EndpointId, SinkSpec, SourceSpec};
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
            id: BusId("bus_master_1".into()),
            display_name: "主通道".into(),
        }))
        .unwrap();
        hub.apply_route_edit(RouteEdit::AddSink(SinkSpec {
            id: loopmaster_audio_core::SinkId("out_spk_1".into()),
            endpoint_id: EndpointId("endpoint-out".into()),
            display_name: "扬声器".into(),
            kind: SinkKind::Device,
            stream_name: None,
            remote_addr: None,
        }))
        .unwrap();
        hub.apply_route_edit(RouteEdit::SetSend(SendSpec::SourceToBus {
            id: SendId("send_mic_to_master".into()),
            source_id: SourceId("src_mic_1".into()),
            bus_id: BusId("bus_master_1".into()),
            gain_db: -6.0,
            muted: false,
            enabled: true,
            channel_map: Vec::new(),
        }))
        .unwrap();
    }

    #[test]
    fn control_mapping_matches_frozen_protocol() {
        let gain = serde_json::from_str::<ControlMessage>(
            r#"{"seq":101,"action":"set_send_gain","data":{"send_id":"send_mic_to_master","gain_db":-6.0}}"#,
        )
        .unwrap();
        assert!(matches!(
            control_to_edit(&gain).unwrap(),
            RouteEdit::SetSendGain { .. }
        ));

        let add = serde_json::from_str::<ControlMessage>(
            r#"{"seq":103,"action":"add_send","data":{"send_id":"s1","source_id":"src_mic_1","output_channel_id":"bus_master_1"}}"#,
        )
        .unwrap();
        match control_to_edit(&add).unwrap() {
            RouteEdit::SetSend(SendSpec::SourceToBus {
                gain_db,
                muted,
                enabled,
                ref channel_map,
                ..
            }) => {
                assert_eq!(gain_db, 0.0);
                assert!(!muted);
                assert!(enabled);
                assert!(channel_map.is_empty());
            }
            other => panic!("add_send 应映射为 SetSend(SourceToBus)，得到 {other:?}"),
        }

        let unknown =
            serde_json::from_str::<ControlMessage>(r#"{"seq":1,"action":"bogus","data":{}}"#)
                .unwrap();
        assert!(control_to_edit(&unknown).is_err());
    }

    #[test]
    fn apply_control_ack_and_idempotent_seq() {
        let hub = temp_hub();
        populate_graph(&hub);
        let mut cache = SeqCache::default();
        let ack = handle_control(
            &hub,
            r#"{"seq":101,"action":"set_send_gain","data":{"send_id":"send_mic_to_master","gain_db":-3.0}}"#,
            &mut cache,
        )
        .expect("应返回响应");
        assert!(ack.contains(r#""ack":"set_send_gain""#), "{ack}");
        assert!(ack.contains(r#""seq":101"#), "{ack}");
        assert_eq!(
            hub.route_snapshot().sends[0].gain_db(),
            -3.0,
            "增益应写入权威状态"
        );

        // 重复 seq：幂等，返回同一响应且不重复应用。
        let again = handle_control(
            &hub,
            r#"{"seq":101,"action":"set_send_gain","data":{"send_id":"send_mic_to_master","gain_db":0.0}}"#,
            &mut cache,
        )
        .expect("应返回响应");
        assert_eq!(again, ack, "重复 seq 应返回缓存响应");
        assert_eq!(
            hub.route_snapshot().sends[0].gain_db(),
            -3.0,
            "重复 seq 不得改变状态"
        );
    }

    #[test]
    fn invalid_control_returns_error_without_mutation() {
        let hub = temp_hub();
        populate_graph(&hub);
        let mut cache = SeqCache::default();
        let response = handle_control(
            &hub,
            r#"{"seq":7,"action":"set_send_gain","data":{"send_id":"missing","gain_db":-3.0}}"#,
            &mut cache,
        )
        .expect("应返回响应");
        assert!(response.contains(r#""error":"rejected""#), "{response}");
        assert_eq!(hub.route_snapshot().sends[0].gain_db(), -6.0, "状态不变");
    }

    #[test]
    fn initial_state_projects_authoritative_snapshot() {
        let hub = temp_hub();
        populate_graph(&hub);
        let state = build_initial_state(&hub);
        assert_eq!(state.sources.len(), 1);
        assert_eq!(state.sources[0].kind, "device_capture");
        assert_eq!(state.output_channels.len(), 1);
        assert_eq!(state.external_outputs.len(), 1);
        let send = &state.sends[0];
        assert_eq!(send.send_id, "send_mic_to_master");
        assert_eq!(send.source.as_deref(), Some("src_mic_1"));
        assert_eq!(send.output_channel.as_deref(), Some("bus_master_1"));
        assert!(send.external_output.is_none());
        assert_eq!(send.gain_db, -6.0);
        assert_eq!(state.sample_rate, INTERNAL_SAMPLE_RATE);
        assert!(state.state_revision >= 4, "至少包含 4 次编辑");
    }

    #[test]
    fn meter_frame_encoding_matches_protocol() {
        let nodes = vec![
            ("send_mic_to_master".to_owned(), -6.0f32, -18.0f32),
            ("send_master_to_spk".to_owned(), -3.0f32, -12.0f32),
        ];
        let frame = encode_meter_frame(&nodes, 1234);
        assert_eq!(frame[0], 0x01);
        assert_eq!(frame[1], 2);
        assert_eq!(u32::from_le_bytes(frame[2..6].try_into().unwrap()), 1234);

        // 第一条节点
        let mut cursor = 6;
        let len0 = frame[cursor] as usize;
        cursor += 1;
        assert_eq!(&frame[cursor..cursor + len0], b"send_mic_to_master");
        cursor += len0;
        let peak0 = f32::from_le_bytes(frame[cursor..cursor + 4].try_into().unwrap());
        assert!((peak0 + 6.0).abs() < 1e-5);
        let rms0 = f32::from_le_bytes(frame[cursor + 4..cursor + 8].try_into().unwrap());
        assert!((rms0 + 18.0).abs() < 1e-5);
    }

    #[test]
    fn amp_to_dbfs_is_floor_and_log() {
        assert!((amp_to_dbfs(1.0) - 0.0).abs() < 1e-5);
        assert!((amp_to_dbfs(0.5) + 6.0206).abs() < 1e-3);
        assert_eq!(amp_to_dbfs(0.0), -120.0);
        assert_eq!(amp_to_dbfs(1e-12), -120.0);
    }

    #[test]
    fn seq_cache_evicts_oldest() {
        let mut cache = SeqCache::default();
        for seq in 0..MAX_SEQ_CACHE + 10 {
            cache.insert(seq as u64, format!("r{seq}"));
        }
        assert!(!cache.responses.contains_key(&0), "最旧 seq 应被淘汰");
        assert_eq!(cache.responses.len(), MAX_SEQ_CACHE);
        assert!(cache.responses.contains_key(&(MAX_SEQ_CACHE as u64 + 9)));
    }
}
