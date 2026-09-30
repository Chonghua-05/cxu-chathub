//! Mojang 版本更新播报（roadmap v0.5）：轮询官方补丁说明 feed，检测到新版本后
//! 经 LLM 翻译正文，渲染「译后 / 译前」两张黑底白字长图，打包成合并转发
//! 聊天记录发到 `chatroom.group_ids` 白名单群。
//!
//! 数据源与判断逻辑：
//! - feed 默认 `https://launchercontent.mojang.com/v2/javaPatchNotes.json`——
//!   **v1 端点（不带 /v2/）2024 年起已冻结**（停在 1.20.4-rc1），必须用 v2；
//!   v2 列表条目只有 `shortText`，完整正文按需从 `<feed 目录>/contentPath` 拉取，
//!   响应可能带 UTF-8 BOM，解析前剥掉；
//! - 列表**最新在前**；新版本 = 列表前缀中尚未播报的条目（首启只记录基线不播报，
//!   避免上线风暴）；按从旧到新的顺序播报，单条失败则中止等下轮重试（保序）；
//! - 播报状态持久化在 `state.json` 的 `announced_patches`（保留最近 50 条）。
//!
//! 降级链（任何一环缺失都发得出去，只是内容缩水）：
//! LLM 未配置 / 翻译失败 → 只发原文；长图渲染失败 → 降级为纯文本节点；
//! QQ 未连接 / 无白名单群 → 记日志下轮重试。
//!
//! 原文链接：正文 feed 无单篇直链，minecraft.net 文章 slug 不可稳定推导，
//! 统一给官方补丁说明总览页 [`PATCH_NOTES_PAGE`]。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{error, info, warn};

use crate::agent::llm::LlmClient;
use crate::config::PatchBroadcastConfig;
use crate::services::status_render::Block;
use crate::state::StateStore;
use crate::subsystem::Subsystem;

/// 官方补丁说明总览页（「原文链接」节点指向这里；可按版本号找到原文）。
const PATCH_NOTES_PAGE: &str = "https://www.minecraft.net/en-us/patch-notes";

/// 合并转发里各节点的发送者昵称（QQ 群合并转发会显示在每个节点上）。
const NODE_NICKNAME: &str = "Mojang 更新播报";

/// 正文翻译提示词：保留 HTML 结构，只翻文本，MC 术语用官方中文译名。
const TRANSLATE_PROMPT: &str = "你是 Minecraft 更新公告的翻译器。把用户给出的 HTML 更新说明翻译成简体中文：保留所有 HTML 标签与结构原样，只翻译文本内容；Minecraft 领域内容遵循官方中文译名；不要添加任何解释，不要增删内容。";

/// 纯文本降级时摘录的长度上限（UTF-8 安全截断）。
const TEXT_FALLBACK_MAX_CHARS: usize = 1500;

/// 单轮待播报条目数的安全上限：超过视为 feed 异常（重置 / 裁剪导致已播报
/// 条目消失），只记录基线不播报——防止把整份历史当新版本倒进群里。
const MAX_BURST_ANNOUNCEMENTS: usize = 10;

/// feed 列表条目（只取播报需要的字段；列表最新在前）。
#[derive(Debug, Clone, Deserialize)]
struct FeedEntry {
    id: String,
    version: String,
    title: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    date: String,
    /// 相对 feed 目录的正文路径（如 `javaPatchNotes/xx.json`）
    #[serde(rename = "contentPath")]
    content_path: String,
}

/// 长图渲染器抽象：返回 None = 渲染不可用（降级为纯文本节点，不丢消息）。
#[async_trait]
pub trait PatchRenderer: Send + Sync {
    /// `title` 为长图顶栏标题，`html` 为正文（Mojang 原文或 LLM 译文）。
    async fn render(&self, title: &str, html: &str) -> Option<Vec<u8>>;
}

/// 真实渲染器：HTML 粗剥离成文本块 → 复用 /server 的纯 Rust 渲染管线出黑底白字长图。
pub struct BulletinRenderer;

#[async_trait]
impl PatchRenderer for BulletinRenderer {
    async fn render(&self, title: &str, html: &str) -> Option<Vec<u8>> {
        let blocks = strip_html_blocks(html);
        crate::services::status_render::render_bulletin_png(title, &blocks).ok()
    }
}

