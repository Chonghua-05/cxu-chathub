//! 服务装配与生命周期：把 OneBot 服务端、chatroom 双向同步、ChatBridge、玩家追踪、
//! 命令路由接到一起（对应 Python 版 `main.py` 的 `BridgeService`）。
//!
//! v0.4 起按子服务边界组织（见 [`crate::subsystem`]）：消息路径拆在同级的
//! [`qq`] / [`chatroom`] / [`game`] 子模块（`impl BridgeService` 的分区），
//! 有生命周期的部件统一注册进 [`Subsystem`] 清单——顺序 start、逆序 stop，
//! 健康快照汇入 `/healthz` 与 `/api/status` 的 `subsystems` 数组。
//!
//! 出站能力通过 [`Hub`] 暴露，回源应答通过 [`ReplySink`]——两者都是未来 agent
//! 技能的依赖边界（技能不感知消息来自哪一端、经哪条协议发出）。

mod chatroom;
mod game;
mod qq;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine as _;
use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::Value;
use tracing::{error, info, warn};

use crate::adapters::chatbridge::ChatBridgeClient;
use crate::adapters::chatroom_auth::ChatroomAuth;
use crate::adapters::chatroom_read::{AuthTokenProvider, ChatroomReader};
use crate::adapters::forward_api::{ForwardApi, PostMessage, PostSource};
use crate::adapters::onebot::{GroupMessageHandler, OneBotServer};
use crate::api::ApiServer;
use crate::config::AppConfig;
use crate::router::handlers::{QqForwardRelay, SlashCommandAdapter, SnapshotRelay};
use crate::router::{CommandHandler, CommandInfo, CommandRouter, Hub};
use crate::services::commands::CommandService;
use crate::services::forwarder::{ChatroomForwarder, ForwarderStats};
use crate::services::player_events::PlayerEventDetector;
use crate::state::StateStore;
use crate::subsystem::{Subsystem, SubsystemHealth};

/// chatroom 读方向轮询间隔（秒）。
const CHATROOM_POLL_SECS: u64 = 10;
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

pub struct BridgeService {
    cfg: AppConfig,
    state: Arc<StateStore>,
    forward_api: Arc<ForwardApi>,
    forwarder: Arc<ChatroomForwarder>,
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
    /// chatroom 轮询子服务的 typed 句柄（注册消息回调用；生命周期走 subsystems）。
    chatroom_poll: Arc<ChatroomPollSubsystem>,
    /// 全部子服务：统一生命周期（顺序 start、逆序 stop）与健康检查。
    subsystems: Vec<Arc<dyn Subsystem>>,
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
        let mut skill_arcs: Vec<Arc<dyn CommandHandler>> = Vec::new();
        for skill in agent_skills {
            let meta = skill.info();
            info!("注册 agent 技能: {} ({})", meta.name, meta.trigger);
            let arc: Arc<dyn CommandHandler> = Arc::new(skill);
            skill_arcs.push(arc.clone());
            router.register(arc);
        }

        // LLM 智能路由（灰度）：注册在最末——只有显式命令全部不认领的消息才会
        // 到这里；LLM 拒绝路由时消息照常进转发流水线。见 docs/agent-design.md §4
        let routable = skill_arcs
            .into_iter()
            .map(|handler| {
                let info = handler.info();
                crate::agent::routing::RoutableSkill { handler, info }
            })
            .collect();
        match crate::agent::routing::LlmSkillRouter::new(&cfg.agent, routable) {
            Ok(router_handler) => {
                info!(
                    groups = ?cfg.agent.routing.group_ids,
                    "LLM 智能路由已启用（灰度）"
                );
                router.register(Arc::new(router_handler));
            }
            Err(reason) => {
                if cfg.agent.routing.enabled {
                    warn!("LLM 智能路由未启用: {reason}");
                }
            }
        }

