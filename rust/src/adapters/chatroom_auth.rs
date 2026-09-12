//! chatroom 用户侧鉴权：refresh_token -> access_token（读方向用）。
//!
//! 写方向走官方 Forward Bot API（Bearer 静态 token），不需要这里；
//! 读方向（`!q`、chatroom→游戏）仍需要普通用户 JWT。
//!
//! 对齐 Python 版 `chatroom_bridge/chatroom_auth.py`：
//! - [`decode_jwt_payload`] 解析 JWT payload（不校验签名，只取 exp / user id）；
//! - [`ChatroomAuth`] 维护 access_token，并把轮换出的 refresh_token 持久化到
//!   [`StateStore`](crate::state::StateStore)；
//! - `ensure_token` 在内存 token 失效时回退到配置里的初始值再试一次（swap-and-retry，
//!   仍失败则恢复现场）。

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde_json::{json, Value};
use tracing::{error, info, warn};

use crate::state::StateStore;

pub const REFRESH_ENDPOINT: &str = "/api/auth/refresh";

/// 提前 5 分钟刷新（Python `REFRESH_MARGIN`）。
const REFRESH_MARGIN_SECS: i64 = 300;

/// 当前 Unix 秒（Python `time.time()` 的整数近似，余量 300s 下误差可忽略）。
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 按字符数截断（对齐 Python `body[:n]`）。
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Python `str(data.get(key) or "")`：缺失/null/空串/0 → ""；数值按字符串转换；
/// 布尔与容器一律按空处理。
fn json_str_field(data: &Value, key: &str) -> String {
    match data.get(key) {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(Value::Number(n)) if n.as_f64() != Some(0.0) => n.to_string(),
        _ => String::new(),
    }
}

/// Python：按 `user_id`/`uid`/`id`/`sub` 顺序取第一个能当整数用的值；
/// 整数直接用（浮点不算 int，跳过），纯数字字符串转 int，其余跳过。
fn extract_user_id(payload: &Value) -> Option<i64> {
    for key in ["user_id", "uid", "id", "sub"] {
        match payload.get(key) {
            Some(Value::Number(n)) => {
                if let Some(v) = n.as_i64() {
                    return Some(v);
                }
            }
            Some(Value::String(s)) => {
                // Python str.isdigit()：空串为 False，负号/正号也不是数字
                let parsed = if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
                    s.parse::<i64>().ok()
                } else {
                    None
                };
                if let Some(v) = parsed {
                    return Some(v);
                }
            }
            _ => {}
        }
    }
    None
}

/// 解析 JWT payload（不校验签名，只取 exp / user id）。
///
/// 容错：任何失败（段数不足、base64 非法、JSON 非法）都返回 `{}`。
/// 复刻 Python `urlsafe_b64decode`：先把 `-_` 翻译回 `+/`，补齐 `=` 到 4 的倍数，
/// 再按标准字母表解码。
pub fn decode_jwt_payload(token: &str) -> Value {
    // Python: token.split(".")[1] —— 只要求至少两段
    let payload_b64 = match token.split('.').nth(1) {
        Some(p) => p,
        None => return json!({}),
    };
    let normalized: String = payload_b64
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    // Python: payload_b64 += "=" * (-len(payload_b64) % 4)
    let padding = (4 - normalized.len() % 4) % 4;
    let padded = format!("{normalized}{}", "=".repeat(padding));
    let decoded = match base64::engine::general_purpose::STANDARD.decode(padded.as_bytes()) {
        Ok(decoded) => decoded,
        Err(_) => return json!({}),
    };
    serde_json::from_slice(&decoded).unwrap_or_else(|_| json!({}))
}

/// 内存中的鉴权状态（Python 的各 `_xxx` 实例属性）。
struct AuthState {
    /// 配置文件里的初始 refresh_token（回退用）。
    config_refresh_token: String,
    /// 当前生效的 refresh_token：store 持久化值优先于配置值；轮换后指向新值。
    refresh_token: String,
    access_token: String,
    /// access_token 过期时刻（Unix 秒）；0 表示尚无 token。
    expires_at: i64,
    user_id: Option<i64>,
}

