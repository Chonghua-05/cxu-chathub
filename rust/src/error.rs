//! 统一错误类型。错误信息面向日志与排查，**绝不携带 token**。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("配置文件不存在: {0}")]
    NotFound(String),
    #[error("配置文件不是合法 JSON: {0}")]
    InvalidJson(String),
    #[error("配置根节点必须是对象")]
    NotObject,
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
}

/// Forward API 调用失败。`message` 只含端点与原因，不携带 Bearer token。
#[derive(Debug, Error)]
#[error("{message}")]
pub struct ForwardApiError {
    pub message: String,
    pub status: Option<u16>,
    pub body: String,
}

impl ForwardApiError {
    pub fn new(
        message: impl Into<String>,
        status: Option<u16>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            status,
            body: body.into(),
        }
    }
}
