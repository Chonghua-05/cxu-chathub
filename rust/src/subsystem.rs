//! 子服务边界（roadmap v0.4 服务化）：统一生命周期与健康检查的最小抽象。
//!
//! 每个子服务（QQ 桥接 / chatroom 同步 / 游戏互通 / HTTP API / 玩家事件 /
//! 命令响应）实现 [`Subsystem`]，由 `service::BridgeService` 统一注册、
//! 顺序 start、逆序 stop；健康快照 [`SubsystemHealth`] 汇入 `/healthz`
//! 与 `/api/status` 的 `subsystems` 数组，监控与运维只看这一份。
//!
//! 没有独立后台任务的子服务（玩家事件、命令响应）用默认空 start/stop，
//! 只参与健康报告——它们的存在感在 `/api/status` 的统计字段里。

use serde::Serialize;

/// 一个子服务的健康快照。
#[derive(Debug, Serialize)]
pub struct SubsystemHealth {
    /// 稳定标识（监控按名字取值）：如 `qq-bridge` / `chatroom-sync` / `game-link`。
    pub name: &'static str,
    /// 是否健康。「未启用」不算不健康（配置选择，不是故障）；
    /// 「已启用但断连」才算。
    pub healthy: bool,
    /// 人类可读的状态说明（日志/面板展示用）。
    pub detail: String,
}

/// 子服务：统一生命周期与健康检查。
#[async_trait::async_trait]
pub trait Subsystem: Send + Sync {
    /// 稳定标识。
    fn name(&self) -> &'static str;
    /// 启动后台任务 / 监听。装配失败返回 io::Error（进程退出）。
    async fn start(&self) -> std::io::Result<()> {
        Ok(())
    }
    /// 停止：终止后台任务、断开连接。实现需幂等。
    async fn stop(&self) {}
    /// 当前健康快照。
    fn health(&self) -> SubsystemHealth;
}
