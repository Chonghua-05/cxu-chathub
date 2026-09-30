//! QQ 桥接子路径：群消息进入统一路由（消费语义）、QQ 群发送与回源应答 sink。

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::Value;
use tracing::{error, info, warn};

use crate::adapters::onebot::{GroupMessage, OneBotConnection};
use crate::router::{DispatchCtx, InboundMessage, ReplySink, Source};
use crate::services::forwarder::ImageResolver;

use super::BridgeService;

impl BridgeService {
    // --- QQ 群事件 ---
    pub async fn handle_group_message(
        self: Arc<Self>,
        conn: Arc<OneBotConnection>,
        msg: GroupMessage,
    ) {
        // 空白名单 = 不限制；非空则只放行白名单群
        let whitelist = self.group_ids_snapshot();
        if !whitelist.is_empty() && !whitelist.contains(&msg.group_id) {
            return;
        }
        self.recent.push("qq", &msg.display_name(), &msg.text());

        let origin = QQReplySink {
            conn: conn.clone(),
            group_id: msg.group_id,
        };
        let inbound = InboundMessage {
            source: Source::QQ,
            text: msg.text(),
            group_id: msg.group_id,
            user_id: msg.user_id,
            at_me: msg.at_qq(conn.self_id()),
        };
        let ctx = DispatchCtx {
            hub: self.as_ref(),
            origin: &origin,
            msg: &inbound,
        };
        if self.router.dispatch(&ctx).await {
            return; // 命令已消费（/chatroom、/server、agent 技能与智能路由）
        }

        let resolver = ConnImageResolver(conn);
        self.forwarder.handle(&resolver, &msg).await;
    }

    /// 向配置的 QQ 群发文本（`!q` / 快照通知 / 玩家上下线推送用）。
    pub async fn send_to_qq_groups(&self, text: &str) -> bool {
        let Some(conn) = self.server.connection() else {
            warn!("QQ 未连接，转发跳过: {}", crate::truncate_chars(text, 40));
            return false;
        };
        let mut sent = false;
        for group_id in self.group_ids_snapshot() {
            match conn.send_group_text(group_id, text).await {
                Ok(_) => {
                    sent = true;
                    info!("转发到 QQ 群 {group_id}: {}", crate::truncate_chars(text, 60));
                }
                Err(err) => error!("转发到 QQ 群 {group_id} 失败: {err}"),
            }
        }
        sent
    }
}

/// 回复到 QQ 群（消息来源端）。
struct QQReplySink {
    conn: Arc<OneBotConnection>,
    group_id: i64,
}

#[async_trait]
impl ReplySink for QQReplySink {
    async fn send_text(&self, text: &str) -> bool {
        match self.conn.send_group_text(self.group_id, text).await {
            Ok(_) => true,
            Err(err) => {
                error!("发送命令响应失败: {err}");
                false
            }
        }
    }

    async fn send_image(&self, png: &[u8]) -> bool {
        let data_uri = format!("base64://{}", base64::engine::general_purpose::STANDARD.encode(png));
        match self.conn.send_group_image(self.group_id, &data_uri).await {
            Ok(_) => true,
            Err(err) => {
                error!("发送命令图片响应失败: {err}");
                false
            }
        }
    }
}

/// 图片引用解析：非 http 引用经 OneBot `get_image` 换取 URL。
struct ConnImageResolver(Arc<OneBotConnection>);

#[async_trait]
impl ImageResolver for ConnImageResolver {
    async fn resolve(&self, file_ref: &str) -> Option<String> {
        let info = self.0.get_image(file_ref).await.ok()?;
        info.get("url").and_then(Value::as_str).map(String::from)
    }
}
