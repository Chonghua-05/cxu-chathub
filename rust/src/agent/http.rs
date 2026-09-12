//! MediaWiki 站点检索（[`MediaWikiSource`]，走站点 `api.php`）。
//!
//! 调用模式对齐 astrbot minecraft_wiki 插件的两段式：**关键词检索 → 整页上下文**。
//! 1. `list=search` 拿命中条目（标题 + 含 HTML 标签的搜索摘要）；查询先做问句
//!    清洗（去掉「查一下/怎么/是什么」等废词，只留检索词，参考插件的关键词提取）；
//! 2. 按 `prop=extracts&explaintext=1` 批量拉正文摘录，snippet 取前 1600 字符
//!    （给 LLM 足够完整的页面上下文）；条目缺 extract 时退化为第一步摘要去 HTML
//!    标签后的纯文本。
//!
//! 失败语义：非 2xx / 网络错误 / JSON 解析失败一律 warn + 空列表——
//! 「查不到就说查不到，不编」（roadmap 非目标约束）。

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tracing::warn;

use super::{DocHit, DocumentSource};

/// snippet 最长字符数（按 Unicode 字符计，不按字节）。
/// 取整页开头而非 400 字摘要——LLM 拿到的上下文越完整，回答越靠谱。
const SNIPPET_MAX_CHARS: usize = 1600;

/// prop=extracts 单次请求的标题批量上限（MediaWiki exlimit 常规上限 50）。
const EXTRACT_BATCH_SIZE: usize = 50;

/// 检索清洗用的问句废词：整词剔除（最长优先），不含检索语义只含提问语气。
const QUESTION_STOPWORDS: &[&str] = &[
    "查询一下", "查一下", "查询", "查下", "帮我查", "帮我", "请问", "看看", "想知道",
    "是多少", "是什么意思", "是什么", "什么叫", "什么是", "怎么用", "怎么做", "怎么样",
    "怎么", "如何", "为什么", "为啥", "多少", "几个", "哪些", "一下",
];

