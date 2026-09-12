//! 读方向：轮询 chatroom 频道消息，供 `!q` 与 chatroom→游戏使用。
//!
//! 对齐 Python 版 `chatroom_bridge/chatroom_read.py`：
//! - [`extract_qq_forward`] 解析 `!q` 前缀消息；
//! - [`ChatroomReader`] 轮询频道消息并维护读游标（游标持久化在 state.json）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tracing::{info, warn};

use crate::state::StateStore;

pub const MESSAGES_ENDPOINT: &str = "/api/channels/{channel_id}/messages";
pub const QQ_FORWARD_PREFIX: &str = "!q";

/// `!q xxx` -> `xxx`；不是 !q 消息返回 None。
///
/// 整个前缀大小写不敏感（`!Q` 同样命中）；返回去掉前缀并 trim 的载荷；
/// 裸 `!q`（空载荷）返回 None。与 Python 版逐行为对齐。
pub fn extract_qq_forward(content: &str) -> Option<String> {
    let stripped = content.trim();
    // Python: stripped.lower().startswith("!q")。Unicode 中仅 'Q' 会小写成 'q'（'!' 无大小写），
    // 因此对前两个字节做 ASCII 不敏感比较与 Python 行为完全等价。
    let prefix = QQ_FORWARD_PREFIX.as_bytes();
    if stripped.len() < prefix.len()
        || !stripped.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix)
    {
        return None;
    }
    // 前缀两个字节均为 ASCII，字节下标 2 一定是字符边界。
    let payload = stripped[prefix.len()..].trim();
    if payload.is_empty() {
        None // Python: return payload or None
    } else {
        Some(payload.to_string())
    }
}

/// 读方向的鉴权依赖抽象（测试用 Fake 实现即可注入；
/// 收尾阶段会为 crate::adapters::chatroom_auth::ChatroomAuth 补一个桥接实现）。
#[async_trait]
pub trait AuthTokenProvider: Send + Sync {
    /// 确保 access_token 可用（必要时刷新）；false 表示当前无法拉取。
    async fn ensure_token(&self) -> bool;
    fn access_token(&self) -> String;
    fn user_id(&self) -> Option<i64>;
}

/// 轮询频道消息并维护读游标（游标持久化在 state.json）。
pub struct ChatroomReader {
    base_url: String,
    channel_id: i64,
    auth: Arc<dyn AuthTokenProvider>,
    store: Option<Arc<StateStore>>,
    client: reqwest::Client,
    cursor: Mutex<i64>,
    /// 首次 poll 是否已完成（Python `_initialized`）。
    initialized: AtomicBool,
    fetch_limit: usize,
    /// Python `skip_backlog_on_start`；Rust 构造 API 未暴露，恒为 true。
    skip_backlog: bool,
}

