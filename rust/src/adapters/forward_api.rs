//! chatroom 官方 Forward Bot API 客户端（写方向，Bearer token）。
//!
//! 文档: backend-go/docs/forward-bot-api.md
//!   POST /api/forward/channels/{id}/upload   -> 200 `{id, filename, content_type, size, url}`
//!   POST /api/forward/channels/{id}/messages -> 201 完整消息对象
//!
//! 对齐 Python 版 `chatroom_bridge/forward_api.py`：
//! - [`is_blocked`] / [`guess_content_type`] 与 Python 的黑名单 / MIME 表逐项一致；
//! - [`ForwardApi`] 校验顺序、端点路径、multipart 字段名、状态码期望与错误文案一致；
//!   token 只放在请求头，不写日志，也不进入任何错误信息。
//!
//! 服务端不去重：同一 source_message_id 重复提交会产生新消息，去重由调用方负责。
//!
//! 与 Python 的两处类型层差异（语义等价）：
//! - Python 的 `source in ("qq", "game")` 运行时校验由 [`PostSource`] 枚举在编译期收窄；
//! - Python 原样透传非法 `content_type`，这里 reqwest 要求 MIME 可解析，
//!   解析失败会在发起 HTTP 前快速报错（`guess_content_type` 的结果恒合法，正常调用不受影响）。

use std::time::Duration;

use serde_json::{json, Value};

use crate::error::ForwardApiError;

/// 附件上限：10MB。
pub const MAX_FILE_SIZE: usize = 10 * 1024 * 1024;

/// 被禁止上传的可执行扩展名（与 Python `BLOCKED_EXTENSIONS` 一字不差；比较时小写化）。
const BLOCKED_EXTENSIONS: &[&str] = &[
    ".exe", ".bat", ".cmd", ".com", ".cpl", ".dll", ".scr", ".msi", ".jar", //
    ".sh", ".bash", ".ps1", ".vbs", ".js", ".wsf", ".apk", ".app", ".deb", ".rpm",
];

/// 已知扩展名 -> Content-Type（与 Python `_MIME_BY_EXT` 一字不差）。
const MIME_BY_EXT: &[(&str, &str)] = &[
    (".png", "image/png"),
    (".jpg", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".gif", "image/gif"),
    (".webp", "image/webp"),
    (".bmp", "image/bmp"),
    (".mp4", "video/mp4"),
    (".webm", "video/webm"),
    (".mov", "video/quicktime"),
    (".mp3", "audio/mpeg"),
    (".wav", "audio/wav"),
    (".ogg", "audio/ogg"),
    (".pdf", "application/pdf"),
    (".txt", "text/plain"),
    (".zip", "application/zip"),
];

const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

/// `os.path.splitext(filename)[1]` 的等价实现：
/// 取 basename 最后一个 `.` 起的扩展名（含点，大小写保留）；
/// 前导点不算扩展名（如 `.bashrc`、`.png`、`.tar.gz` 之外的 `.a.b` 正常取 `.b`）。
fn split_ext(filename: &str) -> &str {
    let basename = filename.rsplit(&['/', '\\'][..]).next().unwrap_or(filename);
    let Some(dot) = basename.rfind('.') else {
        return "";
    };
    // Python 通用 splitext：basename 起始到最后一个点之间全是点 → 视为无扩展名
    if basename[..dot].chars().all(|c| c == '.') {
        return "";
    }
    &basename[dot..]
}

/// 文件扩展名是否在上传黑名单（大小写不敏感）。
pub fn is_blocked(filename: &str) -> bool {
    BLOCKED_EXTENSIONS.contains(&split_ext(filename).to_ascii_lowercase().as_str())
}

/// 按扩展名猜测 Content-Type；未知扩展名回落 `application/octet-stream`。
pub fn guess_content_type(filename: &str) -> &'static str {
    let ext = split_ext(filename).to_ascii_lowercase();
    MIME_BY_EXT
        .iter()
        .find(|(known, _)| *known == ext)
        .map_or(DEFAULT_CONTENT_TYPE, |(_, mime)| *mime)
}

