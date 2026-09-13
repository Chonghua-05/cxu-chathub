//! 端到端装配冒烟测试（对应 Python 版 tests/test_service_smoke.py）：
//! 真实 `BridgeService` + 真实 OneBot WS + mock Forward API / 语音 API。
//! 覆盖：错误 token 401、重复消息去重、/chatroom 命令回发、未知命令落回转发。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, tungstenite::Error as WsError};

use chatroom_bridge::config::AppConfig;
use chatroom_bridge::service::BridgeService;

type Actions = Arc<Mutex<Vec<Value>>>;

async fn spawn_chatroom_mock() -> (String, Arc<AtomicU64>) {
    let posts = Arc::new(AtomicU64::new(0));
    let uploads = Arc::new(AtomicU64::new(0));
    let posts_state = posts.clone();
    let uploads_state = uploads.clone();

    let app = Router::new()
        .route(
            "/api/forward/channels/1/messages",
            post(move || async move {
                let id = 5000 + posts_state.fetch_add(1, Ordering::SeqCst);
                (StatusCode::CREATED, Json(json!({"id": id})))
            }),
        )
        .route(
            "/api/forward/channels/1/upload",
            post(move || async move {
                let id = 7000 + uploads_state.fetch_add(1, Ordering::SeqCst);
                (StatusCode::OK, Json(json!({"id": id})))
            }),
        )
        .route(
            "/api/voice/qqbot/get_voice_channel_people",
            get(|| async {
                Json(json!({"channels": [{"name": "语音频道", "people": ["甲", "乙"]}]}))
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), posts)
}

fn build_config(mock_base: &str, state_path: &str) -> AppConfig {
    serde_json::from_value(json!({
        "onebot": {
            "listen_host": "127.0.0.1",
            "listen_port": 0,
            "path": "/ws",
            "access_token": "tok",
            "self_id": 10000
        },
        "chatroom": {
            "base_url": mock_base,
            "channel_id": 1,
            "forward_token": "ftok",
            "refresh_token": "",
            "group_ids": [123],
            "qq_sync_enabled": true,
            "qq_forward_enabled": true,
            "qq_to_game_enabled": false,
            "player_join_pattern": "^(.+?) 加入了游戏$",
            "player_quit_pattern": "^(.+?) 离开了游戏$",
            "voice_api": format!("{mock_base}/api/voice/qqbot/get_voice_channel_people"),
            "status_api": format!("{mock_base}/api/status"),
            "server_addresses": [["主IP", "game.example.com"]]
        },
        "chatbridge": {"enabled": false, "host": "", "port": 21027, "name": "web", "password": "", "aes_key": ""},
        "commands": {"group_allow_all": true, "allow_from": [], "status_image": false},
        "state_path": state_path,
        "log_level": "WARN"
    }))
    .unwrap()
}

fn group_event(message_id: i64, text: &str) -> Value {
    json!({
        "post_type": "message",
        "message_type": "group",
        "group_id": 123,
        "user_id": 42,
        "message_id": message_id,
        "nickname": "tester",
        "card": "",
        "message": text
    })
}

/// 等待条件成立，超时 panic。
async fn wait_for<F: Fn() -> bool>(cond: F, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("等待超时: {message}");
}

fn bearer_request(addr: SocketAddr, token: &str) -> tokio_tungstenite::tungstenite::http::Request<()> {
    let mut request = format!("ws://{addr}/ws").into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    request
}

/// 以假 NapCat 身份连入：后台任务读方向自动回应所有动作（echo pump）并记录见到的
/// action；返回 mpsc 发送端用于下发群事件。
async fn connect_pumped(addr: SocketAddr, token: &str, actions: Actions) -> mpsc::Sender<Message> {
    let (ws, _) = connect_async(bearer_request(addr, token)).await.unwrap();
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::channel::<Message>(64);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                incoming = rx.recv() => match incoming {
                    Some(message) => { let _ = sink.send(message).await; }
                    None => break,
                },
                outgoing = stream.next() => match outgoing {
                    Some(Ok(msg)) => {
                        let Ok(payload) = msg.into_text() else { continue };
                        let Ok(value) = serde_json::from_str::<Value>(&payload) else { continue };
                        let Some(action) = value.get("action").and_then(Value::as_str).map(String::from) else { continue };
                        let echo = value.get("echo").cloned();
                        actions.lock().unwrap().push(json!({
                            "action": action,
                            "params": value.get("params").cloned().unwrap_or(Value::Null)
                        }));
                        let _ = sink
                            .send(Message::text(
                                json!({"status": "ok", "retcode": 0, "data": null, "echo": echo}).to_string(),
                            ))
                            .await;
                    }
                    _ => break,
                },
            }
        }
    });
    tx
}

