//! agent 技能端到端测试：配置声明 → 路由注册 → QQ 群触发 → LLM 整理 / 降级摘录 / 查不到。
//! 对应 docs/agent-design.md 的核心链路；文档源用本地 fixture，LLM 用 axum mock。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::connect_async;

use chatroom_bridge::config::AppConfig;
use chatroom_bridge::service::BridgeService;

type Actions = Arc<Mutex<Vec<Value>>>;

/// LLM mock：第一次调用返回整理后的回答，之后返回 500（验证降级摘录）。
async fn spawn_llm_mock() -> String {
    let degraded = Arc::new(AtomicBool::new(false));
    let state = degraded.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |body: Json<Value>| {
            let state = state.clone();
            async move {
                let system = body.0["messages"][0]["content"].as_str().unwrap_or("");
                let user = body.0["messages"][1]["content"].as_str().unwrap_or("").to_string();
                // 查询翻译调用（技能层的中间步骤）：恒定成功
                if system.contains("翻译") {
                    return (
                        StatusCode::OK,
                        Json(json!({
                            "choices": [{"message": {"role": "assistant", "content": "piston"}}]
                        })),
                    );
                }
                assert!(user.contains("活塞"), "回答调用的 user prompt 应包含问题与检索片段");
                // 回答调用：第一次成功，之后 500（验证技能的降级摘录路径）
                if state.swap(true, Ordering::SeqCst) {
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "down"})));
                }
                (StatusCode::OK, Json(json!({
                    "choices": [{"message": {"role": "assistant", "content": "活塞是一种红石元件，被推动时会伸出方块臂。[1]"}}]
                })))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn fixture_docs(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("redstone.md"),
        "# 红石元件\n\n活塞是一种红石元件，可以推动最多 12 个方块。\n\n# 观察者\n\n观察者用于检测方块更新。\n",
    )
    .unwrap();
}

fn build_config(llm_base: &str, docs_root: &std::path::Path, state_path: &str) -> AppConfig {
    serde_json::from_value(json!({
        "onebot": {"listen_host": "127.0.0.1", "listen_port": 0, "path": "/ws", "access_token": "tok", "self_id": 10000},
        "chatroom": {"base_url": "https://chatroom.example.com", "channel_id": 1, "forward_token": "", "group_ids": [123], "player_tracking_enabled": false},
        "chatbridge": {"enabled": false, "host": ""},
        "commands": {"group_allow_all": true, "status_image": false},
        "api": {"enabled": false},
        "agent": {
            "enabled": true,
            "llm": {"api_url": format!("{llm_base}/v1/chat/completions"), "api_key": "llmtok", "model": "test-model", "max_answer_chars": 500},
            "skills": [
                {"name": "tmc", "description": "TechMC 文档查询", "max_results": 3,
                 "sources": [{"type": "local", "root": docs_root.to_str().unwrap(), "name": "techmc", "extensions": [".md"]}]}
            ]
        },
        "state_path": state_path,
        "log_level": "WARN"
    }))
    .unwrap()
}

fn group_event(message_id: i64, text: &str) -> Value {
    json!({
        "post_type": "message", "message_type": "group",
        "group_id": 123, "user_id": 42, "message_id": message_id,
        "nickname": "tester", "card": "", "message": text
    })
}

async fn wait_for<F: Fn() -> bool>(cond: F, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("等待超时: {message}");
}

/// 假 NapCat：回应所有动作并记录，返回发送端。
async fn connect_pumped(addr: SocketAddr, actions: Actions) -> mpsc::Sender<Message> {
    let mut request = format!("ws://{addr}/ws").into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Authorization", HeaderValue::from_str("Bearer tok").unwrap());
    let (ws, _) = connect_async(request).await.unwrap();
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

/// 取 pump 记录里第 n 个 send_group_msg 的文本内容。
fn sent_texts(actions: &Actions) -> Vec<String> {
    actions
        .lock()
        .unwrap()
        .iter()
        .filter(|a| a["action"] == "send_group_msg")
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

#[tokio::test]
async fn agent_skill_flow_llm_fallback_and_miss() {
    let temp = tempfile::tempdir().unwrap();
    let docs = temp.path().join("docs");
    fixture_docs(&docs);
    let llm_base = spawn_llm_mock().await;
    let cfg = build_config(
        &llm_base,
        &docs,
        temp.path().join("state.json").to_str().unwrap(),
    );
    let service = BridgeService::new(cfg).unwrap();
    service.start().await.unwrap();
    let addr = service.server().local_addr().unwrap();

    let actions: Actions = Arc::new(Mutex::new(Vec::new()));
    let napcat = connect_pumped(addr, actions.clone()).await;
    wait_for(
        || service.server().stats().connections >= 1,
        "OneBot 连接应已建立",
    )
    .await;

    // 1) !tmc 命中 + LLM 整理：回答含 LLM 内容与出处编号，且消息被消费（不进转发流水线）
    napcat
        .send(Message::text(group_event(1, "!tmc 活塞").to_string()))
        .await
        .unwrap();
    wait_for(
        || !sent_texts(&actions).is_empty(),
        "技能应经 LLM 回复",
    )
    .await;
    let first = sent_texts(&actions)[0].clone();
    assert!(first.contains("红石元件"), "应发送 LLM 整理的回答: {first}");
    assert!(first.contains("[1]"), "LLM 回答应保留出处编号: {first}");

    // 2) 同查询但 LLM 已 500 → 降级摘录：含标题与文件:行号出处
    napcat
        .send(Message::text(group_event(2, "!tmc 活塞").to_string()))
        .await
        .unwrap();
    wait_for(
        || sent_texts(&actions).len() >= 2,
        "LLM 失败应降级为摘录回复",
    )
    .await;
    let second = sent_texts(&actions)[1].clone();
    assert!(second.contains("检索结果"), "降级回复应是摘录格式: {second}");
    assert!(second.contains("redstone.md:"), "摘录应带文件:行号出处: {second}");

    // 3) 查不到 → 明确说查不到
    napcat
        .send(Message::text(group_event(3, "!tc 不存在的东西").to_string()))
        .await
        .unwrap();
    // 注意：!tc 不是注册的技能（只有 tmc），且 !tc 不是 !q → 应落回转发流水线；
    // forward_token 为空 → 无 HTTP 调用，也不应有新的 send_group_msg
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        sent_texts(&actions).len(),
        2,
        "未注册命令不应触发任何回复"
    );

    napcat
        .send(Message::text(group_event(4, "!tmc 下界合金锭").to_string()))
        .await
        .unwrap();
    wait_for(
        || sent_texts(&actions).len() >= 3,
        "查不到也应有回复",
    )
    .await;
    let third = sent_texts(&actions)[2].clone();
    assert!(third.contains("没查到"), "应明确说查不到: {third}");

    service.stop().await;
}