/// 播报出口：把组装好的合并转发节点发到目标群（service 装配时注入，
/// 解耦 OneBot 连接与群白名单细节）。
#[async_trait]
pub trait PatchSendSink: Send + Sync {
    /// 发到全部白名单群；返回是否至少送达一群。
    async fn send_forward(&self, nodes: Value) -> bool;
}

/// 版本更新播报器。无内部可变状态——轮询状态全部在 [`StateStore`]，可重入。
pub struct PatchBroadcaster {
    cfg: PatchBroadcastConfig,
    state: Arc<StateStore>,
    llm: Option<LlmClient>,
    renderer: Option<Arc<dyn PatchRenderer>>,
    sink: Arc<dyn PatchSendSink>,
    bot_uin: i64,
    client: reqwest::Client,
}

impl PatchBroadcaster {
    pub fn new(
        cfg: PatchBroadcastConfig,
        state: Arc<StateStore>,
        llm: Option<LlmClient>,
        renderer: Option<Arc<dyn PatchRenderer>>,
        sink: Arc<dyn PatchSendSink>,
        bot_uin: i64,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .user_agent(concat!(
                "cxu-chathub/",
                env!("CARGO_PKG_VERSION"),
                " (community patch broadcast)"
            ))
            .build()
            .unwrap_or_default();
        Self {
            cfg,
            state,
            llm,
            renderer,
            sink,
            bot_uin,
            client,
        }
    }

    /// feed 目录（正文 URL = 目录 + contentPath）。仅当末段形似文件名（含 `.`）
    /// 且剥掉后仍是合法地址（含 `://`）才剥离——兼容 feed_url 填到目录为止、
    /// 甚至只填裸主机的写法。
    fn feed_base_url(&self) -> String {
        let url = self.cfg.feed_url.trim_end_matches('/');
        match url.rsplit_once('/') {
            Some((base, last)) if last.contains('.') && base.contains("://") => base.to_string(),
            _ => url.to_string(),
        }
    }

    /// 轮询一次：拉 feed → 找未播报的新条目 → 按从旧到新播报。
    /// 单条播报失败即中止（未标记，下轮重试，保住时间顺序）。
    pub async fn poll_once(&self) {
        let Some(entries) = self.fetch_feed().await else {
            return;
        };
        let new_entries: Vec<&FeedEntry> = entries
            .iter()
            .take_while(|entry| !self.state.patch_announced(&entry.id))
            .collect();
        if new_entries.is_empty() {
            return;
        }
        // 首次启用：只把当前最新记录为基线，不播报历史版本（防上线风暴）
        if !self.state.has_announced_patches() {
            if let Some(newest) = entries.first() {
                self.state.mark_patch_announced(&newest.id, &newest.title);
                info!(
                    id = %newest.id,
                    version = %newest.version,
                    "版本更新播报首次运行：记录基线 {}，不播报历史版本",
                    newest.title
                );
            }
            return;
        }
        // 风暴防护：已播报条目从 feed 消失（裁剪/重置）会让整个列表都「未播报」，
        // 一次性待播报超过安全上限时按 feed 异常处理——全部记基线、不播报
        if new_entries.len() > MAX_BURST_ANNOUNCEMENTS {
            warn!(
                count = new_entries.len(),
                max = MAX_BURST_ANNOUNCEMENTS,
                "单轮待播报条目异常偏多，疑似 feed 重置或裁剪：仅记录基线，不播报"
            );
            for entry in &new_entries {
                self.state.mark_patch_announced(&entry.id, &entry.title);
            }
            return;
        }
        info!(count = new_entries.len(), "检测到 {} 个新版本待播报", new_entries.len());
        for entry in new_entries.iter().rev() {
            if !self.announce(entry).await {
                error!(
                    id = %entry.id,
                    version = %entry.version,
                    "版本更新播报失败，下轮轮询重试（后续版本暂停播报以保持顺序）"
                );
                return;
            }
            self.state.mark_patch_announced(&entry.id, &entry.title);
        }
    }

    async fn fetch_feed(&self) -> Option<Vec<FeedEntry>> {
        let response = match self.client.get(&self.cfg.feed_url).send().await {
            Ok(response) => response,
            Err(err) => {
                warn!(url = %self.cfg.feed_url, error = %err, "版本更新 feed 拉取失败");
                return None;
            }
        };
        let raw = match response.error_for_status() {
            Ok(response) => response.text().await,
            Err(err) => {
                warn!(url = %self.cfg.feed_url, error = %err, "版本更新 feed 返回错误状态");
                return None;
            }
        };
        let Ok(raw) = raw else {
            warn!("版本更新 feed 响应读取失败");
            return None;
        };
        match serde_json::from_str::<FeedList>(strip_bom(&raw)) {
            Ok(list) => Some(list.entries),
            Err(err) => {
                warn!(error = %err, "版本更新 feed 解析失败（响应开头: {}）", preview(&raw));
                None
            }
        }
    }

