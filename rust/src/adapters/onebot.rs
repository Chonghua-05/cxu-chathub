//! OneBot v11 反向 WS 服务端 + 动作调用（axum 实现，对应 Python `onebot.py`）。
//!
//! NapCat 作为客户端连入 ``ws://<host>:<port><path>``：
//!
//! * 校验 token（`Authorization: Bearer <token>` 或 `?access_token=`），
//!   校验失败在 WS 升级之前直接返回 401
//! * 收 `post_type=message` / `message_type=group` 事件，解析为 [`GroupMessage`]
//! * 通过同一条连接发送动作（`send_group_msg` 等），按 `echo` 匹配响应
//!
//! 关键设计（Python `_consume` 的文档字符串，且有回归测试）：事件必须在独立的
//! 消费者任务里顺序处理，绝不能在 WS 读取循环内 await 处理器 —— 处理器经常
//! 会通过同一条 WS 调用动作（发回复等），若在读取循环里等待处理器，echo 响应
//! 就永远读不到，只能一直卡到动作超时。这里用「读取循环 → 解析 → 入队 →
//! 专门的消费者任务顺序处理（保持顺序）」复刻该语义：处理器失败只记日志，
//! 绝不杀掉循环。
//!
//! 设计上与旧框架的 aiocqhttp 平台完全解耦：不依赖任何框架对象。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::future::BoxFuture;
use futures_util::{FutureExt, SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{interval_at, sleep, timeout};

/// 单条连接的动作调用超时（Python 默认 20.0s）。
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(20);
/// 心跳：每 30s 发一个 WS Ping（对齐 aiohttp `heartbeat=30.0`）。
const HEARTBEAT: Duration = Duration::from_secs(30);
/// 单条消息上限 16 MiB（对齐 aiohttp `max_msg_size=16 * 1024 * 1024`）。
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// 群消息处理器（Python `GroupMessageHandler` 的 Rust 形式）。
pub type GroupMessageHandler = Arc<
    dyn Fn(Arc<OneBotConnection>, GroupMessage) -> BoxFuture<'static, ()> + Send + Sync,
>;

/// OneBot 消息段（Python 的 `type` 字段改名 `kind`，避免与关键字撞名）。
#[derive(Debug, Clone, Default)]
pub struct Segment {
    pub kind: String,
    pub data: Value,
}

impl Segment {
    /// Python: `str(data.get("text") or "")`
    pub fn text(&self) -> String {
        py_str(self.data.get("text"))
    }

    /// Python: `str(data.get("file") or "")`
    pub fn file(&self) -> String {
        py_str(self.data.get("file"))
    }

    /// Python: `str(data.get("url") or "")`
    pub fn url(&self) -> String {
        py_str(self.data.get("url"))
    }

    /// Python: `str(data.get("summary") or "")`
    pub fn summary(&self) -> String {
        py_str(self.data.get("summary"))
    }
}

/// 群消息事件。
#[derive(Debug, Clone, Default)]
pub struct GroupMessage {
    pub group_id: i64,
    pub user_id: i64,
    pub message_id: i64,
    pub nickname: String,
    pub card: String,
    pub segments: Vec<Segment>,
    pub raw: Value,
}

impl GroupMessage {
    /// Python: `card or nickname or str(user_id)`
    pub fn display_name(&self) -> String {
        if !self.card.is_empty() {
            self.card.clone()
        } else if !self.nickname.is_empty() {
            self.nickname.clone()
        } else {
            self.user_id.to_string()
        }
    }

    /// 所有 text 段拼接后 strip。
    pub fn text(&self) -> String {
        self.segments
            .iter()
            .filter(|seg| seg.kind == "text")
            .map(Segment::text)
            .collect::<String>()
            .trim()
            .to_string()
    }

    /// 第一个 reply 段的 `data["id"]`（Python: `str(value) if value else None`）。
    pub fn reply_message_id(&self) -> Option<String> {
        for seg in &self.segments {
            if seg.kind == "reply" {
                return match seg.data.get("id").filter(|v| is_truthy(v)) {
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(other) => Some(other.to_string()),
                    None => None,
                };
            }
        }
        None
    }

    /// `type == "image"` 的段（克隆返回）。
    pub fn images(&self) -> Vec<Segment> {
        self.segments
            .iter()
            .filter(|seg| seg.kind == "image")
            .cloned()
            .collect()
    }

    /// at 段的 `data["qq"]`。Python: `isinstance(qq, (int, str)) and str(qq).isdigit()`，
    /// 即负数 / 浮点 / 非纯数字字符串都不算。
    pub fn at_user_ids(&self) -> Vec<i64> {
        self.segments
            .iter()
            .filter(|seg| seg.kind == "at")
            .filter_map(|seg| match seg.data.get("qq") {
                // str(-1).isdigit() == False → 负数同样排除
                Some(Value::Number(n)) => n.as_i64().filter(|id| *id >= 0),
                Some(Value::String(s))
                    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    s.parse().ok()
                }
                _ => None,
            })
            .collect()
    }

    /// 调试摘要（对齐 Python `describe`）。
    pub fn describe(&self) -> String {
        format!(
            "群={} 用户={}({}) 消息ID={}",
            self.group_id,
            self.display_name(),
            self.user_id,
            self.message_id
        )
    }
}

