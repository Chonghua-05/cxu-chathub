//! QQ 群命令：/chatroom、/server（/status 为兼容别名）。
//!
//! 命令都不需要 AI，纯 HTTP 查询 + 格式化后回群。
//! 对应 Python 版 `chatroom_bridge/commands.py` 的逐行翻译：
//! 查询失败（HTTP 非 200 / 网络 / 超时）时格式化函数收到 `Value::Null`，
//! 产出「无法获取…」文案；未授权一律静默不响应。

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::Value;

/// 默认地址仅作示例，实际部署请通过 config.json 的 chatroom.* 覆盖
pub const DEFAULT_VOICE_API: &str =
    "https://chatroom.example.com/api/voice/qqbot/get_voice_channel_people";
pub const DEFAULT_STATUS_API: &str = "https://status.example.com/api/qqbot/status";

/// Python `DEFAULT_SERVER_ADDRESSES: list[tuple[str, str]]`（两条）——照抄
pub const DEFAULT_SERVER_ADDRESSES: [(&str, &str); 2] = [
    ("主IP", "game.example.com"),
    ("备用地址", "backup.example.com:25565"),
];

/// 命令响应：文本或图片（图片优先）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CommandResult {
    pub text: String,
    pub image: Option<Vec<u8>>,
}

impl CommandResult {
    pub fn empty(&self) -> bool {
        self.text.is_empty() && self.image.is_none()
    }
}

/// 识别命令，返回 (命令名, 参数)；不是命令则返回 None。
///
/// 与 Python 对齐：strip → 按第一个空格 partition → head 必须以 "/" 开头 →
/// name = head[1:].split("@", 1)[0].lower()（容忍 /cmd@bot 形式）→ (name, rest.trim())。
pub fn parse_command(text: &str) -> Option<(String, String)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (head, rest) = match text.split_once(' ') {
        Some((head, rest)) => (head, rest),
        None => (text, ""),
    };
    if !head.starts_with('/') {
        return None;
    }
    // 容忍 /cmd@bot 形式
    let name = head[1..].split('@').next().unwrap_or("").to_lowercase();
    if name.is_empty() {
        // Python 对 "/" 返回 ("", "")，但 handle() 随即因名字不在命令集而丢弃；
        // 这里直接视为非命令（对 handle 的可观测行为完全等价）。
        return None;
    }
    Some((name, rest.trim().to_string()))
}

/// Python 风格标量转字符串（str(v) / f-string 插值）：None → "None"、
/// True/False 保留 Python 大写、字符串原样、数字与 serde 表示一致。
fn python_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// dict.get(key, default) + f-string：键缺失用 default，存在则 str(v)。
fn py_get_str(obj: &Value, key: &str, default: &str) -> String {
    match obj.get(key) {
        Some(v) => python_str(v),
        None => default.to_string(),
    }
}

/// Python truthiness（bool(v)）。
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Python `str(p).lstrip("• ").strip()`：去掉行首的 • 与空格，再整段 trim。
fn clean_player_name(player: &Value) -> String {
    python_str(player)
        .trim_start_matches(['•', ' '])
        .trim()
        .to_string()
}

/// 格式化语音频道在线人员（与旧插件行为一致）。
pub fn format_voice_channels(data: &Value) -> String {
    let Some(obj) = data.as_object() else {
        return "无法获取语音频道信息。".to_string();
    };
    // data.get("channels", [])：缺失 / null / 非列表 / 空列表 都视为「没有人」
    let Some(Value::Array(channels)) = obj.get("channels") else {
        return "当前没有人在语音频道中。".to_string();
    };
    if channels.is_empty() {
        return "当前没有人在语音频道中。".to_string();
    }
    let total_users = match obj.get("total_users") {
        Some(v) => python_str(v),
        None => "0".to_string(),
    };

    let mut lines: Vec<String> = vec![format!("当前语音频道在线人数: {total_users}"), String::new()];
    for channel in channels {
        if channel.as_object().is_none() {
            continue;
        }
        let channel_name = py_get_str(channel, "channel_name", "未知频道");
        let server_name = py_get_str(channel, "server_name", "未知服务器");
        let users: Vec<&Value> = match channel.get("users") {
            Some(Value::Array(users)) => users.iter().collect(),
            _ => Vec::new(),
        };
        lines.push(format!("【{server_name}】{channel_name} ({}人)", users.len()));
        for user in &users {
            lines.push(format!("  - {}", python_str(user)));
        }
        lines.push(String::new());
    }
    if lines.len() <= 2 {
        return "当前没有人在语音频道中。".to_string();
    }
    lines.join("\n").trim().to_string()
}

