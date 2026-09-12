//! 服务入口：加载配置、装配 `BridgeService` 并常驻运行。
//!
//! 运行：``chatroom-bridge --config /app/config.json``（容器内默认路径不变）。

use std::path::Path;
use std::time::Duration;

use tracing::{error, info, warn};

use chatroom_bridge::config::{describe, load_config};
use chatroom_bridge::service::BridgeService;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let config_path = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .map(String::from)
        .unwrap_or_else(|| "/app/config.json".to_string());

    let code = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime 初始化失败")
        .block_on(run(&config_path));
    std::process::exit(code);
}

async fn run(config_path: &str) -> i32 {
    let cfg = match load_config(Path::new(config_path)) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("{err}");
            return 2;
        }
    };
    setup_logging(&cfg.log_level);
    info!("配置摘要: {}", describe(&cfg));
    if cfg.chatroom.forward_token.is_empty() {
        warn!("chatroom.forward_token 为空：QQ→chatroom 转发会被跳过，直到配置 token");
    }

    let service = match BridgeService::new(cfg) {
        Ok(service) => service,
        Err(err) => {
            error!("服务装配失败: {err}");
            return 2;
        }
    };
    if let Err(err) = service.start().await {
        error!("服务启动失败: {err}");
        return 2;
    }

    match service
        .server()
        .wait_connection(Duration::from_secs(30))
        .await
    {
        None => warn!("30 秒内没有 OneBot 客户端连入，请检查 NapCat 的 websocketClients 配置"),
        Some(conn) => info!("OneBot 已就绪 self_id={}", conn.self_id()),
    }

    wait_stop().await;
    info!("收到停止信号，正在关闭...");
    service.stop().await;
    0
}

fn setup_logging(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(unix)]
async fn wait_stop() {
    use tokio::signal::unix::{signal, SignalKind};
    let Ok(mut terminate) = signal(SignalKind::terminate()) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_stop() {
    let _ = tokio::signal::ctrl_c().await;
}