/// Python 的 falsy 判定（None / False / 0 / "" / 空容器 → false）。
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `str(v or "")`：假值 → 空串；字符串原样；其它类型按 JSON 文本化。
fn py_str(value: Option<&Value>) -> String {
    match value.filter(|v| is_truthy(v)) {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Python `int(v or 0)` 的容错版：缺失 / 假值 → 0；数字或数字字符串 → 数值。
fn json_int(value: Option<&Value>) -> i64 {
    let Some(value) = value.filter(|v| is_truthy(v)) else {
        return 0;
    };
    match value {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_u64().map(|u| u as i64))
            .unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// 把 OneBot message 字段解析成 Segment 列表：
/// 字符串 → 单个 text 段（非空才给）；数组 → 逐段；其它形状一律容忍跳过。
pub fn parse_segments(message: &Value) -> Vec<Segment> {
    if let Some(text) = message.as_str() {
        // 字符串形式（CQ 码兜底）：整段当文本
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![Segment {
                kind: "text".to_string(),
                data: json!({ "text": text }),
            }]
        };
    }
    let mut segments = Vec::new();
    if let Some(items) = message.as_array() {
        for item in items {
            let Some(obj) = item.as_object() else {
                continue; // 非 dict 条目
            };
            let Some(seg_type) = obj.get("type").filter(|v| is_truthy(v)) else {
                continue; // 缺 type 或 type 为假值
            };
            let kind = match seg_type {
                Value::String(s) => s.clone(),
                // Python `str(seg_type)` 的等价兜底（数字等非常规类型）
                other => other.to_string(),
            };
            // dict(data) if isinstance(data, dict) else {}
            let data = obj
                .get("data")
                .filter(|v| v.is_object())
                .cloned()
                .unwrap_or_else(|| json!({}));
            segments.push(Segment { kind, data });
        }
    }
    segments
}

/// 从原始事件里提取群消息；非群消息返回 None。
pub fn parse_group_message(raw: &Value) -> Option<GroupMessage> {
    if raw.get("post_type").and_then(Value::as_str) != Some("message")
        || raw.get("message_type").and_then(Value::as_str) != Some("group")
    {
        return None;
    }
    // sender = raw.get("sender") if isinstance(..., dict) else {}
    let sender = raw.get("sender").and_then(Value::as_object);
    Some(GroupMessage {
        group_id: json_int(raw.get("group_id")),
        user_id: json_int(raw.get("user_id")),
        message_id: json_int(raw.get("message_id")),
        nickname: py_str(sender.and_then(|s| s.get("nickname"))),
        card: py_str(sender.and_then(|s| s.get("card"))),
        segments: parse_segments(raw.get("message").unwrap_or(&Value::Null)),
        raw: raw.clone(),
    })
}

/// 动作调用错误（对应 Python 的 ConnectionError / asyncio.TimeoutError / RuntimeError）。
#[derive(Debug, thiserror::Error)]
pub enum OneBotError {
    #[error("OneBot 连接已关闭")]
    Closed,
    #[error("OneBot 动作超时（{0:?}）")]
    Timeout(Duration),
    #[error("OneBot 动作失败 retcode={retcode} {detail}")]
    Failed { retcode: i64, detail: String },
}

/// 一条已建立的 OneBot 连接。动作调用与事件接收共用这条 WS。
pub struct OneBotConnection {
    /// 发往 WS 写任务的消息通道（动作请求从这里出去）。
    outbound: mpsc::UnboundedSender<Message>,
    /// echo 自增计数（Python `itertools.count(1)`：首个 echo 为 1）。
    counter: AtomicU64,
    call_timeout: Duration,
    /// lifecycle meta 事件里的 self_id。
    self_id: AtomicI64,
    /// WS 是否已关闭（读取循环结束时置位）。
    closed: AtomicBool,
    /// echo -> 等待响应的调用（feed 送 Ok(data) / Err(Failed)；连接退出送 Err(Closed)）。
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, OneBotError>>>>,
}

impl OneBotConnection {
    fn new(outbound: mpsc::UnboundedSender<Message>) -> Self {
        Self {
            outbound,
            counter: AtomicU64::new(0),
            call_timeout: DEFAULT_CALL_TIMEOUT,
            self_id: AtomicI64::new(0),
            closed: AtomicBool::new(false),
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn self_id(&self) -> i64 {
        self.self_id.load(Ordering::Relaxed)
    }

    /// WS 未关闭（对齐 Python `not ws.closed`）。
    pub fn connected(&self) -> bool {
        !self.closed.load(Ordering::Relaxed)
    }

    fn lock_pending(
        &self,
    ) -> MutexGuard<'_, HashMap<u64, oneshot::Sender<Result<Value, OneBotError>>>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 发起动作调用：echo=自增计数；等待响应（默认 20s）；ws 已关闭 → Closed；
    /// retcode 非 0 → Failed。
    pub async fn call(&self, action: &str, params: Value) -> Result<Value, OneBotError> {
        if !self.connected() {
            return Err(OneBotError::Closed);
        }
        let echo = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        self.lock_pending().insert(echo, tx);
        let payload = json!({ "action": action, "params": params, "echo": echo });
        let result = if self.outbound.send(Message::text(payload.to_string())).is_err() {
            // 写任务已退出：连接不再可用
            Err(OneBotError::Closed)
        } else {
            match timeout(self.call_timeout, rx).await {
                Ok(Ok(Ok(data))) => Ok(data),
                Ok(Ok(Err(err))) => Err(err), // feed 报告的动作失败
                Ok(Err(_)) => Err(OneBotError::Closed), // sender 被丢弃 → 连接关闭
                Err(_) => Err(OneBotError::Timeout(self.call_timeout)),
            }
        };
        self.lock_pending().remove(&echo);
        result
    }

    /// 把动作响应喂给等待中的调用（按 echo 匹配 pending）：
    /// `status=="ok"` 或 `retcode==0` → data；否则 → Failed；不是响应返回 false。
    pub fn feed(&self, payload: &Value) -> bool {
        let Some(echo_value) = payload.get("echo") else {
            return false;
        };
        // echo 兼容数字与数字字符串（Python 侧按 str(echo) 匹配）
        let Some(echo) = echo_value
            .as_u64()
            .or_else(|| echo_value.as_str().and_then(|s| s.trim().parse().ok()))
        else {
            return false;
        };
        let Some(sender) = self.lock_pending().remove(&echo) else {
            return false; // 未知 echo（或调用已超时离开）
        };
        let status_ok = payload.get("status").and_then(Value::as_str) == Some("ok");
        let retcode = payload.get("retcode").and_then(Value::as_i64);
        if status_ok || retcode == Some(0) {
            // Python: future.set_result(payload.get("data"))（缺失即 None）
            let _ = sender.send(Ok(payload.get("data").cloned().unwrap_or(Value::Null)));
        } else {
            // Python: detail = message or wording or ""
            let detail = ["message", "wording"]
                .iter()
                .find_map(|key| {
                    let text = py_str(payload.get(*key));
                    if text.is_empty() { None } else { Some(text) }
                })
                .unwrap_or_default();
            // retcode 缺失时 Python 会打印 None，这里以 0 表示
            let retcode = retcode.unwrap_or_default();
            let _ = sender.send(Err(OneBotError::Failed { retcode, detail }));
        }
        true
    }

    // --- 常用动作 ---

    pub async fn send_group_msg(&self, group_id: i64, message: Value) -> Result<Value, OneBotError> {
        self.call(
            "send_group_msg",
            json!({ "group_id": group_id, "message": message }),
        )
        .await
    }

    /// 单个 text 段。
    pub async fn send_group_text(&self, group_id: i64, text: &str) -> Result<Value, OneBotError> {
        self.send_group_msg(group_id, json!([{ "type": "text", "data": { "text": text } }]))
            .await
    }

    /// `file_ref` 形如 "base64://..."。
    pub async fn send_group_image(
        &self,
        group_id: i64,
        file_ref: &str,
    ) -> Result<Value, OneBotError> {
        self.send_group_msg(
            group_id,
            json!([{ "type": "image", "data": { "file": file_ref } }]),
        )
        .await
    }

    /// 取图片的真实下载地址（供转发到 chatroom 时上传）。
    pub async fn get_image(&self, file: &str) -> Result<Value, OneBotError> {
        self.call("get_image", json!({ "file": file })).await
    }
}

/// 各 axum 处理器与 [`OneBotServer`] 共享的运行时状态。
struct Shared {
    access_token: String,
    connection_slot: Mutex<Option<Arc<OneBotConnection>>>,
    queue_tx: Mutex<Option<EventSender>>,
    connections: AtomicU64,
    group_messages: AtomicU64,
    /// stop 信号：serve 的 graceful shutdown 与每条 WS 的读取循环都监听它。
    shutdown: watch::Sender<bool>,
}

/// 事件队列发送端：从 WS 读取循环送入消费者任务。
type EventSender = mpsc::UnboundedSender<(Arc<OneBotConnection>, GroupMessage)>;

impl Shared {
    fn lock_connection(&self) -> MutexGuard<'_, Option<Arc<OneBotConnection>>> {
        self.connection_slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_queue(&self) -> MutexGuard<'_, Option<EventSender>> {
        self.queue_tx.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 最后一条仍连接的连接（Python `connection` 属性：存在且 connected 才返回）。
    fn live_connection(&self) -> Option<Arc<OneBotConnection>> {
        self.lock_connection()
            .clone()
            .filter(|conn| conn.connected())
    }
}

#[derive(Default)]
struct ServerInner {
    started: bool,
    local_addr: Option<SocketAddr>,
    consumer: Option<JoinHandle<()>>,
    serve: Option<JoinHandle<()>>,
}

/// OneBot 反向 WS 服务端。
pub struct OneBotServer {
    host: String,
    port: u16,
    path: String,
    on_group_message: GroupMessageHandler,
    shared: Arc<Shared>,
    inner: Mutex<ServerInner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerStats {
    pub connections: u64,
    pub group_messages: u64,
}

impl OneBotServer {
    pub fn new(
        host: impl Into<String>,
        port: u16,
        path: impl Into<String>,
        access_token: impl Into<String>,
        on_group_message: GroupMessageHandler,
    ) -> Self {
        let mut path = path.into();
        if !path.starts_with('/') {
            path.insert(0, '/'); // Python: path = "/" + path
        }
        let (shutdown, _) = watch::channel(false);
        Self {
            host: host.into(),
            port,
            path,
            on_group_message,
            shared: Arc::new(Shared {
                access_token: access_token.into(),
                connection_slot: Mutex::new(None),
                queue_tx: Mutex::new(None),
                connections: AtomicU64::new(0),
                group_messages: AtomicU64::new(0),
                shutdown,
            }),
            inner: Mutex::new(ServerInner::default()),
        }
    }

    pub fn stats(&self) -> ServerStats {
        ServerStats {
            connections: self.shared.connections.load(Ordering::Relaxed),
            group_messages: self.shared.group_messages.load(Ordering::Relaxed),
        }
    }

    /// 绑定监听、挂路由（GET {path} WS + GET /healthz）、启动消费者任务。
    pub async fn start(&self) -> std::io::Result<()> {
        {
            // started 标志在任何 await 之前落定，杜绝并发重复启动
            let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            if inner.started {
                return Err(std::io::Error::other("OneBot 服务已启动"));
            }
            inner.started = true;
        }

        let app = Router::new()
            .route("/healthz", get(healthz))
            .route(self.path.as_str(), get(ws_handler))
            .with_state(self.shared.clone());

        let listener = TcpListener::bind((self.host.as_str(), self.port)).await?;
        let local_addr = listener.local_addr()?;

        let (queue_tx, mut queue_rx) = mpsc::unbounded_channel();
        *self.shared.lock_queue() = Some(queue_tx);

        // 事件消费者：顺序处理队列。事件必须在读取循环之外处理——处理器经常
        // 会通过同一条 WS 调用动作（发回复等），若在读取循环里 await 处理器，
        // echo 响应就永远读不到，会一直卡到动作超时。处理器失败（panic）只记
        // 日志，绝不杀掉循环。
        let handler = self.on_group_message.clone();
        let consumer = tokio::spawn(async move {
            while let Some((conn, message)) = queue_rx.recv().await {
                let describe = message.describe();
                let handler = AssertUnwindSafe(handler(conn, message));
                if handler.catch_unwind().await.is_err() {
                    tracing::error!("处理群消息失败: {describe}");
                }
            }
        });

        let mut shutdown = self.shared.shutdown.subscribe();
        let serve = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown.changed().await;
                })
                .await;
        });

        {
            let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            inner.local_addr = Some(local_addr);
            inner.consumer = Some(consumer);
            inner.serve = Some(serve);
        }

        tracing::info!(
            "OneBot 反向 WS 服务已启动: ws://{}:{}{}",
            self.host,
            self.port,
            self.path
        );
        Ok(())
    }

    /// 取消消费者、优雅关闭 axum 服务、清空连接槽位。
    pub async fn stop(&self) {
        let (consumer, serve) = {
            let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            (inner.consumer.take(), inner.serve.take())
        };
        if let Some(consumer) = consumer {
            consumer.abort();
            let _ = consumer.await; // 对齐 Python: cancel + 抑制 CancelledError
        }
        // 优雅停机：停止接受新连接；各 WS 读取循环监听同一信号，收到后退出
        let _ = self.shared.shutdown.send(true);
        if let Some(serve) = serve {
            // 连接都已收到 shutdown 信号，正常应立即结束；兜底不无限等待
            let _ = timeout(Duration::from_secs(3), serve).await;
        }
        *self.shared.lock_connection() = None;
    }

    /// 最后一条仍连接的连接。
    pub fn connection(&self) -> Option<Arc<OneBotConnection>> {
        self.shared.live_connection()
    }

    /// 每 0.5s 轮询，直到出现可用连接或超时（超时时做最后一次检查，对齐 Python）。
    pub async fn wait_connection(&self, timeout: Duration) -> Option<Arc<OneBotConnection>> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(conn) = self.connection() {
                return Some(conn);
            }
            sleep(Duration::from_millis(500)).await;
        }
        self.connection()
    }

    /// port=0 时 start() 后可通过它拿真实端口（测试需要）。
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .local_addr
    }
}