        // chatroom 读方向轮询子服务（!q / chatroom→游戏 的数据源；无 token 不启动）
        let chatroom_poll = Arc::new(ChatroomPollSubsystem::new(
            reader.clone(),
            auth.has_refresh_token(),
            Duration::from_secs(CHATROOM_POLL_SECS),
        ));

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
            let server = Arc::new(OneBotServer::new(
                &cfg.onebot.listen_host,
                cfg.onebot.listen_port,
                &cfg.onebot.path,
                &cfg.onebot.access_token,
                handler,
            ));
            let api = Arc::new(ApiServer::new(weak.clone(), &cfg.api));
            // 子服务清单：顺序 = start 顺序，stop 逆序；健康快照见 health_report()。
            // 玩家事件与命令响应无后台任务，只参与健康报告（start/stop 默认空实现）。
            let subsystems: Vec<Arc<dyn Subsystem>> = vec![
                Arc::new(OneBotSubsystem { server: server.clone() }),
                chatroom_poll.clone(),
                Arc::new(ChatBridgeSubsystem {
                    client: chatbridge.clone(),
                    task: Mutex::new(None),
                }),
                Arc::new(ApiSubsystem {
                    api: api.clone(),
                    enabled: cfg.api.enabled,
                    token_configured: !cfg.api.access_token.is_empty(),
                }),
                Arc::new(PlayerEventsSubsystem {
                    enabled: detector.enabled(),
                }),
                Arc::new(CommandResponderSubsystem),
            ];
            BridgeService {
                server,
                api,
                subsystems,
                chatroom_poll,
                cfg,
                state,
                forward_api,
                forwarder,
                reader,
                detector,
                commands,
                chatbridge,
                router: Arc::new(router),
                group_ids,
                game_seq,
                recent: Arc::new(RecentLog::new(RECENT_MESSAGES_CAP)),
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

        // chatroom 轮询的消息派发回调（Weak 解循环，与 OneBot handler 同款）
        let weak = Arc::downgrade(&service);
        service
            .chatroom_poll
            .set_on_messages(Arc::new(move |messages| {
                let weak = weak.clone();
                Box::pin(async move {
                    if let Some(service) = weak.upgrade() {
                        for message in messages {
                            service.dispatch_chatroom_message(message).await;
                        }
                    }
                })
            }));

        // /healthz 的子服务健康清单（Weak 解循环，避免 service→server→service 强引用环）
        let weak = Arc::downgrade(&service);
        service
            .server()
            .set_health_provider(Arc::new(move || {
                weak.upgrade()
                    .map(|service| service.health_report())
                    .unwrap_or_default()
            }));

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

    // --- 生命周期（子服务统一：顺序 start，逆序 stop） ---
    pub async fn start(self: &Arc<Self>) -> std::io::Result<()> {
        for subsystem in &self.subsystems {
            subsystem.start().await?;
        }
        let snapshot = self.state.snapshot();
        info!(
            "服务已启动；状态: forwarded={} cursor={} refresh_token={}",
            snapshot.forwarded_count, snapshot.last_read_message_id, snapshot.has_refresh_token
        );
        Ok(())
    }

    pub async fn stop(&self) {
        for subsystem in self.subsystems.iter().rev() {
            subsystem.stop().await;
        }
        info!("服务已停止");
    }

    /// 全部子服务的健康快照（/healthz 与 /api/status 的 subsystems 数据源）。
    pub fn health_report(&self) -> Vec<SubsystemHealth> {
        self.subsystems.iter().map(|s| s.health()).collect()
    }
}

// --- 子服务实现：统一生命周期与健康检查（见 crate::subsystem） ---

/// QQ 桥接：OneBot 反向 WS 服务端 + 群消息消费。
struct OneBotSubsystem {
    server: Arc<OneBotServer>,
}

#[async_trait]
impl Subsystem for OneBotSubsystem {
    fn name(&self) -> &'static str {
        "qq-bridge"
    }
    async fn start(&self) -> std::io::Result<()> {
        self.server.start().await
    }
    async fn stop(&self) {
        self.server.stop().await;
    }
    fn health(&self) -> SubsystemHealth {
        match self.server.connection() {
            Some(conn) => SubsystemHealth {
                name: self.name(),
                healthy: true,
                detail: format!("已连接 self_id={}", conn.self_id()),
            },
            None => SubsystemHealth {
                name: self.name(),
                healthy: false,
                detail: "OneBot 未连接（等待 NapCat 连入）".into(),
            },
        }
    }
}