#[tokio::test]
async fn smoke_dedup_command_and_auth() {
    let temp = tempfile::tempdir().unwrap();
    let (mock_base, posts) = spawn_chatroom_mock().await;
    let cfg = build_config(
        &mock_base,
        temp.path().join("state.json").to_str().unwrap(),
    );
    let service = BridgeService::new(cfg).unwrap();
    service.start().await.unwrap();
    let addr = service.server().local_addr().unwrap();

    // 1) 错误 token → 握手 401（升级前拒绝）
    match connect_async(bearer_request(addr, "wrong")).await {
        Err(WsError::Http(resp)) => assert_eq!(resp.status(), 401, "错误 token 应在升级前被拒"),
        other => panic!("期望 401 握手拒绝，实际: {other:?}"),
    }

    // 2) 正确 token 连入（单连接：发事件 + 回应动作）
    let actions: Actions = Arc::new(Mutex::new(Vec::new()));
    let napcat = connect_pumped(addr, "tok", actions.clone()).await;
    wait_for(
        || service.server().stats().connections >= 1,
        "OneBot 连接应已建立",
    )
    .await;

    // 3) 重复消息 → 去重：只有一次转发
    for _ in 0..2 {
        napcat
            .send(Message::text(group_event(1, "hello").to_string()))
            .await
            .unwrap();
    }
    wait_for(
        || posts.load(Ordering::SeqCst) == 1,
        "重复消息应只转发一次",
    )
    .await;
    wait_for(
        || service.state().snapshot().forwarded_count == 1,
        "去重表应记录 1 条",
    )
    .await;

    // 4) /chatroom 命令 → send_group_msg 动作回发
    napcat
        .send(Message::text(group_event(2, "/chatroom").to_string()))
        .await
        .unwrap();
    wait_for(
        || {
            actions
                .lock()
                .unwrap()
                .iter()
                .any(|a| a.get("action").and_then(Value::as_str) == Some("send_group_msg"))
        },
        "命令应触发 send_group_msg 回发",
    )
    .await;

    // 5) 未知命令 → 落回转发流水线（当作普通消息转发）
    napcat
        .send(Message::text(group_event(3, "/unknown").to_string()))
        .await
        .unwrap();
    wait_for(
        || posts.load(Ordering::SeqCst) == 2,
        "未知命令应被当作普通消息转发",
    )
    .await;

    // 6) 玩家上下线推送（ChatBridge 事件驱动）：系统广播（author 为空）→ QQ 群推送
    service.on_game_chat("snapshot", "", "Steve 加入了游戏").await;
    wait_for(
        || {
            sent_texts(&actions)
                .iter()
                .any(|text| text.contains("Steve 上线"))
        },
        "上线广播应推送到 QQ 群",
    )
    .await;
    service.on_game_chat("snapshot", "", "Steve 离开了游戏").await;
    wait_for(
        || {
            sent_texts(&actions)
                .iter()
                .any(|text| text.contains("Steve 下线"))
        },
        "下线广播应推送到 QQ 群",
    )
    .await;

    // 7) 防伪造：他人冒充「xx 加入了游戏」（author ≠ 玩家名）不触发推送
    let texts_before = sent_texts(&actions).len();
    service.on_game_chat("web", "Hacker", "Steve 加入了游戏").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        sent_texts(&actions).len(),
        texts_before,
        "他人伪造的上线广播不应触发推送"
    );

    service.stop().await;
}

/// 取 pump 记录里所有 send_group_msg 的文本内容。
fn sent_texts(actions: &Actions) -> Vec<String> {
    actions
        .lock()
        .unwrap()
        .iter()
        .filter(|a| a.get("action").and_then(Value::as_str) == Some("send_group_msg"))
        .filter_map(|a| {
            let texts: Vec<String> = a["params"]["message"]
                .as_array()?
                .iter()
                .filter_map(|seg| seg["data"]["text"].as_str().map(|s| s.to_string()))
                .collect();
            Some(texts.join(""))
        })
        .collect()
}