// --- axum 处理器 ---

async fn healthz(State(shared): State<Arc<Shared>>) -> Json<Value> {
    let conn = shared.live_connection();
    Json(json!({
        "status": "ok",
        "onebot_connected": conn.is_some(),
        "self_id": conn.as_ref().map_or(0, |conn| conn.self_id()),
        "stats": {
            "connections": shared.connections.load(Ordering::Relaxed),
            "group_messages": shared.group_messages.load(Ordering::Relaxed),
        },
    }))
}

async fn ws_handler(
    State(shared): State<Arc<Shared>>,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    if !authorized(&shared.access_token, &headers, query.as_deref()) {
        tracing::warn!("OneBot 连接被拒绝：token 校验失败");
        // 401 必须在 WS 升级之前返回
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    ws.max_message_size(MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| handle_socket(shared, socket))
        .into_response()
}

/// 单条 WS 连接的生命周期：注册连接 → 读取循环（解析 / 入队）→ 收尾清理。
async fn handle_socket(shared: Arc<Shared>, socket: WebSocket) {
    let (outbound, mut outbound_rx) = mpsc::unbounded_channel();
    let conn = Arc::new(OneBotConnection::new(outbound));
    // Python: self._connection = conn（直接覆盖旧连接）
    *shared.lock_connection() = Some(conn.clone());
    let count = shared.connections.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::info!("OneBot 客户端已连接（第 {count} 次）");

    // 写任务独占 sink；动作调用与心跳都经由通道进入这里，避免读写竞争。
    let (mut sink, mut stream) = socket.split();
    let writer = tokio::spawn(async move {
        let mut pinger = interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
        loop {
            tokio::select! {
                _ = pinger.tick() => {
                    if sink.send(Message::Ping(Bytes::new())).await.is_err() {
                        break;
                    }
                }
                outgoing = outbound_rx.recv() => {
                    match outgoing {
                        Some(message) => {
                            if sink.send(message).await.is_err() {
                                break;
                            }
                        }
                        None => break, // 连接对象已丢弃 → 写任务退出
                    }
                }
            }
        }
    });

    let mut shutdown = shared.shutdown.subscribe();
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => handle_text(&shared, &conn, &text),
                    // TEXT 之外的帧忽略；Close / 错误 / 流结束 → 连接结束
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }

    conn.closed.store(true, Ordering::Relaxed);
    // 等待中的动作调用不再有响应：显式送回 Closed（对齐 spec：ws 已关闭 → Closed）
    for (_, sender) in conn.lock_pending().drain() {
        let _ = sender.send(Err(OneBotError::Closed));
    }
    writer.abort();

    // Python: if self._connection is conn: self._connection = None
    let mut slot = shared.lock_connection();
    if slot.as_ref().is_some_and(|current| Arc::ptr_eq(current, &conn)) {
        *slot = None;
    }
    drop(slot);
    tracing::warn!("OneBot 客户端已断开");
}