    async fn fetch_body(&self, entry: &FeedEntry) -> Option<String> {
        let url = format!("{}/{}", self.feed_base_url().trim_end_matches('/'), entry.content_path);
        let response = match self.client.get(&url).send().await {
            Ok(response) => response,
            Err(err) => {
                warn!(id = %entry.id, url = %url, error = %err, "更新正文拉取失败");
                return None;
            }
        };
        let raw = match response.error_for_status() {
            Ok(response) => response.text().await,
            Err(err) => {
                warn!(id = %entry.id, url = %url, error = %err, "更新正文返回错误状态");
                return None;
            }
        };
        let Ok(raw) = raw else {
            warn!(id = %entry.id, "更新正文响应读取失败");
            return None;
        };
        match serde_json::from_str::<EntryBody>(strip_bom(&raw)) {
            Ok(body) => (!body.body.is_empty()).then_some(body.body),
            Err(err) => {
                warn!(id = %entry.id, error = %err, "更新正文解析失败（响应开头: {}）", preview(&raw));
                None
            }
        }
    }

    /// 播报单个条目。任何环节都构造得出发送内容（降级链），只有 QQ 侧
    /// 发送失败才返回 false。
    async fn announce(&self, entry: &FeedEntry) -> bool {
        let body_html = self.fetch_body(entry).await;
        let translated_html = self.translate(body_html.as_deref()).await;
        info!(
            version = %entry.version,
            translated = translated_html.is_some(),
            "开始播报版本更新 {}",
            entry.title
        );

        let mut nodes = Vec::new();
        // 节点 1：标题信息
        let mut header = format!("📦 Minecraft Java 版更新\n{}\n版本：{}（{}）", entry.title, entry.version, entry.kind);
        if !entry.date.is_empty() {
            header.push_str(&format!("\n发布时间：{}", entry.date));
        }
        nodes.push(self.text_node(&header));

        // 节点 2：译后（图 → 文本降级 → 缺席）
        match self
            .render_or_text(translated_html.as_deref(), &entry.title, "🇨🇳 中文翻译（LLM 翻译，可能有误）")
            .await
        {
            NodeOrSkip::Image(png) => nodes.push(self.image_node(&png, "🇨🇳 中文翻译")),
            NodeOrSkip::Text(text) => nodes.push(self.text_node(&text)),
            NodeOrSkip::Skip => {}
        }
        // 节点 3：原文
        match self
            .render_or_text(body_html.as_deref(), &entry.title, "🇬🇧 原文摘录")
            .await
        {
            NodeOrSkip::Image(png) => nodes.push(self.image_node(&png, "🇬🇧 官方原文")),
            NodeOrSkip::Text(text) => nodes.push(self.text_node(&text)),
            NodeOrSkip::Skip => {}
        }
        // 节点 4：官方链接（feed 无单篇直链，给总览页 + 版本号检索提示）
        nodes.push(self.text_node(&format!(
            "🔗 官方补丁说明（按 {version} 查找原文）：\n{PATCH_NOTES_PAGE}",
            version = entry.version
        )));

        self.sink.send_forward(Value::Array(nodes)).await
    }

    /// 翻译：LLM 未配置或正文缺失 → None；翻译失败 → None（发原文）。
    async fn translate(&self, body_html: Option<&str>) -> Option<String> {
        let Some(body) = body_html else {
            return None;
        };
        let Some(llm) = &self.llm else {
            warn!("未配置 agent.llm，版本更新播报跳过翻译（只发原文）");
            return None;
        };
        match llm.complete(TRANSLATE_PROMPT, body).await {
            Ok(translated) if !translated.trim().is_empty() => Some(translated),
            Ok(_) => {
                warn!("翻译返回空结果，版本更新播报只发原文");
                None
            }
            Err(err) => {
                warn!(error = %err, "翻译失败，版本更新播报只发原文");
                None
            }
        }
    }