/// Python `int(m.get("id") or 0)`：缺失/非数值按 0 处理；
/// 数字字符串与浮点按 Python `int()` 语义解析/取整（无法解析按 0）。
fn message_id(message: &Value) -> i64 {
    match message.get("id") {
        None | Some(Value::Null) => 0,
        Some(Value::Number(n)) => n
            .as_i64()
            .unwrap_or_else(|| n.as_f64().map_or(0, |f| f.trunc() as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
        Some(Value::Bool(b)) => i64::from(*b), // Python: int(True) == 1
        _ => 0,
    }
}

impl ChatroomReader {
    /// 读超时 total 15s / connect 5s（对齐 Python aiohttp.ClientTimeout(total=15, sock_connect=5)）；
    /// 游标从 store 播种。
    pub fn new(
        base_url: impl Into<String>,
        channel_id: i64,
        auth: Arc<dyn AuthTokenProvider>,
        store: Option<Arc<StateStore>>,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        let seeded = store.as_ref().map(|s| s.last_read_message_id()).unwrap_or(0);
        Ok(Self {
            // Python: base_url.rstrip("/")
            base_url: base_url.into().trim_end_matches('/').to_string(),
            channel_id,
            auth,
            store,
            client,
            cursor: Mutex::new(seeded),
            initialized: AtomicBool::new(false),
            fetch_limit: 10,
            skip_backlog: true,
        })
    }

    /// 默认 10。
    pub fn with_fetch_limit(mut self, limit: usize) -> Self {
        self.fetch_limit = limit;
        self
    }

    pub fn cursor(&self) -> i64 {
        *self.lock_cursor()
    }

    /// 锁中毒时恢复内部值，不让 panicking 的调用方卡死读方向。
    fn lock_cursor(&self) -> MutexGuard<'_, i64> {
        self.cursor.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn messages_url(&self) -> String {
        format!(
            "{}{}",
            self.base_url,
            MESSAGES_ENDPOINT.replace("{channel_id}", &self.channel_id.to_string())
        )
    }

    /// Python `_advance`：只前进不后退；前进时经 store 持久化。
    fn advance(&self, message_id: i64) {
        {
            let mut cursor = self.lock_cursor();
            if message_id <= *cursor {
                return;
            }
            *cursor = message_id;
        }
        if let Some(store) = &self.store {
            store.set_last_read_message_id(message_id);
        }
    }

    /// 拉取最近消息（API 一般按新→旧返回）。
    ///
    /// ensure_token() 为 false → None；非 200 或网络错误 → None；
    /// 响应体为 JSON 数组，或对象里 data/messages/items 之一是数组；其他形状 → Some(vec![])。
    pub async fn fetch_messages(&self) -> Option<Vec<Value>> {
        if !self.auth.ensure_token().await {
            return None;
        }
        let url = self.messages_url();
        let response = self
            .client
            .get(url)
            .header(
                "Authorization",
                format!("Bearer {}", self.auth.access_token()),
            )
            .query(&[("limit", self.fetch_limit)])
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(err) => {
                warn!("读取频道消息异常: {err}");
                return None;
            }
        };
        let status = response.status();
        if status != reqwest::StatusCode::OK {
            warn!("读取频道消息失败 HTTP {}", status.as_u16());
            return None;
        }
        // Python: resp.json(content_type=None) —— 不校验 Content-Type；解析失败按拉取失败处理。
        let data: Value = match response.json().await {
            Ok(data) => data,
            Err(err) => {
                warn!("读取频道消息解析失败: {err}");
                return None;
            }
        };

        Some(match data {
            Value::Array(items) => items,
            Value::Object(map) => {
                for key in ["data", "messages", "items"] {
                    if let Some(Value::Array(items)) = map.get(key) {
                        return Some(items.clone());
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        })
    }

    /// 返回本次新消息（按旧→新排序）。首次调用只初始化游标（跳过积压）。
    pub async fn poll_once(&self) -> Vec<Value> {
        let messages = match self.fetch_messages().await {
            Some(messages) => messages,
            None => {
                warn!("读取频道消息失败，跳过本轮轮询");
                return Vec::new();
            }
        };

        if !self.initialized.swap(true, Ordering::SeqCst) && self.skip_backlog {
            let newest = messages.iter().map(message_id).max().unwrap_or(0);
            self.advance(newest);
            info!("读方向初始化完成，游标={}（历史消息不重放）", self.cursor());
            return Vec::new();
        }

        let cursor = self.cursor();
        let mut fresh: Vec<Value> = messages
            .into_iter()
            .filter(|message| message_id(message) > cursor)
            .collect();
        fresh.sort_by_key(message_id);
        for message in &fresh {
            self.advance(message_id(message));
        }
        fresh
    }

    /// bot 自己的消息（user_id 等于 auth.user_id()）。
    pub fn is_own_message(&self, message: &Value) -> bool {
        let Some(bot_id) = self.auth.user_id() else {
            return false;
        };
        match message.get("user_id") {
            // 数值比较（浮点 999.0 与整数 999 在 Python 中相等，这里保持一致）。
            Some(Value::Number(n)) => {
                n.as_i64() == Some(bot_id) || n.as_f64() == Some(bot_id as f64)
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::{StatusCode, Uri};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    // ---------- FakeAuth（对齐 Python 测试的 FakeAuth） ----------

    struct FakeAuth {
        token: String,
        user_id: Option<i64>,
        ensure_ok: bool,
        ensure_calls: AtomicUsize,
    }

    impl FakeAuth {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }
    }

    impl Default for FakeAuth {
        fn default() -> Self {
            Self {
                token: "jwt".to_string(),
                user_id: Some(999),
                ensure_ok: true,
                ensure_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl AuthTokenProvider for FakeAuth {
        async fn ensure_token(&self) -> bool {
            self.ensure_calls.fetch_add(1, Ordering::SeqCst);
            self.ensure_ok
        }

        fn access_token(&self) -> String {
            self.token.clone()
        }

        fn user_id(&self) -> Option<i64> {
            self.user_id
        }
    }

    // ---------- axum mock：队列式批量响应，每个 GET 弹出一条 ----------

    enum MockReply {
        Json(Value),
        Status(u16),
    }

    #[derive(Clone, Default)]
    struct MockServer {
        replies: Arc<Mutex<VecDeque<MockReply>>>,
        queries: Arc<Mutex<Vec<String>>>,
    }

    impl MockServer {
        fn push(&self, value: Value) {
            self.replies
                .lock()
                .unwrap()
                .push_back(MockReply::Json(value));
        }

        fn push_status(&self, code: u16) {
            self.replies.lock().unwrap().push_back(MockReply::Status(code));
        }

        fn request_count(&self) -> usize {
            self.queries.lock().unwrap().len()
        }

        fn last_query(&self) -> Option<String> {
            self.queries.lock().unwrap().last().cloned()
        }
    }

    async fn messages_handler(State(state): State<MockServer>, uri: Uri) -> Response {
        state
            .queries
            .lock()
            .unwrap()
            .push(uri.query().unwrap_or("").to_string());
        let reply = state
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| MockReply::Json(Value::Array(vec![])));
        match reply {
            MockReply::Json(value) => Json(value).into_response(),
            MockReply::Status(code) => {
                StatusCode::from_u16(code).unwrap().into_response()
            }
        }
    }

    async fn spawn_mock() -> (String, MockServer) {
        let state = MockServer::default();
        let app = Router::new()
            .route(MESSAGES_ENDPOINT, get(messages_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), state)
    }

    fn make_reader(
        base_url: &str,
        auth: Arc<FakeAuth>,
        store: Option<Arc<StateStore>>,
    ) -> ChatroomReader {
        ChatroomReader::new(base_url.to_string(), 1, auth, store).unwrap()
    }

    fn ids_of(messages: &[Value]) -> Vec<i64> {
        messages.iter().map(message_id).collect()
    }

    // ---------- extract_qq_forward ----------

    #[test]
    fn extract_qq_forward_matches_python_semantics() {
        assert_eq!(extract_qq_forward("!q hello").as_deref(), Some("hello"));
        assert_eq!(extract_qq_forward("!Q  x ").as_deref(), Some("x"));
        assert_eq!(
            extract_qq_forward("  !q   多余空格  ").as_deref(),
            Some("多余空格")
        );
        // 裸 `!q`（含仅有空白）→ None
        assert_eq!(extract_qq_forward("!q"), None);
        assert_eq!(extract_qq_forward("  !q  "), None);
        // 非前缀 → None
        assert_eq!(extract_qq_forward("普通消息"), None);
        assert_eq!(extract_qq_forward("qq 没有感叹号"), None);
        assert_eq!(extract_qq_forward(""), None);
        // Python 语义：仅前缀判断，`!qx` 的载荷是 "x"
        assert_eq!(extract_qq_forward("!qx").as_deref(), Some("x"));
    }

    // ---------- 首轮跳过积压 ----------

    #[tokio::test]
    async fn first_poll_skips_backlog_and_advances_cursor() {
        let (base_url, mock) = spawn_mock().await;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.json")));

        mock.push(json!([
            {"id": 30, "content": "新", "user_id": 1, "username": "甲"},
            {"id": 20, "content": "旧", "user_id": 1, "username": "甲"},
        ]));
        let reader = make_reader(&base_url, FakeAuth::new(), Some(store.clone()));

        assert!(reader.poll_once().await.is_empty());
        assert_eq!(reader.cursor(), 30);
        assert_eq!(store.last_read_message_id(), 30);
        assert_eq!(mock.request_count(), 1);
        assert_eq!(mock.last_query().as_deref(), Some("limit=10"));

        // 下一轮只返回游标之后的消息，并按旧→新排序
        mock.push(json!([
            {"id": 31, "content": "b", "user_id": 2, "username": "乙"},
            {"id": 33, "content": "c", "user_id": 2, "username": "乙"},
            {"id": 30, "content": "新", "user_id": 1, "username": "甲"},
        ]));
        let fresh = reader.poll_once().await;
        assert_eq!(ids_of(&fresh), vec![31, 33]);
        assert_eq!(reader.cursor(), 33);
    }

    // ---------- 增量轮询 ----------

    #[tokio::test]
    async fn poll_once_returns_incremental_messages_sorted() {
        let (base_url, mock) = spawn_mock().await;

        // 首批积压 id 1..5（乱序给也应无碍）
        mock.push(json!([
            {"id": 3, "content": "m3"},
            {"id": 1, "content": "m1"},
            {"id": 5, "content": "m5"},
            {"id": 2, "content": "m2"},
            {"id": 4, "content": "m4"},
        ]));
        // 第二批乱序返回 [7, 6]
        mock.push(json!([
            {"id": 7, "content": "newer"},
            {"id": 6, "content": "new"},
        ]));
        let reader = make_reader(&base_url, FakeAuth::new(), None);

        // 首轮仅初始化游标
        assert!(reader.poll_once().await.is_empty());
        assert_eq!(reader.cursor(), 5);

        let fresh = reader.poll_once().await;
        assert_eq!(ids_of(&fresh), vec![6, 7]); // 旧→新
        assert_eq!(reader.cursor(), 7);
    }

    // ---------- 游标持久化（重启后不再重放） ----------

    #[tokio::test]
    async fn cursor_persists_across_restart() {
        let (base_url, mock) = spawn_mock().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        let store = Arc::new(StateStore::new(&path));
        mock.push(json!([{"id": 42, "content": "x", "user_id": 1}]));
        let reader = make_reader(&base_url, FakeAuth::new(), Some(store.clone()));
        assert!(reader.poll_once().await.is_empty());
        assert_eq!(reader.cursor(), 42);

        // 新 StateStore + 新 reader：游标从 store 播种
        let store2 = Arc::new(StateStore::new(&path));
        assert_eq!(store2.last_read_message_id(), 42);

        mock.push(json!([{"id": 42, "content": "x", "user_id": 1}]));
        let reader2 = make_reader(&base_url, FakeAuth::new(), Some(store2.clone()));
        assert_eq!(reader2.cursor(), 42);
        // 旧消息不重放（不再派发）
        assert!(reader2.poll_once().await.is_empty());
        assert_eq!(reader2.cursor(), 42);
        assert_eq!(mock.request_count(), 2);
    }

    // ---------- bot 自身消息识别 ----------

    #[tokio::test]
    async fn own_message_detection() {
        let (base_url, _mock) = spawn_mock().await;

        let reader = make_reader(
            &base_url,
            Arc::new(FakeAuth {
                user_id: Some(999),
                ..FakeAuth::default()
            }),
            None,
        );
        assert!(reader.is_own_message(&json!({"user_id": 999})));
        assert!(!reader.is_own_message(&json!({"user_id": 1000})));
        // 缺少 user_id 字段
        assert!(!reader.is_own_message(&json!({"content": "no user"})));

        // 未配置 bot id → 一律 false
        let anonymous = make_reader(
            &base_url,
            Arc::new(FakeAuth {
                user_id: None,
                ..FakeAuth::default()
            }),
            None,
        );
        assert!(!anonymous.is_own_message(&json!({"user_id": 999})));
    }

    // ---------- 拉取失败 ----------

    #[tokio::test]
    async fn fetch_failure_returns_empty_and_keeps_cursor() {
        let (base_url, mock) = spawn_mock().await;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.json")));
        store.set_last_read_message_id(100);

        mock.push_status(500);
        mock.push_status(500);
        let reader = make_reader(&base_url, FakeAuth::new(), Some(store.clone()));
        assert_eq!(reader.cursor(), 100);

        assert!(reader.poll_once().await.is_empty());
        assert_eq!(reader.cursor(), 100);
        assert_eq!(mock.request_count(), 1);
        // fetch_messages 本身对非 200 也返回 None
        assert_eq!(reader.fetch_messages().await, None);
        assert_eq!(mock.request_count(), 2);
    }

    #[tokio::test]
    async fn ensure_token_false_short_circuits_before_http() {
        let (base_url, mock) = spawn_mock().await;
        let auth = Arc::new(FakeAuth {
            ensure_ok: false,
            ..FakeAuth::default()
        });
        let reader = make_reader(&base_url, auth.clone(), None);

        assert_eq!(reader.fetch_messages().await, None);
        assert_eq!(mock.request_count(), 0);

        assert!(reader.poll_once().await.is_empty());
        assert_eq!(mock.request_count(), 0);
        assert_eq!(auth.ensure_calls.load(Ordering::SeqCst), 2);
    }

    // ---------- 响应形状容忍 ----------

    #[tokio::test]
    async fn fetch_messages_tolerates_response_shapes() {
        let (base_url, mock) = spawn_mock().await;
        let reader = make_reader(&base_url, FakeAuth::new(), None);

        // 裸数组
        mock.push(json!([{"id": 1}, {"id": 2}]));
        assert_eq!(reader.fetch_messages().await.unwrap().len(), 2);

        // {"data": [...]}
        mock.push(json!({"data": [{"id": 3}]}));
        assert_eq!(reader.fetch_messages().await.unwrap().len(), 1);

        // {"messages": [...]}
        mock.push(json!({"messages": [{"id": 4}, {"id": 5}]}));
        assert_eq!(reader.fetch_messages().await.unwrap().len(), 2);

        // {"items": [...]}
        mock.push(json!({"items": [{"id": 6}]}));
        assert_eq!(reader.fetch_messages().await.unwrap().len(), 1);

        // 对象里没有任何数组字段 → 空
        mock.push(json!({"junk": "不是数组", "total": 0}));
        assert_eq!(reader.fetch_messages().await, Some(Vec::new()));

        // 既非数组也非对象 → 空
        mock.push(json!("既不是数组也不是对象"));
        assert_eq!(reader.fetch_messages().await, Some(Vec::new()));
    }

    // ---------- fetch_limit 出现在查询串 ----------

    #[tokio::test]
    async fn fetch_limit_visible_in_query() {
        let (base_url, mock) = spawn_mock().await;
        mock.push(json!([]));
        let reader = make_reader(&base_url, FakeAuth::new(), None).with_fetch_limit(3);
        reader.fetch_messages().await;
        assert_eq!(mock.last_query().as_deref(), Some("limit=3"));
    }
}
