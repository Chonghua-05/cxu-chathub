//! 检索质量评测（roadmap v0.3「检索质量评测常态化」）。
//!
//! 固定抽样问题集（`rust/eval/*.json`）+ 通过率统计：每条用例 = 查询 + 期望命中
//! （出处子串，any-of）+ 允许的最大排名；跑完输出 pass rate / MRR，供调分块与
//! 评分前后对比。评测对象是检索层（[`DocumentSource`]）——与线上技能同一条
//! `search` 路径，保证离线可复现；问题集的 `corpus` 段与
//! `agent.skills[].sources[]` 完全同构，建源走同一个 [`super::build_source`]。
//!
//! 两种用例形态（对应线上技能的两级行为）：
//! - `query`（关键词形态）：始终执行，构成确定性回归基线；ASCII 查询即使配了
//!   LLM 也不翻译——与技能层行为一致；
//! - `question`（自然语言形态，如中文问句）：需要 `--llm` 提供 LLM 才执行
//!   （按技能层同款流程先翻译再双语检索合并）；无 LLM 时记 **SKIP**，
//!   不计入通过率分母——无 LLM 的部署里它必然查不中（英文语料），
//!   计入失败只会污染关键词回归基线。
//!
//! 运行器见 `examples/eval_retrieval.rs`：
//! `cargo run --release --example eval_retrieval -- rust/eval/<问题集>.json [--llm <config.json>]`

use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tracing::warn;

use super::llm::LlmClient;
use super::skill::TRANSLATE_PROMPT;
use super::{DocHit, DocumentSource};
use crate::config::SourceConfig;

/// 一条评测用例：查询 + 期望命中 + 排名要求。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EvalCase {
    /// 用例标识（报告展示用）
    pub id: String,
    /// 关键词形态查询词（确定性基线；与线上技能的 ASCII 查询行为一致）
    pub query: String,
    /// 自然语言形态问题（如中文问句）。需要 LLM 翻译（`--llm`）才执行，
    /// 否则跳过；非空时优先于 `query`
    pub question: String,
    /// 期望命中：any-of 子串，与「出处 locator + 标题」（percent 解码、小写化后）
    /// 做包含匹配——本地源 locator 是 `文件:行号区间`，云端源是 URL（含中文/空格的
    /// 路径是百分号编码的，先解码再匹配，期望值才能按可读文本书写）
    pub expect: Vec<String>,
    /// 期望命中允许的最大排名（1-based，默认 1 = 必须排第一）
    pub max_rank: usize,
    /// 期望的依据（已实测锚点 / 官方映射类名推断等），报告里展示
    pub note: String,
}

impl Default for EvalCase {
    fn default() -> Self {
        Self {
            id: String::new(),
            query: String::new(),
            question: String::new(),
            expect: Vec::new(),
            max_rank: 1,
            note: String::new(),
        }
    }
}

/// 一份固定抽样问题集：语料声明 + 用例列表。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct QuestionSet {
    /// 问题集名（报告标题用）
    pub name: String,
    pub description: String,
    /// 语料声明，与 `agent.skills[].sources[]` 同构（type: local / mediawiki / repo）；
    /// 缺省视为无效（运行器报错退出）
    pub corpus: Option<SourceConfig>,
    /// 每次检索取的命中数（top-k 评测窗口，默认 5）
    pub top_k: usize,
    pub cases: Vec<EvalCase>,
}

impl Default for QuestionSet {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            corpus: None,
            top_k: 5,
            cases: Vec::new(),
        }
    }
}

/// 单条用例的评测结果。
#[derive(Debug, Clone)]
pub struct CaseResult {
    pub id: String,
    pub query: String,
    pub note: String,
    /// 跳过：自然语言用例在无 LLM 时未执行（不计入通过率）
    pub skipped: bool,
    /// 通过：存在期望命中且排名 ≤ max_rank
    pub pass: bool,
    /// 用例要求的最大排名（原样带回，供报告展示）
    pub max_rank: usize,
    /// 最佳期望命中的排名（1-based）；未命中为 None
    pub rank: Option<usize>,
    /// 实际执行的检索词（翻译激活时含译文，报告展示用）
    pub queries: Vec<String>,
    /// 本次执行是否用了 LLM 译文检索
    pub used_translation: bool,
    /// 检索返回的合并 top-k（排名, 标题, 出处），报告展示用
    pub hits: Vec<(usize, String, String)>,
}

