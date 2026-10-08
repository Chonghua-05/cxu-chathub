//! 配置注册的通用文档查询技能（[`DocQuerySkill`]）。
//!
//! 命令在 config 的 `agent.skills` 里声明（如 `!mc` / `!wiki` / `!tmc`），
//! 每条声明绑定一组 [`DocumentSource`]；一个实例对应一条声明，由装配层
//! 构造后注册进 [`crate::router::CommandRouter`]。三端消息都认领
//! （`matches` 不限来源），触发匹配用 [`crate::router::match_command`]，
//! 边界安全——`!mc` 不会吃掉 `!mcs`。
//!
//! 回答策略：LLM 已配置 → 依据检索片段整理答案（片段编号即出处，
//! 「查不到就说查不到，不编」由 [`crate::config::DEFAULT_SYSTEM_PROMPT`]
//! 硬约束）；LLM 未配置或调用失败 → 自动降级为格式化摘录。
//! 所有分支都消费消息（`handle` 返回 `true`）。

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::join_all;

use crate::agent::llm::LlmClient;
use crate::agent::{DocHit, DocumentSource};
use crate::config::DEFAULT_SYSTEM_PROMPT;
use crate::router::{match_command, CommandHandler, CommandInfo, DispatchCtx, InboundMessage};

/// 合并后的检索命中项：`(来源名, 命中项)`，出处标注用。
type SourcedHit = (String, DocHit);

/// 查询翻译提示词：中文问题 → 英文检索关键词（MC 术语用官方英文名）。
/// pub(crate)：检索质量评测（`agent::eval`）对 question 用例复用同一提示词。
pub(crate) const TRANSLATE_PROMPT: &str = "把下面的中文问题转成一行英文检索关键词（Minecraft 领域术语用官方英文名：守卫者→Guardian、刷怪→mob spawning、袭击→raid）。只输出最多 15 个英文词、空格分隔；禁止中文、禁止解释、禁止换行、禁止标题或列表。";

/// 配置注册的通用文档查询技能：一个实例 = config 里的一条 skill 声明。
pub struct DocQuerySkill {
    name: String,
    trigger: String,
    description: String,
    sources: Vec<Arc<dyn DocumentSource>>,
    /// 各数据源的检索分段，与 `sources` 一一对应（缺省全 1）。多段时按段号升序
    /// 逐段查——「先查文档、再翻源码」靠它实现，后段把前段命中的标识符补进查询词。
    stages: Vec<u8>,
    llm: Option<LlmClient>,
    max_results: usize,
    max_answer_chars: usize,
}

impl DocQuerySkill {
    /// 按一条 skill 声明构建技能。`llm` 为 `None`（未配置 LLM）时
    /// 直接返回检索摘录。
    pub fn new(
        name: impl Into<String>,
        trigger: impl Into<String>,
        description: impl Into<String>,
        sources: Vec<std::sync::Arc<dyn DocumentSource>>,
        llm: Option<LlmClient>,
        max_results: usize,
        max_answer_chars: usize,
    ) -> Self {
        let stages = vec![1u8; sources.len()];
        Self {
            name: name.into(),
            trigger: trigger.into(),
            description: description.into(),
            sources,
            stages,
            llm,
            max_results,
            max_answer_chars,
        }
    }

    /// 设置各数据源的检索分段（与 `sources` 一一对应；长度不符时退回全 1）。
    pub fn with_stages(mut self, stages: Vec<u8>) -> Self {
        self.stages = stages;
        self
    }

