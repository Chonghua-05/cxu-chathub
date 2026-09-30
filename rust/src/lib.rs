//! cxu-chathub 的 Rust 实现：QQ 群 / chatroom / MC 游戏内聊天三端互通的消息中枢。
//!
//! 模块分层：
//! - `adapters/`   协议适配层：onebot / forward_api / chatroom_auth / chatroom_read / chatbridge
//! - `services/`   策略层：forwarder / player_events / commands / status_render / patch_broadcast
//! - `router/`     统一消息路由（三端入站汇入；agent 技能与 LLM 智能路由的接入点）
//! - `agent/`      agent 能力：配置注册式检索问答（文档源 / 技能 / LLM 客户端 / 评测 / 智能路由）
//! - `api/`        独立 HTTP API（Web UI / 外部站点调用；读接口 + token 保护的写接口）
//! - `service/`    BridgeService：装配与生命周期；消息路径按子服务边界分区
//!   （`qq` / `chatroom` / `game`），生命周期与健康检查统一走 [`subsystem::Subsystem`]
//! - `subsystem.rs` 子服务抽象：统一生命周期与健康检查（roadmap v0.4）
//! - `config.rs` / `state.rs` / `error.rs`  基础设施

pub mod adapters;
pub mod agent;
pub mod api;
pub mod config;
pub mod error;
pub mod router;
pub mod service;
pub mod services;
pub mod state;
pub mod subsystem;

/// 按字符数截断（不按字节，避免切开多字节字符产生乱码）。
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// 统一构造 reqwest 客户端：`total_s` = 总超时秒数，`connect_s` = 连接超时秒数
/// （`None` 表示不显式设置，沿用 reqwest 默认）。返回 [`reqwest::ClientBuilder`]，
/// 调用方按需追加 `user_agent` 后自行 `.build()`。
pub(crate) fn http_client(total_s: u64, connect_s: Option<u64>) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(total_s));
    match connect_s {
        Some(secs) => builder.connect_timeout(std::time::Duration::from_secs(secs)),
        None => builder,
    }
}
