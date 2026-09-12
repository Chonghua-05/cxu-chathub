//! 协议适配层：各适配器互不感知，只负责外部协议的字节流与语义。
//! - [`onebot`]      OneBot v11 反向 WS 服务端（NapCat 接入）
//! - [`forward_api`] chatroom 官方 Forward Bot API 客户端（写方向）
//! - [`chatroom_auth`] 用户 JWT 刷新与轮换持久化
//! - [`chatroom_read`] chatroom 读方向轮询与 `!q` 解析
//! - [`chatbridge`]  ChatBridge 客户端（MC 服务端插件，AES-CBC over TCP）

pub mod chatbridge;
pub mod chatroom_auth;
pub mod chatroom_read;
pub mod forward_api;
pub mod onebot;
