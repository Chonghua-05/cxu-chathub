//! cxu-chathub 的 Rust 实现：QQ 群 / chatroom / MC 游戏内聊天三端互通的消息中枢。
//!
//! 模块分层（与 Python 版一一对应）：
//! - `adapters/`  协议适配层：onebot / forward_api / chatroom_auth / chatroom_read / chatbridge
//! - `services/`  策略层：forwarder / player_tracker / commands / status_render
//! - `router/`    统一消息路由（三端入站汇入；agent 技能的接入点）
//! - `agent/`     agent 能力预留层（本阶段仅定义扩展接口，不实现技能）
//! - `api/`       独立 HTTP API（Web UI / 外部站点调用；读接口 + token 保护的写接口）
//! - `service.rs` BridgeService：服务装配与生命周期
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
