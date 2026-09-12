//! 首批 [`CommandHandler`](crate::router::CommandHandler) 实现：
//! 斜杠命令适配器、`!q` 中继、`!snap` 快照通知。
//! 它们同时是未来 agent 技能（`!mc` / `!wiki` / `!tmc`，见 `docs/agent-design.md`）的参照样板。

use std::sync::Arc;

use async_trait::async_trait;

use crate::router::{match_prefix, CommandHandler, CommandInfo, DispatchCtx, InboundMessage, Source};
use crate::services::commands::{parse_command, CommandService};

/// `/chatroom` `/server`（`/status` 为兼容别名）：解析、鉴权与应答委托给 [`CommandService`]。
///
/// 消费语义与 Python 版一致：非命令、未授权（静默）或空结果返回 `false`，
/// 消息继续进入转发流水线；命中并应答后返回 `true`。
pub struct SlashCommandAdapter {
    pub commands: Arc<CommandService>,
}

#[async_trait]
impl CommandHandler for SlashCommandAdapter {
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: "chatroom",
            aliases: &["server", "status"],
            trigger: "/<name>",
            description: "查询语音频道在线名单与服务器状态（/server 支持状态图，QQ 群）",
        }
    }

    fn matches(&self, msg: &InboundMessage) -> bool {
        if !matches!(msg.source, Source::QQ) {
            return false;
        }
        matches!(
            parse_command(&msg.text),
            Some((name, _)) if matches!(name.as_str(), "chatroom" | "server" | "status")
        )
    }

    async fn handle(&self, ctx: &DispatchCtx<'_>) -> bool {
        let Some(result) = self.commands.handle(&ctx.msg.text, ctx.msg.user_id).await else {
            return false;
        };
        if result.empty() {
            return false;
        }
        if let Some(png) = &result.image {
            ctx.origin.send_image(png).await;
        } else {
            ctx.origin.send_text(&result.text).await;
        }
        true
    }
}

/// `!q <内容>`：把 chatroom / 游戏内消息转发到 QQ 群。
/// QQ 端没有这个语义（群里的 `!q xxx` 会原样转发到 chatroom），与 Python 行为一致。
pub struct QqForwardRelay {
    pub enabled: bool,
}

#[async_trait]
impl CommandHandler for QqForwardRelay {
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: "q",
            aliases: &[],
            trigger: "!q <内容>",
            description: "把 chatroom / 游戏内消息转发到 QQ 群",
        }
    }

    fn matches(&self, msg: &InboundMessage) -> bool {
        if !self.enabled || matches!(msg.source, Source::QQ) {
            return false;
        }
        if let Source::Game { author, .. } = &msg.source {
            if author.is_empty() {
                // 系统广播没有作者，不能替玩家发 !q
                return false;
            }
        }
        match_prefix(&msg.text, "!q").is_some_and(|payload| !payload.is_empty())
    }

    async fn handle(&self, ctx: &DispatchCtx<'_>) -> bool {
        let Some(payload) = match_prefix(&ctx.msg.text, "!q").filter(|p| !p.is_empty()) else {
            return false;
        };
        let text = match &ctx.msg.source {
            Source::Chatroom { username } if !username.is_empty() => {
                format!("[Chatroom] {username}: {payload}")
            }
            Source::Chatroom { .. } => format!("[Chatroom]: {payload}"),
            Source::Game { sender, author } => format!("[游戏|{sender}] {author}: {payload}"),
            Source::QQ => return false,
        };
        ctx.hub.qq_send_text(None, &text).await
    }
}

/// `!snap <内容>`：快照服（指定 ChatBridge 客户端名）的更新通知，转发到 QQ 群。
/// 只认 `sender` 客户端名，玩家无法冒用——与 Python 的 `_is_snapshot_notice` 一致。
pub struct SnapshotRelay {
    pub enabled: bool,
    pub sender: String,
    pub prefix: String,
}