    /// 分阶段检索。
    ///
    /// 单段（全部源 stage=1）：并发查所有源、交错合并、按 locator 去重，总量 ≤ max_results
    /// （行为同旧）。多段：按 stage 升序**逐段**查——「先查文档、再翻源码」；每段均分名额
    /// （余数给最后一段、每段至少 1 条），后段把前段命中里榨出的标识符（反引号标识符 /
    /// 驼峰类名）补进查询词，实现「用文档定位、再去源码取」。中文问题有 LLM 时自动加查译文。
    async fn search_queries(&self, original: &str) -> Vec<SourcedHit> {
        let mut queries = vec![original.to_string()];
        if original.chars().any(crate::agent::local::is_cjk) {
            if let Some(llm) = &self.llm {
                match llm.complete(TRANSLATE_PROMPT, original).await {
                    Ok(raw) => {
                        if let Some(kw) = keywords_from_llm(&raw) {
                            if !kw.eq_ignore_ascii_case(original) {
                                queries.push(kw);
                            }
                        } else {
                            tracing::warn!(
                                skill = %self.name,
                                "翻译输出里没有可用英文关键词，只用原文检索"
                            );
                        }
                    }
                    Err(err) => {
                        tracing::warn!(skill = %self.name, error = %err, "查询翻译失败，只用原文检索")
                    }
                }
            }
        }
        if self.max_results == 0 {
            return Vec::new();
        }

        // 按 stage 分组（升序逐段查）。stages 与 sources 不同长时退回单段。
        let stages = if self.stages.len() == self.sources.len() {
            self.stages.clone()
        } else {
            vec![1u8; self.sources.len()]
        };
        let mut by_stage: std::collections::BTreeMap<u8, Vec<usize>> =
            std::collections::BTreeMap::new();
        for (idx, stage) in stages.iter().enumerate() {
            by_stage.entry(*stage).or_default().push(idx);
        }
        let stage_count = by_stage.len().max(1);
        let base = self.max_results / stage_count;
        let remainder = self.max_results % stage_count;

        let mut all: Vec<SourcedHit> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut carry = String::new();
        for (k, idxs) in by_stage.values().enumerate() {
            // 本段名额：均分，余数给最后一段
            let cap = (base + if k + 1 == stage_count { remainder } else { 0 }).max(1);
            let mut stage_queries = queries.clone();
            if !carry.is_empty() {
                stage_queries.push(carry.clone());
            }
            let mut stage_hits: Vec<SourcedHit> = Vec::new();
            for q in &stage_queries {
                for (name, hit) in self.search_sources(idxs, q, cap).await {
                    if seen.insert(hit.locator.clone()) {
                        stage_hits.push((name, hit));
                        if stage_hits.len() >= cap {
                            break;
                        }
                    }
                }
                if stage_hits.len() >= cap {
                    break;
                }
            }
            carry = harvest_terms(&stage_hits);
            all.extend(stage_hits);
            if all.len() >= self.max_results {
                break;
            }
        }
        all.truncate(self.max_results);
        all
    }

    /// 并发查询指定的一组源（每源 limit = cap），交错合并：源0第1条、源1第1条……总量 ≤ cap。
    async fn search_sources(&self, idxs: &[usize], query: &str, cap: usize) -> Vec<SourcedHit> {
        if cap == 0 {
            return Vec::new();
        }
        let per_source: Vec<(String, Vec<DocHit>)> = join_all(idxs.iter().map(|&i| {
            let source = self.sources[i].clone();
            async move {
                let hits = source.search(query, cap).await;
                (source.name().to_string(), hits)
            }
        }))
        .await;
        let mut merged: Vec<SourcedHit> = Vec::new();
        let rounds = per_source
            .iter()
            .map(|(_, hits)| hits.len())
            .max()
            .unwrap_or(0);
        'outer: for idx in 0..rounds {
            for (source, hits) in &per_source {
                if let Some(hit) = hits.get(idx) {
                    merged.push((source.clone(), hit.clone()));
                    if merged.len() >= cap {
                        break 'outer;
                    }
                }
            }
        }
        merged
    }

    /// 生成回答：优先 LLM 整理；LLM 未配置、失败或返回空答案时降级为摘录。
    async fn answer(&self, query: &str, hits: &[SourcedHit]) -> String {
        if let Some(llm) = &self.llm {
            let system = if llm.system_prompt().is_empty() {
                DEFAULT_SYSTEM_PROMPT
            } else {
                llm.system_prompt()
            };
            match llm.complete(system, &build_prompt(query, hits)).await {
                Ok(text) if !text.trim().is_empty() => return text,
                Ok(_) => {
                    tracing::warn!(skill = %self.name, "LLM 返回空答案，降级为检索摘录");
                }
                Err(err) => {
                    tracing::warn!(skill = %self.name, error = %err, "LLM 整理失败，降级为检索摘录");
                }
            }
        } else {
            tracing::warn!(skill = %self.name, "未配置 LLM，直接返回检索摘录");
        }
        self.excerpts(hits)
    }

    /// 摘录格式（降级回答）：
    /// `{trigger} 检索结果：` + 每条 `\n{i}. {title}（{source} · {locator}）\n{snippet}`，
    /// 条目间空行；snippet 为空的条目只有标题行。
    fn excerpts(&self, hits: &[SourcedHit]) -> String {
        let mut text = format!("{} 检索结果：", self.trigger);
        for (idx, (source, hit)) in hits.iter().enumerate() {
            text.push('\n');
            if idx > 0 {
                text.push('\n');
            }
            text.push_str(&format!(
                "{}. {}（{} · {}）",
                idx + 1,
                hit.title,
                source,
                hit.locator
            ));
            if !hit.snippet.is_empty() {
                text.push('\n');
                text.push_str(&hit.snippet);
            }
        }
        text
    }
}