/// 读取循环内的事件分发：动作响应 → meta 事件 → 群消息入队。容忍垃圾数据。
fn handle_text(shared: &Shared, conn: &Arc<OneBotConnection>, text: &str) {
    // JSON 解析失败：跳过（不杀连接）
    let Ok(payload) = serde_json::from_str::<Value>(text) else {
        return;
    };
    if !payload.is_object() {
        return;
    }
    // 动作响应优先于事件分类
    if conn.feed(&payload) {
        return;
    }
    if payload.get("post_type").and_then(Value::as_str) == Some("meta_event") {
        if payload.get("meta_event_type").and_then(Value::as_str) == Some("lifecycle") {
            conn.self_id
                .store(json_int(payload.get("self_id")), Ordering::Relaxed);
        }
        return;
    }
    let Some(message) = parse_group_message(&payload) else {
        return;
    };
    shared.group_messages.fetch_add(1, Ordering::Relaxed);
    if let Some(queue) = shared.lock_queue().as_ref() {
        // 无界队列，send 实际不会失败；即便失败也只是丢弃，不影响读取循环
        let _ = queue.send((conn.clone(), message));
    }
}

// --- token 校验 ---

/// 无 token 配置 → 放行；否则 `Authorization: Bearer <token>` 精确匹配，
/// 或 `?access_token=` 查询参数匹配。
fn authorized(token: &str, headers: &HeaderMap, query: Option<&str>) -> bool {
    if token.is_empty() {
        return true;
    }
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        if let Some(bearer) = value.strip_prefix("Bearer ") {
            if bearer == token {
                return true;
            }
        }
    }
    query
        .and_then(|query| query_param(query, "access_token"))
        .is_some_and(|value| value == token)
}

