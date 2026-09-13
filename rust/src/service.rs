//! 服务装配与生命周期：把 OneBot 服务端、chatroom 双向同步、ChatBridge、玩家追踪、
//! 命令路由接到一起（对应 Python 版 `main.py` 的 `BridgeService`）。
//!
//! 出站能力通过 [`Hub`] 暴露，回源应答通过 [`ReplySink`]——两者都是未来 agent
//! 技能的依赖边界（技能不感知消息来自哪一端、经哪条协议发出）。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine as _;
use serde::Serialize;
use serde_json::Value;
use tracing::{error, info, warn};

use crate::adapters::chatbridge::ChatBridgeClient;
use crate::adapters::chatroom_auth::ChatroomAuth;
use crate::adapters::chatroom_read::{AuthTokenProvider, ChatroomReader};
use crate::adapters::forward_api::{ForwardApi, PostMessage, PostSource};
use crate::adapters::onebot::{
    GroupMessage, GroupMessageHandler, OneBotConnection, OneBotServer,
};
use crate::api::ApiServer;
use crate::config::AppConfig;
use crate::router::handlers::{QqForwardRelay, SlashCommandAdapter, SnapshotRelay};
use crate::router::{
    CommandHandler, CommandInfo, CommandRouter, DispatchCtx, Hub, InboundMessage, ReplySink, Source,
};
use crate::services::commands::CommandService;
use crate::services::forwarder::{ChatroomForwarder, ForwarderStats, ImageResolver};
use crate::services::player_events::PlayerEventDetector;
use crate::state::StateStore;

const CHATROOM_READ_INTERVAL: u64 = 10;
/// 近期消息环形缓冲上限（Web UI / HTTP API 的数据源，仅内存不落盘）
const RECENT_MESSAGES_CAP: usize = 200;
/// 单条消息在缓冲里的文本长度上限
const RECENT_MESSAGE_TEXT_MAX: usize = 500;

/// 近期消息记录（HTTP API `/api/messages` 的条目）。
#[derive(Debug, Clone, Serialize)]
pub struct MessageRecord {
    pub id: u64,
    /// 毫秒时间戳
    pub timestamp: i64,
    /// 来源端：qq / game / chatroom
    pub source: String,
    /// 发送者显示名（游戏系统广播可能为空）
    pub from: String,
    pub text: String,
}

#[derive(Default)]
struct RecentInner {
    next_id: u64,
    items: VecDeque<MessageRecord>,
}

/// 三端入站消息的内存环形缓冲：给 Web UI 与外部站点一个轻量实时数据源。
/// 只在内存里保留最近 [`RECENT_MESSAGES_CAP`] 条，重启即清空（去重与游标仍在 state.json）。
pub struct RecentLog {
    cap: usize,
    inner: Mutex<RecentInner>,
}

impl RecentLog {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            inner: Mutex::new(RecentInner::default()),
        }
    }

    pub fn push(&self, source: &str, from: &str, text: &str) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let id = inner.next_id + 1;
        inner.next_id = id;
        inner.items.push_back(MessageRecord {
            id,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
            source: source.to_string(),
            from: from.to_string(),
            text: truncate_chars(text, RECENT_MESSAGE_TEXT_MAX),
        });
        while inner.items.len() > self.cap {
            inner.items.pop_front();
        }
    }

    /// 最近 `limit` 条，按从旧到新排列。
    pub fn latest(&self, limit: usize) -> Vec<MessageRecord> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        let skip = inner.items.len().saturating_sub(limit);
        inner.items.iter().skip(skip).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|i| i.items.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// 游戏侧事件统一写向 chatroom：合成 `source_id`（时间戳 + 自增序号），
