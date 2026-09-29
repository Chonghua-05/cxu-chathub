//! 策略层：什么该转发、怎么去重、命令如何响应。
//! - [`forwarder`]        QQ 群 → chatroom 转发流水线（文本 / 图片 / 引用 / 本地去重）
//! - [`player_events`]    玩家上下线事件检测（ChatBridge 系统广播 + 可配置正则）
//! - [`commands`]         斜杠命令解析与格式化（/chatroom /server）
//! - [`status_render`]    /server 状态图渲染（Chromium，feature 门控）
//! - [`patch_broadcast`]  Mojang 版本更新播报（轮询 feed → 翻译 → 截图 → 合并转发，v0.5）

pub mod commands;
pub mod forwarder;
pub mod patch_broadcast;
pub mod player_events;
pub mod status_render;
