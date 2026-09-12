//! 独立 HTTP API：Web UI 与外部站点调用本服务的入口（与 OneBot WS 的 6199 隔离）。
//!
//! - 读接口（无 token）：`GET /api/health`、`GET /api/status`、`GET /api/messages?limit=N`
//! - 写接口（必须 `api.access_token`）：`POST /api/relay`——经 [`Hub`](crate::router::Hub)
//!   按目标端（qq / game / chatroom）下发消息
//! - 全部路由带 CORS 头，浏览器端 Web UI 可直接调用；默认只绑回环
//!
//! 安全边界见 `docs/api-design.md`：写接口 token 为空时一律 403；
//! Web UI 前端建议走自己的后端代理，避免把 token 下发到浏览器。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::header::{AUTHORIZATION, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::config::ApiConfig;
use crate::service::BridgeService;

const RECENT_DEFAULT_LIMIT: usize = 50;

#[derive(Clone)]
struct ApiState {
    service: Weak<BridgeService>,
    token: Arc<String>,
    started_at: Instant,
}

pub struct ApiServer {
    host: String,
    port: u16,
    state: ApiState,
    inner: Mutex<ApiInner>,
    shutdown: watch::Sender<bool>,
}

struct ApiInner {
    local_addr: Option<SocketAddr>,
    serve: Option<tokio::task::JoinHandle<()>>,
}

impl ApiServer {
    pub fn new(service: Weak<BridgeService>, cfg: &ApiConfig) -> Self {
        let (shutdown, _) = watch::channel(false);
        Self {
            host: cfg.listen_host.clone(),
            port: cfg.listen_port,
            state: ApiState {
                service,
                token: Arc::new(cfg.access_token.clone()),
                started_at: Instant::now(),
            },
            inner: Mutex::new(ApiInner {
                local_addr: None,
                serve: None,
            }),
            shutdown,
        }
    }

    /// 绑定监听并启动 serve 任务。
    pub async fn start(&self) -> std::io::Result<()> {
        let app = router(self.state.clone()).layer(middleware::from_fn(cors));
        let listener = TcpListener::bind((self.host.as_str(), self.port)).await?;
        let local_addr = listener.local_addr()?;
        let mut shutdown_rx = self.shutdown.subscribe();
        let serve = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.changed().await;
                })
                .await;
        });
        {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inner.local_addr = Some(local_addr);
            inner.serve = Some(serve);
        }
        Ok(())
    }

    pub async fn stop(&self) {
        let serve = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inner.serve.take()
        };
        let _ = self.shutdown.send(true);
        if let Some(serve) = serve {
            let _ = tokio::time::timeout(Duration::from_secs(3), serve).await;
        }
    }

    /// port=0 时 start() 后可拿真实端口（测试需要）。
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .local_addr
    }
}

fn router(state: ApiState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/status", get(status))
        .route("/api/messages", get(messages))
        .route("/api/relay", post(relay))
        .with_state(state)
}

/// 简单 CORS 层：读接口对浏览器直接开放；写接口由 token 保护。
/// OPTIONS 预检在本层短路，不进路由。
async fn cors(req: Request<axum::body::Body>, next: Next) -> Response {
    let is_preflight = req.method() == Method::OPTIONS;
    let mut resp = if is_preflight {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    let headers = resp.headers_mut();
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    headers.insert(
        ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type, Authorization, X-API-Token"),
    );
    resp
}

fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "服务已停止或尚未就绪"})),
    )
        .into_response()
}

async fn health(State(state): State<ApiState>) -> Response {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": state.started_at.elapsed().as_secs(),
    }))
    .into_response()
}

