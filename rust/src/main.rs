//! 服务入口：加载配置、装配 `BridgeService` 并常驻运行。
//!
//! 运行：``chatroom-bridge --config /app/config.json``（容器内默认路径不变）。
//! 信号：Ctrl+C / SIGTERM 优雅停止；**SIGHUP 热重载配置**（最小形态：
//! group_ids 白名单与日志级别热生效，其余差异记日志提示重启，见
//! `BridgeService::apply_reloaded_config` 与 docs/roadmap.md v0.4）。

use std::path::Path;
use std::time::Duration;

use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use chatroom_bridge::config::{describe, load_config};
use chatroom_bridge::service::BridgeService;

/// 日志过滤层的热更新句柄（SIGHUP 改 log_level 用；RUST_LOG 优先时不热更）。
type LogFilterHandle =
    tracing_subscriber::reload::Handle<EnvFilter, tracing_subscriber::Registry>;

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
    // 信号流必须尽早在运行时注册：装配 / 等待连接期间到达的 SIGHUP 不能落入
    // 默认处置（默认行为是直接杀进程）——烟测实测踩过这个坑。
    #[cfg(unix)]
    let mut signals = SignalStreams::register();

    let cfg = match load_config(Path::new(config_path)) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("{err}");
            return 2;
        }
    };
    let log_handle = setup_logging(&cfg.log_level, &cfg.log_format);
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

    #[cfg(unix)]
    return wait_loop(&service, config_path, log_handle, &mut signals).await;
    #[cfg(not(unix))]
    wait_loop(&service, config_path, log_handle).await
}

/// 已注册的终止 / 重载信号流（unix）。
#[cfg(unix)]
struct SignalStreams {
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl SignalStreams {
    fn register() -> Self {
        use tokio::signal::unix::{signal, SignalKind};
        let terminate = signal(SignalKind::terminate()).expect("SIGTERM 处理器注册失败");
        let hangup = signal(SignalKind::hangup()).expect("SIGHUP 处理器注册失败");
        Self { terminate, hangup }
    }
}

/// 信号主循环：Ctrl+C / SIGTERM → 优雅停止；SIGHUP → 热重载配置。
#[cfg(unix)]
async fn wait_loop(
    service: &std::sync::Arc<BridgeService>,
    config_path: &str,
    log_handle: Option<LogFilterHandle>,
    signals: &mut SignalStreams,
) -> i32 {
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = signals.terminate.recv() => break,
            _ = signals.hangup.recv() => {
                info!("收到 SIGHUP，重读配置");
                match load_config(Path::new(config_path)) {
                    Err(err) => error!("SIGHUP 重读配置失败，继续使用旧配置: {err}"),
                    Ok(new_cfg) => {
                        let new_level = service.apply_reloaded_config(&new_cfg);
                        if let (Some(level), Some(handle)) = (new_level, &log_handle) {
                            // RUST_LOG 环境变量优先级高于配置，未设置时才热更新过滤层
                            if std::env::var("RUST_LOG").is_err() {
                                let _ = handle.modify(|filter| *filter = EnvFilter::new(&level));
                                info!("日志级别已热更新: {level}");
                            }
                        }
                    }
                }
            }
        }
    }
    info!("收到停止信号，正在关闭...");
    service.stop().await;
    0
}

/// 日志初始化：级别取 `RUST_LOG` 环境变量（优先）或配置 `log_level`；
/// 格式取配置 `log_format`——text（默认，人类可读）或 json（结构化，便于
/// 接入日志采集 / 监控，见 docs/roadmap.md v0.4）。返回过滤层热更新句柄。
fn setup_logging(level: &str, format: &str) -> Option<LogFilterHandle> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let (filter_layer, handle) = tracing_subscriber::reload::Layer::new(filter);
    // json / text 是不同具体类型，两个分支各自组装（避免 dyn Layer 的适配问题）
    let result = match format.eq_ignore_ascii_case("json") {
        true => tracing_subscriber::registry()
            .with(filter_layer)
            .with(tracing_subscriber::fmt::layer().json())
            .try_init(),
        false => tracing_subscriber::registry()
            .with(filter_layer)
            .with(tracing_subscriber::fmt::layer())
            .try_init(),
    };
    result.ok().map(|_| handle)
}

#[cfg(not(unix))]
async fn wait_loop(
    service: &std::sync::Arc<BridgeService>,
    _config_path: &str,
    _log_handle: Option<LogFilterHandle>,
) -> i32 {
    let _ = tokio::signal::ctrl_c().await;
    info!("收到停止信号，正在关闭...");
    service.stop().await;
    0
}