/// 组装给 LLM 的用户内容：问题 + 带编号与出处的检索片段。
fn build_prompt(query: &str, hits: &[SourcedHit]) -> String {
    let mut prompt = format!("问题：{query}\n\n检索片段：");
    for (idx, (source, hit)) in hits.iter().enumerate() {
        prompt.push_str(&format!(
            "\n[{}] {}（{} · {}）\n{}",
            idx + 1,
            hit.title,
            source,
            hit.locator,
            hit.snippet
        ));
    }
    prompt
}

/// 把 LLM 的翻译输出收敛成英文检索关键词。
///
/// Lean 版模型常无视「只输出关键词」的指令、直接甩一整篇中文长文——直接拿它当查询词会
/// 命中一堆高频噪声词，把检索彻底污染（实测：`介绍一下刷怪机制` 被回成几千字中文，
/// 检索结果全是方块注册/包统计之类的垃圾）。这里只从输出里**抽 ASCII 英文词**（模型即便
/// 写中文也会点到官方英文术语，如 `Ambient Spawning`/`Mob Cap`），去停用词、去重、限量；
/// 抽不到英文词就返回 `None`（调用方退化到只用原文，宁可查不到也不给噪声）。
fn keywords_from_llm(raw: &str) -> Option<String> {
    const STOPS: &[&str] = &[
        "the", "a", "an", "of", "to", "in", "on", "and", "or", "is", "are", "be", "for", "with",
        "that", "this", "it", "as", "by", "at", "from", "into", "when", "which", "can", "will",
        "not", "no", "yes", "you", "your", "its", "their", "they", "has", "have", "was", "were",
        "does", "do", "how", "what", "why", "where", "there", "them", "than", "then", "also",
        "such", "each", "per", "one", "two", "more", "most", "all", "any", "some", "use", "used",
        "using", "about", "between", "within", "without", "through", "during", "before", "after",
        "above", "below", "under", "over", "out", "only", "just", "other", "same", "new", "old",
        "first", "last", "next", "see", "way", "make", "made", "get", "got", "set", "end", "start",
        "part", "parts", "main", "type", "types", "block", "blocks",
    ];
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut total = 0usize;
    for tok in raw.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if tok.len() < 2 || tok.len() > 32 {
            continue;
        }
        let lower = tok.to_ascii_lowercase();
        if STOPS.contains(&lower.as_str()) || lower.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if !seen.insert(lower) {
            continue;
        }
        total += tok.len() + 1;
        if total > 200 {
            break;
        }
        out.push(tok.to_string());
        if out.len() >= 16 {
            break;
        }
    }
    (!out.is_empty()).then(|| out.join(" "))
}

/// 从一段命中里榨取下一段可用的检索词：反引号包裹的标识符（MinecraftDocs 用它标注
/// 类/方法名）+ 内部带大写的驼峰词，按频次取前 8 个、空格连接。用于「先查文档定位、
/// 再翻源码」——把文档里点名的类/方法补进源码查询词。
fn harvest_terms(hits: &[SourcedHit]) -> String {
    use std::collections::HashMap;
    let mut freq: HashMap<String, usize> = HashMap::new();
    for (_, hit) in hits {
        let text = format!("{}\n{}", hit.title, hit.snippet);
        for seg in text.split('`').skip(1).step_by(2) {
            let t = seg.trim();
            if is_ident(t) {
                *freq.entry(t.to_string()).or_insert(0) += 2;
            }
        }
        for tok in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            if is_camel(tok) {
                *freq.entry(tok.to_string()).or_insert(0) += 1;
            }
        }
    }
    let mut terms: Vec<(String, usize)> = freq.into_iter().collect();
    terms.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    terms
        .into_iter()
        .take(8)
        .map(|(t, _)| t)
        .collect::<Vec<_>>()
        .join(" ")
}

