//! QQ 群 -> chatroom 转发流水线（对应 Python `chatroom_bridge/bridge.py`）。
//!
//! 把 OneBot 群消息转成官方 Forward Bot API 的请求：
//! - 文本直接转发；图片先下载（超过 2MB 先压缩）再上传拿 attachment id
//! - 引用消息带上 reply 信息（优先用本地近期消息缓存填充被引用内容）
//! - 以 QQ message_id 作为 source_message_id，并做本地去重
//!
//! 与 Python 的结构差异（语义等价）：
//! - Python 通过 `conn.get_image(ref)` 把非 http 图片引用换成真实 URL；这里
//!   抽象成 [`ImageResolver`] trait，main 装配时用
//!   [`crate::adapters::onebot::OneBotConnection`] 适配（get_image(file) →
//!   info["url"]，失败 → None），测试可注入假实现；
//! - 图片压缩用 `image` crate 对应 Pillow 的 thumbnail（等比缩到 1920 框内、
//!   从不放大）+ JPEG q75；压缩失败回退上传原图（文件名不变）。

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, ImageEncoder};
use indexmap::IndexMap;
use serde_json::Value;

use crate::adapters::forward_api::{ForwardApi, PostMessage, PostSource};
use crate::adapters::onebot::GroupMessage;
use crate::state::StateStore;

/// 单张图片上限：10MB（超过直接跳过该图，文本仍转发）。
pub const MAX_UPLOAD_SIZE: usize = 10 * 1024 * 1024;
/// 超过 2MB 的图片先压缩再上传。
pub const IMAGE_COMPRESS_THRESHOLD: usize = 2 * 1024 * 1024;
/// 压缩时最长边（Pillow thumbnail：等比缩到框内，从不放大）。
pub const IMAGE_MAX_DIMENSION: u32 = 1920;
/// JPEG 压缩质量。
pub const IMAGE_QUALITY: u8 = 75;
/// 近期消息缓存条数上限（FIFO，超出丢最旧）。
pub const RECENT_CACHE_SIZE: usize = 200;

/// 图片引用解析：非 http 引用需要 OneBot get_image 换取 URL。
/// 抽象成 trait 以便测试注入（main 里用 OneBotConnection 适配）。
#[async_trait]
pub trait ImageResolver: Send + Sync {
    /// get_image(file) → info["url"]；失败 → None
    async fn resolve(&self, file_ref: &str) -> Option<String>;
}

/// 转发计划：从群消息静态构造，不发网络请求。
#[derive(Debug, Clone, Default)]
pub struct ForwardPlan {
    pub content: String,
    pub image_refs: Vec<String>,
    pub reply_to: Option<String>,
}

impl ForwardPlan {
    /// Python `plan.empty`：无文本且无图片。
    pub fn is_empty(&self) -> bool {
        self.content.is_empty() && self.image_refs.is_empty()
    }
}

/// 从群消息构造转发计划（不做网络请求）。
pub fn build_forward_plan(msg: &GroupMessage) -> ForwardPlan {
    ForwardPlan {
        content: msg.text(),
        // Python: seg.url or seg.file（两者皆空则跳过该段）
        image_refs: msg
            .images()
            .iter()
            .filter_map(|seg| {
                let url = seg.url();
                let image_ref = if url.is_empty() { seg.file() } else { url };
                if image_ref.is_empty() {
                    None
                } else {
                    Some(image_ref)
                }
            })
            .collect(),
        reply_to: msg.reply_message_id(),
    }
}

/// 转发计数（对齐 Python `stats` 字典）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ForwarderStats {
    pub forwarded: u64,
    pub skipped_duplicate: u64,
    pub failed: u64,
}

/// 把 QQ 群消息转发到 chatroom FORWARD 频道。
pub struct ChatroomForwarder {
    api: Arc<ForwardApi>,
    state: Arc<StateStore>,
    enabled: bool,
    self_id: i64,
    /// 下载图片用的自建 client（对齐 Python start() 的
    /// `aiohttp.ClientTimeout(total=30, sock_connect=5)`）。
    client: reqwest::Client,
    /// message_id -> (display_name, 摘要)；插入序即访问序，FIFO 淘汰。
    recent: Mutex<IndexMap<i64, (String, String)>>,
    forwarded: AtomicU64,
    skipped_duplicate: AtomicU64,
    failed: AtomicU64,
}