/// chatroom 轮询拉到一批消息后的派发回调（service 装配后挂入）。
pub type ChatroomMessagesHandler = dyn Fn(Vec<Value>) -> BoxFuture<'static, ()> + Send + Sync;

/// chatroom 同步：读方向轮询（!q 中继 / chatroom→游戏 的数据源）。
struct ChatroomPollSubsystem {
    reader: Arc<ChatroomReader>,
    /// 无 refresh_token 时轮询不启动（与旧行为一致）。
    token_present: bool,
    interval: Duration,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    on_messages: Mutex<Option<Arc<ChatroomMessagesHandler>>>,
}

impl ChatroomPollSubsystem {
    fn new(reader: Arc<ChatroomReader>, token_present: bool, interval: Duration) -> Self {
        Self {
            reader,
            token_present,
            interval,
            task: Mutex::new(None),
            on_messages: Mutex::new(None),
        }
    }

    fn set_on_messages(&self, handler: Arc<ChatroomMessagesHandler>) {
        *self.on_messages.lock().unwrap() = Some(handler);
    }
}

#[async_trait]
impl Subsystem for ChatroomPollSubsystem {
    fn name(&self) -> &'static str {
        "chatroom-sync"
    }
    async fn start(&self) -> std::io::Result<()> {
        if !self.token_present {
            warn!("未配置 refresh_token：!q 与 chatroom→游戏 已禁用");
            return Ok(());
        }
        let reader = self.reader.clone();
        let on_messages = self.on_messages.lock().unwrap().clone();
        let interval = self.interval;
        let task = tokio::spawn(async move {
            loop {
                let messages = reader.poll_once().await;
                if let Some(on_messages) = &on_messages {
                    on_messages(messages).await;
                }
                tokio::time::sleep(interval).await;
            }
        });
        *self.task.lock().unwrap() = Some(task);
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
    fn health(&self) -> SubsystemHealth {
        if !self.token_present {
            return SubsystemHealth {
                name: self.name(),
                healthy: true,
                detail: "未配置 refresh_token，已禁用".into(),
            };
        }
        SubsystemHealth {
            name: self.name(),
            healthy: true,
            detail: format!("已启用（轮询间隔 {}s）", self.interval.as_secs()),
        }
    }
}

/// 游戏互通：ChatBridge 客户端（AES-CBC over TCP 21027）连接与收发。
struct ChatBridgeSubsystem {
    client: Option<Arc<ChatBridgeClient>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[async_trait]
impl Subsystem for ChatBridgeSubsystem {
    fn name(&self) -> &'static str {
        "game-link"
    }
    async fn start(&self) -> std::io::Result<()> {
        if let Some(client) = &self.client {
            let client = client.clone();
            *self.task.lock().unwrap() = Some(tokio::spawn(async move { client.run().await }));
        }
        Ok(())
    }
    async fn stop(&self) {
        // std MutexGuard 不能跨 await：先取出句柄再等待
        let task = self.task.lock().unwrap().take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        if let Some(client) = &self.client {
            client.stop();
        }
    }
    fn health(&self) -> SubsystemHealth {
        match &self.client {
            None => SubsystemHealth {
                name: self.name(),
                healthy: true,
                detail: "未启用".into(),
            },
            Some(client) if client.is_connected() => SubsystemHealth {
                name: self.name(),
                healthy: true,
                detail: "已连接".into(),
            },
            Some(_) => SubsystemHealth {
                name: self.name(),
                healthy: false,
                detail: "未连接".into(),
            },
        }
    }
}