/// 一份问题集的评测结果。
#[derive(Debug, Clone)]
pub struct SetReport {
    pub name: String,
    pub top_k: usize,
    pub results: Vec<CaseResult>,
    pub elapsed: Duration,
}

impl SetReport {
    /// 计入统计的用例（跳过的不算）。
    fn effective(&self) -> impl Iterator<Item = &CaseResult> {
        self.results.iter().filter(|r| !r.skipped)
    }

    /// 通过用例数（跳过的不算）。
    pub fn pass_count(&self) -> usize {
        self.effective().filter(|r| r.pass).count()
    }

    /// 通过率 = 通过数 / 计入统计的用例数（全跳过或空集时返回 1.0，避免误报门禁失败）。
    pub fn pass_rate(&self) -> f64 {
        let total = self.effective().count();
        if total == 0 {
            return 1.0;
        }
        self.pass_count() as f64 / total as f64
    }

    /// MRR（Mean Reciprocal Rank）：命中项排名倒数的平均，未命中记 0；跳过的不计。
    pub fn mrr(&self) -> f64 {
        let total = self.effective().count();
        if total == 0 {
            return 0.0;
        }
        let sum: f64 = self
            .effective()
            .map(|r| r.rank.map_or(0.0, |rank| 1.0 / rank as f64))
            .sum();
        sum / total as f64
    }
}

/// 跑一份问题集：逐条检索并核对期望（与线上技能同一条检索路径）。
/// `llm` 仅用于 `question` 用例的查询翻译（与技能层同一提示词）。
pub async fn run_question_set(
    set: &QuestionSet,
    source: &dyn DocumentSource,
    llm: Option<&LlmClient>,
) -> SetReport {
    let started = Instant::now();
    let mut results = Vec::with_capacity(set.cases.len());
    for case in &set.cases {
        let base = if case.question.is_empty() {
            case.query.clone()
        } else {
            case.question.clone()
        };
        if !case.question.is_empty() && llm.is_none() {
            results.push(CaseResult {
                id: case.id.clone(),
                query: base,
                note: case.note.clone(),
                skipped: true,
                pass: false,
                max_rank: case.max_rank,
                rank: None,
                queries: Vec::new(),
                used_translation: false,
                hits: Vec::new(),
            });
            continue;
        }

        // 与技能层同款：含 CJK 的查询先经 LLM 译成英文关键词，原文与译文各查一遍、
        // 按 locator 去重合并；纯 ASCII 查询不翻译（无 LLM 成本，行为一致）。
        let mut queries = vec![base.clone()];
        let mut used_translation = false;
        if base.chars().any(super::local::is_cjk) {
            if let Some(llm) = llm {
                match llm.complete(TRANSLATE_PROMPT, &base).await {
                    Ok(translated) => {
                        let translated = translated.trim();
                        if !translated.is_empty() && !translated.eq_ignore_ascii_case(&base) {
                            queries.push(translated.to_string());
                            used_translation = true;
                        }
                    }
                    Err(err) => {
                        warn!(case = %case.id, error = %err, "评测查询翻译失败，只用原文检索")
                    }
                }
            }
        }

        let hits = search_merged(source, &queries, set.top_k).await;
        let rank = best_expect_rank(&case.expect, &hits);
        results.push(CaseResult {
            id: case.id.clone(),
            query: base,
            note: case.note.clone(),
            skipped: false,
            pass: matches!(rank, Some(r) if r <= case.max_rank),
            max_rank: case.max_rank,
            rank,
            queries,
            used_translation,
            hits: hits
                .iter()
                .enumerate()
                .map(|(idx, hit)| (idx + 1, hit.title.clone(), hit.locator.clone()))
                .collect(),
        });
    }
    SetReport {
        name: set.name.clone(),
        top_k: set.top_k,
        results,
        elapsed: started.elapsed(),
    }
}