impl std::fmt::Debug for ChatroomForwarder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatroomForwarder")
            .field("enabled", &self.enabled)
            .field("self_id", &self.self_id)
            .field("stats", &self.stats())
            .finish()
    }
}

impl ChatroomForwarder {
    pub fn new(
        api: Arc<ForwardApi>,
        state: Arc<StateStore>,
        enabled: bool,
        self_id: i64,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        Self {
            api,
            state,
            enabled,
            self_id,
            client,
            recent: Mutex::new(IndexMap::new()),
            forwarded: AtomicU64::new(0),
            skipped_duplicate: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        }
    }

    pub fn stats(&self) -> ForwarderStats {
        ForwarderStats {
            forwarded: self.forwarded.load(Ordering::Relaxed),
            skipped_duplicate: self.skipped_duplicate.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
        }
    }

    /// 返回 true 表示这条消息已被转发。门序对齐 Python handle()。
    pub async fn handle(&self, images: &dyn ImageResolver, msg: &GroupMessage) -> bool {
        if !self.enabled {
            return false;
        }
        // 自己发的消息不回灌（Python: if self._self_id and msg.user_id == self._self_id）
        if self.self_id != 0 && msg.user_id == self.self_id {
            return false;
        }

        let plan = build_forward_plan(msg);
        self.remember(msg);
        if plan.is_empty() {
            return false;
        }

        let key = msg.message_id.to_string();
        if self.state.already_forwarded(&key) {
            self.skipped_duplicate.fetch_add(1, Ordering::Relaxed);
            tracing::debug!("跳过已转发消息 message_id={key}");
            return false;
        }

        // 逐图上传；单图失败仅记日志跳过，绝不中断文本转发
        let mut attachment_ids: Vec<i64> = Vec::new();
        for (index, image_ref) in plan.image_refs.iter().enumerate() {
            match self.upload_image(images, msg, image_ref, index).await {
                Ok(attachment_id) => attachment_ids.push(attachment_id),
                Err(err) => tracing::warn!("图片上传失败（message_id={key}）: {err}"),
            }
        }

        // 引用昵称/内容优先从本地近期消息缓存回填
        let mut reply_nickname = String::new();
        let mut reply_content = String::new();
        if let Some(reply_to) = plan.reply_to.as_deref() {
            // Python: int(plan.reply_to) 仅当纯数字；不在缓存则留空
            let cached = is_digits(reply_to)
                .then(|| reply_to.parse::<i64>().ok())
                .flatten()
                .and_then(|id| self.lock_recent().get(&id).cloned());
            if let Some((nickname, content)) = cached {
                reply_nickname = nickname;
                reply_content = content;
            }
        }

        let mut message = PostMessage::new(PostSource::QQ);
        message.content = plan.content.clone();
        message.source_message_id = key.clone();
        message.sender_qq = Some(msg.user_id);
        message.nickname = msg.display_name();
        message.reply_source_message_id = plan.reply_to.clone().unwrap_or_default();
        message.reply_nickname = reply_nickname;
        message.reply_content = reply_content;
        message.attachment_ids = attachment_ids.clone();

        match self.api.post_message(&message).await {
            Err(err) => {
                self.failed.fetch_add(1, Ordering::Relaxed);
                tracing::warn!("转发到 chatroom 失败（message_id={key}）: {err}");
                false
            }
            Ok(response) => {
                // Python: isinstance(response.get("id"), int) 才写去重表
                if let Some(chatroom_id) = response.get("id").and_then(Value::as_i64) {
                    self.state.mark_forwarded(&key, chatroom_id);
                }
                self.forwarded.fetch_add(1, Ordering::Relaxed);
                let preview = if plan.content.is_empty() {
                    "[媒体]"
                } else {
                    plan.content.as_str()
                };
                tracing::info!(
                    "已转发 QQ 消息到 chatroom: {}: {:.40} ({} 个附件)",
                    msg.display_name(),
                    preview,
                    attachment_ids.len()
                );
                true
            }
        }
    }

