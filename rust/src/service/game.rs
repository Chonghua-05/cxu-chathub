//! 游戏互通子路径：ChatBridge 入站聊天 → 转发 chatroom + 统一路由（!q / 快照 /
//! 玩家上下线推送）；游戏侧事件统一写向 chatroom 的出站辅助。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tracing::warn;

use crate::adapters::chatbridge::ChatBridgeClient;
use crate::adapters::forward_api::{ForwardApi, PostMessage, PostSource};
use crate::router::{DispatchCtx, InboundMessage, ReplySink, Source};

use super::BridgeService;

impl BridgeService {
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
            at_me: false,
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
        let ok = post_game_message(
            &self.forward_api,
            &self.game_seq,
            "game-chat",
            content,
            nickname,
            username,
        )
        .await;
        // /metrics 计数（失败只记日志不重试的策略不变）
        if ok {
            self.game_forward_ok.fetch_add(1, Ordering::Relaxed);
        } else {
            self.game_forward_fail.fetch_add(1, Ordering::Relaxed);
        }
    }
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