    /// HTML → 图（有渲染器）或纯文本摘录（无渲染器 / 渲染失败）；正文缺失 → Skip。
    /// `page_title` 用作长图顶栏。
    async fn render_or_text(
        &self,
        html: Option<&str>,
        page_title: &str,
        text_header: &str,
    ) -> NodeOrSkip {
        let Some(html) = html else {
            return NodeOrSkip::Skip;
        };
        let blocks = strip_html_blocks(html);
        if let Some(renderer) = &self.renderer {
            if let Some(png) = renderer.render(page_title, html).await {
                return NodeOrSkip::Image(png);
            }
            warn!("长图渲染失败，版本更新播报降级为纯文本节点");
        }
        let mut text = format!("{text_header}\n");
        text.push_str(&blocks_to_text(&blocks));
        truncate_chars(&mut text, TEXT_FALLBACK_MAX_CHARS);
        NodeOrSkip::Text(text)
    }

    fn text_node(&self, text: &str) -> Value {
        json!({
            "type": "node",
            "data": {
                "uin": self.node_uin(),
                "nickname": NODE_NICKNAME,
                "content": [ { "type": "text", "data": { "text": text } } ],
            }
        })
    }

    fn image_node(&self, png: &[u8], nickname: &str) -> Value {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(png);
        json!({
            "type": "node",
            "data": {
                "uin": self.node_uin(),
                "nickname": nickname,
                "content": [ { "type": "image", "data": { "file": format!("base64://{encoded}") } } ],
            }
        })
    }

    /// 合并转发节点的发送者 QQ 号（NapCat 自定义节点要求）。`self_id` 未配置
    /// （0）时兜底一个占位号，避免 uin=0 被实现方拒绝。
    fn node_uin(&self) -> String {
        if self.bot_uin > 0 {
            self.bot_uin.to_string()
        } else {
            "10000".into()
        }
    }
}

enum NodeOrSkip {
    Image(Vec<u8>),
    Text(String),
    Skip,
}

#[derive(Deserialize)]
struct FeedList {
    #[serde(default)]
    entries: Vec<FeedEntry>,
}

#[derive(Deserialize)]
struct EntryBody {
    #[serde(default)]
    body: String,
}

/// HTML 粗剥离成文本块（不写完整解析器、不引解析库）：
/// h1~h6 → Title，p → Paragraph，li → Item，br → 空白；
/// a/strong/em/code/span 等只去标签保留文字；其余标签一并剥壳。
fn strip_html_blocks(html: &str) -> Vec<Block> {
    #[derive(Clone, Copy)]
    enum Kind {
        Title,
        Para,
        Item,
    }
    fn flush(out: &mut Vec<Block>, cur: &mut String, kind: &mut Option<Kind>) {
        let text = normalize_ws(cur);
        if !text.is_empty() {
            out.push(match kind.unwrap_or(Kind::Para) {
                Kind::Title => Block::Title(text),
                Kind::Para => Block::Paragraph(text),
                Kind::Item => Block::Item(text),
            });
        }
        cur.clear();
        *kind = None;
    }

    let mut out = Vec::new();
    let mut cur = String::new();
    let mut kind: Option<Kind> = None;
    let mut in_tag = false;
    let mut tag = String::new();
    for ch in html.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let raw = tag.trim();
                let closing = raw.starts_with('/');
                let base = raw
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_end_matches('/')
                    .to_ascii_lowercase();
                match base.as_str() {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        flush(&mut out, &mut cur, &mut kind);
                        if !closing {
                            kind = Some(Kind::Title);
                        }
                    }
                    "p" => {
                        flush(&mut out, &mut cur, &mut kind);
                        if !closing {
                            kind = Some(Kind::Para);
                        }
                    }
                    "li" => {
                        flush(&mut out, &mut cur, &mut kind);
                        if !closing {
                            kind = Some(Kind::Item);
                        }
                    }
                    "br" => cur.push(' '),
                    _ => {}
                }
                tag.clear();
            } else {
                tag.push(ch);
            }
        } else if ch == '<' {
            in_tag = true;
            tag.clear();
        } else {
            cur.push(ch);
        }
    }
    flush(&mut out, &mut cur, &mut kind);
    out
}

