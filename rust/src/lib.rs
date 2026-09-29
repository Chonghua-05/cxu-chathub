//! cxu-chathub 的 Rust 实现：QQ 群 / chatroom / MC 游戏内聊天三端互通的消息中枢。
//!
//! 模块分层：
//! - `adapters/`   协议适配层：onebot / forward_api / chatroom_auth / chatroom_read / chatbridge
//! - `services/`   策略层：forwarder / player_events / commands / status_render
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