/// 把状态 API 的返回格式化成文本卡片（图片渲染见 status_render）。
pub fn format_status(data: &Value) -> String {
    let Some(obj) = data.as_object() else {
        return "无法获取服务器状态。".to_string();
    };
    let mut lines: Vec<String> = vec!["服务器状态".to_string(), String::new()];

    // data.get("network_routes") or []
    if let Some(Value::Array(routes)) = obj.get("network_routes") {
        if !routes.is_empty() {
            lines.push("【节点状态】".to_string());
            for route in routes {
                if route.as_object().is_none() {
                    continue;
                };
                let online = truthy(route.get("online"));
                let mark = if online { "✅" } else { "❌" };
                let detail = if online {
                    let latency = route.get("latency").and_then(Value::as_f64).unwrap_or(0.0);
                    let loss = route.get("packet_loss").and_then(Value::as_f64).unwrap_or(0.0);
                    format!("延迟 {latency:.2}ms / 丢包 {loss:.1}%")
                } else {
                    "离线".to_string()
                };
                lines.push(format!(
                    "{} {} - {}",
                    mark,
                    py_get_str(route, "route_name", "Unknown"),
                    detail
                ));
            }
            lines.push(String::new());
        }
    }

    // data.get("servers") or []
    if let Some(Value::Array(servers)) = obj.get("servers") {
        if !servers.is_empty() {
            lines.push("【服务器状态】".to_string());
            for server in servers {
                if server.as_object().is_none() {
                    continue;
                };
                let online = truthy(server.get("online"));
                let mark = if online { "✅" } else { "❌" };
                // [str(p).lstrip("• ").strip() for p in players] if online else []
                let players: Vec<String> = if online {
                    match server.get("online_players") {
                        Some(Value::Array(list)) => list.iter().map(clean_player_name).collect(),
                        _ => Vec::new(),
                    }
                } else {
                    Vec::new()
                };
                lines.push(format!(
                    "{} {}",
                    mark,
                    py_get_str(server, "server_name", "Unknown")
                ));
                if online {
                    let joined = if players.is_empty() {
                        "(无)".to_string()
                    } else {
                        players.join(", ")
                    };
                    lines.push(format!("   在线 {} 人: {}", players.len(), joined));
                }
            }
            lines.push(String::new());
        }
    }

    lines.join("\n").trim().to_string()
}

/// 每命令计数快照（/status 计入 server）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommandStats {
    pub chatroom: u64,
    pub server: u64,
}

impl CommandStats {
    /// 从 [`CommandService::stats`] 的计数表提取快照。
    pub fn from_map(map: &HashMap<String, u64>) -> Self {
        Self {
            chatroom: map.get("chatroom").copied().unwrap_or(0),
            server: map.get("server").copied().unwrap_or(0),
        }
    }
}

/// 命令分发：/chatroom、/server（/status 为兼容别名）。
pub struct CommandService {
    client: reqwest::Client,
    allow_all: bool,
    allow_from: HashSet<i64>,
    status_image: bool,
    voice_api: String,
    status_api: String,
    server_addresses: Vec<(String, String)>,
    stats: Mutex<HashMap<String, u64>>,
}

