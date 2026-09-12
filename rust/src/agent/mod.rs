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

use std::sync::Arc;

use async_trait::async_trait;
use tracing::warn;

use crate::config::{AgentConfig, SourceConfig};

pub mod http;
pub mod local;
pub mod llm;
pub mod repo;
pub mod skill;

use self::http::MediaWikiSource;
use self::local::LocalDocSource;
use self::llm::LlmClient;
use self::repo::RepoSource;
use self::skill::DocQuerySkill;

/// 默认答案长度上限（agent.llm 未配置时摘录/回答仍按此截断）。
const DEFAULT_MAX_ANSWER_CHARS: usize = 1000;

/// 配置声明 → 可注册的技能实例。**新增查询命令不需要写代码**：config 里加一条
/// skill 声明即可。`agent.llm.api_url` 为空时不建 LLM 客户端（技能降级纯摘录）；
/// 单个数据源无效只跳过该源，全部无效则整个技能不注册。
pub fn build_skills(agent: &AgentConfig) -> Vec<DocQuerySkill> {
    if !agent.enabled {
        return Vec::new();
    }
    let llm = agent.llm.as_ref().filter(|cfg| !cfg.api_url.is_empty()).and_then(|cfg| {
        match LlmClient::new(cfg.clone()) {
            Ok(client) => Some(client),
            Err(err) => {
                warn!("LLM 客户端构建失败，技能将降级为纯检索摘录: {err}");
                None
            }
        }
    });
    let max_answer_chars = agent
        .llm
        .as_ref()
        .map(|cfg| cfg.max_answer_chars)
        .unwrap_or(DEFAULT_MAX_ANSWER_CHARS);

    let mut skills = Vec::new();
    for skill_cfg in &agent.skills {
        if skill_cfg.name.is_empty() {
            warn!("agent.skills 存在未命名技能，已跳过");
            continue;
        }
        let trigger = if skill_cfg.trigger.is_empty() {
            format!("!{}", skill_cfg.name)
        } else {
            skill_cfg.trigger.clone()
        };
        let description = if skill_cfg.description.is_empty() {
            format!("{trigger} 文档查询")
        } else {
            skill_cfg.description.clone()
        };

        let mut sources: Vec<Arc<dyn DocumentSource>> = Vec::new();
        for (index, source_cfg) in skill_cfg.sources.iter().enumerate() {
            match build_source(source_cfg) {
                Ok(source) => sources.push(source),
                Err(err) => warn!(
                    "技能 {} 的第 {} 个数据源无效，已跳过: {err}",
                    skill_cfg.name,
                    index + 1
                ),
            }
        }
        if sources.is_empty() {
            warn!("技能 {} 没有可用数据源，未注册", skill_cfg.name);
            continue;
        }
        skills.push(DocQuerySkill::new(
            skill_cfg.name.clone(),
            trigger,
            description,
            sources,
            llm.clone(),
            skill_cfg.max_results,
            max_answer_chars,
        ));
    }
    skills
}

/// 单条数据源声明 → 具体实现。name 缺省时用类型名兜底（命中结果要标注来源）。
fn build_source(cfg: &SourceConfig) -> Result<Arc<dyn DocumentSource>, String> {
    match cfg {
        SourceConfig::Local { root, extensions, name } => {
            if root.is_empty() {
                return Err("root 为空".into());
            }
            let name = if name.is_empty() { "local" } else { name };
            Ok(Arc::new(LocalDocSource::new(name, root, extensions.clone())))
        }
        SourceConfig::Mediawiki { api_url, name } => {
            if api_url.is_empty() {
                return Err("api_url 为空".into());
            }
            let name = if name.is_empty() { "mediawiki" } else { name };
            MediaWikiSource::new(name, api_url)
                .map(|source| Arc::new(source) as Arc<dyn DocumentSource>)
                .map_err(|err| err.to_string())
        }
        SourceConfig::Repo {
            repo,
            branch,
            subdir,
            site_url,
            extensions,
            name,
        } => {
            if repo.is_empty() {
                return Err("repo 为空".into());
            }
            let name = if name.is_empty() { "repo" } else { name };
            // 缓存放系统临时目录（容器内为 /tmp，随容器生命周期持久）；
            // 语料下载一次后 24h 内不重复下载。
            let cache_root = std::env::temp_dir().join("cxu-chathub-repo-cache");
            RepoSource::new(
                name,
                repo,
                branch,
                subdir.clone(),
                site_url.clone(),
                extensions.clone(),
                cache_root,
            )
            .map(|source| Arc::new(source) as Arc<dyn DocumentSource>)
            .map_err(|err| err.to_string())
        }
    }
}

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
    /// 数据源标识，如 "local-mc-source" / "minecraft-wiki"。
    fn name(&self) -> &str;
    async fn search(&self, query: &str, limit: usize) -> Vec<DocHit>;
}
