//! onebot 集成测试：tokio-tungstenite 充当假 NapCat 客户端，打通真实 WS 连接。
//!
//! 对齐 Python 侧 tests/test_onebot_actions.py（消费者死锁回归 + 事件顺序）与
//! tests/test_service_smoke.py 的 WS 部分（token 校验、healthz、断开清理）。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chatroom_bridge::adapters::onebot::{
    GroupMessage, GroupMessageHandler, OneBotConnection, OneBotServer,
};
use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::{Error as WsError, Message as WsMessage};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const TOKEN: &str = "secret-token";

type ClientWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn noop_handler() -> GroupMessageHandler {
    Arc::new(|_conn: Arc<OneBotConnection>, _message: GroupMessage| {
        Box::pin(async {}) as BoxFuture<'static, ()>
    })
}

fn group_event(message_id: i64) -> Value {
    json!({
        "post_type": "message",
        "message_type": "group",
        "self_id": 1,
        "group_id": 123456789,
        "user_id": 10001,
        "message_id": message_id,
        "sender": { "nickname": "玩家A" },
        "message": [{ "type": "text", "data": { "text": "/chatroom" } }],
    })
}

async fn start_server(token: &str, handler: GroupMessageHandler) -> (OneBotServer, SocketAddr) {
    let server = OneBotServer::new("127.0.0.1", 0, "/ws", token, handler);
    server.start().await.unwrap();
    let addr = server.local_addr().unwrap();
    (server, addr)
}

async fn connect_client(addr: SocketAddr, token: Option<&str>, query: &str) -> ClientWs {
    let url = if query.is_empty() {
        format!("ws://{addr}/ws")
    } else {
        format!("ws://{addr}/ws?{query}")
    };
    let mut request = url.as_str().into_client_request().unwrap();
    if let Some(token) = token {
        request.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
    }
    let (ws, _response) = connect_async(request).await.unwrap();
    ws
}

