//! MediaWiki 站点检索（[`MediaWikiSource`]，走站点 `api.php`）。
//!
//! 两步取数：
//! 1. `list=search` 拿命中条目（标题 + 含 HTML 标签的搜索摘要）；
//! 2. 按 `prop=extracts&explaintext=1` 批量拉正文摘录，snippet 取前 400 字符；
//!    条目缺 extract 时退化为第一步摘要去 HTML 标签后的纯文本。
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
const SNIPPET_MAX_CHARS: usize = 400;

/// prop=extracts 单次请求的标题批量上限（MediaWiki exlimit 常规上限 50）。
const EXTRACT_BATCH_SIZE: usize = 50;

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
        let srlimit = limit.to_string();

        // 第一步：list=search 拿命中条目（保持 API 返回的相关度顺序）。
        let body = match self
            .get_json(&[
                ("action", "query"),
                ("format", "json"),
                ("list", "search"),
                ("srlimit", srlimit.as_str()),
                ("srsearch", query),
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
        if found.is_empty() {
            return Vec::new();
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

        found
            .into_iter()
            .map(|(title, pageid, html)| {
                let snippet = match pageid.and_then(|id| extract_by_pageid.get(&id)) {
                    Some(extract) => truncate_chars(extract, SNIPPET_MAX_CHARS),
                    None => strip_html_tags(&html), // extract 缺失：退化为第一步摘要去标签
                };
                DocHit {
                    locator: format!("{}/wiki/{}", self.base_url, title.replace(' ', "_")),
                    title,
                    snippet,
                }
            })
            .collect()
    }
}

/// 截前 `max` 个 Unicode 字符（不按字节，避免切开多字节字符 panic）。
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
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
        step1: Value,
        step2: Value,
        captured: Mutex<Vec<HashMap<String, String>>>,
    }

    /// `/api.php` 处理器：按 query 参数区分两步（第二步带 `prop=extracts`）。
    async fn handle_api_php(
        State(ctx): State<Arc<MockCtx>>,
        Query(params): Query<HashMap<String, String>>,
    ) -> Response {
        ctx.captured.lock().unwrap().push(params.clone());
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

    /// 起一个随机端口的 axum mock，返回 api.php 地址与上下文（供参数断言）。
    async fn spawn_mock(mode: MockMode, step1: Value, step2: Value) -> (String, Arc<MockCtx>) {
        let ctx = Arc::new(MockCtx {
            mode,
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
    /// （extract 前 400 字符 / 缺 extract 退化为去标签摘要）、limit 透传 srlimit。
    #[tokio::test]
    async fn mediawiki_two_step_search_builds_hits() {
        let long_extract = "活塞是一种红石元件，可以推动方块。".repeat(40); // 680 字符 > 400
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
        // snippet = extract 前 400 字符（按字符截断）
        assert_eq!(hits[0].snippet.chars().count(), 400);
        assert!(hits[0]
            .snippet
            .starts_with("活塞是一种红石元件，可以推动方块。"));
        // 无 extract → 第一步 HTML 摘要去标签
        assert!(!hits[1].snippet.contains('<'));
        assert_eq!(hits[1].snippet, "A sticky piston can pull.");

        // 两次请求的 query 参数断言
        let captured = ctx.captured.lock().unwrap();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].get("list").map(String::as_str), Some("search"));
        assert_eq!(captured[0].get("srlimit").map(String::as_str), Some("2"));
        assert_eq!(
            captured[0].get("srsearch").map(String::as_str),
            Some("piston")
        );
        assert_eq!(
            captured[1].get("prop").map(String::as_str),
            Some("extracts")
        );
        assert_eq!(
            captured[1].get("explaintext").map(String::as_str),
            Some("1")
        );
        assert_eq!(captured[1].get("exlimit").map(String::as_str), Some("max"));
        assert_eq!(
            captured[1].get("titles").map(String::as_str),
            Some("Piston|Sticky Piston")
        );
    }

    /// 第一步 500：warn + 空列表，且不再发第二步。
    #[tokio::test]
    async fn mediawiki_server_error_returns_empty() {
        let (api_url, ctx) = spawn_mock(MockMode::ServerError, json!(null), json!(null)).await;
        let source = MediaWikiSource::new("wiki", api_url.as_str()).unwrap();
        assert!(source.search("piston", 5).await.is_empty());
        assert_eq!(ctx.captured.lock().unwrap().len(), 1);
    }

    /// 第一步返回非法 JSON：warn + 空列表。
    #[tokio::test]
    async fn mediawiki_malformed_json_returns_empty() {
        let (api_url, ctx) = spawn_mock(MockMode::MalformedJson, json!(null), json!(null)).await;
        let source = MediaWikiSource::new("wiki", api_url.as_str()).unwrap();
        assert!(source.search("piston", 5).await.is_empty());
        assert_eq!(ctx.captured.lock().unwrap().len(), 1);
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
