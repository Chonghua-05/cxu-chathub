//! Agent 能力预留层 —— **本阶段不实现任何技能**，只固化扩展接口。
//!
//! 下一阶段在此登记查询技能（每个命令一个 [`crate::router::CommandHandler`]
//! + 一个 [`DocumentSource`]）：
//!
//! | 命令 | 数据源 | 形态 |
//! |------|--------|------|
//! | `!mc` | MC 源码查询 | 本地源码副本检索，回答带文件/行级出处 |
//! | `!wiki` | Minecraft Wiki | 云端 MediaWiki API |
//! | `!tmc` | techmc wiki | 文档源（本地/云端） |
//! | （规划中） | MC 源码释读文档 | 本地文档，帮助理解架构与逻辑设计 |
//!
//! 智能路由预留：[`crate::router::CommandRouter`] 目前按注册顺序做显式命令匹配；
//! 未来在其前置一个 LLM 路由器（读取各 handler 的 [`crate::router::CommandInfo`]
//! 元数据做工具选择），handler 与检索层接口无需改动。
//! 完整设计见 `docs/agent-design.md`。

use async_trait::async_trait;

/// 文档检索命中项。`locator` 为「文件:行号区间」或云端 URL，保证回答可溯源
/// （roadmap v0.3 要求：回答必须带出处）。
#[derive(Debug, Clone)]
pub struct DocHit {
    pub title: String,
    pub locator: String,
    pub snippet: String,
}

/// 文档源抽象：本地文档与云端文档各自实现，技能层无感。
/// 检索失败返回空列表——「查不到就说查不到，不编」（roadmap 非目标约束）。
#[async_trait]
pub trait DocumentSource: Send + Sync {
    /// 数据源标识，如 "local-mc-source" / "minecraft-wiki" / "techmc-wiki"。
    fn name(&self) -> &'static str;
    async fn search(&self, query: &str, limit: usize) -> Vec<DocHit>;
}