    // --- 内部工具 ---

    fn lock_recent(&self) -> MutexGuard<'_, IndexMap<i64, (String, String)>> {
        self.recent.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 把消息摘要放进近期缓存：text 或（有图时）"[图片]"，截到 200 字符，
    /// FIFO 上限 [`RECENT_CACHE_SIZE`]（对齐 Python `_remember`）。
    fn remember(&self, msg: &GroupMessage) {
        let text = msg.text();
        let summary = if !text.is_empty() {
            text
        } else if !msg.images().is_empty() {
            "[图片]".to_string()
        } else {
            String::new()
        };
        let mut recent = self.lock_recent();
        recent.insert(
            msg.message_id,
            (msg.display_name(), truncate_chars(&summary, 200)),
        );
        while recent.len() > RECENT_CACHE_SIZE {
            recent.shift_remove_index(0);
        }
    }

    /// 取图 + 上传，返回 attachment id。
    async fn upload_image(
        &self,
        images: &dyn ImageResolver,
        msg: &GroupMessage,
        image_ref: &str,
        index: usize,
    ) -> Result<i64, String> {
        let (data, filename, content_type) =
            self.fetch_image(images, image_ref, msg, index).await?;
        self.api
            .upload(data, &filename, Some(&content_type))
            .await
            .map_err(|err| err.to_string())
    }

    /// 取图片字节。ref 直接是 http(s) 时就用它，否则经 [`ImageResolver`]
    /// 换取地址；超过 2MB 先压缩（对齐 Python `_fetch_image`）。
    async fn fetch_image(
        &self,
        images: &dyn ImageResolver,
        image_ref: &str,
        msg: &GroupMessage,
        index: usize,
    ) -> Result<(Vec<u8>, String, String), String> {
        let mut url = if image_ref.starts_with("http") {
            image_ref.to_string()
        } else {
            String::new()
        };
        if url.is_empty() {
            if let Some(resolved) = images.resolve(image_ref).await {
                url = resolved;
            }
        }
        if !url.starts_with("http") {
            return Err(format!("无法解析图片地址: {}", truncate_chars(image_ref, 80)));
        }

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|err| format!("下载图片失败: {err}"))?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(format!("下载图片失败 HTTP {status}"));
        }
        let data = response
            .bytes()
            .await
            .map_err(|err| format!("下载图片失败: {err}"))?
            .to_vec();
        if data.len() > MAX_UPLOAD_SIZE {
            return Err(format!("图片超过 10MB（{} 字节）", data.len()));
        }

        // 原始命名；超阈值压缩成功才换成 qq-compressed.jpg
        let mut filename = format!("qq-{}-{}.png", msg.message_id, index);
        let mut content_type = "image/png".to_string();
        let mut payload = data;
        if payload.len() > IMAGE_COMPRESS_THRESHOLD {
            if let Some((compressed, name, mime)) = compress_image(&payload) {
                payload = compressed;
                filename = name;
                content_type = mime;
            }
        }
        Ok((payload, filename, content_type))
    }
}