#[async_trait]
impl CommandHandler for SnapshotRelay {
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: "snap",
            aliases: &[],
            trigger: "!snap <内容>",
            description: "快照服更新通知转发到 QQ 群（仅指定 ChatBridge 客户端可信）",
        }
    }

    fn matches(&self, msg: &InboundMessage) -> bool {
        if !self.enabled {
            return false;
        }
        let Source::Game { sender, .. } = &msg.source else {
            return false;
        };
        sender == &self.sender
            && match_prefix(&msg.text, &self.prefix).is_some_and(|payload| !payload.is_empty())
    }

    async fn handle(&self, ctx: &DispatchCtx<'_>) -> bool {
        let Some(payload) = match_prefix(&ctx.msg.text, &self.prefix).filter(|p| !p.is_empty())
        else {
            return false;
        };
        ctx.hub.qq_send_text(None, &format!("[快照服] {payload}")).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::router::Hub;

    struct FakeHub {
        texts: Mutex<Vec<(Option<i64>, String)>>,
    }

    #[async_trait]
    impl Hub for FakeHub {
        async fn qq_send_text(&self, group_id: Option<i64>, text: &str) -> bool {
            self.texts
                .lock()
                .unwrap()
                .push((group_id, text.to_string()));
            true
        }
        async fn qq_send_image(&self, _group_id: i64, _png: &[u8]) -> bool {
            false
        }
        async fn game_broadcast(&self, _text: &str) -> bool {
            false
        }
        async fn chatroom_post(
            &self,
            _source: &str,
            _content: &str,
            _sender_username: &str,
            _nickname: &str,
        ) -> bool {
            false
        }
    }

    struct NullSink;

    #[async_trait]
    impl crate::router::ReplySink for NullSink {
        async fn send_text(&self, _text: &str) -> bool {
            true
        }
        async fn send_image(&self, _png: &[u8]) -> bool {
            false
        }
    }

    fn game_msg(sender: &str, author: &str, text: &str) -> InboundMessage {
        InboundMessage {
            source: Source::Game {
                sender: sender.into(),
                author: author.into(),
            },
            text: text.into(),
            group_id: 0,
            user_id: 0,
            display_name: author.into(),
        }
    }

    fn chatroom_msg(username: &str, text: &str) -> InboundMessage {
        InboundMessage {
            source: Source::Chatroom {
                username: username.into(),
            },
            text: text.into(),
            group_id: 0,
            user_id: 0,
            display_name: username.into(),
        }
    }

    async fn dispatch(hub: &FakeHub, relay: &dyn CommandHandler, msg: &InboundMessage) -> bool {
        let sink = NullSink;
        let ctx = DispatchCtx {
            hub,
            origin: &sink,
            msg,
        };
        if relay.matches(msg) {
            relay.handle(&ctx).await
        } else {
            false
        }
    }

    #[tokio::test]
    async fn qq_relay_composes_chatroom_and_game_labels() {
        let hub = FakeHub {
            texts: Mutex::new(Vec::new()),
        };
        let relay = QqForwardRelay { enabled: true };

        assert!(dispatch(&hub, &relay, &chatroom_msg("alice", "!q 早安")).await);
        assert!(dispatch(&hub, &relay, &chatroom_msg("", "!Q 无名氏")).await);
        assert!(dispatch(&hub, &relay, &game_msg("snapshot", "bob", "!q 上号")).await);

        let texts = hub.texts.lock().unwrap();
        assert_eq!(texts.len(), 3);
        assert_eq!(texts[0], (None, "[Chatroom] alice: 早安".to_string()));
        assert_eq!(texts[1], (None, "[Chatroom]: 无名氏".to_string()));
        assert_eq!(texts[2], (None, "[游戏|snapshot] bob: 上号".to_string()));
    }

    #[tokio::test]
    async fn qq_relay_ignores_bare_prefix_qq_source_and_system_broadcast() {
        let hub = FakeHub {
            texts: Mutex::new(Vec::new()),
        };
        let relay = QqForwardRelay { enabled: true };

        // 裸 !q 不中继
        assert!(!dispatch(&hub, &relay, &chatroom_msg("a", "!q")).await);
        // QQ 端没有 !q 语义
        assert!(!dispatch(
            &hub,
            &relay,
            &InboundMessage {
                source: Source::QQ,
                text: "!q x".into(),
                group_id: 1,
                user_id: 2,
                display_name: "u".into(),
            }
        )
        .await);
        // 游戏系统广播（无作者）不能发 !q
        assert!(!dispatch(&hub, &relay, &game_msg("server", "", "!q x")).await);
        // 未启用
        let disabled = QqForwardRelay { enabled: false };
        assert!(!dispatch(&hub, &disabled, &chatroom_msg("a", "!q x")).await);
        assert!(hub.texts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn snapshot_relay_requires_matching_sender_and_nonempty_payload() {
        let hub = FakeHub {
            texts: Mutex::new(Vec::new()),
        };
        let relay = SnapshotRelay {
            enabled: true,
            sender: "snapshot".into(),
            prefix: "!snap".into(),
        };

        assert!(dispatch(&hub, &relay, &game_msg("snapshot", "", "!snap 服务端已更新")).await);
        // 玩家冒用被拒绝
        assert!(!dispatch(&hub, &relay, &game_msg("web", "bob", "!snap 假通知")).await);
        // 裸前缀不转发
        assert!(!dispatch(&hub, &relay, &game_msg("snapshot", "", "!snap")).await);
        // 非 Game 来源不匹配
        assert!(!dispatch(&hub, &relay, &chatroom_msg("a", "!snap x")).await);

        let texts = hub.texts.lock().unwrap();
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0], (None, "[快照服] 服务端已更新".to_string()));
    }

    #[test]
    fn slash_adapter_matches_only_known_qq_commands() {
        let adapter = SlashCommandAdapter {
            commands: Arc::new(
                CommandService::new(true, Vec::new(), false, "http://v", "http://s", vec![])
                    .unwrap(),
            ),
        };
        let qq = InboundMessage {
            source: Source::QQ,
            text: "/Server@Bot".into(),
            group_id: 1,
            user_id: 2,
            display_name: "u".into(),
        };
        assert!(adapter.matches(&qq));
        let qq_unknown = InboundMessage {
            text: "/unknown".into(),
            ..qq.clone()
        };
        assert!(!adapter.matches(&qq_unknown));
        // chatroom 来源不匹配
        assert!(!adapter.matches(&chatroom_msg("a", "/chatroom")));
    }
}
