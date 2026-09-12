//! OpenAI 兼容 LLM 客户端（[`LlmClient`]）。
//!
//! 只依赖 chat/completions 这一最小公共接口：POST 配置的 `api_url`，
//! 请求体固定为 model + system/user 两条 message + temperature，
//! 取 `choices[0].message.content` 作为答案。`api_key` 非空时才携带
//! `Authorization: Bearer` 头，且任何错误信息都不含它（对齐 crate
//! 「token 绝不写日志」的约束）。调用失败由上层（`agent::skill`）捕获
//! 并降级为检索摘录——LLM 故障不应导致技能不可用。

use std::time::Duration;

use serde::Deserialize;

use crate::config::LlmConfig;

/// LLM 调用错误。`Display` 与 `Debug` 都不会包含 `api_key`。
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// 请求/响应传输失败：网络、超时、非 2xx 状态等。
    #[error("LLM 请求失败: {0}")]
    Http(#[from] reqwest::Error),
    /// 响应不是预期的 chat/completions 结构（choices 缺失 / 为空 / 不可解析）。
    #[error("LLM 返回异常响应: {0}")]
    BadResponse(String),
}

/// OpenAI 兼容 chat/completions 客户端：持有配置副本与带超时的 HTTP 客户端。
/// Clone 廉价（reqwest::Client 内部是 Arc），多个技能可共享一个客户端。
#[derive(Clone)]
pub struct LlmClient {
    cfg: LlmConfig,
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
    content: String,
}

impl LlmClient {
    /// 构建客户端；HTTP 总超时取 `cfg.timeout_secs`。
    pub fn new(cfg: LlmConfig) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .build()?;
        Ok(Self { cfg, client })
    }

    /// 配置的系统提示词（供技能层组装请求；为空时由调用方回退默认提示词）。
    pub fn system_prompt(&self) -> &str {
        &self.cfg.system_prompt
    }

    /// 发起一次补全：`system` 为系统提示词，`user` 为用户内容。
    ///
    /// - POST `cfg.api_url`，请求体 =
    ///   `{"model", "messages": [{"role":"system",...},{"role":"user",...}], "temperature": 0.2}`；
    /// - `cfg.api_key` 非空时附带 `Authorization: Bearer <key>`；
    /// - 返回 `choices[0].message.content`；choices 缺失或为空 → [`LlmError::BadResponse`]；
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
        });
        let mut request = self.client.post(&self.cfg.api_url).json(&payload);
        if !self.cfg.api_key.is_empty() {
            request = request.bearer_auth(&self.cfg.api_key);
        }
        let response = request.send().await?.error_for_status()?;
        let raw = response.text().await?;
        let parsed: CompletionResponse = serde_json::from_str(&raw).map_err(|err| {
            LlmError::BadResponse(format!("响应不是合法的 chat/completions 结构: {err}"))
        })?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| LlmError::BadResponse("choices 为空".to_string()))?
            .message
            .content;
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

    /// 在 127.0.0.1:0 起 mock 服务器，返回完整 chat/completions 端点 URL。
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

    #[tokio::test]
    async fn llm_sends_expected_request_and_returns_content() {
        let state = Arc::new(MockState::with_content("你好，答案是 42"));
        let url = spawn_mock(state.clone()).await;
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
    }

    #[tokio::test]
    async fn llm_omits_authorization_header_without_api_key() {
        let state = Arc::new(MockState::with_content("ok"));
        let url = spawn_mock(state.clone()).await;
        let llm = client(url, "");

        assert_eq!(llm.complete("s", "u").await.unwrap(), "ok");
        assert!(state.auth.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn llm_empty_or_missing_choices_is_bad_response() {
        // choices 为空数组
        let url = spawn_mock(Arc::new(MockState::with_raw(
            StatusCode::OK,
            serde_json::json!({ "choices": [] }),
        )))
        .await;
        let llm = client(url, "k");
        let err = llm.complete("s", "u").await.unwrap_err();
        assert!(matches!(err, LlmError::BadResponse(_)));

        // choices 字段缺失
        let url = spawn_mock(Arc::new(MockState::with_raw(
            StatusCode::OK,
            serde_json::json!({ "object": "chat.completion" }),
        )))
        .await;
        let llm = client(url, "k");
        let err = llm.complete("s", "u").await.unwrap_err();
        assert!(matches!(err, LlmError::BadResponse(_)));
    }

    #[tokio::test]
    async fn llm_server_error_maps_to_http_error() {
        let url = spawn_mock(Arc::new(MockState::with_raw(
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "error": "boom" }),
        )))
        .await;
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
        .await;
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
        .await;
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