/// Python `str.isdigit()` 的 ASCII 近似（非空且全是 0-9）。
fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// 按字符数截断（对齐 Python `s[:n]`，避免把多字节字符切成乱码）。
fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// 按最长边 1920 + JPEG q75 压缩，失败时返回 None（原图上传）。
fn compress_image(data: &[u8]) -> Option<(Vec<u8>, String, String)> {
    let decoded = image::load_from_memory(data).ok()?;
    // Pillow thumbnail：等比缩到框内，从不放大
    let thumbnail = decoded.thumbnail(IMAGE_MAX_DIMENSION, IMAGE_MAX_DIMENSION);
    let rgb = thumbnail.to_rgb8();
    let mut output = Vec::new();
    let encoder = JpegEncoder::new_with_quality(Cursor::new(&mut output), IMAGE_QUALITY);
    encoder
        .write_image(rgb.as_raw(), rgb.width(), rgb.height(), ExtendedColorType::Rgb8)
        .ok()?;
    Some((output, "qq-compressed.jpg".to_string(), "image/jpeg".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::onebot::Segment;
    use axum::body::Bytes;
    use axum::extract::{Path, State};
    use axum::http::{header, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde_json::json;
    use tempfile::tempdir;

    const TOKEN: &str = "tok";
    const CHANNEL: i64 = 1;

    // ---------- axum mock：Forward API + 图片下载 ----------

    /// (multipart 里的 filename, 原始请求体)
    type UploadRecord = (String, Vec<u8>);

    #[derive(Clone, Default)]
    struct MockState {
        uploads: Arc<Mutex<Vec<UploadRecord>>>,
        /// post_message JSON 请求体
        posts: Arc<Mutex<Vec<Value>>>,
        /// 静态文件：路径名 -> 字节
        files: Arc<std::collections::HashMap<String, Vec<u8>>>,
    }

    impl MockState {
        fn upload_filenames(&self) -> Vec<String> {
            self.uploads
                .lock()
                .unwrap()
                .iter()
                .map(|(name, _)| name.clone())
                .collect()
        }

        fn posts(&self) -> Vec<Value> {
            self.posts.lock().unwrap().clone()
        }
    }

    async fn upload_handler(State(state): State<MockState>, body: Bytes) -> Json<Value> {
        let raw = body.to_vec();
        let filename = extract_filename(&raw);
        let mut uploads = state.uploads.lock().unwrap();
        uploads.push((filename, raw));
        let id = 100 + uploads.len() as i64;
        Json(json!({ "id": id }))
    }

    async fn messages_handler(
        State(state): State<MockState>,
        Json(payload): Json<Value>,
    ) -> Response {
        let mut posts = state.posts.lock().unwrap();
        let id = 500 + posts.len() as i64 + 1;
        posts.push(payload);
        // Forward API 约定 POST /messages 返回 201
        (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
    }

    async fn file_handler(State(state): State<MockState>, Path(name): Path<String>) -> Response {
        match state.files.get(&name) {
            Some(bytes) => (
                [(header::CONTENT_TYPE, "application/octet-stream")],
                bytes.clone(),
            )
                .into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }

    fn extract_filename(body: &[u8]) -> String {
        let marker = b"filename=\"";
        let start = body
            .windows(marker.len())
            .position(|window| window == marker)
            .expect("multipart body 应包含 filename");
        let rest = &body[start + marker.len()..];
        let end = rest
            .iter()
            .position(|byte| *byte == b'"')
            .expect("filename 结束引号");
        String::from_utf8_lossy(&rest[..end]).into_owned()
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        !needle.is_empty() && haystack.windows(needle.len()).any(|window| window == needle)
    }

    async fn spawn_mock() -> (String, MockState) {
        let mut files = std::collections::HashMap::new();
        files.insert("img.png".to_string(), tiny_png());
        files.insert("big.bin".to_string(), vec![0u8; MAX_UPLOAD_SIZE + 1]);
        files.insert(
            "garbage.png".to_string(),
            vec![0xFFu8; IMAGE_COMPRESS_THRESHOLD + 100],
        );
        files.insert("photo.png".to_string(), noise_png(1200, 1200));
        let state = MockState {
            uploads: Arc::new(Mutex::new(Vec::new())),
            posts: Arc::new(Mutex::new(Vec::new())),
            files: Arc::new(files),
        };
        let app = Router::new()
            .route("/api/forward/channels/1/upload", post(upload_handler))
            .route("/api/forward/channels/1/messages", post(messages_handler))
            .route("/{name}", get(file_handler))
            .with_state(state.clone())
            // axum 默认请求体上限 2MB，会挡住 >2MB 的上传测试载荷
            .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), state)
    }

    // ---------- 构造工具 ----------

    fn make_forwarder(
        base: &str,
        state: &Arc<StateStore>,
        enabled: bool,
        self_id: i64,
    ) -> ChatroomForwarder {
        ChatroomForwarder::new(
            Arc::new(ForwardApi::new(base.to_string(), TOKEN, CHANNEL).unwrap()),
            state.clone(),
            enabled,
            self_id,
        )
    }

    fn make_state() -> (tempfile::TempDir, Arc<StateStore>) {
        let dir = tempdir().unwrap();
        let store = Arc::new(StateStore::new(dir.path().join("state.json")));
        (dir, store)
    }

    fn segment(kind: &str, data: Value) -> Segment {
        Segment {
            kind: kind.to_string(),
            data,
        }
    }

    fn group_message(message_id: i64, user_id: i64, segments: Vec<Segment>) -> GroupMessage {
        GroupMessage {
            group_id: 123456789,
            user_id,
            message_id,
            nickname: "玩家A".to_string(),
            card: String::new(),
            segments,
            raw: Value::Null,
        }
    }

    struct NoResolver;

    #[async_trait]
    impl ImageResolver for NoResolver {
        async fn resolve(&self, _file_ref: &str) -> Option<String> {
            None
        }
    }

    /// 固定映射的假 get_image。
    struct MapResolver(Vec<(String, String)>);

    #[async_trait]
    impl ImageResolver for MapResolver {
        async fn resolve(&self, file_ref: &str) -> Option<String> {
            self.0
                .iter()
                .find(|(key, _)| key == file_ref)
                .map(|(_, url)| url.clone())
        }
    }

    fn tiny_png() -> Vec<u8> {
        encode_png(image::RgbImage::from_pixel(2, 2, image::Rgb([200u8, 30, 30])))
    }

    /// 伪随机噪点 PNG：不可压缩，编码后 > 2MB，可正常解码。
    fn noise_png(width: u32, height: u32) -> Vec<u8> {
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut image = image::RgbImage::new(width, height);
        for pixel in image.pixels_mut() {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *pixel = image::Rgb([(seed >> 33) as u8, (seed >> 41) as u8, (seed >> 49) as u8]);
        }
        encode_png(image)
    }

    fn encode_png(image: image::RgbImage) -> Vec<u8> {
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    // ---------- 计划构造 ----------

    #[test]
    fn build_forward_plan_from_segments() {
        let msg = group_message(
            42,
            10001,
            vec![
                segment("reply", json!({ "id": "111" })),
                segment("text", json!({ "text": "看看这个 " })),
                segment("image", json!({ "file": "abc.png", "url": "https://x/y.png" })),
                segment("image", json!({ "file": "def.png" })),
                segment("image", json!({ "url": "" })), // 无 url 无 file → 跳过
                segment("at", json!({ "qq": "10000" })), // 非 image 段忽略
            ],
        );
        let plan = build_forward_plan(&msg);
        assert_eq!(plan.content, "看看这个"); // 拼接后 strip
        assert_eq!(plan.image_refs, ["https://x/y.png", "def.png"]);
        assert_eq!(plan.reply_to.as_deref(), Some("111"));
        assert!(!plan.is_empty());

        let empty = build_forward_plan(&group_message(43, 10001, vec![]));
        assert!(empty.is_empty());
        assert_eq!(empty.reply_to, None);
        assert!(empty.image_refs.is_empty());
    }

    // ---------- 基本转发 / 去重 / 门禁 ----------

    #[tokio::test]
    async fn first_forward_posts_body_and_persists_dedup() {
        let (base, mock) = spawn_mock().await;
        let (dir, state) = make_state();
        let forwarder = make_forwarder(&base, &state, true, 0);

        let msg = group_message(111, 10001, vec![segment("text", json!({ "text": "早上好" }))]);
        assert!(forwarder.handle(&NoResolver, &msg).await);

        let posts = mock.posts();
        assert_eq!(posts.len(), 1);
        let body = &posts[0];
        assert_eq!(body["source"], "qq");
        assert_eq!(body["sender"], json!({ "qq": 10001, "nickname": "玩家A" }));
        assert_eq!(body["content"], "早上好");
        assert_eq!(body["source_message_id"], "111");
        assert_eq!(body["attachment_ids"], json!([]));
        assert!(body.get("reply").is_none());

        // 响应里的 int id 写入去重表，且持久化到磁盘
        assert_eq!(state.forwarded_id("111"), Some(501));
        let reopened = StateStore::new(dir.path().join("state.json"));
        assert_eq!(reopened.forwarded_id("111"), Some(501));
        assert_eq!(
            forwarder.stats(),
            ForwarderStats {
                forwarded: 1,
                skipped_duplicate: 0,
                failed: 0
            }
        );
    }

    #[tokio::test]
    async fn duplicate_message_is_skipped() {
        let (base, mock) = spawn_mock().await;
        let (_dir, state) = make_state();
        let forwarder = make_forwarder(&base, &state, true, 0);

        let msg = group_message(111, 10001, vec![segment("text", json!({ "text": "早上好" }))]);
        assert!(forwarder.handle(&NoResolver, &msg).await);
        assert!(!forwarder.handle(&NoResolver, &msg).await);

        assert_eq!(mock.posts().len(), 1);
        assert_eq!(
            forwarder.stats(),
            ForwarderStats {
                forwarded: 1,
                skipped_duplicate: 1,
                failed: 0
            }
        );
    }

    #[tokio::test]
    async fn disabled_forwarder_makes_no_requests() {
        let (base, mock) = spawn_mock().await;
        let (_dir, state) = make_state();
        let forwarder = make_forwarder(&base, &state, false, 0);

        let msg = group_message(111, 10001, vec![segment("text", json!({ "text": "早上好" }))]);
        assert!(!forwarder.handle(&NoResolver, &msg).await);
        assert!(mock.posts().is_empty());
        assert!(mock.uploads.lock().unwrap().is_empty());
        assert_eq!(forwarder.stats(), ForwarderStats::default());
    }

    #[tokio::test]
    async fn own_message_is_not_rebounded() {
        let (base, mock) = spawn_mock().await;
        let (_dir, state) = make_state();
        let forwarder = make_forwarder(&base, &state, true, 10001);

        let msg = group_message(111, 10001, vec![segment("text", json!({ "text": "自言自语" }))]);
        assert!(!forwarder.handle(&NoResolver, &msg).await);
        assert!(mock.posts().is_empty());
        assert!(mock.uploads.lock().unwrap().is_empty());
    }

    // ---------- 图片流水线 ----------

    #[tokio::test]
    async fn image_url_downloaded_and_uploaded() {
        let (base, mock) = spawn_mock().await;
        let (_dir, state) = make_state();
        let forwarder = make_forwarder(&base, &state, true, 0);

        let msg = group_message(
            222,
            10001,
            vec![segment("image", json!({ "url": format!("{base}/img.png") }))],
        );
        assert!(forwarder.handle(&NoResolver, &msg).await);

        assert_eq!(mock.upload_filenames(), ["qq-222-0.png"]);
        let uploads = mock.uploads.lock().unwrap();
        assert!(contains_bytes(&uploads[0].1, b"Content-Type: image/png"));
        let posts = mock.posts();
        assert_eq!(posts[0]["attachment_ids"], json!([101]));
        assert_eq!(posts[0]["content"], "");
    }

    #[tokio::test]
    async fn non_http_ref_resolved_via_image_resolver() {
        let (base, mock) = spawn_mock().await;
        let (_dir, state) = make_state();
        let forwarder = make_forwarder(&base, &state, true, 0);
        let resolver = MapResolver(vec![(
            "abc.img".to_string(),
            format!("{base}/img.png"),
        )]);

        // resolver 换到 URL → 正常上传
        let ok = group_message(
            230,
            10001,
            vec![segment("image", json!({ "file": "abc.img" }))],
        );
        assert!(forwarder.handle(&resolver, &ok).await);

        // resolver 解析失败 → 该图跳过，文本照转
        let fail = group_message(
            231,
            10001,
            vec![
                segment("text", json!({ "text": "在的" })),
                segment("image", json!({ "file": "ghost.img" })),
            ],
        );
        assert!(forwarder.handle(&resolver, &fail).await);

        assert_eq!(mock.upload_filenames(), ["qq-230-0.png"]);
        let posts = mock.posts();
        assert_eq!(posts[0]["attachment_ids"], json!([101]));
        assert_eq!(posts[1]["content"], "在的");
        assert_eq!(posts[1]["attachment_ids"], json!([]));
        assert_eq!(forwarder.stats().failed, 0); // 图片跳过不算转发失败
    }

    #[tokio::test]
    async fn oversized_download_skipped_but_text_forwarded() {
        let (base, mock) = spawn_mock().await;
        let (_dir, state) = make_state();
        let forwarder = make_forwarder(&base, &state, true, 0);

        let msg = group_message(
            333,
            10001,
            vec![
                segment("text", json!({ "text": "还在吗" })),
                segment("image", json!({ "url": format!("{base}/big.bin") })),
            ],
        );
        assert!(forwarder.handle(&NoResolver, &msg).await);

        // 10MB+1 在下载后即被拒，连上传请求都不会发出
        assert!(mock.uploads.lock().unwrap().is_empty());
        let posts = mock.posts();
        assert_eq!(posts[0]["content"], "还在吗");
        assert_eq!(posts[0]["attachment_ids"], json!([]));
        assert_eq!(forwarder.stats().failed, 0);
    }

    #[tokio::test]
    async fn large_image_is_compressed_to_jpeg() {
        let (base, mock) = spawn_mock().await;
        let (_dir, state) = make_state();
        let forwarder = make_forwarder(&base, &state, true, 0);

        let msg = group_message(
            444,
            10001,
            vec![segment("image", json!({ "url": format!("{base}/photo.png") }))],
        );
        assert!(forwarder.handle(&NoResolver, &msg).await);

        let uploads = mock.uploads.lock().unwrap();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].0, "qq-compressed.jpg");
        assert!(contains_bytes(&uploads[0].1, b"Content-Type: image/jpeg"));
        // 压缩后体积应小于原图（噪点 PNG 约 4.3MB）
        let original = noise_png(1200, 1200);
        assert!(uploads[0].1.len() < original.len());
        let posts = mock.posts();
        assert_eq!(posts[0]["attachment_ids"], json!([101]));
    }

    #[tokio::test]
    async fn compress_failure_uploads_original_bytes() {
        let (base, mock) = spawn_mock().await;
        let (_dir, state) = make_state();
        let forwarder = make_forwarder(&base, &state, true, 0);

        // 2MB+ 的不可解码垃圾 → 压缩失败 → 原图按 qq-{message_id}-{index}.png 上传
        let msg = group_message(
            445,
            10001,
            vec![segment("image", json!({ "url": format!("{base}/garbage.png") }))],
        );
        assert!(forwarder.handle(&NoResolver, &msg).await);

        let uploads = mock.uploads.lock().unwrap();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].0, "qq-445-0.png");
        assert!(contains_bytes(&uploads[0].1, b"Content-Type: image/png"));
        assert!(uploads[0].1.len() > IMAGE_COMPRESS_THRESHOLD);
        let posts = mock.posts();
        assert_eq!(posts[0]["attachment_ids"], json!([101]));
    }

    // ---------- 引用回填 / 近期缓存 ----------

    #[tokio::test]
    async fn reply_info_backfilled_from_recent_cache() {
        let (base, mock) = spawn_mock().await;
        let (_dir, state) = make_state();
        let forwarder = make_forwarder(&base, &state, true, 0);
        let base_url = base.clone();

        // 纯图消息 → 摘要 "[图片]"
        let image_only = group_message(
            310,
            10001,
            vec![segment("image", json!({ "url": format!("{base_url}/img.png") }))],
        );
        assert!(forwarder.handle(&NoResolver, &image_only).await);

        // 超长文本 → 摘要截到 200 字符
        let long_text = "a".repeat(250);
        let long = group_message(
            311,
            10001,
            vec![segment("text", json!({ "text": long_text }))],
        );
        assert!(forwarder.handle(&NoResolver, &long).await);

        let reply_image = group_message(
            312,
            10002,
            vec![
                segment("reply", json!({ "id": "310" })),
                segment("text", json!({ "text": "回图" })),
            ],
        );
        assert!(forwarder.handle(&NoResolver, &reply_image).await);

        let reply_long = group_message(
            313,
            10002,
            vec![
                segment("reply", json!({ "id": "311" })),
                segment("text", json!({ "text": "回长文" })),
            ],
        );
        assert!(forwarder.handle(&NoResolver, &reply_long).await);

        // 不在缓存 → 只带 source_message_id，昵称/内容留空
        let reply_unknown = group_message(
            314,
            10002,
            vec![
                segment("reply", json!({ "id": "999999" })),
                segment("text", json!({ "text": "回谁？" })),
            ],
        );
        assert!(forwarder.handle(&NoResolver, &reply_unknown).await);

        // 非数字 id → 不做缓存查找，原样透传
        let reply_text_id = group_message(
            315,
            10002,
            vec![
                segment("reply", json!({ "id": "abc" })),
                segment("text", json!({ "text": "回文本 id" })),
            ],
        );
        assert!(forwarder.handle(&NoResolver, &reply_text_id).await);

        let posts = mock.posts();
        assert_eq!(posts[2]["reply"], json!({
            "source_message_id": "310",
            "nickname": "玩家A",
            "content": "[图片]",
        }));
        assert_eq!(posts[3]["reply"]["content"], "a".repeat(200));
        assert_eq!(posts[4]["reply"], json!({
            "source_message_id": "999999",
            "nickname": "",
            "content": "",
        }));
        assert_eq!(posts[5]["reply"]["source_message_id"], "abc");
        assert_eq!(posts[5]["reply"]["nickname"], "");
    }

    #[tokio::test]
    async fn recent_cache_fifo_eviction() {
        let (base, mock) = spawn_mock().await;
        let (_dir, state) = make_state();
        let forwarder = make_forwarder(&base, &state, true, 0);

        // 201 条空消息只进缓存，不产生 HTTP（plan.empty 在去重之前返回）
        for id in 1..=201i64 {
            let filler = group_message(id, 10001, vec![]);
            assert!(!forwarder.handle(&NoResolver, &filler).await);
        }
        assert!(mock.posts().is_empty());

        // 回复最旧的消息：已被挤出缓存 → 无回填
        let old = group_message(
            300,
            10001,
            vec![
                segment("reply", json!({ "id": "1" })),
                segment("text", json!({ "text": "早" })),
            ],
        );
        assert!(forwarder.handle(&NoResolver, &old).await);

        // 回复最新的消息：仍在缓存
        let newest = group_message(
            301,
            10001,
            vec![
                segment("reply", json!({ "id": "201" })),
                segment("text", json!({ "text": "晚" })),
            ],
        );
        assert!(forwarder.handle(&NoResolver, &newest).await);

        let posts = mock.posts();
        assert_eq!(posts[0]["reply"]["source_message_id"], "1");
        assert_eq!(posts[0]["reply"]["nickname"], "");
        assert_eq!(posts[1]["reply"]["source_message_id"], "201");
        assert_eq!(posts[1]["reply"]["nickname"], "玩家A");
    }

    // ---------- 转发失败 ----------

    #[tokio::test]
    async fn post_failure_counts_as_failed() {
        // 无上传路由、messages 恒 500 的裸 mock：post_message 必然失败
        let app = Router::new().route(
            "/api/forward/channels/1/messages",
            post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base = format!("http://{addr}");

        let (_dir, state) = make_state();
        let forwarder = make_forwarder(&base, &state, true, 0);
        let msg = group_message(777, 10001, vec![segment("text", json!({ "text": "你好" }))]);
        assert!(!forwarder.handle(&NoResolver, &msg).await);
        assert_eq!(
            forwarder.stats(),
            ForwarderStats {
                forwarded: 0,
                skipped_duplicate: 0,
                failed: 1
            }
        );
        // 失败的消息不进去重表，下次还能重试
        assert!(!state.already_forwarded("777"));
    }
}