/// 解常见实体 + 折叠空白。
fn normalize_ws(text: &str) -> String {
    let decoded = text
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 文本降级用：块拼成纯文本。
fn blocks_to_text(blocks: &[Block]) -> String {
    blocks
        .iter()
        .map(|b| match b {
            Block::Title(s) | Block::Paragraph(s) => s.clone(),
            Block::Item(s) => format!("• {s}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_chars(text: &mut String, max_chars: usize) {
    if let Some((cut, _)) = text.char_indices().nth(max_chars) {
        text.truncate(cut);
        text.push_str("…\n（内容过长已截断，见原文链接）");
    }
}

/// 剥 UTF-8 BOM（Mojang 部分端点的响应带 BOM，serde_json 不容忍）。
fn strip_bom(raw: &str) -> &str {
    raw.strip_prefix('\u{FEFF}').unwrap_or(raw)
}

fn preview(raw: &str) -> String {
    raw.chars().take(120).collect()
}

/// 播报子系统（v0.4 子服务边界）：轮询循环 + 健康报告。
pub struct PatchBroadcastSubsystem {
    broadcaster: Arc<PatchBroadcaster>,
    enabled: bool,
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl PatchBroadcastSubsystem {
    pub fn new(broadcaster: Arc<PatchBroadcaster>, enabled: bool) -> Self {
        Self {
            broadcaster,
            enabled,
            task: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait]
impl Subsystem for PatchBroadcastSubsystem {
    fn name(&self) -> &'static str {
        "patch-broadcast"
    }
    async fn start(&self) -> std::io::Result<()> {
        if !self.enabled {
            info!("版本更新播报未启用（patch_broadcast.enabled=false）");
            return Ok(());
        }
        let broadcaster = self.broadcaster.clone();
        let interval = Duration::from_secs(self.broadcaster.cfg.poll_interval_secs.max(60));
        let task = tokio::spawn(async move {
            loop {
                broadcaster.poll_once().await;
                tokio::time::sleep(interval).await;
            }
        });
        *self.task.lock().unwrap() = Some(task);
        info!(
            interval_secs = interval.as_secs(),
            "版本更新播报已启用（下限 60s，防打爆官方 feed）"
        );
        Ok(())
    }
    async fn stop(&self) {
        // std MutexGuard 不能跨 await：先取出句柄再等待
        let task = self.task.lock().unwrap().take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }
    fn health(&self) -> crate::subsystem::SubsystemHealth {
        if !self.enabled {
            return crate::subsystem::SubsystemHealth {
                name: self.name(),
                healthy: true,
                detail: "未启用".into(),
            };
        }
        crate::subsystem::SubsystemHealth {
            name: self.name(),
            healthy: true,
            detail: format!(
                "已启用（每 {}s 轮询官方 feed）",
                self.broadcaster.cfg.poll_interval_secs
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex as StdMutex;

    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::Router;

    use crate::config::LlmConfig;

    fn config(feed_url: String) -> PatchBroadcastConfig {
        PatchBroadcastConfig {
            enabled: true,
            feed_url,
            poll_interval_secs: 1800,
        }
    }

    fn llm(api_url: String) -> LlmClient {
        LlmClient::new(LlmConfig {
            api_url,
            api_key: String::new(),
            model: "test".into(),
            timeout_secs: 5,
            max_answer_chars: 100000,
            system_prompt: "test".into(),
        })
        .unwrap()
    }

    /// 记录 send_forward 调用的假出口。
    #[derive(Default)]
    struct FakeSink {
        sends: StdMutex<Vec<Value>>,
    }

    #[async_trait]
    impl PatchSendSink for FakeSink {
        async fn send_forward(&self, nodes: Value) -> bool {
            self.sends.lock().unwrap().push(nodes);
            true
        }
    }

    impl FakeSink {
        fn texts(&self) -> Vec<String> {
            self.sends
                .lock()
                .unwrap()
                .iter()
                .flat_map(|forward| {
                    forward.as_array().unwrap().iter().filter_map(|node| {
                        node["data"]["content"][0]["data"]["text"]
                            .as_str()
                            .map(String::from)
                    })
                })
                .collect()
        }
    }

    /// 返回固定 PNG 的假渲染器（记录收到的 HTML 供断言）。
    struct FakeRenderer {
        pages: StdMutex<Vec<String>>,
    }

    impl FakeRenderer {
        fn new() -> Arc<Self> {
            Arc::new(Self { pages: StdMutex::new(Vec::new()) })
        }
    }

    #[async_trait]
    impl PatchRenderer for FakeRenderer {
        async fn render(&self, _title: &str, html: &str) -> Option<Vec<u8>> {
            self.pages.lock().unwrap().push(html.to_string());
            Some(vec![1, 2, 3])
        }
    }

    // ---------- mock feed / mock LLM ----------

    struct FeedState {
        /// (feed 列表 JSON, id → 正文 JSON)
        list: StdMutex<String>,
        bodies: StdMutex<std::collections::HashMap<String, String>>,
    }

    async fn feed_list(State(st): State<Arc<FeedState>>) -> impl IntoResponse {
        st.list.lock().unwrap().clone()
    }

    async fn feed_body(
        State(st): State<Arc<FeedState>>,
        axum::extract::Path(id): axum::extract::Path<String>,
    ) -> impl IntoResponse {
        // contentPath 是 "javaPatchNotes/{id}.json"，路径参数带后缀，剥掉再查
        let id = id.trim_end_matches(".json");
        st.bodies
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .map_or((StatusCode::NOT_FOUND, "missing".into()), |body| {
                (StatusCode::OK, body)
            })
    }

    fn entry_json(id: &str) -> Value {
        json!({
            "id": id, "version": id, "title": format!("Minecraft {id}"),
            "type": "snapshot", "date": "2026-09-22T13:38:53.000Z",
            "contentPath": format!("javaPatchNotes/{id}.json")
        })
    }

    fn body_json(id: &str) -> String {
        json!({ "body": format!("<p>{id} 的更新说明</p><ul><li>修复了一些问题</li></ul>") }).to_string()
    }

    async fn spawn_feed() -> (String, Arc<FeedState>) {
        let state = Arc::new(FeedState {
            list: StdMutex::new(json!({ "entries": [] }).to_string()),
            bodies: StdMutex::new(std::collections::HashMap::new()),
        });
        let app = Router::new()
            .route("/v2/javaPatchNotes.json", get(feed_list))
            .route("/v2/javaPatchNotes/{id}", get(feed_body))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/v2/javaPatchNotes.json"), state)
    }

    async fn spawn_llm(translated: &'static str) -> String {
        use axum::Json;
        let app = Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move |Json(_): Json<Value>| async move {
                Json(json!({
                    "choices": [ { "message": { "role": "assistant", "content": translated } } ]
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/v1/chat/completions")
    }

    fn broadcaster(
        cfg: PatchBroadcastConfig,
        state: Arc<StateStore>,
        llm: Option<LlmClient>,
        renderer: Option<Arc<dyn PatchRenderer>>,
        sink: Arc<FakeSink>,
    ) -> PatchBroadcaster {
        PatchBroadcaster::new(cfg, state, llm, renderer, sink, 10000)
    }

    // ---------- 用例 ----------

    /// 全链路（无 LLM、无渲染器）：首启基线不播报 → 新版本播报文本节点
    /// → 重复轮询不再播报；正文缺失时降级播报（标题 + 链接）并标记。
    #[tokio::test]
    async fn broadcast_full_flow_baseline_detection_and_retry() {
        let (feed_url, feed) = spawn_feed().await;
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateStore::new(dir.path().join("state.json")));
        let sink = Arc::new(FakeSink::default());
        let bc = broadcaster(config(feed_url), state.clone(), None, None, sink.clone());

        // 首启：feed 有 1 条 → 只记基线，不发送
        *feed.list.lock().unwrap() = json!({ "entries": [entry_json("a1")] }).to_string();
        feed.bodies
            .lock()
            .unwrap()
            .insert("a1".into(), body_json("a1"));
        bc.poll_once().await;
        assert!(sink.sends.lock().unwrap().is_empty(), "基线不得播报");
        assert!(state.patch_announced("a1"));

        // 新版本出现 → 播报（无 LLM 无渲染器 → 文本节点：标题 + 原文摘录 + 链接）
        *feed.list.lock().unwrap() =
            json!({ "entries": [entry_json("b2"), entry_json("a1")] }).to_string();
        feed.bodies
            .lock()
            .unwrap()
            .insert("b2".into(), body_json("b2"));
        bc.poll_once().await;
        assert_eq!(sink.sends.lock().unwrap().len(), 1);
        let texts = sink.texts().join("\n");
        assert!(
            texts.contains("Minecraft b2"),
            "节点文本缺标题，实际: {texts}"
        );
        assert!(texts.contains("修复了一些问题"), "原文摘录应进节点");
        assert!(texts.contains(PATCH_NOTES_PAGE));
        assert!(state.patch_announced("b2"));

        // 再轮询：无新版本
        bc.poll_once().await;
        assert_eq!(sink.sends.lock().unwrap().len(), 1);

        // body 拉取失败：发送会成功（文本降级），但这里直接移除 body →
        // 正文缺失 → 节点缺失但发送仍成功（降级链）——验证 Skip 分支
        *feed.list.lock().unwrap() =
            json!({ "entries": [entry_json("c3"), entry_json("b2")] }).to_string();
        bc.poll_once().await;
        assert_eq!(sink.sends.lock().unwrap().len(), 2);
        let texts = sink.texts().join("\n");
        assert!(texts.contains("Minecraft c3"));
        assert!(state.patch_announced("c3"));
    }

    /// 带渲染器 + LLM：两个长图节点（译后 + 原文）+ 图片 base64 节点格式。
    #[tokio::test]
    async fn broadcast_with_renderer_and_llm_sends_image_nodes() {
        let (feed_url, feed) = spawn_feed().await;
        let url = spawn_llm("<p>中文翻译内容</p>").await;
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateStore::new(dir.path().join("state.json")));
        let sink = Arc::new(FakeSink::default());
        let renderer = FakeRenderer::new();
        let bc = broadcaster(
            config(feed_url),
            state.clone(),
            Some(llm(url)),
            Some(renderer.clone()),
            sink.clone(),
        );

        // 基线
        *feed.list.lock().unwrap() = json!({ "entries": [entry_json("old")] }).to_string();
        feed.bodies
            .lock()
            .unwrap()
            .insert("old".into(), body_json("old"));
        bc.poll_once().await;

        // 新版本 → 译后图 + 原文图
        *feed.list.lock().unwrap() = json!({ "entries": [entry_json("new")] }).to_string();
        feed.bodies
            .lock()
            .unwrap()
            .insert("new".into(), body_json("new"));
        bc.poll_once().await;

        assert_eq!(sink.sends.lock().unwrap().len(), 1);
        let forward = sink.sends.lock().unwrap()[0].clone();
        let nodes = forward.as_array().unwrap();
        // 标题节点 + 译后图节点 + 原文图节点 + 链接节点
        assert_eq!(nodes.len(), 4);
        assert!(nodes[1]["data"]["content"][0]["data"]["file"]
            .as_str()
            .unwrap()
            .starts_with("base64://"));
        assert_eq!(nodes[1]["data"]["nickname"], "🇨🇳 中文翻译");
        assert_eq!(nodes[2]["data"]["nickname"], "🇬🇧 官方原文");
        // 渲染器收到两页 HTML，译文页含 LLM 输出
        let pages = renderer.pages.lock().unwrap();
        assert_eq!(pages.len(), 2);
        assert!(pages[0].contains("中文翻译内容"));
        assert!(pages[1].contains("new 的更新说明"));
    }

    /// 播报顺序：两个新版本按从旧到新发。
    #[tokio::test]
    async fn broadcast_sends_oldest_first() {
        let (feed_url, feed) = spawn_feed().await;
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateStore::new(dir.path().join("state.json")));
        let sink = Arc::new(FakeSink::default());
        let bc = broadcaster(config(feed_url), state.clone(), None, None, sink.clone());

        *feed.list.lock().unwrap() = json!({ "entries": [entry_json("old")] }).to_string();
        feed.bodies
            .lock()
            .unwrap()
            .insert("old".into(), body_json("old"));
        bc.poll_once().await;

        // 同一批出现两个新版本（feed 最新在前）
        *feed.list.lock().unwrap() =
            json!({ "entries": [entry_json("n2"), entry_json("n1"), entry_json("old")] })
                .to_string();
        bc.poll_once().await;

        let sends = sink.sends.lock().unwrap();
        assert_eq!(sends.len(), 2);
        let first = sends[0].as_array().unwrap()[0]["data"]["content"][0]["data"]["text"]
            .as_str()
            .unwrap();
        let second = sends[1].as_array().unwrap()[0]["data"]["content"][0]["data"]["text"]
            .as_str()
            .unwrap();
        assert!(first.contains("n1"), "先发旧的: {first}");
        assert!(second.contains("n2"), "后发新的: {second}");
    }

    /// 风暴防护：已播报条目从 feed 消失（模拟 feed 裁剪/重置）导致整份列表
    /// 都「未播报」时，超过单轮安全上限 → 全部记基线、一条不发。
    #[tokio::test]
    async fn broadcast_storm_guard_marks_without_announcing() {
        let (feed_url, feed) = spawn_feed().await;
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateStore::new(dir.path().join("state.json")));
        let sink = Arc::new(FakeSink::default());
        let bc = broadcaster(config(feed_url), state.clone(), None, None, sink.clone());

        // 正常基线
        *feed.list.lock().unwrap() = json!({ "entries": [entry_json("old")] }).to_string();
        bc.poll_once().await;

        // feed 重置：15 条全新条目，"old" 消失
        let burst: Vec<Value> = (0..15).map(|i| entry_json(&format!("x{i}"))).collect();
        *feed.list.lock().unwrap() = json!({ "entries": burst }).to_string();
        bc.poll_once().await;

        assert!(sink.sends.lock().unwrap().is_empty(), "风暴不得播报");
        assert!(state.patch_announced("x0") && state.patch_announced("x14"));
        // "old" 保持已播报（保留上限 50 条内不会淘汰）
        assert!(state.patch_announced("old"));
    }

    /// feed_base_url：文件名段剥离 + 目录写法与裸主机写法的边界。
    #[test]
    fn feed_base_url_handles_directory_forms() {
        let make = |feed_url: &str| {
            let bc = PatchBroadcaster::new(
                config(feed_url.into()),
                Arc::new(StateStore::new("/tmp/unused-state.json")),
                None,
                None,
                Arc::new(FakeSink::default()),
                0,
            );
            bc.feed_base_url()
        };
        assert_eq!(
            make("https://launchercontent.mojang.com/v2/javaPatchNotes.json"),
            "https://launchercontent.mojang.com/v2"
        );
        assert_eq!(
            make("https://launchercontent.mojang.com/v2/"),
            "https://launchercontent.mojang.com/v2"
        );
        // 目录写法（末段无扩展名）：不剥离
        assert_eq!(
            make("https://mirror.example.com/mojang/v2"),
            "https://mirror.example.com/mojang/v2"
        );
        // 裸主机：不产生 "https:" 退化
        assert_eq!(make("https://host.example.com"), "https://host.example.com");
    }

    /// HTML 剥离：块分类（标题/段落/列表）+ 去内联标签 + 解实体。
    #[test]
    fn strip_html_blocks_classifies_and_strips() {
        let html = "<h1>标题</h1><p>第一段</p>\n<p>第二 <b>加粗</b> 与 <a href=\"x\">链接</a>&amp;实体</p><ul><li>条目一</li><li>条目二</li></ul>";
        let blocks = strip_html_blocks(html);
        assert!(matches!(&blocks[0], Block::Title(t) if t == "标题"));
        assert!(matches!(&blocks[1], Block::Paragraph(t) if t == "第一段"));
        assert!(matches!(&blocks[3], Block::Item(t) if t == "条目一"));
        let text = blocks_to_text(&blocks);
        assert!(text.contains("第二 加粗 与 链接&实体"));
        assert!(text.contains("• 条目一"));
        assert!(!text.contains('<'));
    }

    /// BOM 容忍：feed / 正文都带 BOM 也能解析。
    #[tokio::test]
    async fn broadcast_tolerates_bom() {
        let (feed_url, feed) = spawn_feed().await;
        *feed.list.lock().unwrap() = format!("\u{FEFF}{}", json!({ "entries": [entry_json("a1")] }));
        feed.bodies
            .lock()
            .unwrap()
            .insert("a1".into(), format!("\u{FEFF}{}", body_json("a1")));
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateStore::new(dir.path().join("state.json")));
        let sink = Arc::new(FakeSink::default());
        let bc = broadcaster(config(feed_url), state.clone(), None, None, sink.clone());

        bc.poll_once().await; // 基线
        assert!(state.patch_announced("a1"));

        // 新版本 + BOM 正文 → 播报成功且摘录含正文
        *feed.list.lock().unwrap() =
            format!("\u{FEFF}{}", json!({ "entries": [entry_json("b2"), entry_json("a1")] }));
        feed.bodies
            .lock()
            .unwrap()
            .insert("b2".into(), format!("\u{FEFF}{}", body_json("b2")));
        bc.poll_once().await;
        let texts = sink.texts().join("\n");
        assert!(texts.contains("b2 的更新说明"), "实际: {texts}");
    }
}