/// 独立 HTTP API（默认 127.0.0.1:8199，配置段 api）。
struct ApiSubsystem {
    api: Arc<ApiServer>,
    enabled: bool,
    token_configured: bool,
}

#[async_trait]
impl Subsystem for ApiSubsystem {
    fn name(&self) -> &'static str {
        "http-api"
    }
    async fn start(&self) -> std::io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        self.api.start().await?;
        if !self.token_configured {
            warn!("API 写接口已禁用：请配置 api.access_token 后重启（读接口不受影响）");
        }
        if let Some(addr) = self.api.local_addr() {
            info!("HTTP API 已就绪: http://{addr}");
        }
        Ok(())
    }
    async fn stop(&self) {
        self.api.stop().await;
    }
    fn health(&self) -> SubsystemHealth {
        if !self.enabled {
            return SubsystemHealth {
                name: self.name(),
                healthy: true,
                detail: "未启用".into(),
            };
        }
        let detail = match self.api.local_addr() {
            Some(addr) => format!("已就绪 http://{addr}"),
            None => "已配置（未启动）".into(),
        };
        SubsystemHealth {
            name: self.name(),
            healthy: true,
            detail,
        }
    }
}

/// 玩家上下线推送（ChatBridge 事件驱动，无后台任务；健康 = 正则是否配置齐全）。
struct PlayerEventsSubsystem {
    enabled: bool,
}

#[async_trait]
impl Subsystem for PlayerEventsSubsystem {
    fn name(&self) -> &'static str {
        "player-events"
    }
    fn health(&self) -> SubsystemHealth {
        SubsystemHealth {
            name: self.name(),
            healthy: true,
            detail: if self.enabled {
                "已启用（ChatBridge 事件驱动）".into()
            } else {
                "未配置正则，未启用".into()
            },
        }
    }
}

/// 命令响应（/chatroom /server、!q、快照、agent 技能与智能路由；无后台任务）。
struct CommandResponderSubsystem;

#[async_trait]
impl Subsystem for CommandResponderSubsystem {
    fn name(&self) -> &'static str {
        "command-responder"
    }
    fn health(&self) -> SubsystemHealth {
        SubsystemHealth {
            name: self.name(),
            healthy: true,
            detail: "就绪".into(),
        }
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

// 回源应答 sink（QQReplySink / GameReplySink / ChatroomReplySink）与图片引用解析
// 分别在子模块 qq.rs / game.rs / chatroom.rs 里——各端消息路径就近放在一起。

#[cfg(test)]
mod tests {
    use super::*;

    /// 子服务清单、健康报告与统一生命周期：start/stop 全链路走一遍
    /// （onebot listen_port=0 随机端口，避免与其它测试抢 6199）。
    #[tokio::test]
    async fn subsystems_lifecycle_and_health_report() {
        let cfg: AppConfig = serde_json::from_str(
            r#"{
                "onebot": { "listen_port": 0 },
                "chatroom": { "base_url": "https://chatroom.example.com", "group_ids": [1] },
                "api": { "enabled": true, "listen_port": 0 }
            }"#,
        )
        .unwrap();
        let service = BridgeService::new(cfg).unwrap();

        let names: Vec<&str> = service
            .health_report()
            .iter()
            .map(|health| health.name)
            .collect();
        assert_eq!(
            names,
            [
                "qq-bridge",
                "chatroom-sync",
                "game-link",
                "http-api",
                "player-events",
                "command-responder",
            ]
        );

        // 未启用的 chatbridge 不算不健康；未连接的 qq-bridge 才算
        let report = service.health_report();
        let game = report.iter().find(|h| h.name == "game-link").unwrap();
        assert!(game.healthy);
        let qq = report.iter().find(|h| h.name == "qq-bridge").unwrap();
        assert!(!qq.healthy);

        // 统一生命周期：顺序 start（含 http-api 随机端口绑定）、逆序 stop
        service.start().await.unwrap();
        assert!(service.api_local_addr().is_some());
        service.stop().await;
    }
}