/// 按字符数截断（对齐 Python `body[:n]`，避免把多字节字符切成乱码）。
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// 消息来源。Python 里是 `"qq" | "game"` 字符串，这里用枚举在编译期收窄。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostSource {
    QQ,
    Game,
}

impl PostSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QQ => "qq",
            Self::Game => "game",
        }
    }
}

/// 待转发消息（对应 Python `post_message` 的关键字参数）。
#[derive(Debug, Clone)]
pub struct PostMessage {
    pub source: PostSource,
    pub content: String,
    pub source_message_id: String,
    pub sender_qq: Option<i64>,
    pub sender_username: String,
    pub nickname: String,
    pub reply_source_message_id: String,
    pub reply_nickname: String,
    pub reply_content: String,
    pub attachment_ids: Vec<i64>,
}

impl PostMessage {
    /// 全空消息：字符串为空、集合为空。
    pub fn new(source: PostSource) -> Self {
        Self {
            source,
            content: String::new(),
            source_message_id: String::new(),
            sender_qq: None,
            sender_username: String::new(),
            nickname: String::new(),
            reply_source_message_id: String::new(),
            reply_nickname: String::new(),
            reply_content: String::new(),
            attachment_ids: Vec::new(),
        }
    }
}

/// 官方 forward 接口封装。token 只放在请求头，不写日志。
pub struct ForwardApi {
    base_url: String,
    token: String,
    channel_id: i64,
    client: reqwest::Client,
}