/// 假 NapCat：转发测试方要发的事件；对任何动作回 `{"status":"ok","retcode":0,...}`。
/// 返回事件发送端（drop 即关闭连接）与任务句柄。
fn spawn_fake_napcat(
    ws: ClientWs,
    record: Option<Arc<Mutex<Vec<Value>>>>,
) -> (mpsc::UnboundedSender<Value>, JoinHandle<()>) {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let handle = tokio::spawn(async move {
        let (mut sink, mut stream) = ws.split();
        loop {
            tokio::select! {
                outgoing = out_rx.recv() => match outgoing {
                    Some(payload) => {
                        if sink.send(WsMessage::text(payload.to_string())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                incoming = stream.next() => match incoming {
                    Some(Ok(WsMessage::Text(text))) => {
                        let Ok(payload) = serde_json::from_str::<Value>(&text) else {
                            continue;
                        };
                        if let Some(record) = &record {
                            record.lock().unwrap().push(payload.clone());
                        }
                        if payload.get("action").is_some() {
                            let reply = json!({
                                "status": "ok",
                                "retcode": 0,
                                "data": { "detail": "pong" },
                                "echo": payload.get("echo").cloned().unwrap_or(Value::Null),
                            });
                            if sink.send(WsMessage::text(reply.to_string())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                    _ => break,
                },
            }
        }
        let _ = sink.close().await;
    });
    (out_tx, handle)
}

async fn wait_for(condition: impl Fn() -> bool, timeout: Duration, what: &'static str) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "等待超时: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn get_healthz(addr: SocketAddr) -> Value {
    reqwest::get(format!("http://{addr}/healthz"))
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()
}

/// 处理器内调用 OneBot 动作必须能拿到 echo 响应，且远快于超时（死锁回归）。
#[tokio::test]
async fn action_call_inside_handler_resolves_quickly() {
    let results: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let elapsed_ms = Arc::new(AtomicU64::new(u64::MAX));
    let handler: GroupMessageHandler = {
        let results = results.clone();
        let elapsed_ms = elapsed_ms.clone();
        Arc::new(move |conn: Arc<OneBotConnection>, _message: GroupMessage| {
            let results = results.clone();
            let elapsed_ms = elapsed_ms.clone();
            Box::pin(async move {
                let started = Instant::now();
                let data = conn
                    .call("get_status", json!({}))
                    .await
                    .expect("处理器内的动作调用必须成功");
                elapsed_ms.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                results.lock().unwrap().push(data);
            })
        })
    };
    let (server, addr) = start_server(TOKEN, handler).await;
    let ws = connect_client(addr, Some(TOKEN), "").await;
    let (out_tx, _pump) = spawn_fake_napcat(ws, None);

    out_tx.send(group_event(1)).unwrap();
    wait_for(
        || !results.lock().unwrap().is_empty(),
        Duration::from_secs(3),
        "处理器应拿到动作响应",
    )
    .await;

    assert_eq!(
        results.lock().unwrap().as_slice(),
        [json!({ "detail": "pong" })]
    );
    let elapsed = elapsed_ms.load(Ordering::Relaxed);
    assert!(elapsed < 3000, "动作往返不应等到超时（实际 {elapsed} ms）");

    drop(out_tx);
    server.stop().await;
}

/// 连续多条事件必须按到达顺序处理（消费者顺序保证）。
#[tokio::test]
async fn event_order_is_preserved() {
    let seen: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
    let handler: GroupMessageHandler = {
        let seen = seen.clone();
        Arc::new(move |_conn: Arc<OneBotConnection>, message: GroupMessage| {
            let seen = seen.clone();
            Box::pin(async move {
                seen.lock().unwrap().push(message.message_id);
                // 第一条放慢，若乱序处理立刻暴露
                if message.message_id == 1 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
        })
    };
    let (server, addr) = start_server(TOKEN, handler).await;
    let ws = connect_client(addr, Some(TOKEN), "").await;
    let (out_tx, _pump) = spawn_fake_napcat(ws, None);

    for mid in [1, 2, 3] {
        out_tx.send(group_event(mid)).unwrap();
    }
    wait_for(
        || seen.lock().unwrap().len() >= 3,
        Duration::from_secs(3),
        "应按顺序处理 3 条事件",
    )
    .await;

    assert_eq!(seen.lock().unwrap().as_slice(), [1, 2, 3]);

    drop(out_tx);
    server.stop().await;
}

/// 错误 token → 升级前 HTTP 401；Bearer 头与 ?access_token= 查询参数都放行。
#[tokio::test]
async fn token_auth_rejects_wrong_and_accepts_bearer_and_query() {
    let (server, addr) = start_server(TOKEN, noop_handler()).await;

    // 错误 token：握手必须以 401 拒绝（tungstenite 暴露 HTTP 错误响应状态）
    let mut request = format!("ws://{addr}/ws").into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Authorization", HeaderValue::from_static("Bearer wrong"));
    match connect_async(request).await {
        Err(WsError::Http(response)) => assert_eq!(response.status(), StatusCode::UNAUTHORIZED),
        Ok(_) => panic!("错误 token 应被 401 拒绝"),
        Err(other) => panic!("意外的握手错误: {other}"),
    }
    assert_eq!(server.stats().connections, 0, "被拒连接不计入统计");

    // Bearer 头 → 接受
    let ws = connect_client(addr, Some(TOKEN), "").await;
    let (out_tx, _pump) = spawn_fake_napcat(ws, None);
    wait_for(
        || server.connection().is_some(),
        Duration::from_secs(3),
        "Bearer 连接应建立",
    )
    .await;

    // ?access_token= 查询参数 → 同样接受
    let ws2 = connect_client(addr, None, "access_token=secret-token").await;
    let (out_tx2, _pump2) = spawn_fake_napcat(ws2, None);
    wait_for(
        || server.stats().connections >= 2,
        Duration::from_secs(3),
        "query token 连接应建立",
    )
    .await;

    drop(out_tx);
    drop(out_tx2);
    server.stop().await;
}

/// 未配置 token → 任何客户端都能连入。
#[tokio::test]
async fn no_token_allows_any_client() {
    let (server, addr) = start_server("", noop_handler()).await;
    let ws = connect_client(addr, None, "").await;
    let (out_tx, _pump) = spawn_fake_napcat(ws, None);
    wait_for(
        || server.connection().is_some(),
        Duration::from_secs(3),
        "无 token 应可直接连入",
    )
    .await;
    drop(out_tx);
    server.stop().await;
}

/// /healthz：无连接与有连接两种状态，以及 lifecycle 事件写入 self_id。
#[tokio::test]
async fn healthz_reports_connection_self_id_and_stats() {
    let (server, addr) = start_server(TOKEN, noop_handler()).await;

    let health = get_healthz(addr).await;
    assert_eq!(health["status"], "ok");
    assert_eq!(health["onebot_connected"], false);
    assert_eq!(health["self_id"], 0);
    assert_eq!(health["stats"]["connections"], 0);
    assert_eq!(health["stats"]["group_messages"], 0);

    let ws = connect_client(addr, Some(TOKEN), "").await;
    let (out_tx, _pump) = spawn_fake_napcat(ws, None);
    out_tx
        .send(json!({
            "post_type": "meta_event",
            "meta_event_type": "lifecycle",
            "self_id": 10000,
        }))
        .unwrap();
    out_tx.send(group_event(7)).unwrap();

    // 轮询直到 healthz 反映 self_id 与事件计数
    let deadline = Instant::now() + Duration::from_secs(3);
    let health = loop {
        let current = get_healthz(addr).await;
        if current["self_id"] == 10000 && current["stats"]["group_messages"].as_u64() == Some(1) {
            break current;
        }
        assert!(Instant::now() < deadline, "healthz 未反映连接与事件");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(health["status"], "ok");
    assert_eq!(health["onebot_connected"], true);
    assert_eq!(health["self_id"], 10000);
    assert!(health["stats"]["connections"].as_u64().unwrap() >= 1);
    assert_eq!(health["stats"]["group_messages"], 1);

    drop(out_tx);
    server.stop().await;
}

/// 客户端断开后 connection() 应回落为 None。
#[tokio::test]
async fn connection_slot_cleared_after_client_closes() {
    let (server, addr) = start_server(TOKEN, noop_handler()).await;
    let ws = connect_client(addr, Some(TOKEN), "").await;
    let (out_tx, _pump) = spawn_fake_napcat(ws, None);
    wait_for(
        || server.connection().is_some(),
        Duration::from_secs(3),
        "连接应建立",
    )
    .await;

    drop(out_tx); // 客户端关闭
    wait_for(
        || server.connection().is_none(),
        Duration::from_secs(3),
        "断开后 connection() 应为 None",
    )
    .await;

    server.stop().await;
}

/// 动作走线格式：send_group_text → send_group_msg，group_id 与 message 段符合 OneBot 规范。
#[tokio::test]
async fn send_group_text_wire_format() {
    let actions: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let handler: GroupMessageHandler = {
        Arc::new(move |conn: Arc<OneBotConnection>, _message: GroupMessage| {
            Box::pin(async move {
                conn.send_group_text(123456789, "命令响应")
                    .await
                    .expect("发送动作必须成功");
            })
        })
    };
    let (server, addr) = start_server(TOKEN, handler).await;
    let ws = connect_client(addr, Some(TOKEN), "").await;
    let (out_tx, _pump) = spawn_fake_napcat(ws, Some(actions.clone()));
    out_tx.send(group_event(1)).unwrap();

    wait_for(
        || !actions.lock().unwrap().is_empty(),
        Duration::from_secs(3),
        "客户端应收到动作请求",
    )
    .await;

    let action = actions.lock().unwrap()[0].clone();
    assert_eq!(action["action"], "send_group_msg");
    assert_eq!(action["params"]["group_id"], 123456789);
    assert_eq!(
        action["params"]["message"],
        json!([{ "type": "text", "data": { "text": "命令响应" } }])
    );
    assert!(action.get("echo").is_some(), "动作必须携带 echo");

    drop(out_tx);
    server.stop().await;
}
