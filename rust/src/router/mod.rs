//! 统一消息路由：三端（QQ 群 / 游戏 / chatroom）入站消息汇入 [`CommandRouter`]，
//! 由注册的 [`CommandHandler`] 依次认领。
//!
//! 本阶段注册的 handler：斜杠命令（`/chatroom` `/server`）、`!q` 中继、`!snap` 快照通知，
//! 行为与 Python 版逐一对齐。
//!
//! 下一阶段 agent 技能（`!mc` / `!wiki` / `!tmc`）各自实现 [`CommandHandler`] 后
//! `register` 一行即可接入；智能路由（LLM 选取 handler）只需在
//! [`CommandRouter::dispatch`] 前替换匹配策略，handler 与出站接口无需改动。
//! 设计与扩展指南见 `docs/agent-design.md`。

pub mod handlers;

use async_trait::async_trait;
use std::sync::Arc;

/// 消息来源端。游戏侧同时携带 ChatBridge 的 `sender`（客户端名，可鉴别服务端来源）
/// 与 `author`（玩家名，可为空——系统广播无作者）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    QQ,
    Game { sender: String, author: String },
    Chatroom { username: String },
}

/// 三端统一的入站消息。
#[derive(Debug, Clone)]
pub struct InboundMessage {
    pub source: Source,
    pub text: String,
    /// QQ 群号；游戏 / chatroom 来源为 0
    pub group_id: i64,
    /// QQ 用户号；其他来源为 0
    pub user_id: i64,
    pub display_name: String,
}

impl InboundMessage {
    pub fn source_name(&self) -> &'static str {
        match self.source {
            Source::QQ => "qq",
            Source::Game { .. } => "game",
            Source::Chatroom { .. } => "chatroom",
        }
    }
}

/// 回复到消息来源端的出口。斜杠命令等「回源应答」走这里。
#[async_trait]
pub trait ReplySink: Send + Sync {
    async fn send_text(&self, text: &str) -> bool;
    /// 发图（PNG）。默认实现仅记录不支持，QQ 端覆盖。
    async fn send_image(&self, png: &[u8]) -> bool {
        let _ = png;
        false
    }
}

/// 跨端中继出口：`!q` 中继、快照通知、以及未来 agent 回答落库（chatroom_post）都走这里。
#[async_trait]
pub trait Hub: Send + Sync {
    /// `group_id=None` 表示发到所有配置群。
    async fn qq_send_text(&self, group_id: Option<i64>, text: &str) -> bool;
    async fn qq_send_image(&self, group_id: i64, png: &[u8]) -> bool;
    async fn game_broadcast(&self, text: &str) -> bool;
    /// 写入 chatroom 目标频道（Forward API）。
    async fn chatroom_post(
        &self,
        source: &str,
        content: &str,
        sender_username: &str,
        nickname: &str,
    ) -> bool;
}

/// 一次派发的上下文：handler 只依赖这两个出口与消息本身，与具体传输解耦。
pub struct DispatchCtx<'a> {
    pub hub: &'a dyn Hub,
    pub origin: &'a dyn ReplySink,
    pub msg: &'a InboundMessage,
}

/// handler 元数据：`/api/status` 的 capabilities 与未来智能路由（LLM）都据此
/// 发现能力；字段全部 owned——内置命令用静态串，配置注册的技能用动态串。
#[derive(Debug, Clone)]
pub struct CommandInfo {
    /// 主名，如 "q" / "snap" / "server"；配置注册的技能如 "mc" / "wiki" / "tmc"
    pub name: String,
    pub aliases: Vec<String>,
    /// 触发形态说明，如 "!<name> <query>" / "/<name>"
    pub trigger: String,
    /// 能力描述（选取依据，写给路由器/人看）
    pub description: String,
}

/// 命令处理器。返回 `true` 表示消息被消费（QQ 端语义：不再进入转发流水线）。
///
/// 游戏与 chatroom 端的派发不采用消费语义：原始消息仍按既有行为
/// 广播/转发，handler 只负责自己那部分中继动作（与 Python 版行为一致）。
#[async_trait]
pub trait CommandHandler: Send + Sync {
    fn info(&self) -> CommandInfo;
    fn matches(&self, msg: &InboundMessage) -> bool;
    async fn handle(&self, ctx: &DispatchCtx<'_>) -> bool;
}

#[derive(Default)]
pub struct CommandRouter {
    handlers: Vec<Arc<dyn CommandHandler>>,
}

impl CommandRouter {
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
        }
    }

    /// 注册顺序即优先级；前缀互为前缀关系的命令（如 `!tmc` 与 `!mc`）先长者。
    pub fn register(&mut self, handler: Arc<dyn CommandHandler>) {
        self.handlers.push(handler);
    }

    pub fn handlers(&self) -> &[Arc<dyn CommandHandler>] {
        &self.handlers
    }

    /// 依次询问各 handler：首个 `matches` 且 `handle` 返回 true 的即消费。
    pub async fn dispatch(&self, ctx: &DispatchCtx<'_>) -> bool {
        for handler in &self.handlers {
            if handler.matches(ctx.msg) && handler.handle(ctx).await {
                return true;
            }
        }
        false
    }
}

/// `!<prefix><payload>` 风格匹配助手：整个前缀大小写不敏感（与 Python 版
/// `extract_qq_forward` 对齐，`!Q` 同样命中），返回去掉前缀并 trim 的载荷。
/// 多字节字符开头时安全退回 None（不做边界切割）。
pub fn match_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let trimmed = text.trim();
    if trimmed.len() < prefix.len() || !trimmed.is_char_boundary(prefix.len()) {
        return None;
    }
    if trimmed[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(trimmed[prefix.len()..].trim())
    } else {
        None
    }
}

/// 命令触发匹配：前缀大小写不敏感，且前缀后必须是**边界**（行尾或空白），
/// 避免 `!mc` 误吃 `!mcs` 这类更长的命令（`match_prefix` 无此约束）。
/// 返回去掉前缀并 trim 的查询词。
pub fn match_command<'a>(text: &'a str, trigger: &str) -> Option<&'a str> {
    let trimmed = text.trim();
    if trimmed.len() < trigger.len() || !trimmed.is_char_boundary(trigger.len()) {
        return None;
    }
    if !trimmed[..trigger.len()].eq_ignore_ascii_case(trigger) {
        return None;
    }
    let rest = &trimmed[trigger.len()..];
    match rest.chars().next() {
        None | Some(' ') | Some('\t') => Some(rest.trim()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_match_is_case_insensitive_and_trims() {
        assert_eq!(match_prefix("!Q  hello ", "!q"), Some("hello"));
        assert_eq!(match_prefix("!q", "!q"), Some(""));
        assert_eq!(match_prefix(" !q x", "!q"), Some("x"));
        assert_eq!(match_prefix("!qx", "!q"), Some("x"));
        assert_eq!(match_prefix("hello", "!q"), None);
        assert_eq!(match_prefix("！q x", "!q"), None);
    }

    #[test]
    fn command_match_requires_boundary() {
        assert_eq!(match_command("!mc 活塞", "!mc"), Some("活塞"));
        assert_eq!(match_command("!MC", "!mc"), Some(""));
        assert_eq!(match_command(" !tmc x", "!tmc"), Some("x"));
        // !mc 不能吃掉 !mcs
        assert_eq!(match_command("!mcs x", "!mc"), None);
        assert_eq!(match_command("!mcx", "!mc"), None);
        // !tmc 不被 !tm 或 !m 命中
        assert_eq!(match_command("!tmc x", "!mc"), None);
        assert_eq!(match_command("hello", "!mc"), None);
    }
}