/// 维护 access_token，并把轮换后的 refresh_token 持久化。
pub struct ChatroomAuth {
    base_url: String,
    client: reqwest::Client,
    store: Option<Arc<StateStore>>,
    inner: Mutex<AuthState>,
}

impl ChatroomAuth {
    /// 读超时 total 15s（对齐 Python `aiohttp.ClientTimeout(total=15)`）。
    ///
    /// `refresh_token` 传配置文件里的初始值；`store` 提供时其持久化的
    /// refresh_token（若非空）优先于配置值。
    pub fn new(
        base_url: impl Into<String>,
        refresh_token: impl Into<String>,
        store: Option<Arc<StateStore>>,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?;
        let config_refresh_token = refresh_token.into();
        let persisted = store.as_ref().map(|s| s.refresh_token()).unwrap_or_default();
        // Python: self._refresh_token = store.refresh_token if store and store.refresh_token else refresh_token
        let effective = if persisted.is_empty() {
            config_refresh_token.clone()
        } else {
            persisted
        };
        Ok(Self {
            // Python: base_url.rstrip("/")
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
            store,
            inner: Mutex::new(AuthState {
                config_refresh_token,
                refresh_token: effective,
                access_token: String::new(),
                expires_at: 0,
                user_id: None,
            }),
        })
    }

    /// 锁中毒时恢复内部值，不让 panicking 的调用方卡死读方向。
    fn lock(&self) -> MutexGuard<'_, AuthState> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 当前 access_token（可能为空串：尚未刷新成功）。
    pub fn access_token(&self) -> String {
        self.lock().access_token.clone()
    }

    /// bot 自己的 chatroom user id，用于过滤自身消息。
    pub fn user_id(&self) -> Option<i64> {
        self.lock().user_id
    }

    /// 有效 refresh token 非空（store 持久化值优先于配置值，二者任一非空即可）。
    pub fn has_refresh_token(&self) -> bool {
        let state = self.lock();
        !state.refresh_token.is_empty() || !state.config_refresh_token.is_empty()
    }

    /// 缓存未过期（余量 ≥ 300s）→ true；否则 refresh()；
    /// 内存 refresh_token 失效时，回退到配置里的初始值再试一次（仍失败恢复现场）。
    pub async fn ensure_token(&self) -> bool {
        {
            let state = self.lock();
            if !state.access_token.is_empty()
                && unix_now() < state.expires_at - REFRESH_MARGIN_SECS
            {
                return true;
            }
        }
        if self.refresh().await {
            return true;
        }
        let (config, current) = {
            let state = self.lock();
            (state.config_refresh_token.clone(), state.refresh_token.clone())
        };
        if !config.is_empty() && current != config {
            warn!("refresh_token 刷新失败，回退到配置文件中的初始值");
            let previous = {
                let mut state = self.lock();
                std::mem::replace(&mut state.refresh_token, config)
            };
            if self.refresh().await {
                return true;
            }
            // 仍失败：恢复现场
            let mut state = self.lock();
            state.refresh_token = previous;
        }
        false
    }