impl ForwardApi {
    /// Client timeout total 30s / connect 5s（对齐 Python
    /// `aiohttp.ClientTimeout(total=30, sock_connect=5)`）。
    pub fn new(
        base_url: impl Into<String>,
        token: impl Into<String>,
        channel_id: i64,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self {
            // Python: base_url.rstrip("/")
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            channel_id,
            client,
        })
    }

    /// token 非空且 channel_id > 0。
    pub fn configured(&self) -> bool {
        !self.token.is_empty() && self.channel_id > 0
    }

    fn url(&self, suffix: &str) -> String {
        format!(
            "{}/api/forward/channels/{}{}",
            self.base_url, self.channel_id, suffix
        )
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// 上传附件，返回 attachment id（≤10MB，可执行扩展名会被服务端拒绝）。
    pub async fn upload(
        &self,
        data: Vec<u8>,
        filename: &str,
        content_type: Option<&str>,
    ) -> Result<i64, ForwardApiError> {
        // 与 Python 相同的快速失败顺序：配置 → 非空 → 大小 → 扩展名。
        if !self.configured() {
            return Err(ForwardApiError::new("forward token / channel_id 未配置", None, ""));
        }
        if data.is_empty() {
            return Err(ForwardApiError::new("附件为空", None, ""));
        }
        if data.len() > MAX_FILE_SIZE {
            return Err(ForwardApiError::new(
                format!("附件超过 10MB 上限（{} 字节）", data.len()),
                None,
                "",
            ));
        }
        if is_blocked(filename) {
            // Python 这里展示的是未小写化的原始扩展名
            return Err(ForwardApiError::new(
                format!("被禁止的扩展名: {}", split_ext(filename)),
                None,
                "",
            ));
        }

        // Python: content_type or guess_content_type(filename)（空串同样回落）
        let content_type = content_type
            .filter(|ct| !ct.is_empty())
            .unwrap_or_else(|| guess_content_type(filename));
        let part = reqwest::multipart::Part::bytes(data)
            .file_name(filename.to_string())
            .mime_str(content_type)
            .map_err(|err| {
                ForwardApiError::new(format!("无效的 content_type: {err}"), None, "")
            })?;
        let form = reqwest::multipart::Form::new().part("file", part);

        let response = self
            .client
            .post(self.url("/upload"))
            .header("Authorization", self.auth_header())
            .multipart(form)
            .send()
            .await
            .map_err(|err| {
                ForwardApiError::new(
                    format!("上传请求失败: {err}"),
                    err.status().map(|s| s.as_u16()),
                    "",
                )
            })?;

        let status = response.status().as_u16();
        let body = response.text().await.map_err(|err| {
            ForwardApiError::new(format!("上传响应读取失败: {err}"), Some(status), "")
        })?;
        if status != 200 {
            return Err(ForwardApiError::new(
                format!("上传失败 HTTP {status}"),
                Some(status),
                truncate_chars(&body, 300),
            ));
        }
        // Python: resp.json(content_type=None) —— 不校验 Content-Type
        let payload: Value = serde_json::from_str(&body).map_err(|_| {
            ForwardApiError::new(
                format!("上传响应不是 JSON: {}", truncate_chars(&body, 200)),
                Some(status),
                "",
            )
        })?;

        let Some(attachment_id) = payload.get("id").and_then(Value::as_i64) else {
            return Err(ForwardApiError::new(
                format!("上传响应缺少 id: {payload}"),
                Some(status),
                "",
            ));
        };
        Ok(attachment_id)
    }

    /// 转发一条消息；content 与 attachment_ids 至少要有一个非空。
    pub async fn post_message(&self, message: &PostMessage) -> Result<Value, ForwardApiError> {
        let attachments = message.attachment_ids.clone();
        if message.content.trim().is_empty() && attachments.is_empty() {
            return Err(ForwardApiError::new(
                "content 与 attachment_ids 不能同时为空",
                None,
                "",
            ));
        }
        if !self.configured() {
            return Err(ForwardApiError::new("forward token / channel_id 未配置", None, ""));
        }

        // sender 只带出现的键；Python `if sender_qq:` 的 0 是 falsy → 不写入
        let mut sender = serde_json::Map::new();
        if let Some(qq) = message.sender_qq.filter(|qq| *qq != 0) {
            sender.insert("qq".to_string(), json!(qq));
        }
        if !message.sender_username.is_empty() {
            sender.insert("username".to_string(), json!(message.sender_username));
        }
        if !message.nickname.is_empty() {
            sender.insert("nickname".to_string(), json!(message.nickname));
        }

        let mut payload = json!({
            "source": message.source.as_str(),
            "sender": Value::Object(sender),
            "content": message.content,
            "source_message_id": message.source_message_id,
            "attachment_ids": attachments,
        });
        if !message.reply_source_message_id.is_empty() {
            payload["reply"] = json!({
                "source_message_id": message.reply_source_message_id,
                "nickname": message.reply_nickname,
                "content": message.reply_content,
            });
        }

        let response = self
            .client
            .post(self.url("/messages"))
            .header("Authorization", self.auth_header())
            .json(&payload)
            .send()
            .await
            .map_err(|err| {
                ForwardApiError::new(
                    format!("转发请求失败: {err}"),
                    err.status().map(|s| s.as_u16()),
                    "",
                )
            })?;

        let status = response.status().as_u16();
        let body = response.text().await.map_err(|err| {
            ForwardApiError::new(format!("转发响应读取失败: {err}"), Some(status), "")
        })?;
        if status != 201 {
            return Err(ForwardApiError::new(
                format!("转发失败 HTTP {status}"),
                Some(status),
                truncate_chars(&body, 300),
            ));
        }
        // Python: resp.json(content_type=None) —— 不校验 Content-Type
        serde_json::from_str(&body).map_err(|_| {
            ForwardApiError::new(
                format!("转发响应不是 JSON: {}", truncate_chars(&body, 200)),
                Some(status),
                "",
            )
        })
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
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    const TOKEN: &str = "tok";
    const CHANNEL: i64 = 1;

    // ---------- axum mock：统一捕获请求，按队列回放响应 ----------

    struct Captured {
        path: String,
        authorization: Option<String>,
        content_type: Option<String>,
        body: Vec<u8>,
        json: Option<Value>,
    }

    enum Reply {
        Json(u16, Value),
        Raw(u16, String),
    }

    #[derive(Clone)]
    struct MockState {
        requests: Arc<Mutex<Vec<Captured>>>,
        replies: Arc<Mutex<VecDeque<Reply>>>,
    }

    impl MockState {
        fn push_json(&self, code: u16, value: Value) {
            self.replies.lock().unwrap().push_back(Reply::Json(code, value));
        }

        fn push_raw(&self, code: u16, text: &str) {
            self.replies
                .lock()
                .unwrap()
                .push_back(Reply::Raw(code, text.to_string()));
        }

        fn count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    async fn capture_handler(
        State(state): State<MockState>,
        headers: HeaderMap,
        uri: Uri,
        body: Bytes,
    ) -> Response {
        state.requests.lock().unwrap().push(Captured {
            path: uri.path().to_string(),
            authorization: headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            content_type: headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            body: body.to_vec(),
            json: serde_json::from_slice::<Value>(&body).ok(),
        });
        let reply = state
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Reply::Raw(500, String::new()));
        match reply {
            Reply::Json(code, value) => {
                (StatusCode::from_u16(code).unwrap(), Json(value)).into_response()
            }
            Reply::Raw(code, text) => (StatusCode::from_u16(code).unwrap(), text).into_response(),
        }
    }

    async fn spawn_mock() -> (String, MockState) {
        let state = MockState {
            requests: Arc::new(Mutex::new(Vec::new())),
            replies: Arc::new(Mutex::new(VecDeque::new())),
        };
        let app = Router::new()
            .route(
                &format!("/api/forward/channels/{CHANNEL}/upload"),
                post(capture_handler),
            )
            .route(
                &format!("/api/forward/channels/{CHANNEL}/messages"),
                post(capture_handler),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), state)
    }

    fn make_api(base_url: &str) -> ForwardApi {
        ForwardApi::new(base_url.to_string(), TOKEN, CHANNEL).unwrap()
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
    }

    // ---------- post_message ----------

    #[tokio::test]
    async fn post_message_payload_shape() {
        let (base, mock) = spawn_mock().await;
        mock.push_json(201, json!({"id": 1}));
        let api = make_api(&base);

        let mut message = PostMessage::new(PostSource::QQ);
        message.content = "你好".to_string();
        message.source_message_id = "1234567890".to_string();
        message.sender_qq = Some(10001);
        message.nickname = "玩家A".to_string();
        message.reply_source_message_id = "987654321".to_string();
        message.reply_nickname = "玩家A".to_string();
        message.reply_content = "引用内容".to_string();

        let result = api.post_message(&message).await.unwrap();
        assert_eq!(result, json!({"id": 1}));

        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let req = &requests[0];
        assert_eq!(req.path, "/api/forward/channels/1/messages");
        assert_eq!(req.authorization.as_deref(), Some("Bearer tok"));
        assert_eq!(req.content_type.as_deref(), Some("application/json"));
        let body = req.json.as_ref().unwrap();
        assert_eq!(body["source"], "qq");
        // sender 只带出现的键（sender_username 为空 → 无 username）
        assert_eq!(body["sender"], json!({"qq": 10001, "nickname": "玩家A"}));
        assert_eq!(body["content"], "你好");
        assert_eq!(body["source_message_id"], "1234567890");
        assert_eq!(body["reply"]["source_message_id"], "987654321");
        assert_eq!(body["reply"]["nickname"], "玩家A");
        assert_eq!(body["reply"]["content"], "引用内容");
        assert_eq!(body["attachment_ids"], json!([]));
    }

    #[tokio::test]
    async fn post_message_sender_and_reply_omission_rules() {
        let (base, mock) = spawn_mock().await;
        mock.push_json(201, json!({"id": 1}));
        let api = make_api(&base);

        // Python `if sender_qq:`：0 是 falsy → 不写入 sender；无回复 → 无 reply 对象
        let mut message = PostMessage::new(PostSource::QQ);
        message.content = "x".to_string();
        message.sender_qq = Some(0);
        api.post_message(&message).await.unwrap();

        // content 为空但带附件 → 放行；game 来源原样上送
        let mut with_attachments = PostMessage::new(PostSource::Game);
        with_attachments.source_message_id = "42".to_string();
        with_attachments.attachment_ids = vec![5, 7];
        mock.push_json(201, json!({"id": 2}));
        api.post_message(&with_attachments).await.unwrap();

        let requests = mock.requests.lock().unwrap();
        let first = requests[0].json.as_ref().unwrap();
        assert_eq!(first["sender"], json!({}));
        assert!(first.get("reply").is_none());
        let second = requests[1].json.as_ref().unwrap();
        assert_eq!(second["source"], "game");
        assert_eq!(second["attachment_ids"], json!([5, 7]));
        assert_eq!(second["source_message_id"], "42");
    }

    #[tokio::test]
    async fn post_message_requires_content_or_attachment() {
        let (base, mock) = spawn_mock().await;
        let api = make_api(&base);

        let mut blank = PostMessage::new(PostSource::QQ);
        blank.content = "   ".to_string();
        let err = api.post_message(&blank).await.unwrap_err();
        assert_eq!(err.message, "content 与 attachment_ids 不能同时为空");
        assert_eq!(err.status, None);

        assert!(api.post_message(&PostMessage::new(PostSource::QQ)).await.is_err());
        assert_eq!(mock.count(), 0); // 未发任何 HTTP 请求
    }

    #[tokio::test]
    async fn post_message_surfaces_401_without_leaking_token() {
        let (base, mock) = spawn_mock().await;
        mock.push_raw(401, r#"{"error":"unauthorized"}"#);
        let api = make_api(&base);

        let mut message = PostMessage::new(PostSource::QQ);
        message.content = "x".to_string();
        message.source_message_id = "1".to_string();
        let err = api.post_message(&message).await.unwrap_err();

        assert_eq!(err.status, Some(401));
        assert_eq!(err.message, "转发失败 HTTP 401");
        assert!(err.body.contains("unauthorized"));
        // token 绝不出现在 message / body / Debug 里
        assert!(!err.message.contains(TOKEN));
        assert!(!err.body.contains(TOKEN));
        assert!(!format!("{err:?}").contains(TOKEN));
    }

    #[tokio::test]
    async fn post_message_rejects_non_201_with_body() {
        let (base, mock) = spawn_mock().await;
        mock.push_raw(500, "boom");
        let api = make_api(&base);
        let mut message = PostMessage::new(PostSource::QQ);
        message.content = "x".to_string();
        let err = api.post_message(&message).await.unwrap_err();
        assert_eq!(err.status, Some(500));
        assert_eq!(err.message, "转发失败 HTTP 500");
        assert_eq!(err.body, "boom");
    }

    #[tokio::test]
    async fn post_message_rejects_non_json_201() {
        let (base, mock) = spawn_mock().await;
        mock.push_raw(201, "不是 JSON");
        let api = make_api(&base);
        let mut message = PostMessage::new(PostSource::QQ);
        message.content = "x".to_string();
        let err = api.post_message(&message).await.unwrap_err();
        assert_eq!(err.status, Some(201));
        assert!(err.message.contains("转发响应不是 JSON"));
    }

    #[tokio::test]
    async fn unconfigured_fails_fast_without_http() {
        let (base, mock) = spawn_mock().await;
        let api = ForwardApi::new(base.as_str(), "", CHANNEL).unwrap();
        assert!(!api.configured());
        let mut message = PostMessage::new(PostSource::QQ);
        message.content = "x".to_string();
        let err = api.post_message(&message).await.unwrap_err();
        assert_eq!(err.message, "forward token / channel_id 未配置");
        assert_eq!(err.status, None);

        let upload_err = api.upload(b"x".to_vec(), "a.png", None).await.unwrap_err();
        assert_eq!(upload_err.message, "forward token / channel_id 未配置");

        // channel_id 非正同样视为未配置
        let api_zero = ForwardApi::new(base.as_str(), TOKEN, 0).unwrap();
        assert!(!api_zero.configured());
        assert_eq!(mock.count(), 0);
    }

    // ---------- upload ----------

    #[tokio::test]
    async fn upload_returns_id_and_multipart_shape() {
        let (base, mock) = spawn_mock().await;
        mock.push_json(200, json!({"id": 77, "filename": "a.png"}));
        let api = make_api(&base);

        let id = api
            .upload(b"\x89PNG\r\n\x1a\n".to_vec(), "a.png", Some("image/png"))
            .await
            .unwrap();
        assert_eq!(id, 77);

        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let req = &requests[0];
        assert_eq!(req.path, "/api/forward/channels/1/upload");
        assert_eq!(req.authorization.as_deref(), Some("Bearer tok"));
        let ct = req.content_type.as_deref().unwrap_or("");
        assert!(ct.starts_with("multipart/form-data"), "content-type: {ct}");
        assert!(contains_bytes(&req.body, b"name=\"file\""));
        assert!(contains_bytes(&req.body, b"filename=\"a.png\""));
        assert!(contains_bytes(&req.body, b"Content-Type: image/png"));
        assert!(contains_bytes(&req.body, b"\x89PNG\r\n\x1a\n"));
    }

    #[tokio::test]
    async fn upload_defaults_content_type_from_filename() {
        let (base, mock) = spawn_mock().await;
        mock.push_json(200, json!({"id": 1}));
        let api = make_api(&base);
        assert_eq!(api.upload(b"hello".to_vec(), "n.txt", None).await.unwrap(), 1);
        // 空串 content_type 与 None 一样回落（Python: content_type or guess）
        mock.push_json(200, json!({"id": 2}));
        assert_eq!(
            api.upload(b"hi".to_vec(), "weird.unknownext", Some(""))
                .await
                .unwrap(),
            2
        );
        let requests = mock.requests.lock().unwrap();
        assert!(contains_bytes(&requests[0].body, b"Content-Type: text/plain"));
        assert!(contains_bytes(
            &requests[1].body,
            b"Content-Type: application/octet-stream"
        ));
    }

    #[tokio::test]
    async fn upload_rejects_oversize_before_http() {
        let (base, mock) = spawn_mock().await;
        let api = make_api(&base);
        let data = vec![b'x'; MAX_FILE_SIZE + 1];
        let err = api.upload(data, "big.zip", None).await.unwrap_err();
        assert!(err.message.contains("10MB"), "message: {}", err.message);
        assert_eq!(err.status, None);
        assert_eq!(mock.count(), 0);
    }

    #[tokio::test]
    async fn upload_rejects_blocked_extension_before_http() {
        let (base, mock) = spawn_mock().await;
        let api = make_api(&base);
        let err = api.upload(b"MZ".to_vec(), "evil.exe", None).await.unwrap_err();
        assert_eq!(err.message, "被禁止的扩展名: .exe");
        assert_eq!(mock.count(), 0);
        // 大写扩展名同样拦截（错误信息保留原始大小写，与 Python 一致）
        let err = api.upload(b"MZ".to_vec(), "EVIL.EXE", None).await.unwrap_err();
        assert_eq!(err.message, "被禁止的扩展名: .EXE");
        assert_eq!(mock.count(), 0);
    }

    #[tokio::test]
    async fn upload_rejects_empty_data_before_http() {
        let (base, mock) = spawn_mock().await;
        let api = make_api(&base);
        let err = api.upload(Vec::new(), "a.png", None).await.unwrap_err();
        assert_eq!(err.message, "附件为空");
        assert_eq!(mock.count(), 0);
    }

    #[tokio::test]
    async fn upload_rejects_non_200() {
        let (base, mock) = spawn_mock().await;
        mock.push_raw(413, "too large");
        let api = make_api(&base);
        let err = api.upload(b"x".to_vec(), "a.png", None).await.unwrap_err();
        assert_eq!(err.status, Some(413));
        assert_eq!(err.message, "上传失败 HTTP 413");
        assert_eq!(err.body, "too large");
    }

    #[tokio::test]
    async fn upload_rejects_non_json_200() {
        let (base, mock) = spawn_mock().await;
        mock.push_raw(200, "not json");
        let api = make_api(&base);
        let err = api.upload(b"x".to_vec(), "a.png", None).await.unwrap_err();
        assert_eq!(err.status, Some(200));
        assert!(err.message.contains("上传响应不是 JSON"));
    }

    #[tokio::test]
    async fn upload_rejects_missing_id() {
        let (base, mock) = spawn_mock().await;
        mock.push_json(200, json!({"filename": "a.png"}));
        let api = make_api(&base);
        let err = api.upload(b"x".to_vec(), "a.png", None).await.unwrap_err();
        assert!(err.message.contains("上传响应缺少 id"));
        // 字符串 id 不算 int（Python isinstance(id, int)）
        mock.push_json(200, json!({"id": "42"}));
        let err = api.upload(b"x".to_vec(), "a.png", None).await.unwrap_err();
        assert!(err.message.contains("上传响应缺少 id"));
    }

    // ---------- is_blocked / guess_content_type ----------

    #[test]
    fn is_blocked_matches_python_list() {
        for ext in [
            ".exe", ".bat", ".cmd", ".com", ".cpl", ".dll", ".scr", ".msi", ".jar", ".sh",
            ".bash", ".ps1", ".vbs", ".js", ".wsf", ".apk", ".app", ".deb", ".rpm",
        ] {
            assert!(is_blocked(&format!("file{ext}")), "{ext} 应被拦截");
            assert!(
                is_blocked(&format!("file{}", ext.to_uppercase())),
                "大写 {ext} 应被拦截"
            );
        }
        for name in ["a.png", "a.txt", "a.py", "noext", "a.tar.gz", ".bashrc", ".png", ""] {
            assert!(!is_blocked(name), "{name} 不应被拦截");
        }
    }

    #[test]
    fn guess_content_type_matches_python_table() {
        assert_eq!(guess_content_type("a.png"), "image/png");
        assert_eq!(guess_content_type("b.JPG"), "image/jpeg");
        assert_eq!(guess_content_type("c.jpeg"), "image/jpeg");
        assert_eq!(guess_content_type("d.gif"), "image/gif");
        assert_eq!(guess_content_type("e.webp"), "image/webp");
        assert_eq!(guess_content_type("f.bmp"), "image/bmp");
        assert_eq!(guess_content_type("g.mp4"), "video/mp4");
        assert_eq!(guess_content_type("h.webm"), "video/webm");
        assert_eq!(guess_content_type("i.mov"), "video/quicktime");
        assert_eq!(guess_content_type("j.mp3"), "audio/mpeg");
        assert_eq!(guess_content_type("k.wav"), "audio/wav");
        assert_eq!(guess_content_type("l.ogg"), "audio/ogg");
        assert_eq!(guess_content_type("m.pdf"), "application/pdf");
        assert_eq!(guess_content_type("n.txt"), "text/plain");
        assert_eq!(guess_content_type("o.zip"), "application/zip");
        assert_eq!(guess_content_type("unknown.xyz"), "application/octet-stream");
        assert_eq!(guess_content_type("noext"), "application/octet-stream");
        assert_eq!(guess_content_type("dir/video.MP4"), "video/mp4");
        assert_eq!(guess_content_type(".bashrc"), "application/octet-stream");
    }
}