/// 多查询合并检索：逐查询取 top-k、按 locator 去重、总量 ≤ top_k
/// （与 `DocQuerySkill::search_queries` 同语义）。
async fn search_merged(
    source: &dyn DocumentSource,
    queries: &[String],
    top_k: usize,
) -> Vec<DocHit> {
    let mut merged: Vec<DocHit> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if top_k == 0 {
        return merged;
    }
    for query in queries {
        for hit in source.search(query, top_k).await {
            if seen.insert(hit.locator.clone()) {
                merged.push(hit);
                if merged.len() >= top_k {
                    return merged;
                }
            }
        }
    }
    merged
}

/// 期望命中检查：any-of 子串（locator + 标题、percent 解码、小写化）→ 最佳排名。
fn best_expect_rank(expect: &[String], hits: &[DocHit]) -> Option<usize> {
    let needles: Vec<String> = expect
        .iter()
        .map(|e| e.to_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    if needles.is_empty() {
        return None;
    }
    hits.iter().enumerate().find_map(|(idx, hit)| {
        let haystack = format!(
            "{} {}",
            percent_decode(&hit.locator),
            percent_decode(&hit.title)
        )
        .to_lowercase();
        needles
            .iter()
            .any(|needle| haystack.contains(needle))
            .then_some(idx + 1)
    })
}

/// 最小 percent-decode：`%XX` 还原为字节，其余原样保留；非法序列（如结尾孤立 `%`）
/// 原样保留。云端源 locator 里的路径是百分号编码的（含中文/空格的文件名），
/// 期望值按可读文本书写，匹配前先解码。
fn percent_decode(input: &str) -> String {
    fn hex_val(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(high * 16 + low);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use axum::Json;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::Router;

    use crate::agent::local::LocalDocSource;
    use crate::config::LlmConfig;

    /// 建临时语料：`guide.md`（第一节「活塞」、第二节「末影」）与 `misc.md`（无关内容）。
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("guide.md"),
            "# 活塞与红石\n活塞推动方块向上运动。\n\n## 末地传送门\n末影之眼可以定位要塞。\n",
        )
        .unwrap();
        std::fs::write(root.join("misc.md"), "完全无关的句子。\n").unwrap();
        (dir, root)
    }

    fn set_from_json(json: &str) -> QuestionSet {
        serde_json::from_str(json).expect("问题集 JSON 解析失败")
    }

    // ---------- LLM mock（与 skill.rs 测试同款套路） ----------

    type ReplyFn = Box<dyn Fn() -> (StatusCode, serde_json::Value) + Send + Sync>;

    struct MockState {
        bodies: std::sync::Mutex<Vec<serde_json::Value>>,
        reply: ReplyFn,
    }

    impl MockState {
        fn with_content(content: &str) -> Self {
            let content = content.to_string();
            Self {
                bodies: std::sync::Mutex::new(Vec::new()),
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
    }

    async fn handler(State(st): State<std::sync::Arc<MockState>>, Json(body): Json<serde_json::Value>) -> Response {
        st.bodies.lock().unwrap().push(body);
        let (status, json) = (st.reply)();
        (status, Json(json)).into_response()
    }

    async fn spawn_mock(state: std::sync::Arc<MockState>) -> String {
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
            system_prompt: "test".into(),
        })
        .unwrap()
    }

    // ---------- 用例 ----------

    /// 解析 + 默认值：max_rank 缺省 1、top_k 缺省 5、question 缺省为空。
    #[test]
    fn question_set_parses_with_defaults() {
        let set = set_from_json(
            r#"{
                "name": "demo",
                "corpus": { "type": "local", "root": "/tmp/docs" },
                "cases": [ { "id": "a", "query": "活塞", "expect": ["guide.md"] } ]
            }"#,
        );
        assert_eq!(set.top_k, 5);
        assert_eq!(set.cases.len(), 1);
        assert_eq!(set.cases[0].max_rank, 1);
        assert!(set.cases[0].question.is_empty());
        assert!(matches!(set.corpus, Some(SourceConfig::Local { .. })));
    }

    /// 端到端：真实 LocalDocSource + 问题集 → 命中排名、通过判定与指标。
    #[tokio::test]
    async fn eval_runs_against_local_source_and_reports() {
        let (_dir, root) = fixture();
        let set = set_from_json(&format!(
            r#"{{
                "name": "demo",
                "corpus": {{ "type": "local", "root": "{}" }},
                "top_k": 3,
                "cases": [
                    {{ "id": "hit-first", "query": "活塞", "expect": ["guide.md"], "max_rank": 1 }},
                    {{ "id": "hit-second", "query": "末影", "expect": ["guide.md"], "max_rank": 1 }},
                    {{ "id": "miss", "query": "无关", "expect": ["guide.md"], "max_rank": 3 }}
                ]
            }}"#,
            root.display()
        ));
        let source = super::super::build_source(set.corpus.as_ref().unwrap()).unwrap();
        let report = run_question_set(&set, source.as_ref(), None).await;

        assert_eq!(report.results.len(), 3);
        // 「活塞」：guide.md 第一节标题加成排第一
        assert!(report.results[0].pass);
        assert_eq!(report.results[0].rank, Some(1));
        // 「末影」：只有第二节命中，但文件级聚合后仍是该文件（排名 1）
        assert!(report.results[1].pass);
        // 「无关」：misc.md 不含 expect 的 guide.md → 未命中 → 失败
        assert!(!report.results[2].pass);
        assert_eq!(report.results[2].rank, None);

        assert_eq!(report.pass_count(), 2);
        assert!((report.pass_rate() - 2.0 / 3.0).abs() < 1e-9);
        assert!((report.mrr() - 2.0 / 3.0).abs() < 1e-9);
    }

    /// max_rank 边界：命中在第 2 名时，max_rank=1 失败、max_rank=2 通过。
    #[tokio::test]
    async fn eval_max_rank_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.md"), "# 活塞\n活塞内容。\n").unwrap();
        std::fs::write(root.join("b.md"), "活塞提及但主体是末影内容。\n").unwrap();
        let set = set_from_json(&format!(
            r#"{{
                "name": "rank",
                "corpus": {{ "type": "local", "root": "{}" }},
                "top_k": 5,
                "cases": [
                    {{ "id": "strict", "query": "活塞", "expect": ["b.md"], "max_rank": 1 }},
                    {{ "id": "loose", "query": "活塞", "expect": ["b.md"], "max_rank": 2 }}
                ]
            }}"#,
            root.display()
        ));
        let source = super::super::build_source(set.corpus.as_ref().unwrap()).unwrap();
        let report = run_question_set(&set, source.as_ref(), None).await;

        // a.md 标题加成必排第一，b.md 第二
        assert_eq!(report.results[0].rank, Some(2));
        assert!(!report.results[0].pass);
        assert!(report.results[1].pass);
    }

    /// expect 大小写不敏感；空 expect / 空查询不通过。
    #[tokio::test]
    async fn eval_expect_is_case_insensitive_and_empty_expect_fails() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("Guide.md"), "# 活塞\n内容。\n").unwrap();
        let set = set_from_json(&format!(
            r#"{{
                "name": "case",
                "corpus": {{ "type": "local", "root": "{}" }},
                "cases": [
                    {{ "id": "upper", "query": "活塞", "expect": ["GUIDE.MD"] }},
                    {{ "id": "empty-expect", "query": "活塞", "expect": [] }}
                ]
            }}"#,
            root.display()
        ));
        let source = super::super::build_source(set.corpus.as_ref().unwrap()).unwrap();
        let report = run_question_set(&set, source.as_ref(), None).await;
        assert!(report.results[0].pass);
        assert!(!report.results[1].pass);
        assert_eq!(report.results[1].rank, None);
    }

    /// question 用例 + 无 LLM → SKIP，不计入通过率分母。
    #[tokio::test]
    async fn eval_question_case_skips_without_llm() {
        let (_dir, root) = fixture();
        let set = set_from_json(&format!(
            r#"{{
                "name": "skip",
                "corpus": {{ "type": "local", "root": "{}" }},
                "cases": [
                    {{ "id": "nl", "question": "活塞怎么推动方块", "expect": ["guide.md"] }},
                    {{ "id": "kw", "query": "活塞", "expect": ["guide.md"] }}
                ]
            }}"#,
            root.display()
        ));
        let source = LocalDocSource::new("t", &root, vec![".md".into()]);
        let report = run_question_set(&set, &source, None).await;

        assert!(report.results[0].skipped);
        assert!(!report.results[0].pass);
        // 分母只有 keyword 用例 → 通过率 1.0
        assert!((report.pass_rate() - 1.0).abs() < 1e-9);
        assert!(report.results[1].pass);
    }

    /// question 用例 + LLM → 先译文检索再原文检索（与技能层同款），译文命中即通过。
    #[tokio::test]
    async fn eval_question_case_translates_via_llm_and_merges() {
        let state = std::sync::Arc::new(MockState::with_content("piston pushes blocks"));
        let url = spawn_mock(state.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // 语料只有英文正文：原文中文查询必然落空，命中必须来自译文查询
        std::fs::write(root.join("piston.md"), "The piston pushes blocks upward.\n").unwrap();
        let set = set_from_json(&format!(
            r#"{{
                "name": "nl",
                "corpus": {{ "type": "local", "root": "{}" }},
                "cases": [
                    {{ "id": "nl-piston", "question": "活塞怎么推动方块", "expect": ["piston.md"] }}
                ]
            }}"#,
            root.display()
        ));
        let source = LocalDocSource::new("t", &root, vec![".md".into()]);
        let llm = llm_client(url);
        let report = run_question_set(&set, &source, Some(&llm)).await;

        assert!(!report.results[0].skipped);
        assert!(report.results[0].used_translation);
        assert_eq!(
            report.results[0].queries,
            vec!["活塞怎么推动方块".to_string(), "piston pushes blocks".to_string()]
        );
        assert!(report.results[0].pass);
        assert_eq!(report.results[0].rank, Some(1));
        // LLM 被调用了一次（翻译）
        assert_eq!(state.bodies.lock().unwrap().len(), 1);
    }

    /// ASCII 关键词用例 + LLM → 不触发翻译（与技能层一致，零 LLM 成本）。
    #[tokio::test]
    async fn eval_ascii_query_skips_translation() {
        let state = std::sync::Arc::new(MockState::with_content("piston"));
        let url = spawn_mock(state.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("piston.md"), "The piston pushes blocks upward.\n").unwrap();
        let set = set_from_json(&format!(
            r#"{{
                "name": "kw",
                "corpus": {{ "type": "local", "root": "{}" }},
                "cases": [ {{ "id": "kw", "query": "piston", "expect": ["piston.md"] }} ]
            }}"#,
            root.display()
        ));
        let source = LocalDocSource::new("t", &root, vec![".md".into()]);
        let llm = llm_client(url);
        let report = run_question_set(&set, &source, Some(&llm)).await;

        assert!(report.results[0].pass);
        assert!(!report.results[0].used_translation);
        assert_eq!(report.results[0].queries.len(), 1);
        assert_eq!(state.bodies.lock().unwrap().len(), 0);
    }

    /// percent-decode：中文多字节、空格、非法序列原样保留。
    #[test]
    fn percent_decode_handles_cjk_space_and_invalid() {
        assert_eq!(percent_decode("%E8%AE%A1%E5%88%92%E5%88%BB"), "计划刻");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%ZZ"), "%ZZ");
        assert_eq!(percent_decode("plain"), "plain");
    }
}