/// 失败只记日志不重试（服务端不去重，重试会造成重复写入）。
async fn post_game_message(
    api: &ForwardApi,
    seq: &AtomicU64,
    prefix: &str,
    content: &str,
    nickname: &str,
    username: &str,
) -> bool {
    if !api.configured() {
        return false;
    }
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq_no = seq.fetch_add(1, Ordering::Relaxed) + 1;
    let mut message = PostMessage::new(PostSource::Game);
    message.content = content.to_string();
    message.source_message_id = format!("{prefix}-{millis}-{seq_no}");
    message.sender_username = username.to_string();
    message.nickname = nickname.to_string();
    match api.post_message(&message).await {
        Ok(_) => true,
        Err(err) => {
            warn!("游戏侧消息转发失败: {err}");
            false
        }
    }
}

pub struct BridgeService {
    cfg: AppConfig,
    state: Arc<StateStore>,
    forward_api: Arc<ForwardApi>,
    forwarder: Arc<ChatroomForwarder>,
    auth: Arc<ChatroomAuth>,
    reader: Arc<ChatroomReader>,
    detector: PlayerEventDetector,
    commands: Arc<CommandService>,
    server: Arc<OneBotServer>,
    chatbridge: Option<Arc<ChatBridgeClient>>,
    router: Arc<CommandRouter>,
    group_ids: Vec<i64>,
    game_seq: Arc<AtomicU64>,
    recent: Arc<RecentLog>,
    api: Arc<ApiServer>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl BridgeService {
    /// 装配全部子系统。构造函数都是同步的（reqwest client 构建），只有
    /// OneBot 回调经 `Weak` 解循环；`ChatBridge` 回调在构造完成后挂接。
    pub fn new(cfg: AppConfig) -> Result<Arc<Self>, reqwest::Error> {
        let state = Arc::new(StateStore::new(&cfg.state_path));
        let forward_api = Arc::new(ForwardApi::new(
            &cfg.chatroom.base_url,
            &cfg.chatroom.forward_token,
            cfg.chatroom.channel_id,
        )?);
        let forwarder = Arc::new(ChatroomForwarder::new(
            forward_api.clone(),
            state.clone(),
            cfg.chatroom.qq_sync_enabled,
            cfg.onebot.self_id,
        ));
        let auth = Arc::new(ChatroomAuth::new(
            &cfg.chatroom.base_url,
            &cfg.chatroom.refresh_token,
            Some(state.clone()),
        )?);
        let reader = Arc::new(ChatroomReader::new(
            &cfg.chatroom.base_url,
            cfg.chatroom.channel_id,
            auth.clone() as Arc<dyn AuthTokenProvider>,
            Some(state.clone()),
        )?);

        let game_seq = Arc::new(AtomicU64::new(0));

        let commands = Arc::new(CommandService::new(
            cfg.commands.group_allow_all,
            cfg.commands.allow_from.clone(),
            cfg.commands.status_image,
            &cfg.chatroom.voice_api,
            &cfg.chatroom.status_api,
            cfg.chatroom.server_address_pairs(),
        )?);

        // 玩家上下线推送：ChatBridge 系统广播 + 可配置正则（事件驱动，
        // 替代旧的状态网站轮询差分——轮询会丢单次事件）
        let detector = match PlayerEventDetector::new(
            &cfg.chatroom.player_join_pattern,
            &cfg.chatroom.player_quit_pattern,
        ) {
            Ok(detector) => detector,
            Err(err) => {
                warn!("玩家上下线正则非法，上下线推送已禁用: {err}");
                PlayerEventDetector::disabled()
            }
        };

        let mut group_ids: Vec<i64> = cfg.group_ids().into_iter().collect();
        group_ids.sort_unstable();

        let chatbridge = if cfg.chatbridge.enabled && !cfg.chatbridge.host.is_empty() {
            Some(Arc::new(ChatBridgeClient::new(
                &cfg.chatbridge.host,
                cfg.chatbridge.port,
                &cfg.chatbridge.name,
                &cfg.chatbridge.password,
                &cfg.chatbridge.aes_key,
            )))
        } else {
            None
        };

        let mut router = CommandRouter::new();
        router.register(Arc::new(SlashCommandAdapter {
            commands: commands.clone(),
        }));
        router.register(Arc::new(QqForwardRelay {
            enabled: cfg.chatroom.qq_forward_enabled,
        }));
        router.register(Arc::new(SnapshotRelay {
            enabled: cfg.chatroom.qq_forward_enabled,
            sender: cfg.chatroom.snapshot_sender.clone(),
            prefix: cfg.chatroom.snapshot_prefix.clone(),
        }));

        // agent 技能：全部由配置声明（见 docs/agent-design.md），trigger 长者先注册
        // 避免前缀遮蔽（如 !tmc 先于 !tm）
        let mut agent_skills = crate::agent::build_skills(&cfg.agent);
        agent_skills.sort_by(|a, b| {
            b.info()
                .trigger
                .len()
                .cmp(&a.info().trigger.len())
        });
        for skill in agent_skills {
            let meta = skill.info();
            info!("注册 agent 技能: {} ({})", meta.name, meta.trigger);
            router.register(Arc::new(skill));
        }

        let service = Arc::new_cyclic(|weak: &Weak<BridgeService>| {
            let handler: GroupMessageHandler = {
                let weak = weak.clone();
                Arc::new(move |conn, msg| {
                    let weak = weak.clone();
                    Box::pin(async move {
                        if let Some(service) = weak.upgrade() {
                            service.handle_group_message(conn, msg).await;
                        }
                    })
                })
            };
            BridgeService {
                server: Arc::new(OneBotServer::new(
                    &cfg.onebot.listen_host,
                    cfg.onebot.listen_port,
                    &cfg.onebot.path,
                    &cfg.onebot.access_token,
                    handler,
                )),
                api: Arc::new(ApiServer::new(weak.clone(), &cfg.api)),
                cfg,
                state,
                forward_api,
                forwarder,
                auth,
                reader,
                detector,
                commands,
                chatbridge,
                router: Arc::new(router),
                group_ids,
                game_seq,
                recent: Arc::new(RecentLog::new(RECENT_MESSAGES_CAP)),
                tasks: Mutex::new(Vec::new()),
            }
        });

        if let Some(client) = &service.chatbridge {
            let weak = Arc::downgrade(&service);
            client.set_on_chat(Arc::new(move |sender, author, message| {
                let weak = weak.clone();
                Box::pin(async move {
                    if let Some(service) = weak.upgrade() {
                        service.on_game_chat(&sender, &author, &message).await;
                    }
                })
            }));
        }

        Ok(service)
    }

    pub fn state(&self) -> Arc<StateStore> {
        self.state.clone()
    }

    pub fn server(&self) -> Arc<OneBotServer> {
        self.server.clone()
    }

    // --- HTTP API 读取的状态访问器 ---
    pub fn forwarder_stats(&self) -> ForwarderStats {
        self.forwarder.stats()
    }

    /// 玩家上下线推送是否已启用（正则配置齐全）。
    pub fn player_events_enabled(&self) -> bool {
        self.detector.enabled()
    }

    pub fn command_stats(&self) -> std::collections::HashMap<String, u64> {
        self.commands.stats()
    }

    pub fn chatbridge_enabled(&self) -> bool {
        self.chatbridge.is_some()
    }

    pub fn chatbridge_connected(&self) -> bool {
        self.chatbridge
            .as_ref()
            .map(|c| c.is_connected())
            .unwrap_or(false)
    }

    /// 已注册的命令能力清单（Web UI 展示与未来智能路由的能力发现共用）。
    pub fn capabilities(&self) -> Vec<CommandInfo> {
        self.router.handlers().iter().map(|h| h.info()).collect()
    }

    pub fn recent_log(&self) -> Arc<RecentLog> {
        self.recent.clone()
    }

    pub fn api_local_addr(&self) -> Option<std::net::SocketAddr> {
        self.api.local_addr()
    }

    // --- 生命周期 ---
    pub async fn start(self: &Arc<Self>) -> std::io::Result<()> {
        self.server.start().await?;
        {
            let mut tasks = self.tasks.lock().unwrap();
            if self.auth.has_refresh_token() {
                let service = self.clone();
                tasks.push(tokio::spawn(async move { service.chatroom_loop().await }));
            } else {
                warn!("未配置 refresh_token：!q 与 chatroom→游戏 已禁用");
            }
            if let Some(client) = &self.chatbridge {
                let client = client.clone();
                tasks.push(tokio::spawn(async move { client.run().await }));
            }
        }
        if self.cfg.api.enabled {
            self.api.start().await?;
            if self.cfg.api.access_token.is_empty() {
                warn!("API 写接口已禁用：请配置 api.access_token 后重启（读接口不受影响）");
            }
            if let Some(addr) = self.api.local_addr() {
                info!("HTTP API 已就绪: http://{addr}");
            }
        }
        let snapshot = self.state.snapshot();
        info!(
            "服务已启动；状态: forwarded={} cursor={} refresh_token={}",
            snapshot.forwarded_count, snapshot.last_read_message_id, snapshot.has_refresh_token
        );
        Ok(())
    }

    pub async fn stop(&self) {
        let tasks: Vec<_> = self.tasks.lock().unwrap().drain(..).collect();
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
        if let Some(client) = &self.chatbridge {
            client.stop();
        }
        self.api.stop().await;
        self.server.stop().await;
        info!("服务已停止");
    }

    // --- QQ 群事件 ---
    pub async fn handle_group_message(self: Arc<Self>, conn: Arc<OneBotConnection>, msg: GroupMessage) {
        if !self.group_ids.is_empty() && !self.group_ids.contains(&msg.group_id) {
            return;
        }
        self.recent.push("qq", &msg.display_name(), &msg.text());

        let origin = QQReplySink {
            conn: conn.clone(),
            group_id: msg.group_id,
        };
        let inbound = InboundMessage {
            source: Source::QQ,
            text: msg.text(),
            group_id: msg.group_id,
            user_id: msg.user_id,
            display_name: msg.display_name(),
        };
        let ctx = DispatchCtx {
            hub: self.as_ref(),
            origin: &origin,
            msg: &inbound,
        };
        if self.router.dispatch(&ctx).await {
            return; // 命令已消费（/chatroom、/server、未来 agent 技能）
        }

        let resolver = ConnImageResolver(conn);
        self.forwarder.handle(&resolver, &msg).await;
    }

    /// 向配置的 QQ 群发文本（`!q` / 快照通知中继用）。
    pub async fn send_to_qq_groups(&self, text: &str) -> bool {
        let Some(conn) = self.server.connection() else {
            warn!("QQ 未连接，转发跳过: {}", truncate_chars(text, 40));
            return false;
        };
        let mut sent = false;
        for group_id in &self.group_ids {
            match conn.send_group_text(*group_id, text).await {
                Ok(_) => {
                    sent = true;
                    info!("转发到 QQ 群 {group_id}: {}", truncate_chars(text, 60));
                }
                Err(err) => error!("转发到 QQ 群 {group_id} 失败: {err}"),
            }
        }
        sent
    }

    // --- chatroom 读方向 ---
    async fn chatroom_loop(self: Arc<Self>) {
        loop {
            let messages = self.reader.poll_once().await;
            for message in messages {
                self.dispatch_chatroom_message(message).await;
            }
            tokio::time::sleep(Duration::from_secs(CHATROOM_READ_INTERVAL)).await;
        }
    }

    async fn dispatch_chatroom_message(&self, message: Value) {
        let content = message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if content.is_empty() {
            return;
        }
        let username = message
            .get("username")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if self.reader.is_own_message(&message) {
            return;
        }
        self.recent.push("chatroom", &username, &content);

        // 路由（!q 中继等）；消费语义在这里不适用——原始消息仍照常广播到游戏
        let inbound = InboundMessage {
            source: Source::Chatroom {
                username: username.clone(),
            },
            text: content.clone(),
            group_id: 0,
            user_id: 0,
            display_name: username.clone(),
        };
        let sink = ChatroomReplySink::new(&self.forward_api, &self.game_seq);
        let ctx = DispatchCtx {
            hub: self,
            origin: &sink,
            msg: &inbound,
        };
        self.router.dispatch(&ctx).await;

        if self.cfg.chatroom.qq_to_game_enabled {
            if let Some(client) = &self.chatbridge {
                if client.is_connected() {
                    let game_msg = if username.is_empty() {
                        format!("[Chatroom] {content}")
                    } else {
                        format!("[Chatroom] {username}: {content}")
                    };
                    client.broadcast_chat(&game_msg, "").await;
                }
            }
        }
    }

    // --- 游戏 → chatroom ---
    pub async fn on_game_chat(&self, sender: &str, author: &str, message: &str) {
        let content = message.trim();
        if content.is_empty() {
            return;
        }
        let text = if !author.is_empty() {
            format!("🎮 [{sender}] {author}: {content}")
        } else {
            format!("🟢 {content}")
        };
        let nickname = if author.is_empty() { sender } else { author };
        self.recent.push("game", nickname, content);
        self.forward_game_chat(&text, nickname, author).await;

        // 路由（!q / 快照通知中继）；原始消息上面已照常转发 chatroom
        let inbound = InboundMessage {
            source: Source::Game {
                sender: sender.to_string(),
                author: author.to_string(),
            },
            text: content.to_string(),
            group_id: 0,
            user_id: 0,
            display_name: nickname.to_string(),
        };
        let sink = GameReplySink(self.chatbridge.clone());
        let ctx = DispatchCtx {
            hub: self,
            origin: &sink,
            msg: &inbound,
        };
        self.router.dispatch(&ctx).await;

        // 玩家上下线推送（ChatBridge 事件驱动，替代旧的状态网站轮询差分——
        // 轮询可能丢单次事件）。防伪造门：只认系统广播（author 为空）或
        // 玩家自报（author == 玩家名），他人冒充「xx 加入了游戏」不会触发。
        if self.detector.enabled() {
            if let Some(event) = self.detector.detect(sender, content) {
                if author.is_empty() || author == event.player {
                    let text = event.push_text();
                    self.send_to_qq_groups(&text).await;
                }
            }
        }
    }

    async fn forward_game_chat(&self, content: &str, nickname: &str, username: &str) {
        post_game_message(
            &self.forward_api,
            &self.game_seq,
            "game-chat",
            content,
            nickname,
            username,
        )
        .await;
    }
}

// --- 出站能力：router::Hub 的服务端实现 ---
#[async_trait]
impl Hub for BridgeService {
    async fn qq_send_text(&self, group_id: Option<i64>, text: &str) -> bool {
        match group_id {
            Some(group_id) => {
                let Some(conn) = self.server.connection() else {
                    warn!("QQ 未连接，定向发送跳过");
                    return false;
                };
                match conn.send_group_text(group_id, text).await {
                    Ok(_) => true,
                    Err(err) => {
                        error!("发送 QQ 文本到群 {group_id} 失败: {err}");
                        false
                    }
                }
            }
            None => self.send_to_qq_groups(text).await,
        }
    }