async fn status(State(state): State<ApiState>) -> Response {
    let Some(service) = state.service.upgrade() else {
        return unavailable();
    };
    let onebot_stats = service.server().stats();
    let self_id = service.server().connection().map(|c| c.self_id()).unwrap_or(0);
    let snapshot = service.state().snapshot();
    let forwarder = service.forwarder_stats();
    let tracker = service.tracker_stats();
    let capabilities: Vec<Value> = service
        .capabilities()
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "aliases": c.aliases,
                "trigger": c.trigger,
                "description": c.description,
            })
        })
        .collect();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": state.started_at.elapsed().as_secs(),
        "onebot": {
            "connected": service.server().connection().is_some(),
            "self_id": self_id,
            "stats": {
                "connections": onebot_stats.connections,
                "group_messages": onebot_stats.group_messages,
            },
        },
        "chatbridge": {
            "enabled": service.chatbridge_enabled(),
            "connected": service.chatbridge_connected(),
        },
        "forwarder": {
            "forwarded": forwarder.forwarded,
            "skipped_duplicate": forwarder.skipped_duplicate,
            "failed": forwarder.failed,
        },
        "tracker": {
            "online": tracker.online,
            "offline": tracker.offline,
            "failed": tracker.failed,
        },
        "commands": service.command_stats(),
        "state": {
            "forwarded_count": snapshot.forwarded_count,
            "last_read_message_id": snapshot.last_read_message_id,
            "has_refresh_token": snapshot.has_refresh_token,
        },
        "recent_messages": service.recent_log().len(),
        "capabilities": capabilities,
    }))
    .into_response()
}

async fn messages(
    State(state): State<ApiState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(service) = state.service.upgrade() else {
        return unavailable();
    };
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(RECENT_DEFAULT_LIMIT);
    let records = service.recent_log().latest(limit);
    Json(json!({"messages": records})).into_response()
}

#[derive(Deserialize)]
struct RelayRequest {
    /// "qq"（group_id 缺省=所有配置群）| "game" | "chatroom"
    target: String,
    text: String,
    group_id: Option<i64>,
}

async fn relay(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<RelayRequest>,
) -> Response {
    if !authorized(&state, &headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "error": "写接口需要 api.access_token（Authorization: Bearer <token> 或 X-API-Token）"
            })),
        )
            .into_response();
    }
    let Some(service) = state.service.upgrade() else {
        return unavailable();
    };
    if body.text.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "text 不能为空"})),
        )
            .into_response();
    }
    use crate::router::Hub as _;
    let result = match body.target.as_str() {
        "qq" => service.qq_send_text(body.group_id, &body.text).await,
        "game" => service.game_broadcast(&body.text).await,
        "chatroom" => {
            service
                .chatroom_post("api", &body.text, "", "api")
                .await
        }
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "error": format!("未知 target: {other}（qq / game / chatroom）")})),
            )
                .into_response()
        }
    };
    Json(json!({"ok": result})).into_response()
}

/// 写接口鉴权：token 未配置一律拒绝；支持 `X-API-Token` 与 `Authorization: Bearer`。
fn authorized(state: &ApiState, headers: &HeaderMap) -> bool {
    if state.token.is_empty() {
        return false;
    }
    if let Some(value) = headers.get("X-API-Token").and_then(|v| v.to_str().ok()) {
        return value == state.token.as_str();
    }
    if let Some(value) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        return value.strip_prefix("Bearer ") == Some(state.token.as_str());
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_auth_accepts_header_and_bearer() {
        let state = ApiState {
            service: Weak::new(),
            token: Arc::new("tok123".into()),
            started_at: Instant::now(),
        };
        let mut headers = HeaderMap::new();
        assert!(!authorized(&state, &headers));
        headers.insert("X-API-Token", HeaderValue::from_static("tok123"));
        assert!(authorized(&state, &headers));
        let mut bearer = HeaderMap::new();
        bearer.insert(AUTHORIZATION, HeaderValue::from_static("Bearer tok123"));
        assert!(authorized(&state, &bearer));
        let mut wrong = HeaderMap::new();
        wrong.insert(AUTHORIZATION, HeaderValue::from_static("Bearer nope"));
        assert!(!authorized(&state, &wrong));
    }

    #[test]
    fn empty_token_rejects_everything() {
        let state = ApiState {
            service: Weak::new(),
            token: Arc::new(String::new()),
            started_at: Instant::now(),
        };
        let mut headers = HeaderMap::new();
        headers.insert("X-API-Token", HeaderValue::from_static("anything"));
        assert!(!authorized(&state, &headers));
    }
}
