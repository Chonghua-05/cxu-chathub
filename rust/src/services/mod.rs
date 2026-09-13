//! 策略层：什么该转发、怎么去重、命令如何响应。
//! - [`forwarder`]      QQ 群 → chatroom 转发流水线（文本 / 图片 / 引用 / 本地去重）
//! - [`player_events`]  玩家上下线事件检测（ChatBridge 系统广播 + 可配置正则）
//! - [`commands`]       斜杠命令解析与格式化（/chatroom /server）
//! - [`status_render`]  /server 状态图渲染（Chromium，feature 门控）

pub mod commands;
pub mod forwarder;
pub mod player_events;
pub mod status_render;
