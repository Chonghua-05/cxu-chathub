//! OpenAI 兼容 LLM 客户端（[`LlmClient`]）。
//!
//! 只依赖 chat/completions 这一最小公共接口。对各家「OpenAI 兼容」网关的
//! 适配收敛在这里：
//!
//! - **端点归一化**（[`normalize_endpoint`]）：`api_url` 支持三种写法——根地址
//!   （补 `/v1/chat/completions`）、以 `/v1`..`/v4` 结尾（补 `/chat/completions`）、
//!   完整端点（原样）；尾斜杠与首尾空白容忍；
//! - **请求体**：model + system/user 两条 message + temperature，显式
//!   `"stream": false`（个别网关缺省即流式，本客户端只吃完整 JSON）；
//! - **响应**：取 `choices[0].message.content`，兼容字符串与分段数组
//!   （`[{"type":"text","text":…}]`，部分多模态代理的形态）；null / 缺失按
//!   异常处理；非 JSON 响应（配错端点时常见 200 + HTML）报错带响应开头片段；
//! - `api_key` 非空时才携带 `Authorization: Bearer` 头（本地无鉴权网关可用），
//!   且任何错误信息都不含它（对齐 crate「token 绝不写日志」的约束）。
//!
//! 配置错误（api_url/model 为空、非 http(s)）在 [`LlmClient::new`] 即报错——
//! 装配日志里直接可见，不用等第一次调用才失败。调用失败由上层（`agent::skill`）
//! 捕获并降级为检索摘录——LLM 故障不应导致技能不可用。

use std::time::Duration;

use serde::Deserialize;

use crate::config::LlmConfig;

/// LLM 调用错误。`Display` 与 `Debug` 都不会包含 `api_key`。
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// 请求/响应传输失败：网络、超时、非 2xx 状态等。
    #[error("LLM 请求失败: {0}")]
    Http(#[from] reqwest::Error),
    /// 响应不是预期的 chat/completions 结构（choices/content 缺失 / 不可解析）。
    #[error("LLM 返回异常响应: {0}")]
    BadResponse(String),
}

/// OpenAI 兼容 chat/completions 客户端：持有配置副本、归一化端点与带超时的
/// HTTP 客户端。Clone 廉价（reqwest::Client 内部是 Arc），多个技能可共享一个客户端。
#[derive(Clone)]
pub struct LlmClient {
    cfg: LlmConfig,
    /// 归一化后的完整端点（见 [`normalize_endpoint`]）。
    endpoint: String,
    client: reqwest::Client,
}

/// chat/completions 响应中我们关心的最小字段集（其余字段忽略）。
#[derive(Deserialize)]
struct CompletionResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    /// 三种真实形态：字符串（标准）、分段数组（部分网关/多模态代理）、
    /// null（内容被截或走了 reasoning 通道——视为异常）。
    #[serde(default)]
    content: Option<serde_json::Value>,
}

/// 归一化 chat/completions 端点（调用前 api_url 已校验非空且为 http(s)）：
/// - 已是完整端点（`/chat/completions` 结尾）→ 去尾斜杠原样；
/// - 末段是 `/v1`..`/v4` → 补 `/chat/completions`；
/// - 其余（根地址）→ 补 `/v1/chat/completions`。
fn normalize_endpoint(api_url: &str) -> String {
    let trimmed = api_url.trim().trim_end_matches('/');
    let lower = trimmed.to_lowercase();
    if lower.ends_with("/chat/completions") {
        return trimmed.to_string();
    }
    let last_segment = trimmed.rsplit('/').next().unwrap_or("");
    if matches!(last_segment.to_lowercase().as_str(), "v1" | "v2" | "v3" | "v4") {
        format!("{trimmed}/chat/completions")
    } else {
        format!("{trimmed}/v1/chat/completions")
    }
}

/// 从 `message.content` 提取文本：字符串原样；数组拼接全部 `text` 字段
/// （`{"type":"text","text":…}` 分段）；缺失 / null / 其它形态 → None。
fn extract_content(value: Option<serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(text) => Some(text),
        serde_json::Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    out.push_str(text);
                }
            }
            Some(out)
        }
        _ => None,
    }
}

