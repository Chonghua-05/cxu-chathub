//! 配置加载：字段名与 config.json 完全一致。
//! 未知字段忽略；配置段缺失或类型不符时退回默认值（对齐 Python 版 `_build` 的容忍行为）。
//! token 只从文件读取，绝不写日志。

use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

use crate::error::ConfigError;

/// 配置段宽松解析：缺失/非对象/字段类型不符时退回默认值并告警。
fn lenient<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        Some(value @ serde_json::Value::Object(_)) => Ok(T::deserialize(value).unwrap_or_else(|err| {
            tracing::warn!("配置段解析失败，使用默认值: {err}");
            T::default()
        })),
        _ => Ok(T::default()),
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OneBotConfig {
    pub listen_host: String,
    pub listen_port: u16,
    pub path: String,
    pub access_token: String,
    pub self_id: i64,
}

impl Default for OneBotConfig {
    fn default() -> Self {
        Self {
            listen_host: "0.0.0.0".into(),
            listen_port: 6200,
            path: "/ws".into(),
            access_token: String::new(),
            self_id: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ChatroomConfig {
    /// 示例值，按实际部署地址覆盖
    pub base_url: String,
    pub channel_id: i64,
    pub forward_token: String,
    pub refresh_token: String,
    pub group_ids: Vec<i64>,
    pub poll_interval: u64,
    pub debounce_count: u32,
    pub qq_sync_enabled: bool,
    pub qq_forward_enabled: bool,
    pub qq_to_game_enabled: bool,
    pub player_tracking_enabled: bool,
    pub snapshot_sender: String,
    pub snapshot_prefix: String,
    /// 以下三项是部署方自己的服务地址，示例值必须在 config.json 里覆盖
    pub voice_api: String,
    pub status_api: String,
    pub server_addresses: Vec<Vec<String>>,
}

impl Default for ChatroomConfig {
    fn default() -> Self {
        Self {
            base_url: "https://chatroom.example.com".into(),
            channel_id: 1,
            forward_token: String::new(),
            refresh_token: String::new(),
            group_ids: Vec::new(),
            poll_interval: 10,
            debounce_count: 2,
            qq_sync_enabled: true,
            qq_forward_enabled: true,
            qq_to_game_enabled: true,
            player_tracking_enabled: false,
            snapshot_sender: "snapshot".into(),
            snapshot_prefix: "!snap".into(),
            voice_api:
                "https://chatroom.example.com/api/voice/qqbot/get_voice_channel_people".into(),
            status_api: "https://status.example.com/api/qqbot/status".into(),
            server_addresses: vec![vec!["主IP".into(), "game.example.com".into()]],
        }
    }
}

impl ChatroomConfig {
    /// `[["主IP","game.example.com"]]` → `[("主IP","game.example.com")]`，跳过畸形条目。
    pub fn server_address_pairs(&self) -> Vec<(String, String)> {
        self.server_addresses
            .iter()
            .filter_map(|pair| {
                let mut it = pair.iter();
                let first = it.next()?.clone();
                let second = it.next()?.clone();
                Some((first, second))
            })
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ChatBridgeConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub name: String,
    pub password: String,
    pub aes_key: String,
}

impl Default for ChatBridgeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: String::new(),
            port: 21027,
            name: "web".into(),
            password: String::new(),
            aes_key: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CommandsConfig {
    pub group_allow_all: bool,
    pub allow_from: Vec<i64>,
    pub status_image: bool,
}

impl Default for CommandsConfig {
    fn default() -> Self {
        Self {
            group_allow_all: true,
            allow_from: Vec::new(),
            status_image: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    /// 独立 HTTP API（Web UI / 其他站点调用）；默认关闭，显式开启
    pub enabled: bool,
    /// 默认只绑回环；对外暴露时务必配置 access_token 并评估网络位置
    pub listen_host: String,
    pub listen_port: u16,
    /// 写接口（POST /api/relay）必需；为空时写接口一律 403
    pub access_token: String,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_host: "127.0.0.1".into(),
            listen_port: 8199,
            access_token: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    #[serde(default, deserialize_with = "lenient")]
    pub onebot: OneBotConfig,
    #[serde(default, deserialize_with = "lenient")]
    pub chatroom: ChatroomConfig,
    #[serde(default, deserialize_with = "lenient")]
    pub chatbridge: ChatBridgeConfig,
    #[serde(default, deserialize_with = "lenient")]
    pub commands: CommandsConfig,
    #[serde(default, deserialize_with = "lenient")]
    pub api: ApiConfig,
    #[serde(default = "default_state_path")]
    pub state_path: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

fn default_state_path() -> String {
    "/data/state.json".into()
}

fn default_log_level() -> String {
    "INFO".into()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            onebot: OneBotConfig::default(),
            chatroom: ChatroomConfig::default(),
            chatbridge: ChatBridgeConfig::default(),
            commands: CommandsConfig::default(),
            api: ApiConfig::default(),
            state_path: default_state_path(),
            log_level: default_log_level(),
        }
    }
}

impl AppConfig {
    pub fn group_ids(&self) -> std::collections::HashSet<i64> {
        self.chatroom.group_ids.iter().copied().collect()
    }
}

/// 读取并解析配置：容忍 BOM（utf-8-sig），根节点必须是对象。
pub fn load_config(path: &Path) -> Result<AppConfig, ConfigError> {
    let bytes = std::fs::read(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ConfigError::NotFound(path.display().to_string())
        } else {
            ConfigError::Io(err)
        }
    })?;
    let bom: &[u8] = &[0xEF, 0xBB, 0xBF];
    let slice = bytes.strip_prefix(bom).unwrap_or(&bytes);
    let data: serde_json::Value =
        serde_json::from_slice(slice).map_err(|err| ConfigError::InvalidJson(err.to_string()))?;
    if !data.is_object() {
        return Err(ConfigError::NotObject);
    }
    let mut cfg: AppConfig = serde_json::from_value(data)
        .map_err(|err| ConfigError::InvalidJson(format!("配置解析失败: {err}")))?;
    cfg.chatroom.base_url = cfg
        .chatroom
        .base_url
        .trim_end_matches('/')
        .to_string();
    cfg.log_level = cfg.log_level.to_uppercase();
    Ok(cfg)
}

fn mask(token: &str) -> &'static str {
    if token.is_empty() {
        "(空)"
    } else {
        "***"
    }
}

/// 可安全打印的配置摘要（token 一律替换为占位符）。
pub fn describe(cfg: &AppConfig) -> serde_json::Value {
    let mut group_ids: Vec<i64> = cfg.group_ids().into_iter().collect();
    group_ids.sort_unstable();
    serde_json::json!({
        "onebot": {
            "listen": format!(
                "{}:{}{}",
                cfg.onebot.listen_host, cfg.onebot.listen_port, cfg.onebot.path
            ),
            "access_token": mask(&cfg.onebot.access_token),
            "self_id": cfg.onebot.self_id,
        },
        "chatroom": {
            "base_url": cfg.chatroom.base_url,
            "channel_id": cfg.chatroom.channel_id,
            "forward_token": mask(&cfg.chatroom.forward_token),
            "refresh_token": mask(&cfg.chatroom.refresh_token),
            "group_ids": group_ids,
        },
        "chatbridge": {
            "enabled": cfg.chatbridge.enabled,
            "endpoint": if cfg.chatbridge.host.is_empty() {
                "(未配置)".to_string()
            } else {
                format!("{}:{}", cfg.chatbridge.host, cfg.chatbridge.port)
            },
        },
        "api": {
            "enabled": cfg.api.enabled,
            "listen": format!("{}:{}", cfg.api.listen_host, cfg.api.listen_port),
            "access_token": mask(&cfg.api.access_token),
        },
        "state_path": cfg.state_path,
        "log_level": cfg.log_level,
    })
}