    async fn qq_send_image(&self, group_id: i64, png: &[u8]) -> bool {
        let Some(conn) = self.server.connection() else {
            warn!("QQ 未连接，图片发送跳过");
            return false;
        };
        let data_uri = format!("base64://{}", base64::engine::general_purpose::STANDARD.encode(png));
        match conn.send_group_image(group_id, &data_uri).await {
            Ok(_) => true,
            Err(err) => {
                error!("发送 QQ 图片到群 {group_id} 失败: {err}");
                false
            }
        }
    }

    async fn game_broadcast(&self, text: &str) -> bool {
        match self.chatbridge.as_ref().filter(|c| c.is_connected()) {
            Some(client) => {
                client.broadcast_chat(text, "").await;
                true
            }
            None => false,
        }
    }

    async fn chatroom_post(
        &self,
        source: &str,
        content: &str,
        sender_username: &str,
        nickname: &str,
    ) -> bool {
        if !self.forward_api.configured() {
            return false;
        }
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let seq_no = self.game_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mut message = PostMessage::new(if source == "qq" {
            PostSource::QQ
        } else {
            PostSource::Game
        });
        message.content = content.to_string();
        message.source_message_id = format!("hub-{millis}-{seq_no}");
        message.sender_username = sender_username.to_string();
        message.nickname = nickname.to_string();
        match self.forward_api.post_message(&message).await {
            Ok(_) => true,
            Err(err) => {
                warn!("chatroom 写入失败: {err}");
                false
            }
        }
    }
}

// --- 回源应答 sink ---
/// 回复到 QQ 群（消息来源端）。
struct QQReplySink {
    conn: Arc<OneBotConnection>,
    group_id: i64,
}

#[async_trait]
impl ReplySink for QQReplySink {
    async fn send_text(&self, text: &str) -> bool {
        match self.conn.send_group_text(self.group_id, text).await {
            Ok(_) => true,
            Err(err) => {
                error!("发送命令响应失败: {err}");
                false
            }
        }
    }

