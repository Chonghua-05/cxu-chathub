//! HTTP API 集成测试：health / status / messages 读接口、relay 写接口鉴权、CORS 预检。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use chatroom_bridge::config::AppConfig;
use chatroom_bridge::service::BridgeService;

async fn spawn_chatroom_mock() -> String {
    let counter = Arc::new(AtomicU64::new(0));
    let state = counter.clone();
    let app = Router::new().route(
        "/api/forward/channels/1/messages",
        post(move || async move {
            let id = 9000 + state.fetch_add(1, Ordering::SeqCst);
            (StatusCode::CREATED, Json(json!({"id": id})))
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn build_config(mock_base: &str, state_path: &str, token: &str) -> AppConfig {
    serde_json::from_value(json!({
        "onebot": {"listen_host": "127.0.0.1", "listen_port": 0, "path": "/ws", "access_token": "", "self_id": 10000},
        "chatroom": {"base_url": mock_base, "channel_id": 1, "forward_token": "ftok", "refresh_token": "", "group_ids": [123], "qq_to_game_enabled": false},
        "chatbridge": {"enabled": false, "host": ""},
        "commands": {"group_allow_all": true, "status_image": false},
        "api": {"enabled": true, "listen_host": "127.0.0.1", "listen_port": 0, "access_token": token},
        "state_path": state_path,
        "log_level": "WARN"
    }))
    .unwrap()
}

async fn wait_for<F: Fn() -> bool>(cond: F, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("等待超时: {message}");
}

#[tokio::test]
async fn api_read_write_and_auth() {
    let temp = tempfile::tempdir().unwrap();
    let mock_base = spawn_chatroom_mock().await;
    let cfg = build_config(
        &mock_base,
        temp.path().join("state.json").to_str().unwrap(),
        "apitok",
    );
    let service = BridgeService::new(cfg).unwrap();
    service.start().await.unwrap();
    let addr = service
        .api_local_addr()
        .expect("API 应已启动并报告端口");
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // 读接口：health
    let health: Value = client
        .get(format!("{base}/api/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["status"], "ok");
    assert!(health["version"].is_string());

    // 读接口：status 能力清单包含已注册 handler
    let status: Value = client
        .get(format!("{base}/api/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = status["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"chatroom") && names.contains(&"q") && names.contains(&"snap"));
    assert_eq!(status["chatbridge"]["enabled"], false);
    assert_eq!(status["state"]["forwarded_count"], 0);

    // 游戏侧消息进入环形缓冲后可在 /api/messages 读到
    service.on_game_chat("mc", "steve", "大家好").await;
    wait_for(
        || service.recent_log().len() == 1,
        "游戏消息应进入近期消息缓冲",
    )
    .await;
    let messages: Value = client
        .get(format!("{base}/api/messages?limit=10"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let first = &messages["messages"][0];
    assert_eq!(first["source"], "game");
    assert_eq!(first["from"], "steve");
    assert_eq!(first["text"], "大家好");

    // 写接口：无 token → 403
    let resp = client
        .post(format!("{base}/api/relay"))
        .json(&json!({"target": "chatroom", "text": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // 写接口：带 token → chatroom 目标经 mock 成功
    let resp = client
        .post(format!("{base}/api/relay"))
        .header("X-API-Token", "apitok")
        .json(&json!({"target": "chatroom", "text": "来自 API 的消息"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);

    // 写接口：带 token → game 目标但 ChatBridge 未连接 → 200 且 ok:false
    let resp = client
        .post(format!("{base}/api/relay"))
        .header("X-API-Token", "apitok")
        .json(&json!({"target": "game", "text": "游戏广播"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false);

    // 未知 target → 400
    let resp = client
        .post(format!("{base}/api/relay"))
        .header("X-API-Token", "apitok")
        .json(&json!({"target": "irc", "text": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // CORS 预检：204 + 允许头
    let resp = client
        .request(reqwest::Method::OPTIONS, format!("{base}/api/relay"))
        .header("Origin", "https://ui.example.com")
        .header("Access-Control-Request-Method", "POST")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("*")
    );

    service.stop().await;
}

#[tokio::test]
async fn api_relay_requires_token_when_unconfigured() {
    let temp = tempfile::tempdir().unwrap();
    let mock_base = spawn_chatroom_mock().await;
    let cfg = build_config(
        &mock_base,
        temp.path().join("state.json").to_str().unwrap(),
        "",
    );
    let service = BridgeService::new(cfg).unwrap();
    service.start().await.unwrap();
    let addr: SocketAddr = service.api_local_addr().unwrap();
    let client = reqwest::Client::new();

    // 读接口不受影响
    let resp = client
        .get(format!("http://{addr}/api/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // token 未配置 → 写接口一律 403（即使带了任意 token 头）
    let resp = client
        .post(format!("http://{addr}/api/relay"))
        .header("X-API-Token", "anything")
        .json(&json!({"target": "chatroom", "text": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    service.stop().await;
}
