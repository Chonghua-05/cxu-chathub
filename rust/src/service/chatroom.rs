//! chatroom 同步子路径：读方向轮询拉到的消息进入统一路由（!q 中继），
//! 原始消息照常广播到游戏；回源应答经 Forward API 以 bot 身份写回频道。

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tracing::warn;

use crate::adapters::chatroom_auth::ChatroomAuth;
use crate::adapters::chatroom_read::AuthTokenProvider;
use crate::adapters::forward_api::{ForwardApi, PostSource};
use crate::router::{DispatchCtx, InboundMessage, ReplySink, Source};

use super::{post_with_seq, BridgeService};

impl BridgeService {
    pub(super) async fn dispatch_chatroom_message(&self, message: Value) {
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
            at_me: false,
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
        // sender_username/nickname 留空：服务端以 bot 账号发布，归属由服务端映射决定
        match post_with_seq(&self.api, &self.seq, "agent-reply-", PostSource::Game, text, "", "").await
        {
            Ok(()) => true,
            Err(err) => {
                warn!("chatroom 回源应答写入失败: {err}");
                false
            }
        }
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