/// 取查询字符串里的某个参数（对齐 aiohttp `request.query`：%XX 解码 + '+'→空格）。
fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = match pair.split_once('=') {
            Some((name, value)) => (name, value),
            None => (pair, ""),
        };
        if decode_component(name) == key {
            Some(decode_component(value))
        } else {
            None
        }
    })
}

/// 最小 percent 解码：%XX 与 '+'（对齐 parse_qsl 行为）。
fn decode_component(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = |byte: u8| (byte as char).to_digit(16).map(|digit| digit as u8);
                if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                    out.push(high * 16 + low);
                    index += 3;
                } else {
                    out.push(b'%');
                    index += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_group_message() -> Value {
        json!({
            "post_type": "message",
            "message_type": "group",
            "self_id": 10000,
            "group_id": 123456789,
            "user_id": 10001,
            "message_id": 1234567890,
            "sender": { "user_id": 10001, "nickname": "玩家A", "card": "" },
            "message": [
                { "type": "reply", "data": { "id": "987654321" } },
                { "type": "text", "data": { "text": "看看这个 " } },
                { "type": "image", "data": { "file": "abc.png", "url": "https://multimedia.nt.qq.com.cn/x" } },
                { "type": "at", "data": { "qq": "10000" } },
            ],
        })
    }

    #[test]
    fn parse_segments_tolerates_shapes() {
        // 字符串形式（CQ 码兜底）：整段当文本
        let plain = parse_segments(&json!("纯文本"));
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].kind, "text");
        assert_eq!(plain[0].text(), "纯文本");

        assert!(parse_segments(&json!("")).is_empty());
        assert!(parse_segments(&Value::Null).is_empty());
        assert!(parse_segments(&json!(12345)).is_empty());

        let segments = parse_segments(&json!([{ "type": "text", "data": { "text": "hi" } }]));
        assert_eq!(segments[0].text(), "hi");

        // 垃圾条目容忍：非 dict / 缺 type / data=null
        let segments = parse_segments(&json!([
            { "no_type": 1 },
            "junk",
            { "type": "face", "data": null },
        ]));
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].kind, "face");
        assert!(segments[0]
            .data
            .as_object()
            .is_some_and(|data| data.is_empty()));

        // data 非 dict → 空对象；正常 data 原样保留
        let segments = parse_segments(&json!([
            { "type": "text", "data": "oops" },
            { "type": "at", "data": { "qq": 5 } },
        ]));
        assert_eq!(segments[0].data, json!({}));
        assert_eq!(segments[1].data, json!({ "qq": 5 }));
    }

    #[test]
    fn parse_group_message_fields() {
        let msg = parse_group_message(&raw_group_message()).unwrap();
        assert_eq!(msg.group_id, 123456789);
        assert_eq!(msg.user_id, 10001);
        assert_eq!(msg.message_id, 1234567890);
        assert_eq!(msg.display_name(), "玩家A");
        assert_eq!(msg.text(), "看看这个"); // 拼接后 strip
        assert_eq!(msg.reply_message_id().as_deref(), Some("987654321"));
        let files: Vec<String> = msg.images().iter().map(Segment::file).collect();
        assert_eq!(files, ["abc.png"]);
        assert_eq!(msg.images()[0].url(), "https://multimedia.nt.qq.com.cn/x");
        assert_eq!(msg.at_user_ids(), [10000]);
        assert_eq!(
            msg.describe(),
            "群=123456789 用户=玩家A(10001) 消息ID=1234567890"
        );
    }

    #[test]
    fn card_takes_precedence_over_nickname() {
        let mut raw = raw_group_message();
        raw["sender"] = json!({ "nickname": "玩家A", "card": "[1.21]PlayerCard" });
        let msg = parse_group_message(&raw).unwrap();
        assert_eq!(msg.display_name(), "[1.21]PlayerCard");
    }

    #[test]
    fn display_name_falls_back_to_user_id() {
        let mut raw = raw_group_message();
        raw["sender"] = json!({});
        let msg = parse_group_message(&raw).unwrap();
        assert_eq!(msg.display_name(), "10001");
    }

    #[test]
    fn non_group_events_are_ignored() {
        assert!(parse_group_message(&json!({ "post_type": "message", "message_type": "private" }))
            .is_none());
        assert!(parse_group_message(&json!({ "post_type": "notice", "notice_type": "group_recall" }))
            .is_none());
        assert!(parse_group_message(
            &json!({ "post_type": "meta_event", "meta_event_type": "lifecycle" })
        )
        .is_none());
    }

    #[test]
    fn blank_and_at_only_messages() {
        let mut raw = raw_group_message();
        raw["message"] = json!([{ "type": "at", "data": { "qq": "10001" } }]);
        let msg = parse_group_message(&raw).unwrap();
        assert_eq!(msg.text(), "");
        assert!(msg.images().is_empty());
        assert_eq!(msg.at_user_ids(), [10001]);
        assert!(msg.reply_message_id().is_none());
    }

    #[test]
    fn at_user_ids_rejects_non_digits() {
        let mut raw = raw_group_message();
        raw["message"] = json!([
            { "type": "at", "data": { "qq": -1 } },
            { "type": "at", "data": { "qq": "abc" } },
            { "type": "at", "data": { "qq": null } },
            { "type": "at", "data": { "qq": 12.5 } },
            { "type": "at", "data": { "qq": 20002 } },
        ]);
        let msg = parse_group_message(&raw).unwrap();
        assert_eq!(msg.at_user_ids(), [20002]);
    }

    // ---------- feed / call ----------

    async fn wait_pending(conn: &OneBotConnection) {
        for _ in 0..200 {
            if !conn.lock_pending().is_empty() {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("call 应先注册 pending");
    }

    #[tokio::test]
    async fn feed_routes_ok_response_to_data() {
        let (outbound, _rx) = mpsc::unbounded_channel();
        let conn = Arc::new(OneBotConnection::new(outbound));

        let task_conn = conn.clone();
        let call = tokio::spawn(async move { task_conn.call("get_status", json!({})).await });
        wait_pending(&conn).await;
        // echo 同时兼容数字与字符串
        assert!(conn.feed(&json!({
            "status": "ok", "retcode": 0,
            "data": { "detail": "pong" }, "echo": "1",
        })));
        let data = call.await.unwrap().unwrap();
        assert_eq!(data, json!({ "detail": "pong" }));
        // 调用完成后 pending 已清理
        assert!(conn.lock_pending().is_empty());
    }

    #[tokio::test]
    async fn feed_maps_failure_to_failed_error() {
        let (outbound, _rx) = mpsc::unbounded_channel();
        let conn = Arc::new(OneBotConnection::new(outbound));

        let task_conn = conn.clone();
        let call = tokio::spawn(async move {
            task_conn
                .call("send_group_text", json!({}))
                .await
        });
        wait_pending(&conn).await;
        assert!(conn.feed(&json!({
            "status": "failed", "retcode": 1200,
            "message": "words too long", "wording": "应该用 wording", "echo": 1,
        })));
        let err = call.await.unwrap().unwrap_err();
        assert!(matches!(err, OneBotError::Failed { retcode: 1200, .. }));
        // detail 取第一个真值：message 优先于 wording
        assert_eq!(err.to_string(), "OneBot 动作失败 retcode=1200 words too long");

        // wording 兜底
        let task_conn = conn.clone();
        let call = tokio::spawn(async move { task_conn.call("x", json!({})).await });
        wait_pending(&conn).await;
        assert!(conn.feed(&json!({
            "status": "failed", "retcode": 7, "wording": "fallback", "echo": 2,
        })));
        let err = call.await.unwrap().unwrap_err();
        assert_eq!(err.to_string(), "OneBot 动作失败 retcode=7 fallback");
    }

    #[tokio::test]
    async fn feed_ignores_unknown_or_missing_echo() {
        let (outbound, _rx) = mpsc::unbounded_channel();
        let conn = OneBotConnection::new(outbound);

        assert!(!conn.feed(&json!({ "status": "ok", "retcode": 0, "data": {} })));
        assert!(!conn.feed(&json!({ "status": "ok", "retcode": 0, "echo": 999 })));
        assert!(!conn.feed(&json!({ "status": "ok", "retcode": 0, "echo": null })));
        assert!(!conn.feed(&json!("junk")));
    }

    #[tokio::test]
    async fn closed_connection_call_fails_fast() {
        let (outbound, _rx) = mpsc::unbounded_channel();
        let conn = OneBotConnection::new(outbound);
        conn.closed.store(true, Ordering::Relaxed);
        let err = conn.call("get_status", json!({})).await.unwrap_err();
        assert_eq!(err.to_string(), "OneBot 连接已关闭");
    }
}