/// 问句清洗：剔除问句废词与首尾标点，保留检索词与版本号（`1.21`、`25w09a`）。
/// 插件（astrbot minecraft_wiki）的 focus_keywords 思路的零成本版——
/// 把「帮我查一下黑曜石的爆炸抗性是多少？」收敛成「黑曜石的爆炸抗性」。
pub(crate) fn clean_search_query(query: &str) -> String {
    let mut cleaned = query.to_string();
    for word in QUESTION_STOPWORDS {
        cleaned = cleaned.replace(word, " ");
    }
    cleaned
        .split_whitespace()
        .map(|term| term.trim_matches(|c: char| "？?。，,！!、·~".contains(c)))
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// MediaWiki 站点检索（api.php）：list=search 拿命中 → 批量 prop=extracts 拿正文摘录。
/// 失败/超时 → 空 vec + warn（「查不到就说查不到」）。
pub struct MediaWikiSource {
    name: String,
    /// api.php 完整地址，如 `https://minecraft.wiki/w/api.php`。
    api_url: String,
    /// 条目 URL 前缀：api_url 去掉结尾 `/api.php`（无则整串）。
    base_url: String,
    /// 超时 15s、连接 5s 的共享客户端。
    client: reqwest::Client,
}

impl MediaWikiSource {
    /// `api_url` 形如 `https://minecraft.wiki/w/api.php`；
    /// 条目 URL = `{base_url}/wiki/{标题，空格换 _}`。
    pub fn new(
        name: impl Into<String>,
        api_url: impl Into<String>,
    ) -> Result<Self, reqwest::Error> {
        let api_url = api_url.into();
        let base_url = api_url
            .strip_suffix("/api.php")
            .unwrap_or(&api_url)
            .to_string();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            // MediaWiki API 规范要求自报身份；Cloudflare 会对空 UA 直接 403
            .user_agent(concat!("cxu-chathub/", env!("CARGO_PKG_VERSION"), " (community doc bot)"))
            .build()?;
        Ok(Self {
            name: name.into(),
            api_url,
            base_url,
            client,
        })
    }

    /// GET api.php 并解析 JSON；网络错误 / 非 2xx / 解析失败一律 warn + None
    /// （调用方收到 None 即返回空列表，不扩散错误）。
    async fn get_json(&self, params: &[(&str, &str)]) -> Option<Value> {
        let resp = match self.client.get(&self.api_url).query(params).send().await {
            Ok(resp) => resp,
            Err(err) => {
                warn!(source = %self.name, url = %self.api_url, error = %err, "MediaWiki 请求失败");
                return None;
            }
        };
        if !resp.status().is_success() {
            warn!(source = %self.name, status = %resp.status(), "MediaWiki 返回非 2xx");
            return None;
        }
        match resp.json::<Value>().await {
            Ok(body) => Some(body),
            Err(err) => {
                warn!(source = %self.name, error = %err, "MediaWiki 响应 JSON 解析失败");
                None
            }
        }
    }
}

#[async_trait]
impl DocumentSource for MediaWikiSource {
    fn name(&self) -> &str {
        &self.name
    }

    async fn search(&self, query: &str, limit: usize) -> Vec<DocHit> {
        if limit == 0 || query.trim().is_empty() {
            return Vec::new();
        }
        // 问句清洗后再检索（废词会稀释 MediaWiki 全文检索的相关度）
        let cleaned = clean_search_query(query);
        if cleaned.is_empty() {
            return Vec::new();
        }
        let srlimit = limit.to_string();

        // 第零步：标题直达（对齐 astrbot minecraft_wiki 插件的 get_page_by_title /
        // `title:` 前缀思路）：把清洗后的检索词逐个当条目名试查（一次批量请求，
        // `redirects=1` 自动跟随重定向），命中即拿整页摘录并排在最前——「活塞」
        // 这类标准条目名不该输给只在正文里提到它的快照页。
        let title_hits = self.lookup_titles(&cleaned).await;
        if title_hits.len() >= limit {
            return title_hits
                .into_iter()
                .take(limit)
                .map(|(title, extract)| self.doc_hit_from_extract(&title, &extract))
                .collect();
        }

        // 第一步：list=search 拿命中条目（保持 API 返回的相关度顺序）。
        let body = match self
            .get_json(&[
                ("action", "query"),
                ("format", "json"),
                ("list", "search"),
                ("srlimit", srlimit.as_str()),
                ("srsearch", cleaned.as_str()),
            ])
            .await
        {
            Some(body) => body,
            None => return Vec::new(),
        };
        let found: Vec<(String, Option<u64>, String)> = body
            .pointer("/query/search")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let title = item.get("title")?.as_str()?.to_string();
                        let pageid = item.get("pageid").and_then(Value::as_u64);
                        let html = item
                            .get("snippet")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        Some((title, pageid, html))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // 标题直达已命中的条目不再重复出现
        let direct_titles: std::collections::HashSet<&str> =
            title_hits.iter().map(|(title, _)| title.as_str()).collect();
        let found: Vec<_> = found
            .into_iter()
            .filter(|(title, _, _)| !direct_titles.contains(title.as_str()))
            .collect();
        if found.is_empty() {
            // 只有标题直达命中：直接返回，不再发第二步请求
            return title_hits
                .into_iter()
                .take(limit)
                .map(|(title, extract)| self.doc_hit_from_extract(&title, &extract))
                .collect();
        }

        // 第二步：批量 prop=extracts 拉正文摘录（pageid -> extract）。
        let mut extract_by_pageid: HashMap<u64, String> = HashMap::new();
        for batch in found.chunks(EXTRACT_BATCH_SIZE) {
            let titles = batch
                .iter()
                .map(|(title, _, _)| title.as_str())
                .collect::<Vec<_>>()
                .join("|");
            let body = match self
                .get_json(&[
                    ("action", "query"),
                    ("format", "json"),
                    ("prop", "extracts"),
                    ("explaintext", "1"),
                    ("exlimit", "max"),
                    ("titles", titles.as_str()),
                ])
                .await
            {
                Some(body) => body,
                None => return Vec::new(),
            };
            let Some(pages) = body.pointer("/query/pages").and_then(Value::as_object) else {
                continue;
            };
            for (key, page) in pages {
                let Ok(pageid) = key.parse::<u64>() else {
                    continue; // 负数 pageid 表示条目缺失，取不到 extract
                };
                if let Some(extract) = page.get("extract").and_then(Value::as_str) {
                    extract_by_pageid.insert(pageid, extract.to_string());
                }
            }
        }

        // 合并：标题直达命中在前，全文搜索命中补足到 limit
        let mut hits: Vec<DocHit> = title_hits
            .into_iter()
            .map(|(title, extract)| self.doc_hit_from_extract(&title, &extract))
            .collect();
        for (title, pageid, html) in found {
            if hits.len() >= limit {
                break;
            }
            let snippet = match pageid.and_then(|id| extract_by_pageid.get(&id)) {
                Some(extract) => truncate_chars(extract, SNIPPET_MAX_CHARS),
                None => strip_html_tags(&html), // extract 缺失：退化为第一步摘要去标签
            };
            hits.push(DocHit {
                locator: format!("{}/wiki/{}", self.base_url, title.replace(' ', "_")),
                title,
                snippet,
            });
        }
        hits
    }
}

/// 标题直达的检索词上限（一次批量 `titles=` 请求）。
const TITLE_LOOKUP_MAX_TERMS: usize = 3;

impl MediaWikiSource {
    /// 标题直达：把清洗后的查询词（最多 [`TITLE_LOOKUP_MAX_TERMS`] 个）当条目名
    /// 批量试查，返回 (条目名, 整页纯文本)。试查前逐词剥离尾部虚词——
    /// 问句清洗后残留的「活塞是」要收敛成「活塞」才对得上条目名。
    /// 请求失败/全部缺失 → 空 vec（错误不扩散，交给全文搜索兜底）。
    async fn lookup_titles(&self, cleaned_query: &str) -> Vec<(String, String)> {
        let terms: Vec<String> = cleaned_query
            .split_whitespace()
            .take(TITLE_LOOKUP_MAX_TERMS)
            .map(strip_trailing_particles)
            .filter(|term| !term.is_empty())
            .collect();
        if terms.is_empty() {
            return Vec::new();
        }
        let titles = terms.join("|");
        let body = match self
            .get_json(&[
                ("action", "query"),
                ("format", "json"),
                ("redirects", "1"),
                ("prop", "extracts"),
                ("explaintext", "1"),
                ("titles", titles.as_str()),
            ])
            .await
        {
            Some(body) => body,
            None => return Vec::new(),
        };
        let Some(pages) = body.pointer("/query/pages").and_then(Value::as_object) else {
            return Vec::new();
        };
        pages
            .values()
            .filter_map(|page| {
                if page.get("missing").is_some() {
                    return None; // 没有这个条目名
                }
                let title = page.get("title")?.as_str()?.to_string();
                let extract = page.get("extract")?.as_str()?.to_string();
                Some((title, extract))
            })
            .collect()
    }

    /// 条目名 + 整页摘录 → 命中项（locator = {base}/wiki/{标题，空格换 _}）。
    fn doc_hit_from_extract(&self, title: &str, extract: &str) -> DocHit {
        DocHit {
            locator: format!("{}/wiki/{}", self.base_url, title.replace(' ', "_")),
            title: title.to_string(),
            snippet: truncate_chars(extract, SNIPPET_MAX_CHARS),
        }
    }
}

/// 截前 `max` 个 Unicode 字符（不按字节，避免切开多字节字符 panic）。
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// 剥掉词尾的虚词单字（是/的/了/吗/呢/吧/啊）——「活塞是」→「活塞」。
/// 只动尾部、不动开头，避免伤到「的」在词中间的专名。
fn strip_trailing_particles(term: &str) -> String {
    let mut current = term;
    loop {
        let trimmed = current.trim_end_matches(|c| {
            matches!(c, '是' | '的' | '了' | '吗' | '呢' | '吧' | '啊')
        });
        if trimmed.is_empty() || trimmed.len() == current.len() {
            return current.to_string();
        }
        current = trimmed;
    }
}

/// 去掉搜索摘要里的 HTML 标签（如 `<span class="searchmatch">`），保留纯文本。
/// 实体（`&amp;` 等）按原样保留——保持简单。
fn strip_html_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Query, State};
    use axum::http::{header, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::{Json, Router};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// mock 行为模式。
    #[derive(Clone, Copy)]
    enum MockMode {
        /// 正常两步应答。
        Normal,
        /// 第一步直接 500。
        ServerError,
        /// 第一步返回非法 JSON。
        MalformedJson,
    }

    struct MockCtx {
        mode: MockMode,
        /// 标题直达请求（带 `redirects` 参数）的应答。
        title_lookup: Value,
        step1: Value,
        step2: Value,
        captured: Mutex<Vec<HashMap<String, String>>>,
    }

    /// `/api.php` 处理器：按 query 参数区分请求（标题直达带 `redirects`，
    /// 第二步带 `prop=extracts`，第一步是 `list=search`）。
    async fn handle_api_php(
        State(ctx): State<Arc<MockCtx>>,
        Query(params): Query<HashMap<String, String>>,
    ) -> Response {
        ctx.captured.lock().unwrap().push(params.clone());
        if params.contains_key("redirects") {
            return Json(ctx.title_lookup.clone()).into_response();
        }
        if params.contains_key("prop") {
            return Json(ctx.step2.clone()).into_response();
        }
        match ctx.mode {
            MockMode::Normal => Json(ctx.step1.clone()).into_response(),
            MockMode::ServerError => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            MockMode::MalformedJson => (
                [(header::CONTENT_TYPE, "application/json")],
                r#"{broken json"#,
            )
                .into_response(),
        }
    }

    /// 起一个随机端口的 axum mock（标题直达默认全部缺失 → 走全文搜索兜底）。
    async fn spawn_mock(mode: MockMode, step1: Value, step2: Value) -> (String, Arc<MockCtx>) {
        spawn_mock_with_title(
            mode,
            json!({"query": {"pages": {}}}),
            step1,
            step2,
        )
        .await
    }

    /// 同 [`spawn_mock`]，但可指定标题直达请求的应答。
    async fn spawn_mock_with_title(
        mode: MockMode,
        title_lookup: Value,
        step1: Value,
        step2: Value,
    ) -> (String, Arc<MockCtx>) {
        let ctx = Arc::new(MockCtx {
            mode,
            title_lookup,
            step1,
            step2,
            captured: Mutex::new(Vec::new()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/api.php", axum::routing::get(handle_api_php))
            .with_state(ctx.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/api.php"), ctx)
    }

    /// 两步流程：标题、locator（base + /wiki/ + 空格换 _）、snippet
    /// （extract 前 1600 字符 / 缺 extract 退化为去标签摘要）、limit 透传 srlimit。
    #[tokio::test]
    async fn mediawiki_two_step_search_builds_hits() {
        let long_extract = "活塞是一种红石元件，可以推动方块。".repeat(100); // 1700 字符 > 1600
        let step1 = json!({
            "query": { "search": [
                { "title": "Piston", "pageid": 111,
                  "snippet": "A <span class=\"searchmatch\">piston</span> is a block." },
                { "title": "Sticky Piston", "pageid": 222,
                  "snippet": "A sticky <span class=\"searchmatch\">piston</span> can pull." }
            ]}
        });
        let step2 = json!({
            "query": { "pages": {
                "111": { "pageid": 111, "title": "Piston", "extract": long_extract },
                // 无 extract 字段 → 退化为第一步摘要去标签
                "222": { "pageid": 222, "title": "Sticky Piston" }
            }}
        });
        let (api_url, ctx) = spawn_mock(MockMode::Normal, step1, step2).await;
        let source = MediaWikiSource::new("minecraft-wiki", api_url.as_str()).unwrap();

        let hits = source.search("piston", 2).await;

        assert_eq!(hits.len(), 2);
        // 顺序 = 第一步返回的相关度顺序
        assert_eq!(hits[0].title, "Piston");
        assert_eq!(hits[1].title, "Sticky Piston");
        // locator = base_url + /wiki/ + 标题（空格换 _）
        let base = api_url.trim_end_matches("/api.php");
        assert_eq!(source.base_url, base);
        assert_eq!(hits[0].locator, format!("{base}/wiki/Piston"));
        assert_eq!(hits[1].locator, format!("{base}/wiki/Sticky_Piston"));
        // snippet = extract 前 1600 字符（按字符截断，给 LLM 完整页面上下文）
        assert_eq!(hits[0].snippet.chars().count(), 1600);
        assert!(hits[0]
            .snippet
            .starts_with("活塞是一种红石元件，可以推动方块。"));
        // 无 extract → 第一步 HTML 摘要去标签
        assert!(!hits[1].snippet.contains('<'));
        assert_eq!(hits[1].snippet, "A sticky piston can pull.");

        // 三次请求的 query 参数断言：标题直达 → 全文搜索 → 批量摘录
        let captured = ctx.captured.lock().unwrap();
        assert_eq!(captured.len(), 3);
        assert_eq!(
            captured[0].get("redirects").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            captured[0].get("titles").map(String::as_str),
            Some("piston")
        );
        assert_eq!(captured[1].get("list").map(String::as_str), Some("search"));
        assert_eq!(captured[1].get("srlimit").map(String::as_str), Some("2"));
        assert_eq!(
            captured[1].get("srsearch").map(String::as_str),
            Some("piston")
        );
        assert_eq!(
            captured[2].get("prop").map(String::as_str),
            Some("extracts")
        );
        assert_eq!(
            captured[2].get("explaintext").map(String::as_str),
            Some("1")
        );
        assert_eq!(captured[2].get("exlimit").map(String::as_str), Some("max"));
        assert_eq!(
            captured[2].get("titles").map(String::as_str),
            Some("Piston|Sticky Piston")
        );
    }

    /// 标题直达命中：清洗后的词正好是条目名（如「活塞」）→ 整页摘录排最前，
    /// 全文搜索里同标题的条目去重不重复出现。
    #[tokio::test]
    async fn mediawiki_title_direct_hit_beats_fulltext() {
        let extract = "活塞（Piston）是一种红石元件。".repeat(20);
        // 标题直达：查询词「活塞」命中条目
        let title_lookup = json!({
            "query": { "pages": {
                "7528": { "pageid": 7528, "title": "活塞", "extract": extract }
            }}
        });
        // 全文搜索同样返回「活塞」+ 一个快照页
        let step1 = json!({
            "query": { "search": [
                { "title": "活塞", "pageid": 7528, "snippet": "…<span>活塞</span>…" },
                { "title": "Java版17w49b", "pageid": 333,
                  "snippet": "激活的<span class=\"searchmatch\">活塞</span>被推出时…" }
            ]}
        });
        let step2 = json!({
            "query": { "pages": {
                "333": { "pageid": 333, "title": "Java版17w49b" }
            }}
        });
        let (api_url, ctx) =
            spawn_mock_with_title(MockMode::Normal, title_lookup, step1, step2).await;
        let source = MediaWikiSource::new("minecraft-wiki", api_url.as_str()).unwrap();

        let hits = source.search("活塞", 2).await;

        // 标题直达在前；搜索结果里的「活塞」被去重，只剩快照页
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "活塞");
        assert!(hits[0].snippet.starts_with("活塞（Piston）"));
        assert_eq!(hits[1].title, "Java版17w49b");
        assert_eq!(ctx.captured.lock().unwrap().len(), 3);
    }

    /// 全文搜索 500（标题直达正常应答但全缺失）：warn + 空列表，且不再发第二步。
    #[tokio::test]
    async fn mediawiki_server_error_returns_empty() {
        let (api_url, ctx) = spawn_mock(MockMode::ServerError, json!(null), json!(null)).await;
        let source = MediaWikiSource::new("wiki", api_url.as_str()).unwrap();
        assert!(source.search("piston", 5).await.is_empty());
        // 标题直达 + 全文搜索两次请求，第二步（批量摘录）不再发
        assert_eq!(ctx.captured.lock().unwrap().len(), 2);
    }

    /// 全文搜索返回非法 JSON：warn + 空列表。
    #[tokio::test]
    async fn mediawiki_malformed_json_returns_empty() {
        let (api_url, ctx) = spawn_mock(MockMode::MalformedJson, json!(null), json!(null)).await;
        let source = MediaWikiSource::new("wiki", api_url.as_str()).unwrap();
        assert!(source.search("piston", 5).await.is_empty());
        assert_eq!(ctx.captured.lock().unwrap().len(), 2);
    }

    /// 目标不可达（连接被拒）：空列表，不 panic。
    #[tokio::test]
    async fn mediawiki_unreachable_returns_empty() {
        let source = MediaWikiSource::new("wiki", "http://127.0.0.1:1/api.php").unwrap();
        assert!(source.search("piston", 5).await.is_empty());
    }

    /// base_url 推导：去掉结尾 `/api.php`；无该后缀则整串保留。
    #[test]
    fn mediawiki_base_url_derivation() {
        let stripped = MediaWikiSource::new("a", "https://minecraft.wiki/w/api.php").unwrap();
        assert_eq!(stripped.base_url, "https://minecraft.wiki/w");
        let untouched = MediaWikiSource::new("b", "https://example.com/rest.php").unwrap();
        assert_eq!(untouched.base_url, "https://example.com/rest.php");
    }
}

#[cfg(test)]
mod clean_query_tests {
    use super::clean_search_query;

    #[test]
    fn strips_question_words_and_punctuation() {
        assert_eq!(
            clean_search_query("帮我查一下黑曜石的爆炸抗性是多少？"),
            "黑曜石的爆炸抗性"
        );
        assert_eq!(clean_search_query("活塞怎么工作？"), "活塞 工作");
        assert_eq!(
            clean_search_query("1.21 的刷怪塔怎么建"),
            "1.21 的刷怪塔 建"
        );
        // 「我的世界」这类专名不被虚词误伤（词表里没有单字虚词）
        assert_eq!(clean_search_query("我的世界末影人"), "我的世界末影人");
        assert_eq!(clean_search_query("piston"), "piston");
        assert_eq!(clean_search_query("？？？"), "");
    }
}