/// 像标识符吗：字母开头，仅含字母数字/下划线/点，长度 3..=40。
fn is_ident(s: &str) -> bool {
    let len = s.chars().count();
    (3..=40).contains(&len)
        && s.starts_with(|c: char| c.is_ascii_alphabetic())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// 驼峰词：首字母大写且后面还有大写（区分 `RaidManager` 与句首词 `The`），长度 ≥ 4。
fn is_camel(s: &str) -> bool {
    let len = s.chars().count();
    (4..=40).contains(&len)
        && s.starts_with(|c: char| c.is_ascii_uppercase())
        && s.chars().skip(1).any(|c| c.is_ascii_uppercase())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// UTF-8 安全截断：超过 `max_chars` 个字符时按字符边界切掉尾部并追加标记。
fn truncate_answer(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        // 整条不超过上限，原样返回
        None => text.to_string(),
        // char_indices 保证 cut 落在字符边界上，切片不会 panic
        Some((cut, _)) => format!("{}…（已截断）", &text[..cut]),
    }
}

#[async_trait]
impl CommandHandler for DocQuerySkill {
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: self.name.clone(),
            aliases: vec![],
            trigger: self.trigger.clone(),
            description: self.description.clone(),
        }
    }

    /// 三端都认领；只做触发匹配，空查询也认领（交给 `handle` 发用法提示）。
    fn matches(&self, msg: &InboundMessage) -> bool {
        match_command(&msg.text, &self.trigger).is_some()
    }

    async fn handle(&self, ctx: &DispatchCtx<'_>) -> bool {
        let Some(query) = match_command(&ctx.msg.text, &self.trigger) else {
            // matches() 已把关；防御性兜底：不认领
            return false;
        };
        // 1. 空查询 → 用法提示
        if query.is_empty() {
            let _ = ctx
                .origin
                .send_text(&format!("用法：{} <查询词>", self.trigger))
                .await;
            return true;
        }
        // 2. 并发查所有源并合并（中文问题自动加查 LLM 译文，见 `search_queries`）
        let hits = self.search_queries(query).await;
        // 3. 无命中 → 明确告知查不到，不编
        if hits.is_empty() {
            let _ = ctx
                .origin
                .send_text(&format!(
                    "{} 没查到与「{}」相关的内容",
                    self.trigger, query
                ))
                .await;
            return true;
        }
        // 4/5. LLM 整理（失败降级摘录），统一截断后回源
        let answer = self.answer(query, &hits).await;
        let _ = ctx
            .origin
            .send_text(&truncate_answer(&answer, self.max_answer_chars))
            .await;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use axum::Json;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::Router;

    use crate::config::{LlmConfig, DEFAULT_SYSTEM_PROMPT};
    use crate::router::{Hub, ReplySink, Source};

    // ---------- 测试桩 ----------

    /// 固定命中的假文档源：记录收到的 (query, limit) 供断言。
    struct FakeSource {
        name: &'static str,
        hits: Vec<DocHit>,
        requests: Mutex<Vec<(String, usize)>>,
    }

    impl FakeSource {
        fn new(name: &'static str, hits: Vec<DocHit>) -> Self {
            Self {
                name,
                hits,
                requests: Mutex::new(Vec::new()),
            }
        }

        fn hit(title: &str, locator: &str, snippet: &str) -> DocHit {
            DocHit {
                title: title.into(),
                locator: locator.into(),
                snippet: snippet.into(),
            }
        }

        fn requests(&self) -> Vec<(String, usize)> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl DocumentSource for FakeSource {
        fn name(&self) -> &str {
            self.name
        }
        async fn search(&self, query: &str, limit: usize) -> Vec<DocHit> {
            self.requests
                .lock()
                .unwrap()
                .push((query.to_string(), limit));
            self.hits.iter().take(limit).cloned().collect()
        }
    }

    /// no-op Hub（handle 不会用到，仅为构造 DispatchCtx）。
    struct NoopHub;

    #[async_trait]
    impl Hub for NoopHub {
        async fn qq_send_text(&self, _group_id: Option<i64>, _text: &str) -> bool {
            true
        }
        async fn game_broadcast(&self, _text: &str) -> bool {
            false
        }
        async fn chatroom_post(
            &self,
            _source: &str,
            _content: &str,
            _sender_username: &str,
            _nickname: &str,
        ) -> bool {
            false
        }
    }

    /// 捕获 send_text 文本的假出口。
    #[derive(Default)]
    struct FakeSink {
        texts: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ReplySink for FakeSink {
        async fn send_text(&self, text: &str) -> bool {
            self.texts.lock().unwrap().push(text.to_string());
            true
        }
    }

    // ---------- LLM mock ----------

    type ReplyFn = Box<dyn Fn() -> (StatusCode, serde_json::Value) + Send + Sync>;

    struct MockState {
        body: Mutex<serde_json::Value>,
        reply: ReplyFn,
    }

    impl MockState {
        fn with_content(content: &str) -> Self {
            let content = content.to_string();
            Self {
                body: Mutex::new(serde_json::Value::Null),
                reply: Box::new(move || {
                    (
                        StatusCode::OK,
                        serde_json::json!({
                            "choices": [
                                { "message": { "role": "assistant", "content": content } }
                            ]
                        }),
                    )
                }),
            }
        }

        fn with_raw(status: StatusCode, json: serde_json::Value) -> Self {
            Self {
                body: Mutex::new(serde_json::Value::Null),
                reply: Box::new(move || (status, json.clone())),
            }
        }
    }

    async fn handler(
        State(st): State<Arc<MockState>>,
        Json(body): Json<serde_json::Value>,
    ) -> Response {
        *st.body.lock().unwrap() = body;
        let (status, json) = (st.reply)();
        (status, Json(json)).into_response()
    }

    async fn spawn_mock(state: Arc<MockState>) -> String {
        let app = Router::new()
            .route("/v1/chat/completions", post(handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/v1/chat/completions")
    }

    fn llm_client(api_url: String) -> LlmClient {
        LlmClient::new(LlmConfig {
            api_url,
            api_key: String::new(),
            model: "test-model".into(),
            timeout_secs: 5,
            max_answer_chars: 1000,
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
        })
        .unwrap()
    }

    // ---------- 构造助手 ----------

    fn skill(
        trigger: &str,
        sources: Vec<Arc<dyn DocumentSource>>,
        llm: Option<LlmClient>,
        max_results: usize,
    ) -> DocQuerySkill {
        DocQuerySkill::new(
            trigger.trim_start_matches('!'),
            trigger,
            "文档查询",
            sources,
            llm,
            max_results,
            1000,
        )
    }

    fn msg(text: &str) -> InboundMessage {
        InboundMessage {
            source: Source::QQ,
            text: text.into(),
            group_id: 1,
            user_id: 2,
            at_me: false,
        }
    }

    async fn run(skill: &DocQuerySkill, sink: &FakeSink, text: &str) -> bool {
        let hub = NoopHub;
        let m = msg(text);
        let ctx = DispatchCtx {
            hub: &hub,
            origin: sink,
            msg: &m,
        };
        skill.handle(&ctx).await
    }

    // ---------- 用例 ----------

    #[test]
    fn skill_matches_case_boundary_and_info() {
        let s = skill("!mc", vec![], None, 5);
        // 大小写不敏感
        assert!(s.matches(&msg("!mc 活塞")));
        assert!(s.matches(&msg("!MC 活塞")));
        // 空查询也认领（发用法提示）
        assert!(s.matches(&msg("!mc")));
        assert!(s.matches(&msg(" !mc\t")));
        // 边界：不吃掉更长的命令 / 非触发文本
        assert!(!s.matches(&msg("!mcs x")));
        assert!(!s.matches(&msg("!mcx")));
        assert!(!s.matches(&msg("!tm x")));
        assert!(!s.matches(&msg("hello !mc")));
        // 三端来源都认领
        assert!(s.matches(&InboundMessage {
            source: Source::Game {
                sender: "web".into(),
                author: "bob".into()
            },
            text: "!mc 活塞".into(),
            group_id: 0,
            user_id: 0,
            at_me: false,
        }));
        assert!(s.matches(&InboundMessage {
            source: Source::Chatroom {
                username: "alice".into()
            },
            text: "!mc 活塞".into(),
            group_id: 0,
            user_id: 0,
            at_me: false,
        }));

        let info = s.info();
        assert_eq!(info.name, "mc");
        assert!(info.aliases.is_empty());
        assert_eq!(info.trigger, "!mc");
        assert_eq!(info.description, "文档查询");
    }

    #[tokio::test]
    async fn skill_empty_query_sends_usage() {
        let s = skill("!mc", vec![], None, 5);
        let sink = FakeSink::default();

        assert!(run(&s, &sink, "!mc").await);
        assert!(run(&s, &sink, "!MC").await);
        assert_eq!(
            sink.texts.lock().unwrap().as_slice(),
            ["用法：!mc <查询词>", "用法：!mc <查询词>"]
        );
    }

    #[tokio::test]
    async fn skill_no_hits_reports_not_found() {
        let s = skill(
            "!mc",
            vec![Arc::new(FakeSource::new("alpha", vec![]))],
            None,
            5,
        );
        let sink = FakeSink::default();

        assert!(run(&s, &sink, "!mc 活塞").await);
        assert_eq!(
            sink.texts.lock().unwrap().as_slice(),
            ["!mc 没查到与「活塞」相关的内容"]
        );
    }

    #[test]
    fn keywords_from_llm_pulls_english_terms_out_of_a_chinese_essay() {
        // 模型无视「只输出关键词」，甩了一整篇中文长文：只抽英文术语，别带中文
        let essay = "Minecraft 中的刷怪机制主要分为两大类：\n## 一、自然刷怪（Ambient Spawning）\n光照 ≤ 7，Mob Cap 上限……";
        let kw = keywords_from_llm(essay).unwrap();
        for need in ["Minecraft", "Ambient", "Spawning", "Mob", "Cap"] {
            assert!(kw.contains(need), "missing {need} in {kw}");
        }
        assert!(!kw.chars().any(crate::agent::local::is_cjk), "no CJK expected: {kw}");
        // 正常的关键词输出照常
        assert_eq!(
            keywords_from_llm("mob spawning mechanics").unwrap(),
            "mob spawning mechanics"
        );
        // 全是中文、没有任何英文词 → None（调用方退化到只用原文）
        assert!(keywords_from_llm("刷怪机制如何工作，也没有英文术语").is_none());
    }

    #[tokio::test]
    async fn skill_staged_sources_query_doc_first_and_carry_identifiers() {
        // 文档层（stage 1）命中里点名了 `RaidManager`：源码层（stage 2）应收到把该标识符
        // 补进查询词的请求，且输出里文档片段排在源码片段之前。
        let doc = Arc::new(FakeSource::new(
            "minecraftdocs",
            vec![FakeSource::hit(
                "Raids",
                "docs/raid.md",
                "the `RaidManager` owns raids",
            )],
        ));
        let src = Arc::new(FakeSource::new(
            "mc-source",
            vec![FakeSource::hit("Raid.java", "26.1/Raid.java:1-60", "code")],
        ));
        let s = skill("!mc", vec![doc.clone(), src.clone()], None, 4).with_stages(vec![1, 2]);
        let sink = FakeSink::default();
        assert!(run(&s, &sink, "!mc raid mechanics").await);

        // 文档层只按原查询词查一次
        assert_eq!(doc.requests().len(), 1);
        // 源码层收到的查询词里带上了文档点名的标识符
        assert!(
            src.requests().iter().any(|(q, _)| q.contains("RaidManager")),
            "src requests: {:?}",
            src.requests()
        );

        // 输出里文档片段编号在前、源码在后
        let out = sink.texts.lock().unwrap().join("\n");
        let doc_pos = out.find("docs/raid.md").expect("doc hit present");
        let src_pos = out.find("26.1/Raid.java").expect("src hit present");
        assert!(doc_pos < src_pos, "doc should precede source: {out}");
    }

    #[tokio::test]
    async fn skill_merge_interleaves_sources_and_caps_at_max_results() {
        // 上限 3：alpha[0], beta[0], alpha[1]——交错且封顶
        let alpha = Arc::new(FakeSource::new(
            "alpha",
            vec![
                FakeSource::hit("活塞-1", "a:1", "片段A1"),
                FakeSource::hit("活塞-2", "a:2", "片段A2"),
                FakeSource::hit("活塞-3", "a:3", "片段A3"),
            ],
        ));
        let beta = Arc::new(FakeSource::new(
            "beta",
            vec![
                FakeSource::hit("活塞-1b", "b:1", "片段B1"),
                FakeSource::hit("活塞-2b", "b:2", "片段B2"),
            ],
        ));
        let s = skill("!mc", vec![alpha.clone(), beta.clone()], None, 3);
        let sink = FakeSink::default();
        assert!(run(&s, &sink, "!mc 活塞").await);
        let expected = "\
!mc 检索结果：
1. 活塞-1（alpha · a:1）
片段A1

2. 活塞-1b（beta · b:1）
片段B1

3. 活塞-2（alpha · a:2）
片段A2";
        assert_eq!(sink.texts.lock().unwrap().as_slice(), [expected]);

        // 每源收到的 limit = max_results，查询词正确
        assert_eq!(alpha.requests(), vec![("活塞".to_string(), 3)]);
        assert_eq!(beta.requests(), vec![("活塞".to_string(), 3)]);

        // 上限 5：两源全交错（alpha[0], beta[0], alpha[1], beta[1], alpha[2]）
        let alpha2 = FakeSource::new(
            "alpha",
            vec![
                FakeSource::hit("活塞-1", "a:1", "片段A1"),
                FakeSource::hit("活塞-2", "a:2", "片段A2"),
                FakeSource::hit("活塞-3", "a:3", "片段A3"),
            ],
        );
        let beta2 = FakeSource::new(
            "beta",
            vec![
                FakeSource::hit("活塞-1b", "b:1", "片段B1"),
                FakeSource::hit("活塞-2b", "b:2", "片段B2"),
            ],
        );
        let s = skill("!mc", vec![Arc::new(alpha2), Arc::new(beta2)], None, 5);
        let sink = FakeSink::default();
        assert!(run(&s, &sink, "!mc 活塞").await);
        let expected = "\
!mc 检索结果：
1. 活塞-1（alpha · a:1）
片段A1

2. 活塞-1b（beta · b:1）
片段B1

3. 活塞-2（alpha · a:2）
片段A2

4. 活塞-2b（beta · b:2）
片段B2

5. 活塞-3（alpha · a:3）
片段A3";
        assert_eq!(sink.texts.lock().unwrap().as_slice(), [expected]);
    }

    #[tokio::test]
    async fn skill_llm_success_sends_answer_and_prompt_cites_sources() {
        let state = Arc::new(MockState::with_content("活塞可以推动方块"));
        let url = spawn_mock(state.clone()).await;
        let s = skill(
            "!mc",
            vec![
                Arc::new(FakeSource::new(
                    "alpha",
                    vec![FakeSource::hit("活塞", "Piston.java:10", "活塞推动方块")],
                )),
                Arc::new(FakeSource::new(
                    "beta",
                    vec![FakeSource::hit("粘性活塞", "sticky:5", "可拉回方块")],
                )),
            ],
            Some(llm_client(url)),
            5,
        );
        let sink = FakeSink::default();

        assert!(run(&s, &sink, "!mc 活塞").await);
        // 发送的是 LLM 答案，而不是摘录
        assert_eq!(
            sink.texts.lock().unwrap().as_slice(),
            ["活塞可以推动方块"]
        );

        // prompt：system = 配置的默认提示词；user = 问题 + 编号片段 + 出处
        let body = state.body.lock().unwrap().clone();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], DEFAULT_SYSTEM_PROMPT);
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(
            messages[1]["content"],
            "问题：活塞\n\n检索片段：\n[1] 活塞（alpha · Piston.java:10）\n活塞推动方块\n[2] 粘性活塞（beta · sticky:5）\n可拉回方块"
        );
    }

    #[tokio::test]
    async fn skill_llm_failure_falls_back_to_excerpts() {
        // LLM 500 → 降级为摘录格式
        let url = spawn_mock(Arc::new(MockState::with_raw(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({}),
        )))
        .await;
        let s = skill(
            "!mc",
            vec![
                Arc::new(FakeSource::new(
                    "alpha",
                    vec![FakeSource::hit("活塞", "Piston.java:10", "活塞推动方块")],
                )),
                Arc::new(FakeSource::new(
                    "beta",
                    // 空 snippet：条目只有标题行
                    vec![FakeSource::hit("粘性活塞", "sticky:5", "")],
                )),
            ],
            Some(llm_client(url)),
            5,
        );
        let sink = FakeSink::default();

        assert!(run(&s, &sink, "!mc 活塞").await);
        let expected = "\
!mc 检索结果：
1. 活塞（alpha · Piston.java:10）
活塞推动方块

2. 粘性活塞（beta · sticky:5）";
        assert_eq!(sink.texts.lock().unwrap().as_slice(), [expected]);
    }

    #[tokio::test]
    async fn skill_without_llm_uses_excerpt_format() {
        let s = skill(
            "!mc",
            vec![Arc::new(FakeSource::new(
                "alpha",
                vec![FakeSource::hit("活塞", "Piston.java:10", "活塞推动方块")],
            ))],
            None,
            5,
        );
        let sink = FakeSink::default();

        assert!(run(&s, &sink, "!mc 活塞").await);
        let expected = "\
!mc 检索结果：
1. 活塞（alpha · Piston.java:10）
活塞推动方块";
        assert_eq!(sink.texts.lock().unwrap().as_slice(), [expected]);
    }

    #[tokio::test]
    async fn skill_truncates_overlong_answer() {
        let state = Arc::new(MockState::with_content(&"啊".repeat(120)));
        let url = spawn_mock(state).await;
        let s = DocQuerySkill::new(
            "mc",
            "!mc",
            "文档查询",
            vec![Arc::new(FakeSource::new(
                "alpha",
                vec![FakeSource::hit("活塞", "Piston.java:10", "片段")],
            ))],
            Some(llm_client(url)),
            5,
            50,
        );
        let sink = FakeSink::default();

        assert!(run(&s, &sink, "!mc 活塞").await);
        let expected = format!("{}…（已截断）", "啊".repeat(50));
        assert_eq!(sink.texts.lock().unwrap().as_slice(), [expected.as_str()]);
    }

    #[test]
    fn skill_truncate_is_utf8_safe() {
        assert_eq!(truncate_answer("abc", 5), "abc");
        assert_eq!(truncate_answer("abc", 3), "abc");
        assert_eq!(truncate_answer("abcd", 3), "abc…（已截断）");
        // 多字节字符边界：不 panic、不切半个字符
        assert_eq!(truncate_answer("🦀🦀🦀", 2), "🦀🦀…（已截断）");
        assert_eq!(truncate_answer("", 10), "");
        assert_eq!(truncate_answer("活塞", 1), "活…（已截断）");
    }

    /// 中文问题 + LLM → 先译文检索再原文检索；同一命中按 locator 去重。
    #[tokio::test]
    async fn skill_translates_cjk_query_via_llm_and_dedupes() {
        let state = Arc::new(MockState::with_content("guardian spawning"));
        let url = spawn_mock(state.clone()).await;
        let source = Arc::new(FakeSource::new(
            "mc-source",
            vec![FakeSource::hit("Guardian", "Guardian.java:421", "守卫者生成")],
        ));
        let s = skill("!mc", vec![source.clone()], Some(llm_client(url)), 5);
        let sink = FakeSink::default();

        assert!(run(&s, &sink, "!mc 守卫者怎么刷怪").await);
        // 两次检索：原文在前，LLM 译文在后
        let requests = source.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0, "守卫者怎么刷怪");
        assert_eq!(requests[1].0, "guardian spawning");
        // 两次查询命中同一 locator → prompt 里只出现一次（去重生效）
        let body = state.body.lock().unwrap().clone();
        let prompt = body["messages"][1]["content"].as_str().unwrap();
        assert!(prompt.contains("[1] Guardian（mc-source · Guardian.java:421）"));
        assert!(!prompt.contains("[2]"));
    }

    /// 纯 ASCII 查询不做翻译（无 LLM 调用成本）。
    #[tokio::test]
    async fn skill_ascii_query_skips_translation() {
        let state = Arc::new(MockState::with_content("answer"));
        let url = spawn_mock(state.clone()).await;
        let source = Arc::new(FakeSource::new(
            "mc-source",
            vec![FakeSource::hit("Spawner", "Spawner.java:1", "片段")],
        ));
        let s = skill("!mc", vec![source.clone()], Some(llm_client(url)), 5);

        assert!(run(&s, &FakeSink::default(), "!mc spawner").await);
        assert_eq!(source.requests().len(), 1);
    }

    /// 翻译调用失败 → 只用原文检索，行为不变。
    #[tokio::test]
    async fn skill_translation_failure_falls_back_to_original_query() {
        let url = spawn_mock(Arc::new(MockState::with_raw(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({}),
        )))
        .await;
        let source = Arc::new(FakeSource::new(
            "mc-source",
            vec![FakeSource::hit("Guardian", "Guardian.java:421", "片段")],
        ));
        let s = skill("!mc", vec![source.clone()], Some(llm_client(url)), 5);
        let sink = FakeSink::default();

        assert!(run(&s, &sink, "!mc 守卫者").await);
        assert_eq!(source.requests().len(), 1);
        assert_eq!(source.requests()[0].0, "守卫者");
        // 答案阶段的 LLM 也失败 → 摘录降级
        assert!(sink.texts.lock().unwrap()[0].contains("检索结果"));
    }
}