    async fn send_image(&self, png: &[u8]) -> bool {
        let data_uri = format!("base64://{}", base64::engine::general_purpose::STANDARD.encode(png));
        match self.conn.send_group_image(self.group_id, &data_uri).await {
            Ok(_) => true,
            Err(err) => {
                error!("发送命令图片响应失败: {err}");
                false
            }
        }
    }
}

/// 回复到游戏（广播）。
struct GameReplySink(Option<Arc<ChatBridgeClient>>);

#[async_trait]
impl ReplySink for GameReplySink {
    async fn send_text(&self, text: &str) -> bool {
        match self.0.as_ref().filter(|c| c.is_connected()) {
            Some(client) => {
                client.broadcast_chat(text, "").await;
                true
            }
            None => false,
        }
    }
}

/// 回复到 chatroom 频道：经 Forward API 以 bot 身份写回
/// （agent 技能回答 chatroom 端提问时的「落库」路径）。
struct ChatroomReplySink {
    api: Arc<ForwardApi>,
    seq: Arc<AtomicU64>,
}

impl ChatroomReplySink {
    fn new(api: &Arc<ForwardApi>, seq: &Arc<AtomicU64>) -> Self {
        Self {
            api: api.clone(),
            seq: seq.clone(),
        }
    }
}

#[async_trait]
impl ReplySink for ChatroomReplySink {
    async fn send_text(&self, text: &str) -> bool {
        if !self.api.configured() {
            warn!("chatroom 回源应答跳过：Forward API 未配置");
            return false;
        }
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let seq_no = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mut message = PostMessage::new(PostSource::Game);
        message.content = text.to_string();
        message.source_message_id = format!("agent-reply-{millis}-{seq_no}");
        // sender_username 留空：服务端以 bot 账号发布，归属由服务端映射决定
        match self.api.post_message(&message).await {
            Ok(_) => true,
            Err(err) => {
                warn!("chatroom 回源应答写入失败: {err}");
                false
            }
        }
    }
}

/// 图片引用解析：非 http 引用经 OneBot `get_image` 换取 URL。
struct ConnImageResolver(Arc<OneBotConnection>);

#[async_trait]
impl ImageResolver for ConnImageResolver {
    async fn resolve(&self, file_ref: &str) -> Option<String> {
        let info = self.0.get_image(file_ref).await.ok()?;
        info.get("url").and_then(Value::as_str).map(String::from)
    }
}

/// 读方向鉴权桥：`ChatroomAuth` 满足 `ChatroomReader` 所需的 trait。
#[async_trait]
impl AuthTokenProvider for ChatroomAuth {
    async fn ensure_token(&self) -> bool {
        ChatroomAuth::ensure_token(self).await
    }
    fn access_token(&self) -> String {
        ChatroomAuth::access_token(self)
    }
    fn user_id(&self) -> Option<i64> {
        ChatroomAuth::user_id(self)
    }
}
