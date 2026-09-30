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

/// 读取 HTTP 响应体并强制大小上限（字节）。先看 `Content-Length`（服务端诚实
/// 声明时提前拒绝），再流式累计兜底（防谎报 / 缺失长度头的响应撑爆内存）。
/// 错误统一收敛为 `Err(String)`，由调用方走各自的降级路径。
pub(crate) async fn read_body_capped(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    if let Some(len) = response.content_length() {
        if len > max_bytes as u64 {
            return Err(format!("响应体 {len} 字节超过上限 {max_bytes}"));
        }
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| format!("读取响应体失败: {err}"))?
    {
        if body.len() + chunk.len() > max_bytes {
            return Err(format!("响应体超过上限 {max_bytes} 字节（流式读取中止）"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::read_body_capped;

    use axum::body::{Body, Bytes};
    use axum::response::Response;
    use futures_util::stream;

    /// 起一个只对 GET /data 回固定响应体的本地服务，返回其地址。
    async fn spawn_body_server(
        make_body: impl Fn() -> Response + Clone + Send + Sync + 'static,
    ) -> std::net::SocketAddr {
        let app = axum::Router::new().route(
            "/data",
            axum::routing::get(move || std::future::ready(make_body())),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn reads_body_within_cap() {
        let addr = spawn_body_server(|| {
            Response::builder()
                .body(Body::from(vec![0u8; 100]))
                .unwrap()
        })
        .await;
        let response = reqwest::get(format!("http://{addr}/data")).await.unwrap();
        let body = read_body_capped(response, 1_000).await.unwrap();
        assert_eq!(body.len(), 100);
    }

    #[tokio::test]
    async fn rejects_oversized_content_length_upfront() {
        let addr = spawn_body_server(|| {
            Response::builder()
                .body(Body::from(vec![0u8; 2_000]))
                .unwrap()
        })
        .await;
        let response = reqwest::get(format!("http://{addr}/data")).await.unwrap();
        let err = read_body_capped(response, 1_000).await.unwrap_err();
        assert!(err.contains("超过上限"), "应报超限: {err}");
    }

    #[tokio::test]
    async fn reads_chunked_body_within_cap() {
        // Body::from_stream 不带 Content-Length（chunked），走流式累计路径
        let addr = spawn_body_server(|| {
            Response::builder()
                .body(Body::from_stream(stream::iter([[0u8; 300], [1u8; 300]].map(
                    |chunk| Ok::<Bytes, std::io::Error>(Bytes::from(chunk.to_vec())),
                ))))
                .unwrap()
        })
        .await;
        let response = reqwest::get(format!("http://{addr}/data")).await.unwrap();
        let body = read_body_capped(response, 1_000).await.unwrap();
        assert_eq!(body.len(), 600);
    }

    #[tokio::test]
    async fn aborts_oversized_chunked_body() {
        let addr = spawn_body_server(|| {
            Response::builder()
                .body(Body::from_stream(stream::iter((0..3).map(|_| {
                    Ok::<Bytes, std::io::Error>(Bytes::from(vec![0u8; 500]))
                }))))
                .unwrap()
        })
        .await;
        let response = reqwest::get(format!("http://{addr}/data")).await.unwrap();
        let err = read_body_capped(response, 1_000).await.unwrap_err();
        assert!(err.contains("超过上限"), "流式超限应中止: {err}");
    }
}