impl LlmClient {
    /// 构建客户端：校验 api_url / model，HTTP 总超时取 `cfg.timeout_secs`（下限 1s）。
    /// Err 文案面向装配日志（配置错误启动即可见，不等第一次调用）。
    pub fn new(cfg: LlmConfig) -> Result<Self, String> {
        if cfg.api_url.trim().is_empty() {
            return Err("api_url 未配置".into());
        }
        if cfg.model.trim().is_empty() {
            return Err("model 未配置".into());
        }
        let lower = cfg.api_url.to_lowercase();
        if !lower.starts_with("http://") && !lower.starts_with("https://") {
            return Err(format!("api_url 必须以 http(s):// 开头: {}", cfg.api_url));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs.max(1)))
            .build()
            .map_err(|err| format!("HTTP 客户端构建失败: {err}"))?;
        let endpoint = normalize_endpoint(&cfg.api_url);
        Ok(Self {
            cfg,
            endpoint,
            client,
        })
    }

    /// 配置的系统提示词（供技能层组装请求；为空时由调用方回退默认提示词）。
    pub fn system_prompt(&self) -> &str {
        &self.cfg.system_prompt
    }

    /// 发起一次补全：`system` 为系统提示词，`user` 为用户内容。
    ///
    /// - POST 归一化端点（[`normalize_endpoint`]），请求体 =
    ///   `{"model", "messages": [{"role":"system",...},{"role":"user",...}],
    ///   "temperature": 0.2, "stream": false}`；
    /// - `cfg.api_key` 非空时附带 `Authorization: Bearer <key>`；
    /// - 返回 `choices[0].message.content`（兼容字符串/分段数组）；
    ///   choices / content 缺失或为空 → [`LlmError::BadResponse`]；
    /// - 网络与 HTTP 状态错误 → [`LlmError::Http`]。
    ///
    /// 错误信息绝不包含 `api_key`。
    pub async fn complete(&self, system: &str, user: &str) -> Result<String, LlmError> {
        let payload = serde_json::json!({
            "model": self.cfg.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
            "temperature": 0.2,
            // 显式关流式：个别网关缺省即流式，本客户端只吃完整 JSON
            "stream": false,
        });
        let mut request = self.client.post(&self.endpoint).json(&payload);
        if !self.cfg.api_key.is_empty() {
            request = request.bearer_auth(&self.cfg.api_key);
        }
        let response = request.send().await?.error_for_status()?;
        let raw = response.text().await?;
        let parsed: CompletionResponse = serde_json::from_str(&raw).map_err(|err| {
            // 带响应开头片段：配错端点时常见 200 + HTML，片段足以定位
            LlmError::BadResponse(format!(
                "响应不是合法的 chat/completions 结构: {err}；响应开头: {}",
                raw.chars().take(120).collect::<String>()
            ))
        })?;
        let content = extract_content(
            parsed
                .choices
                .into_iter()
                .next()
                .ok_or_else(|| LlmError::BadResponse("choices 为空".to_string()))?
                .message
                .content,
        )
        .ok_or_else(|| LlmError::BadResponse("message.content 缺失或为空".to_string()))?;
        Ok(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use axum::Json;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode, Uri};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::Router;

    use crate::config::DEFAULT_SYSTEM_PROMPT;

    type ReplyFn = Box<dyn Fn() -> (StatusCode, serde_json::Value) + Send + Sync>;

    /// axum mock 服务器的共享状态：捕获请求路径 / Authorization 头 / 请求体，
    /// 并按 `reply` 闭包生成响应。
    struct MockState {
        path: Mutex<String>,
        auth: Mutex<Option<String>>,
        body: Mutex<serde_json::Value>,
        reply: ReplyFn,
    }

    impl MockState {
        fn new(reply: ReplyFn) -> Self {
            Self {
                path: Mutex::new(String::new()),
                auth: Mutex::new(None),
                body: Mutex::new(serde_json::Value::Null),
                reply,
            }
        }

        /// 正常响应：choices[0].message.content = content
        fn with_content(content: &str) -> Self {
            let content = content.to_string();
            Self::new(Box::new(move || {
                (
                    StatusCode::OK,
                    serde_json::json!({
                        "choices": [
                            { "message": { "role": "assistant", "content": content } }
                        ]
                    }),
                )
            }))
        }

        /// 任意 (状态码, JSON) 响应。
        fn with_raw(status: StatusCode, json: serde_json::Value) -> Self {
            Self::new(Box::new(move || (status, json.clone())))
        }
    }

    async fn handler(
        State(st): State<Arc<MockState>>,
        uri: Uri,
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> Response {
        *st.path.lock().unwrap() = uri.path().to_string();
        *st.auth.lock().unwrap() = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned);
        *st.body.lock().unwrap() = body;
        let (status, json) = (st.reply)();
        (status, Json(json)).into_response()
    }

    /// 在 127.0.0.1:0 起 mock 服务器，返回 (根地址, 完整端点 URL)。
    async fn spawn_mock(state: Arc<MockState>) -> (String, String) {
        let app = Router::new()
            .route("/v1/chat/completions", post(handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base = format!("http://{addr}");
        let endpoint = format!("{base}/v1/chat/completions");
        (base, endpoint)
    }

    fn client(api_url: String, api_key: &str) -> LlmClient {
        LlmClient::new(LlmConfig {
            api_url,
            api_key: api_key.into(),
            model: "test-model".into(),
            timeout_secs: 5,
            max_answer_chars: 1000,
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
        })
        .unwrap()
    }

    // ---------- 端点与配置校验 ----------

    /// 端点归一化：根地址 / 版本前缀 / 完整端点三种写法 + 尾斜杠容忍。
    #[test]
    fn normalize_endpoint_accepts_three_url_forms() {
        let cases = [
            ("https://api.x.com", "https://api.x.com/v1/chat/completions"),
            ("https://api.x.com/", "https://api.x.com/v1/chat/completions"),
            (" https://api.x.com ", "https://api.x.com/v1/chat/completions"),
            ("https://api.x.com/v1", "https://api.x.com/v1/chat/completions"),
            ("https://api.x.com/v1/", "https://api.x.com/v1/chat/completions"),
            (
                "https://gw.x.com/api/v2",
                "https://gw.x.com/api/v2/chat/completions",
            ),
            (
                "https://api.x.com/v1/chat/completions",
                "https://api.x.com/v1/chat/completions",
            ),
            (
                "https://api.x.com/v1/chat/completions/",
                "https://api.x.com/v1/chat/completions",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(normalize_endpoint(input), expected, "输入: {input}");
        }
    }

    /// 配置校验：api_url / model 为空、非 http(s) 在构建期报错。
    /// （LlmClient 不实现 Debug——cfg 里有 api_key，Debug 输出会泄漏；断言用 matches!。）
    #[test]
    fn client_new_validates_config() {
        let cfg = |api_url: &str, model: &str| LlmConfig {
            api_url: api_url.into(),
            api_key: String::new(),
            model: model.into(),
            timeout_secs: 5,
            max_answer_chars: 1000,
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
        };
        let err = |result: Result<LlmClient, String>| match result {
            Err(msg) => msg,
            Ok(_) => panic!("应构建失败"),
        };
        assert!(err(LlmClient::new(cfg("", "m"))).contains("api_url"));
        assert!(err(LlmClient::new(cfg("ftp://x.com", "m"))).contains("http(s)"));
        assert!(err(LlmClient::new(cfg("https://x.com/v1", ""))).contains("model"));
        assert!(LlmClient::new(cfg("https://x.com/v1", "m")).is_ok());
    }

    // ---------- 请求/响应行为 ----------

    #[tokio::test]
    async fn llm_sends_expected_request_and_returns_content() {
        let state = Arc::new(MockState::with_content("你好，答案是 42"));
        let (_base, url) = spawn_mock(state.clone()).await;
        let llm = client(url, "test-key-123");

        let out = llm.complete("系统提示", "用户问题").await.unwrap();
        assert_eq!(out, "你好，答案是 42");

        assert_eq!(*state.path.lock().unwrap(), "/v1/chat/completions");
        assert_eq!(
            state.auth.lock().unwrap().as_deref(),
            Some("Bearer test-key-123")
        );
        let body = state.body.lock().unwrap().clone();
        assert_eq!(body["model"], "test-model");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "系统提示");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "用户问题");
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["stream"], false);
    }

    /// 根地址写法：归一化端点后命中 mock 的 /v1/chat/completions。
    #[tokio::test]
    async fn llm_base_url_form_hits_normalized_endpoint() {
        let state = Arc::new(MockState::with_content("ok"));
        let (base, _endpoint) = spawn_mock(state.clone()).await;
        let llm = client(base, "");

        assert_eq!(llm.complete("s", "u").await.unwrap(), "ok");
        assert_eq!(*state.path.lock().unwrap(), "/v1/chat/completions");
    }

    #[tokio::test]
    async fn llm_omits_authorization_header_without_api_key() {
        let state = Arc::new(MockState::with_content("ok"));
        let (_base, url) = spawn_mock(state.clone()).await;
        let llm = client(url, "");

        assert_eq!(llm.complete("s", "u").await.unwrap(), "ok");
        assert!(state.auth.lock().unwrap().is_none());
    }

    /// content 为分段数组（部分网关形态）→ 拼接全部 text 字段。
    #[tokio::test]
    async fn llm_accepts_content_as_parts_array() {
        let url = spawn_mock(Arc::new(MockState::with_raw(
            StatusCode::OK,
            serde_json::json!({
                "choices": [ { "message": { "content": [
                    { "type": "text", "text": "第一段" },
                    { "type": "text", "text": "第二段" }
                ] } } ]
            }),
        )))
        .await
        .1;
        let llm = client(url, "k");
        assert_eq!(llm.complete("s", "u").await.unwrap(), "第一段第二段");
    }

    /// content 为 null / 缺失（如内容走了 reasoning 通道）→ 明确报 BadResponse，
    /// 上层降级为摘录而不是 panic 或返回空答案。
    #[tokio::test]
    async fn llm_null_or_missing_content_is_bad_response() {
        for body in [
            serde_json::json!({ "choices": [ { "message": { "content": null } } ] }),
            serde_json::json!({ "choices": [ { "message": {} } ] }),
        ] {
            let url = spawn_mock(Arc::new(MockState::with_raw(StatusCode::OK, body)))
                .await
                .1;
            let llm = client(url, "k");
            let err = llm.complete("s", "u").await.unwrap_err();
            assert!(matches!(err, LlmError::BadResponse(_)));
        }
    }

    #[tokio::test]
    async fn llm_empty_or_missing_choices_is_bad_response() {
        // choices 为空数组
        let url = spawn_mock(Arc::new(MockState::with_raw(
            StatusCode::OK,
            serde_json::json!({ "choices": [] }),
        )))
        .await
        .1;
        let llm = client(url, "k");
        let err = llm.complete("s", "u").await.unwrap_err();
        assert!(matches!(err, LlmError::BadResponse(_)));

        // choices 字段缺失
        let url = spawn_mock(Arc::new(MockState::with_raw(
            StatusCode::OK,
            serde_json::json!({ "object": "chat.completion" }),
        )))
        .await
        .1;
        let llm = client(url, "k");
        let err = llm.complete("s", "u").await.unwrap_err();
        assert!(matches!(err, LlmError::BadResponse(_)));
    }

    /// 200 + 非 chat/completions 结构（配错端点的常见形态，如拿到 HTML 首页）
    /// → BadResponse 且报错带响应片段。
    #[tokio::test]
    async fn llm_non_json_body_is_bad_response_with_snippet() {
        let url = spawn_mock(Arc::new(MockState::with_raw(
            StatusCode::OK,
            serde_json::json!("<!DOCTYPE html><html><body>首页</body></html>"),
        )))
        .await
        .1;
        let llm = client(url, "k");
        let err = llm.complete("s", "u").await.unwrap_err();
        match err {
            LlmError::BadResponse(msg) => {
                assert!(msg.contains("DOCTYPE"), "报错应带响应片段: {msg}");
            }
            other => panic!("应为 BadResponse: {other:?}"),
        }
    }

    #[tokio::test]
    async fn llm_server_error_maps_to_http_error() {
        let url = spawn_mock(Arc::new(MockState::with_raw(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "error": "boom" }),
        )))
        .await
        .1;
        let llm = client(url, "k");
        let err = llm.complete("s", "u").await.unwrap_err();
        assert!(matches!(err, LlmError::Http(_)));
    }

    #[tokio::test]
    async fn llm_errors_never_leak_api_key() {
        const KEY: &str = "sk-leak-me-9f3a";

        // Http 分支（500）
        let url = spawn_mock(Arc::new(MockState::with_raw(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({}),
        )))
        .await
        .1;
        let llm = client(url, KEY);
        let err = llm.complete("s", "u").await.unwrap_err();
        assert!(
            !format!("{err}").contains(KEY),
            "LlmError::Http 的 Display 泄漏了 api_key"
        );
        assert!(
            !format!("{err:?}").contains(KEY),
            "LlmError::Http 的 Debug 泄漏了 api_key"
        );

        // BadResponse 分支（choices 为空）
        let url = spawn_mock(Arc::new(MockState::with_raw(
            StatusCode::OK,
            serde_json::json!({ "choices": [] }),
        )))
        .await
        .1;
        let llm = client(url, KEY);
        let err = llm.complete("s", "u").await.unwrap_err();
        assert!(
            !format!("{err}").contains(KEY),
            "LlmError::BadResponse 的 Display 泄漏了 api_key"
        );
        assert!(
            !format!("{err:?}").contains(KEY),
            "LlmError::BadResponse 的 Debug 泄漏了 api_key"
        );
    }
}