impl CommandService {
    /// `server_addresses` 为空时回退默认地址列表（对应 Python 的
    /// `if server_addresses else list(DEFAULT_SERVER_ADDRESSES)`）。
    pub fn new(
        allow_all: bool,
        allow_from: Vec<i64>,
        status_image: bool,
        voice_api: impl Into<String>,
        status_api: impl Into<String>,
        server_addresses: Vec<(String, String)>,
    ) -> Result<Self, reqwest::Error> {
        // Python COMMAND_TIMEOUT: total=20, sock_connect=5, sock_read=15
        // reqwest 对应：总超时 20s + 连接超时 5s
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self {
            client,
            allow_all,
            allow_from: allow_from.into_iter().collect(),
            status_image,
            voice_api: voice_api.into(),
            status_api: status_api.into(),
            server_addresses: if server_addresses.is_empty() {
                DEFAULT_SERVER_ADDRESSES
                    .iter()
                    .map(|(label, value)| (label.to_string(), value.to_string()))
                    .collect()
            } else {
                server_addresses
            },
            stats: Mutex::new(HashMap::new()),
        })
    }

    pub fn allowed(&self, user_id: i64) -> bool {
        self.allow_all || self.allow_from.contains(&user_id)
    }

    /// 命令计数表（仅已受理的命令会计入；/status 归到 "server"）。
    pub fn stats(&self) -> HashMap<String, u64> {
        self.lock_stats().clone()
    }

    fn lock_stats(&self) -> MutexGuard<'_, HashMap<String, u64>> {
        self.stats
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// 返回命令响应；None 表示不响应。
    ///
    /// None：非命令 / 名字不在 (chatroom, server, status) / 未授权——**静默**，与 Python 一致。
    /// /status 归一化为 "server"（stats 计在 "server" 下）。
    /// /chatroom → GET voice_api → format_voice_channels；/server → GET status_api；
    /// status_image=true 时先尝试渲染 PNG（失败自动回退文本）。
    pub async fn handle(&self, text: &str, user_id: i64) -> Option<CommandResult> {
        let (name, _args) = parse_command(text)?;
        if !matches!(name.as_str(), "chatroom" | "server" | "status") {
            return None;
        }
        if !self.allowed(user_id) {
            return None;
        }
        // 兼容别名：/status 与 /server 同义，统计也归到 server
        let name: &str = if name == "status" { "server" } else { name.as_str() };
        *self.lock_stats().entry(name.to_string()).or_insert(0) += 1;

        if name == "chatroom" {
            let data = self.get_json(&self.voice_api).await;
            return Some(CommandResult {
                text: format_voice_channels(&data),
                image: None,
            });
        }

        let data = self.get_json(&self.status_api).await;
        if self.status_image {
            let png = crate::services::status_render::render_status_png(
                &data,
                Some(&self.server_addresses),
            )
            .await;
            if let Some(png) = png {
                return Some(CommandResult {
                    text: String::new(),
                    image: Some(png),
                });
            }
        }
        Some(CommandResult {
            text: format_status(&data),
            image: None,
        })
    }

    /// 非 200 → 警告日志 + Null；reqwest 客户端/网络错误 → Null；
    /// 成功 → 解析 JSON（不校验 content-type，对应 Python `resp.json(content_type=None)`）。
    /// 格式化函数拿到 Null 后产出「无法获取…」文案。
    async fn get_json(&self, url: &str) -> Value {
        let response = match self.client.get(url).send().await {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!("命令查询异常 {}: {}", url, err);
                return Value::Null;
            }
        };
        if response.status() != reqwest::StatusCode::OK {
            tracing::warn!("命令查询失败 {} HTTP {}", url, response.status().as_u16());
            return Value::Null;
        }
        match response.json::<Value>().await {
            Ok(data) => data,
            // Python 这里 JSONDecodeError 会向外抛；Rust 侧同样回退 Null（同为「无法获取…」文案）
            Err(err) => {
                tracing::warn!("命令响应解析失败 {}: {}", url, err);
                Value::Null
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::{StatusCode, Uri};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::Router;
    use serde_json::json;
    use std::sync::Arc;

    // ---------- 测试用固定数据（镜像 tests/test_commands.py 的 VOICE_DATA / STATUS_DATA） ----------

    const VOICE_PATH: &str = "/api/voice/qqbot/get_voice_channel_people";
    const STATUS_PATH: &str = "/api/qqbot/status";

    fn voice_data() -> Value {
        json!({
            "total_users": 2,
            "channels": [
                {"server_name": "主节点", "channel_name": "大厅", "users": ["玩家A", "甲"]},
            ],
        })
    }

    fn status_data() -> Value {
        json!({
            "network_routes": [
                {"route_name": "节点A", "online": true, "latency": 21.5, "packet_loss": 0.0},
            ],
            "servers": [
                {"server_name": "服务端A", "online": true, "online_players": ["• 玩家A"]},
                {"server_name": "服务端B", "online": false, "online_players": []},
            ],
        })
    }

    // ---------- axum mock：捕获命中的 URL，按状态回放 ----------

    #[derive(Clone)]
    struct MockState {
        requests: Arc<Mutex<Vec<String>>>,
        status: Arc<Mutex<u16>>,
        payload: Arc<Mutex<Value>>,
        text_plain: Arc<Mutex<bool>>,
    }

    async fn capture(State(state): State<MockState>, uri: Uri) -> Response {
        state.requests.lock().unwrap().push(uri.to_string());
        let status = *state.status.lock().unwrap();
        if status != 200 {
            return (StatusCode::from_u16(status).unwrap(), "boom").into_response();
        }
        let body = serde_json::to_string(&state.payload.lock().unwrap().clone()).unwrap();
        let content_type = if *state.text_plain.lock().unwrap() {
            "text/plain"
        } else {
            "application/json"
        };
        ([(axum::http::header::CONTENT_TYPE, content_type)], body).into_response()
    }

    async fn spawn_mock(payload: Value) -> (String, MockState) {
        let state = MockState {
            requests: Arc::new(Mutex::new(Vec::new())),
            status: Arc::new(Mutex::new(200)),
            payload: Arc::new(Mutex::new(payload)),
            text_plain: Arc::new(Mutex::new(false)),
        };
        let app = Router::new()
            .route(VOICE_PATH, get(capture))
            .route(STATUS_PATH, get(capture))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), state)
    }

    fn make_service(
        base: &str,
        allow_all: bool,
        allow_from: Vec<i64>,
        status_image: bool,
    ) -> CommandService {
        CommandService::new(
            allow_all,
            allow_from,
            status_image,
            format!("{base}{VOICE_PATH}"),
            format!("{base}{STATUS_PATH}"),
            Vec::new(), // 空 → 使用默认地址列表
        )
        .unwrap()
    }

    // ---------- parse_command ----------

    #[test]
    fn parse_command_forms() {
        assert_eq!(
            parse_command("/chatroom"),
            Some(("chatroom".to_string(), String::new()))
        );
        assert_eq!(
            parse_command("/status 额外"),
            Some(("status".to_string(), "额外".to_string()))
        );
        assert_eq!(
            parse_command("/Server@10000"),
            Some(("server".to_string(), String::new()))
        );
        assert_eq!(
            parse_command("/Server@Bot x"),
            Some(("server".to_string(), "x".to_string()))
        );
        // 多个空格：按第一个空格 partition，参数整体 trim
        assert_eq!(
            parse_command("/chatroom  extra"),
            Some(("chatroom".to_string(), "extra".to_string()))
        );
        assert_eq!(parse_command("chatroom"), None);
        assert_eq!(parse_command(""), None);
    }

    #[test]
    fn parse_command_slash_only_is_not_a_command() {
        // Python 对 "/" 返回 ("", "")，handle() 因名字不在命令集而丢弃；
        // Rust 侧直接视为非命令，handle("/") 的可观测行为一致（None，不计数）。
        assert_eq!(parse_command("/"), None);
        assert_eq!(parse_command("  /  "), None);
    }

    // ---------- format_voice_channels ----------

    #[test]
    fn format_voice_channels_canned() {
        assert_eq!(
            format_voice_channels(&voice_data()),
            "当前语音频道在线人数: 2\n\n【主节点】大厅 (2人)\n  - 玩家A\n  - 甲"
        );
    }

    #[test]
    fn format_voice_channels_empty_and_invalid() {
        assert_eq!(
            format_voice_channels(&json!({"channels": [], "total_users": 0})),
            "当前没有人在语音频道中。"
        );
        assert_eq!(
            format_voice_channels(&Value::Null),
            "无法获取语音频道信息。"
        );
        assert_eq!(format_voice_channels(&json!("文本")), "无法获取语音频道信息。");
        // channels 非列表 → 视为没有人
        assert_eq!(
            format_voice_channels(&json!({"channels": "x"})),
            "当前没有人在语音频道中。"
        );
        // 缺省字段回退默认文案；全部频道非法时仍视为没有人
        assert_eq!(
            format_voice_channels(&json!({"channels": [{"foo": 1}]})),
            "当前语音频道在线人数: 0\n\n【未知服务器】未知频道 (0人)"
        );
        assert_eq!(
            format_voice_channels(&json!({"channels": ["非法", {"channel_name": "大厅", "users": ["玩家A"]}]})),
            "当前语音频道在线人数: 0\n\n【未知服务器】大厅 (1人)\n  - 玩家A"
        );
    }

    // ---------- format_status ----------

    #[test]
    fn format_status_canned() {
        // 注意：Python 在整个 servers 循环结束后才追加一个空行，服务器之间没有空行
        assert_eq!(
            format_status(&status_data()),
            "服务器状态\n\n\
             【节点状态】\n\
             ✅ 节点A - 延迟 21.50ms / 丢包 0.0%\n\n\
             【服务器状态】\n\
             ✅ 服务端A\n\
             \u{20}  在线 1 人: 玩家A\n\
             ❌ 服务端B"
        );
    }

    #[test]
    fn format_status_null_and_edge_cases() {
        assert_eq!(format_status(&Value::Null), "无法获取服务器状态。");
        assert_eq!(format_status(&json!(42)), "无法获取服务器状态。");
        // 玩家名清洗：行首 • 与空格全部剥掉
        let data = json!({
            "servers": [
                {"server_name": "服A", "online": true, "online_players": ["••  玩家B", " 玩家C "]},
                {"server_name": "服B", "online": true, "online_players": []},
            ]
        });
        let text = format_status(&data);
        assert!(text.contains("✅ 服A\n   在线 2 人: 玩家B, 玩家C"), "{text}");
        assert!(text.contains("✅ 服B\n   在线 0 人: (无)"), "{text}");
        // 空路由 / 空服务器列表 → 只有标题
        assert_eq!(format_status(&json!({})), "服务器状态");
        assert_eq!(
            format_status(&json!({"network_routes": [], "servers": []})),
            "服务器状态"
        );
    }

    // ---------- CommandService ----------

    #[tokio::test]
    async fn chatroom_command_end_to_end() {
        let (base, mock) = spawn_mock(voice_data()).await;
        let service = make_service(&base, true, Vec::new(), false);

        let result = service.handle("/chatroom", 1).await;
        let result = result.expect("应响应 /chatroom");
        assert!(result.image.is_none());
        assert!(result.text.contains("在线人数: 2"), "{}", result.text);
        assert!(result.text.contains("【主节点】大厅"), "{}", result.text);
        // 命中了 voice_api
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
        assert_eq!(mock.requests.lock().unwrap()[0], VOICE_PATH);
    }

    #[tokio::test]
    async fn status_alias_counts_under_server() {
        let (base, _mock) = spawn_mock(status_data()).await;
        let service = make_service(&base, true, Vec::new(), false);

        // /status 是 /server 的兼容别名，返回同样的文本卡片
        let result = service.handle("/status", 1).await.unwrap();
        assert!(result.image.is_none());
        assert!(result.text.contains("服务端A"), "{}", result.text);

        service.handle("/chatroom", 1).await.unwrap();
        service.handle("/status", 1).await.unwrap();

        let stats = service.stats();
        assert_eq!(stats.get("server"), Some(&2));
        assert_eq!(stats.get("chatroom"), Some(&1));
        assert_eq!(
            CommandStats::from_map(&stats),
            CommandStats { chatroom: 1, server: 2 }
        );
    }

    #[tokio::test]
    async fn permission_denied_is_silent() {
        let (base, mock) = spawn_mock(voice_data()).await;
        let service = make_service(&base, false, vec![42], false);

        assert!(service.handle("/chatroom", 1).await.is_none());
        // 未授权静默：不计数、不打接口
        assert!(service.stats().is_empty());
        assert!(mock.requests.lock().unwrap().is_empty());
        // 白名单内正常响应并计数
        assert!(service.handle("/chatroom", 42).await.is_some());
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
        assert_eq!(service.stats().get("chatroom"), Some(&1));
    }

    #[tokio::test]
    async fn unknown_and_plain_text_ignored() {
        let (base, mock) = spawn_mock(status_data()).await;
        let service = make_service(&base, true, Vec::new(), false);

        assert!(service.handle("/hello", 1).await.is_none());
        assert!(service.handle("普通文本", 1).await.is_none());
        assert!(service.handle("/", 1).await.is_none());
        assert!(service.handle("", 1).await.is_none());
        assert!(mock.requests.lock().unwrap().is_empty());
        assert!(service.stats().is_empty());
    }

    #[tokio::test]
    async fn http_error_yields_unavailable_strings() {
        let (base, mock) = spawn_mock(voice_data()).await;
        *mock.status.lock().unwrap() = 500;
        let service = make_service(&base, true, Vec::new(), false);

        let result = service.handle("/chatroom", 1).await.unwrap();
        assert_eq!(result.text, "无法获取语音频道信息。");
        let result = service.handle("/server", 1).await.unwrap();
        assert_eq!(result.text, "无法获取服务器状态。");
        // Python 在查询前就计数，失败同样计入
        assert_eq!(service.stats().get("chatroom"), Some(&1));
        assert_eq!(service.stats().get("server"), Some(&1));
    }

    #[tokio::test]
    async fn connection_refused_yields_unavailable_string() {
        // 先占一个端口再释放，保证连接被立即拒绝
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let service = CommandService::new(
            true,
            Vec::new(),
            false,
            format!("http://{addr}{VOICE_PATH}"),
            DEFAULT_STATUS_API,
            Vec::new(),
        )
        .unwrap();

        let result = service.handle("/chatroom", 1).await.unwrap();
        assert_eq!(result.text, "无法获取语音频道信息。");
    }

    #[tokio::test]
    async fn json_parsed_regardless_of_content_type() {
        // 对应 Python resp.json(content_type=None)：不校验 content-type
        let (base, mock) = spawn_mock(voice_data()).await;
        *mock.text_plain.lock().unwrap() = true;
        let service = make_service(&base, true, Vec::new(), false);

        let result = service.handle("/chatroom", 1).await.unwrap();
        assert!(result.text.contains("在线人数: 2"), "{}", result.text);
    }

    #[tokio::test]
    async fn status_image_falls_back_to_text_when_render_unavailable() {
        // 数据不是对象时渲染必返回 None（对应 Python _render_status 的 isinstance 检查），
        // 因此无论是否启用 status-image / 是否有浏览器，都确定性地走文本回退。
        let (base, _mock) = spawn_mock(json!([1, 2, 3])).await;
        let service = make_service(&base, true, Vec::new(), true);

        let result = service.handle("/server", 1).await.unwrap();
        assert!(result.image.is_none());
        assert_eq!(result.text, "无法获取服务器状态。");
        assert_eq!(service.stats().get("server"), Some(&1));
    }

    #[test]
    fn command_result_empty_semantics() {
        assert!(CommandResult::default().empty());
        assert!(!CommandResult { text: "x".into(), image: None }.empty());
        assert!(!CommandResult { text: String::new(), image: Some(vec![1]) }.empty());
    }

    #[test]
    fn default_addresses_copied_from_python() {
        assert_eq!(
            DEFAULT_SERVER_ADDRESSES,
            [
                ("主IP", "game.example.com"),
                ("备用地址", "backup.example.com:25565"),
            ]
        );
    }
}