    /// POST `/api/auth/refresh` 换新 access_token。
    ///
    /// 200 → 缓存 access/exp/user_id，轮换出的 refresh_token 持久化到 store；
    /// 其余情况一律返回 false（reqwest 错误不外泄，只记日志）。
    pub async fn refresh(&self) -> bool {
        // Python: token = self._refresh_token or self._config_refresh_token
        let token = {
            let state = self.lock();
            if state.refresh_token.is_empty() {
                state.config_refresh_token.clone()
            } else {
                state.refresh_token.clone()
            }
        };
        if token.is_empty() {
            warn!("没有 refresh_token，读方向不可用（!q / chatroom→游戏）");
            return false;
        }

        let response = match self
            .client
            .post(format!("{}{}", self.base_url, REFRESH_ENDPOINT))
            .json(&json!({ "refresh_token": token }))
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                error!("refresh_token 刷新异常: {err}");
                return false;
            }
        };

        let status = response.status().as_u16();
        let body = match response.text().await {
            Ok(body) => body,
            Err(err) => {
                error!("refresh_token 刷新响应读取失败: {err}");
                return false;
            }
        };

        if status != 200 {
            let snippet = truncate_chars(&body, 200);
            error!("refresh_token 刷新失败 HTTP {status}: {snippet}");
            if body.to_lowercase().contains("invalid refresh token") {
                error!(
                    "refresh_token 已失效：请重新登录 chatroom 后在 Local Storage 取 refresh_token 更新到配置"
                );
            }
            return false;
        }

        // Python: resp.json(content_type=None) —— 不校验 Content-Type
        let data: Value = match serde_json::from_str(&body) {
            Ok(data) => data,
            Err(_) => {
                error!("refresh_token 刷新返回 200 但响应不是 JSON");
                return false;
            }
        };

        let access = json_str_field(&data, "access_token");
        if access.is_empty() {
            error!("刷新返回 200 但没有 access_token");
            return false;
        }
        let new_refresh = json_str_field(&data, "refresh_token");

        {
            let mut state = self.lock();
            state.access_token = access.clone();
            if !new_refresh.is_empty() {
                state.refresh_token = new_refresh.clone();
            }

            // Python: float(exp) if isinstance(exp, (int, float)) else time.time() + 1800
            let payload = decode_jwt_payload(&access);
            state.expires_at = match payload.get("exp") {
                Some(Value::Number(n)) => n
                    .as_f64()
                    .map(|f| f as i64)
                    .unwrap_or_else(|| unix_now() + 1800),
                _ => unix_now() + 1800,
            };
            if let Some(user_id) = extract_user_id(&payload) {
                state.user_id = Some(user_id);
            }
            info!("access_token 已刷新（user_id={:?}）", state.user_id);
        }

        if !new_refresh.is_empty() {
            if let Some(store) = &self.store {
                store.set_refresh_token(new_refresh);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode, Uri};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::{Json, Router};
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::json;
    use std::collections::VecDeque;

    // ---------- JWT 构造 ----------

    fn b64url(data: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(data)
    }

    fn make_jwt_payload(payload: &Value) -> String {
        format!(
            "{}.{}.{}",
            b64url(br#"{"alg":"HS256","typ":"JWT"}"#),
            b64url(payload.to_string().as_bytes()),
            "signature"
        )
    }

    fn make_jwt(exp: i64, user_id: i64) -> String {
        make_jwt_payload(&json!({ "exp": exp, "user_id": user_id }))
    }

    // ---------- axum mock：记录收到的 refresh_token，按队列回放响应 ----------

    enum Reply {
        Json(Value),
        Raw(u16, String),
    }

    #[derive(Clone)]
    struct MockState {
        tokens: Arc<Mutex<Vec<String>>>,
        replies: Arc<Mutex<VecDeque<Reply>>>,
    }

    impl MockState {
        fn push_json(&self, value: Value) {
            self.replies.lock().unwrap().push_back(Reply::Json(value));
        }

        fn push_raw(&self, code: u16, text: &str) {
            self.replies
                .lock()
                .unwrap()
                .push_back(Reply::Raw(code, text.to_string()));
        }

        fn tokens(&self) -> Vec<String> {
            self.tokens.lock().unwrap().clone()
        }
    }

    async fn refresh_handler(
        State(state): State<MockState>,
        _headers: HeaderMap,
        _uri: Uri,
        body: Bytes,
    ) -> Response {
        let token = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|v| {
                v.get("refresh_token")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default();
        state.tokens.lock().unwrap().push(token);
        let reply = state
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Reply::Raw(500, String::new()));
        match reply {
            Reply::Json(value) => Json(value).into_response(),
            Reply::Raw(code, text) => (StatusCode::from_u16(code).unwrap(), text).into_response(),
        }
    }

    async fn spawn_mock() -> (String, MockState) {
        let state = MockState {
            tokens: Arc::new(Mutex::new(Vec::new())),
            replies: Arc::new(Mutex::new(VecDeque::new())),
        };
        let app = Router::new()
            .route(REFRESH_ENDPOINT, post(refresh_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), state)
    }

    fn make_auth(base_url: &str, token: &str, store: Option<Arc<StateStore>>) -> ChatroomAuth {
        ChatroomAuth::new(base_url.to_string(), token.to_string(), store).unwrap()
    }

    fn make_store() -> (tempfile::TempDir, Arc<StateStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.json")));
        (dir, store)
    }

    // ---------- decode_jwt_payload ----------

    #[test]
    fn decode_jwt_payload_parses_valid_token() {
        let payload = decode_jwt_payload(&make_jwt(1_700_000_000, 42));
        assert_eq!(payload["exp"], 1_700_000_000);
        assert_eq!(payload["user_id"], 42);

        // 无 padding 的 base64url（真实 JWT 形态）
        let manual = format!("aaa.{}.bbb", b64url(br#"{"exp":1}"#));
        assert_eq!(decode_jwt_payload(&manual)["exp"], 1);
    }

    #[test]
    fn decode_jwt_payload_tolerates_garbage() {
        assert_eq!(decode_jwt_payload(""), json!({}));
        assert_eq!(decode_jwt_payload("not-a-jwt"), json!({}));
        assert_eq!(decode_jwt_payload("a."), json!({}));
        assert_eq!(decode_jwt_payload("a.b"), json!({}));
        assert_eq!(decode_jwt_payload("a.@@@@.c"), json!({}));
        assert_eq!(decode_jwt_payload("a.b.c.d"), json!({}));
        // payload 是合法 JSON 但不是对象 → 与 Python json.loads 一致，原样返回
        let numeric = format!("x.{}", b64url(b"123"));
        assert_eq!(decode_jwt_payload(&numeric), json!(123));
    }

    // ---------- user_id 提取 ----------

    #[test]
    fn user_id_key_priority_matches_python() {
        assert_eq!(
            extract_user_id(&json!({"user_id": 1, "uid": 2, "id": 3, "sub": "4"})),
            Some(1)
        );
        assert_eq!(extract_user_id(&json!({"uid": 2, "id": 3, "sub": "4"})), Some(2));
        assert_eq!(extract_user_id(&json!({"id": 3, "sub": "4"})), Some(3));
        assert_eq!(extract_user_id(&json!({"sub": "4"})), Some(4));
        // 纯数字字符串
        assert_eq!(extract_user_id(&json!({"user_id": "12"})), Some(12));
        // 浮点不是 int（Python isinstance(5.5, int) == False）→ 跳到下一个 key
        assert_eq!(extract_user_id(&json!({"user_id": 5.5, "sub": "6"})), Some(6));
        // 非数字字符串 / null → 跳过
        assert_eq!(
            extract_user_id(&json!({"user_id": "abc", "uid": null, "id": 9})),
            Some(9)
        );
        // 0 是合法 int
        assert_eq!(extract_user_id(&json!({"uid": 0})), Some(0));
        // 全部缺失
        assert_eq!(extract_user_id(&json!({"exp": 1})), None);
        assert_eq!(extract_user_id(&json!({})), None);
    }

    // ---------- 轮换与持久化 ----------

    #[tokio::test]
    async fn refresh_rotates_and_persists_to_store() {
        let (base, mock) = spawn_mock().await;
        let (_dir, store) = make_store();
        let auth = make_auth(&base, "rt-config", Some(store.clone()));

        assert!(auth.has_refresh_token());
        assert_eq!(auth.access_token(), "");
        assert_eq!(auth.user_id(), None);

        let access = make_jwt(unix_now() + 3600, 42);
        mock.push_json(json!({
            "access_token": access.clone(),
            "refresh_token": "rt-rotated",
        }));
        assert!(auth.ensure_token().await);
        assert_eq!(auth.access_token(), access);
        assert_eq!(auth.user_id(), Some(42));
        assert_eq!(store.refresh_token(), "rt-rotated");
        assert_eq!(mock.tokens(), vec!["rt-config".to_string()]);

        // 余量充足（≥300s）：第二次 ensure 不发请求
        assert!(auth.ensure_token().await);
        assert_eq!(mock.tokens().len(), 1);
    }

    #[tokio::test]
    async fn near_expiry_triggers_refresh() {
        let (base, mock) = spawn_mock().await;
        let auth = make_auth(&base, "rt-1", None);

        // 余量 < 300s 的 token 也要刷新
        let short_lived = make_jwt(unix_now() + 200, 7);
        mock.push_json(json!({
            "access_token": short_lived.clone(),
            "refresh_token": "rt-2",
        }));
        assert!(auth.ensure_token().await);

        // 过期在即 → 再次刷新；新 token 余量充足后停住
        let long_lived = make_jwt(unix_now() + 3600, 7);
        // 第二个响应不带 refresh_token → 不轮换、不覆盖内存值
        mock.push_json(json!({ "access_token": long_lived.clone() }));
        assert!(auth.ensure_token().await);
        assert_eq!(
            mock.tokens(),
            vec!["rt-1".to_string(), "rt-2".to_string()]
        );
        assert_eq!(auth.access_token(), long_lived);
        assert_eq!(auth.user_id(), Some(7));

        assert!(auth.ensure_token().await);
        assert_eq!(mock.tokens().len(), 2);
    }

    // ---------- 非 200 / 异常 ----------

    #[tokio::test]
    async fn refresh_non_200_returns_false() {
        let (base, mock) = spawn_mock().await;
        let auth = make_auth(&base, "rt-x", None);

        mock.push_raw(401, "Invalid Refresh Token");
        assert!(!auth.ensure_token().await);
        assert_eq!(auth.access_token(), "");
        // 内存值 == 配置值 → 不触发回退重试，只有一个请求
        assert_eq!(mock.tokens().len(), 1);
        assert!(auth.has_refresh_token());

        // 与 Python 一致：刷新从未成功过 → 内存值仍等于配置值 → 不触发回退重试
        // （第二个 ensure 同样只发一个请求，共 2 个；真正的回退路径见 fallback 用例）
        mock.push_raw(500, "boom");
        assert!(!auth.ensure_token().await);
        assert_eq!(mock.tokens().len(), 2);
    }

    #[tokio::test]
    async fn missing_refresh_token_fails_without_http() {
        let (base, mock) = spawn_mock().await;
        let auth = make_auth(&base, "", None);
        assert!(!auth.has_refresh_token());
        assert!(!auth.ensure_token().await);
        assert_eq!(mock.tokens().len(), 0);
        assert_eq!(auth.user_id(), None);
    }

    // ---------- exp 缺失回退 now+1800 ----------

    #[tokio::test]
    async fn refresh_without_exp_falls_back_to_30_minutes() {
        let (base, mock) = spawn_mock().await;
        let (_dir, store) = make_store();
        let auth = make_auth(&base, "rt", Some(store.clone()));

        // 无 exp、无轮换、user_id 用 uid 字符串
        let access = make_jwt_payload(&json!({"uid": "77"}));
        mock.push_json(json!({ "access_token": access.clone() }));
        assert!(auth.ensure_token().await);
        assert_eq!(auth.access_token(), access);
        assert_eq!(auth.user_id(), Some(77));
        assert_eq!(store.refresh_token(), ""); // 无轮换 → 不写 store

        // exp 缺失 → now+1800，余量充足：第二次不发请求
        assert!(auth.ensure_token().await);
        assert_eq!(mock.tokens().len(), 1);
    }

    // ---------- swap-and-retry 回退 ----------

    #[tokio::test]
    async fn ensure_token_falls_back_to_original_config_token() {
        let (base, mock) = spawn_mock().await;
        let (_dir, store) = make_store();
        let auth = make_auth(&base, "rt-original", Some(store.clone()));

        // 第一次刷新成功：轮换出 rt-rotated（余量故意不足，方便触发第二次）
        let first_access = make_jwt(unix_now() + 100, 1);
        mock.push_json(json!({
            "access_token": first_access.clone(),
            "refresh_token": "rt-rotated",
        }));
        assert!(auth.ensure_token().await);
        assert_eq!(store.refresh_token(), "rt-rotated");

        // 轮换出的 token 已被服务端吊销；用配置里的原始 token 再试成功
        let second_access = make_jwt(unix_now() + 3600, 2);
        mock.push_raw(401, "invalid refresh token");
        mock.push_json(json!({
            "access_token": second_access.clone(),
            "refresh_token": "rt-final",
        }));
        assert!(auth.ensure_token().await);
        assert_eq!(
            mock.tokens(),
            vec![
                "rt-original".to_string(),
                "rt-rotated".to_string(),
                "rt-original".to_string(),
            ]
        );
        assert_eq!(auth.access_token(), second_access);
        assert_eq!(auth.user_id(), Some(2));
        // 重试成功后按响应轮换持久化
        assert_eq!(store.refresh_token(), "rt-final");

        // 新 token 余量充足 → 不再请求
        assert!(auth.ensure_token().await);
        assert_eq!(mock.tokens().len(), 3);
    }

    #[tokio::test]
    async fn ensure_token_total_failure_keeps_state_consistent() {
        let (base, mock) = spawn_mock().await;
        let auth = make_auth(&base, "rt-original", None);

        let first_access = make_jwt(unix_now() + 100, 1);
        mock.push_json(json!({
            "access_token": first_access.clone(),
            "refresh_token": "rt-rotated",
        }));
        assert!(auth.ensure_token().await);

        // 轮换 token 与回退 token 都被拒 → false，现场恢复
        mock.push_raw(401, "invalid refresh token");
        mock.push_raw(403, "denied");
        assert!(!auth.ensure_token().await);
        assert_eq!(
            mock.tokens(),
            vec![
                "rt-original".to_string(),
                "rt-rotated".to_string(),
                "rt-original".to_string(),
            ]
        );
        // access_token 保留旧值，has_refresh_token 仍为 true
        assert_eq!(auth.access_token(), first_access);
        assert!(auth.has_refresh_token());
    }

    // ---------- store 持久化值优先 ----------

    #[tokio::test]
    async fn store_persisted_token_takes_precedence() {
        let (base, mock) = spawn_mock().await;
        let (_dir, store) = make_store();
        store.set_refresh_token("rt-stored".to_string());

        let auth = make_auth(&base, "rt-config", Some(store));
        assert!(auth.has_refresh_token());
        let access = make_jwt(unix_now() + 3600, 5);
        mock.push_json(json!({
            "access_token": access.clone(),
            "refresh_token": "rt-next",
        }));
        assert!(auth.ensure_token().await);
        // 发出去的是 store 里的持久化值，而不是配置值
        assert_eq!(mock.tokens(), vec!["rt-stored".to_string()]);
    }

    // ---------- 响应形状容忍 ----------

    #[tokio::test]
    async fn refresh_rejects_200_without_access_token() {
        let (base, mock) = spawn_mock().await;
        let auth = make_auth(&base, "rt", None);
        mock.push_json(json!({"refresh_token": "rt-next"}));
        assert!(!auth.refresh().await);
        assert_eq!(auth.access_token(), "");
        // access_token 为空/数字 0 → Python `or ""` 语义按空处理
        mock.push_json(json!({"access_token": 0}));
        assert!(!auth.refresh().await);
        // 200 但响应不是 JSON → false
        mock.push_raw(200, "not json");
        assert!(!auth.refresh().await);
        assert_eq!(mock.tokens().len(), 3);
    }
}
