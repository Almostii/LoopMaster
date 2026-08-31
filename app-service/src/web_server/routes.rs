//! Web 控制台路由：HTTPS 静态资源 + REST API（子任务 1 范围）。
//!
//! - `GET /api/health`：最小暴露，仅返回 `{"ok":true}`，不返回 node_id、
//!   网卡地址、配对状态或版本细节（排期 §2.2 配对凭证隔离验收项）；
//! - `/api/auth/*`：占位（501），配对与可信设备在子任务 4 实现；
//! - 静态资源：rust-embed 编译期内联 `frontend-remote/dist`，SPA fallback；
//!   `/ws` 双向通道属子任务 2，此处不注册。

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rust_embed::RustEmbed;

/// 编译期内联的远程控制台静态资源。
///
/// `dist` 不进 Git；干净检出的构建顺序是先 `frontend-remote` 前端构建再
/// Cargo（`scripts/build-remote.mjs`）。`app-service/build.rs` 会在 dist 缺失
/// 时生成占位页，保证 `cargo build/test` 不隐式依赖开发机残留产物。
#[derive(RustEmbed)]
#[folder = "../frontend-remote/dist"]
struct RemoteUiAssets;

/// 静态文件路由（含 SPA fallback 到 index.html）。
async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match RemoteUiAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref())], content.data).into_response()
        }
        None => match RemoteUiAssets::get("index.html") {
            Some(index) => ([(header::CONTENT_TYPE, "text/html")], index.data).into_response(),
            None => (StatusCode::NOT_FOUND, "404 Not Found").into_response(),
        },
    }
}

/// 最小健康检查：不返回任何秘密。
async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

/// `/api/auth/*` 占位：配对与可信设备持久化在子任务 4 实现。
///
/// 返回 501 而非 404，便于区分"路由已规划未实现"与"路径不存在"，
/// 同时不暴露任何鉴权信息。
async fn auth_placeholder() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "auth_not_available",
            "message": "配对与可信设备功能尚未实现（子任务 4）"
        })),
    )
        .into_response()
}

/// 组装应用路由。
pub fn router() -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/auth/pair", post(auth_placeholder))
        .route("/api/auth/session", get(auth_placeholder))
        .route("/api/auth/forget", post(auth_placeholder))
        .fallback(static_handler)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode as Status};
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_returns_ok_only() {
        let response = router()
            .oneshot(Request::get("/api/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), Status::OK);
        let body = axum::body::to_bytes(response.into_body(), 64)
            .await
            .unwrap();
        assert_eq!(&body[..], br#"{"ok":true}"#);
    }

    #[tokio::test]
    async fn auth_routes_are_explicit_placeholders() {
        for (method, path) in [
            ("POST", "/api/auth/pair"),
            ("GET", "/api/auth/session"),
            ("POST", "/api/auth/forget"),
        ] {
            let response = router()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                Status::NOT_IMPLEMENTED,
                "{method} {path}"
            );
        }
    }

    #[tokio::test]
    async fn unknown_path_falls_back_to_index_or_404() {
        let response = router()
            .oneshot(Request::get("/pair").body(Body::empty()).unwrap())
            .await
            .unwrap();
        // 占位构建下有 index.html → 200；正式构建下同样应回 index.html。
        assert_eq!(response.status(), Status::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap()
            .to_owned();
        assert!(content_type.starts_with("text/html"));
    }
}
