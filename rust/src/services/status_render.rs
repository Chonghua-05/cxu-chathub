//! `/status`、`/server` 的 HTML → PNG 渲染（Chromium）。
//!
//! 渲染产物以 PNG 字节交给上层（适配层 base64 后发图），因此不依赖与
//! 宿主机共享文件系统。对应 Python 版 `chatroom_bridge/status_render.py`：
//! - [`build_status_html`] 纯模板填充，不依赖浏览器，可单测；
//! - 真正渲染由可选 feature `status-image`（headless_chrome）门控，未启用或
//!   浏览器不可用时 [`render_status_png`] 返回 None，上层回退文本。

use serde_json::Value;

/// 模板经 include_str! 嵌入；背景图 include_bytes!（约 800KB，可接受）
pub const TEMPLATE: &str = include_str!("templates/status.html");
pub const BACKGROUND_JPG: &[u8] = include_bytes!("templates/status-bg.jpg");

/// Python 默认 `ADDRESSES`（与 commands::DEFAULT_SERVER_ADDRESSES 相同）
pub const DEFAULT_ADDRESSES: [(&str, &str); 2] = [
    ("主IP", "game.example.com"),
    ("备用地址", "backup.example.com:25565"),
];

// ---------- 与 commands.rs 相同语义的小工具（Python 侧两个模块也是各自实现） ----------

/// Python 风格标量转字符串（str(v)）。
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

/// dict.get(key, default) + f-string。
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

/// Python `str(p).lstrip("• ").strip()`。
fn clean_player_name(player: &Value) -> String {
    python_str(player)
        .trim_start_matches(['•', ' '])
        .trim()
        .to_string()
}

// ---------- 模板填充 ----------

/// 填充状态页模板（不依赖浏览器，可单测）。
///
/// 对应 Python `template.format(background=…, addresses=…, routes=…, servers=…)`：
/// 先替换四个单花括号占位符，再把 CSS 里成对的 `{{ }}` 收拢为单花括号。
pub fn build_status_html(
    data: &Value,
    background_base64: Option<&str>,
    addresses: Option<&[(String, String)]>,
) -> String {
    // addresses or ADDRESSES：None / 空列表都回退默认地址
    let default_addresses: Vec<(String, String)> = DEFAULT_ADDRESSES
        .iter()
        .map(|(label, value)| (label.to_string(), value.to_string()))
        .collect();
    let resolved: &[(String, String)] = match addresses {
        Some(list) if !list.is_empty() => list,
        _ => &default_addresses,
    };

    let addresses_html: String = resolved
        .iter()
        .map(|(label, value)| {
            format!(
                "<div class=\"address-row\"><span class=\"address-label\">{label}</span>\
                 <span class=\"address-value\">{value}</span></div>"
            )
        })
        .collect();

    let mut routes_html = String::new();
    if let Some(Value::Array(routes)) = data.get("network_routes") {
        for route in routes {
            if route.as_object().is_none() {
                continue;
            };
            let online = truthy(route.get("online"));
            let (icon, text) = if online { ("✅", "在线") } else { ("❌", "离线") };
            let color = if online { "#4caf50" } else { "#f44336" };
            let latency = if online {
                format!("{:.2}ms", route.get("latency").and_then(Value::as_f64).unwrap_or(0.0))
            } else {
                "N/A".to_string()
            };
            let loss = if online {
                format!("{:.1}%", route.get("packet_loss").and_then(Value::as_f64).unwrap_or(0.0))
            } else {
                "N/A".to_string()
            };
            routes_html.push_str(&format!(
                r#"
            <div class="item">
                <div class="item-name">{name}</div>
                <div class="status" style="color: {color};">{icon} {text}</div>
                <div class="detail">延迟: {latency}</div>
                <div class="detail">丢包: {loss}</div>
            </div>"#,
                name = py_get_str(route, "route_name", "Unknown"),
                color = color,
                icon = icon,
                text = text,
                latency = latency,
                loss = loss,
            ));
        }
    }

    let mut servers_html = String::new();
    if let Some(Value::Array(servers)) = data.get("servers") {
        for server in servers {
            if server.as_object().is_none() {
                continue;
            };
            let online = truthy(server.get("online"));
            let (icon, text) = if online { ("✅", "在线") } else { ("❌", "离线") };
            let color = if online { "#4caf50" } else { "#f44336" };
            let mut players_html = String::new();
            if online {
                let players: Vec<String> = match server.get("online_players") {
                    Some(Value::Array(list)) => list.iter().map(clean_player_name).collect(),
                    _ => Vec::new(),
                };
                players_html = format!(
                    "<div class='players'>{}</div>",
                    players
                        .iter()
                        .map(|p| format!("<div class='player'>{p}</div>"))
                        .collect::<String>()
                );
                if players.is_empty() {
                    players_html = "<div class='players'><div class='player'>(无)</div></div>".to_string();
                }
            }
            servers_html.push_str(&format!(
                r#"
            <div class="item">
                <div class="item-name">{name}</div>
                <div class="status" style="color: {color};">{icon} {text}</div>
                {players_html}
            </div>"#,
                name = py_get_str(server, "server_name", "Unknown"),
                color = color,
                icon = icon,
                text = text,
                players_html = players_html,
            ));
        }
    }

    let background = match background_base64 {
        Some(base64) => {
            format!("background: url('data:image/jpeg;base64,{base64}') no-repeat center center;")
        }
        None => "background: linear-gradient(135deg, #1a1a2e 0%, #16213e 50%, #0f3460 100%);"
            .to_string(),
    };

    TEMPLATE.replace("{background}", &background)
        .replace("{addresses}", &addresses_html)
        .replace("{routes}", &routes_html)
        .replace("{servers}", &servers_html)
        .replace("{{", "{")
        .replace("}}", "}")
}

// ---------- 渲染 ----------

/// 渲染错误；`Display` 消息进入上层「回退文本」的警告日志。
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// 未启用 `status-image` feature（编译期降级，行为等同渲染失败回退文本）
    #[error("status-image feature 未启用")]
    Unsupported(&'static str),

    /// 找不到可用的 Chrome/Chromium 可执行文件
    #[cfg(feature = "status-image")]
    #[error("未找到可用的 Chrome/Chromium（可通过环境变量 CHROME_PATH 指定路径）")]
    ChromeNotFound,

    /// Chromium 启动 / 交互失败（headless_chrome 返回 anyhow 错误，这里保留其消息）
    #[cfg(feature = "status-image")]
    #[error("Chromium 渲染失败: {0}")]
    Chrome(String),

    /// 注入 HTML 的脚本序列化失败
    #[cfg(feature = "status-image")]
    #[error("HTML 序列化失败: {0}")]
    Serialize(String),
}

/// Playwright 渲染器等价物；浏览器不可用时返回 Err，由上层回退到文本。
pub struct StatusRenderer {
    pub width: u32,
    pub height: u32,
    pub addresses: Option<Vec<(String, String)>>,
}

impl StatusRenderer {
    /// 默认 1500x1400（与 Python 一致）。
    pub fn new(addresses: Option<Vec<(String, String)>>) -> Self {
        Self {
            width: 1500,
            height: 1400,
            addresses,
        }
    }

    #[cfg(feature = "status-image")]
    pub async fn render(&self, data: &Value) -> Result<Vec<u8>, RenderError> {
        use headless_chrome::protocol::cdp::{Emulation, Page};
        use headless_chrome::{Browser, LaunchOptionsBuilder};

        // Python：背景图懒加载（PIL 重编码 JPEG q=60 → base64），失败用渐变兜底
        let background = load_background();
        let html = build_status_html(data, background.as_deref(), self.addresses.as_deref());

        // 定位浏览器；找不到即失败（上层回退文本）
        let path = find_chrome().ok_or(RenderError::ChromeNotFound)?;

        let options = LaunchOptionsBuilder::default()
            .headless(true)
            .sandbox(false) // 进程内等价追加 --no-sandbox（对应 Python 启动参数）
            .window_size(Some((self.width, self.height)))
            .path(Some(path))
            .args(vec![std::ffi::OsStr::new("--disable-dev-shm-usage")])
            .build()
            .map_err(chrome_err)?;
        let browser = Browser::new(options).map_err(chrome_err)?;
        let tab = browser.new_tab().map_err(chrome_err)?;
        tab.navigate_to("about:blank").map_err(chrome_err)?;
        tab.wait_until_navigated().map_err(chrome_err)?;

        // crate 没有 set_content 辅助：向 about:blank 写入文档（JSON 字符串即合法 JS 字面量）
        let script = format!(
            "document.open();document.write({});document.close();",
            serde_json::to_string(&html).map_err(|err| RenderError::Serialize(err.to_string()))?
        );
        tab.evaluate(&script, false).map_err(chrome_err)?;

        // 主路径（对应 Playwright）：量取 body 包围盒 → 视口贴合其尺寸 → 截整页；
        // 量取 / 视口覆盖失败则退化为直接按 body 元素（box-model 裁剪）截图。
        let fitted = tab.find_element("body").and_then(|body| {
            let model = body.get_box_model()?;
            let width = model.width.round().clamp(1.0, 20000.0) as u32;
            let height = model.height.round().clamp(1.0, 20000.0) as u32;
            tab.call_method(Emulation::SetDeviceMetricsOverride {
                width,
                height,
                device_scale_factor: 1.0,
                mobile: false,
                scale: None,
                screen_width: None,
                screen_height: None,
                position_x: None,
                position_y: None,
                dont_set_visible_size: None,
                screen_orientation: None,
                viewport: None,
                display_feature: None,
                device_posture: None,
            })?;
            Ok(())
        });

        let png = match fitted {
            Ok(()) => tab.capture_screenshot(Page::CaptureScreenshotFormatOption::Png, None, None, true),
            Err(_) => tab
                .find_element("body")
                .and_then(|body| body.capture_screenshot(Page::CaptureScreenshotFormatOption::Png)),
        };
        png.map_err(chrome_err)
    }

    #[cfg(not(feature = "status-image"))]
    pub async fn render(&self, _data: &Value) -> Result<Vec<u8>, RenderError> {
        // 编译期降级：恒返回 Unsupported，上层据此回退文本
        Err(RenderError::Unsupported("status-image feature 未启用"))
    }
}

#[cfg(feature = "status-image")]
fn chrome_err<E: std::fmt::Display>(err: E) -> RenderError {
    RenderError::Chrome(err.to_string())
}

/// 定位 Chrome/Chromium：先看环境变量 CHROME_PATH，再探测常见系统路径。
#[cfg(feature = "status-image")]
fn find_chrome() -> Option<std::path::PathBuf> {
    if let Ok(path) = std::env::var("CHROME_PATH") {
        let path = path.trim();
        if !path.is_empty() {
            return Some(std::path::PathBuf::from(path));
        }
    }
    [
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome",
    ]
    .into_iter()
    .map(std::path::PathBuf::from)
    .find(|path| path.exists())
}

/// 对应 Python `_load_background`：解码内嵌 JPEG → RGB → quality=60 重编码 → base64。
/// 失败返回 None（模板回退渐变背景）。
#[cfg(feature = "status-image")]
fn load_background() -> Option<String> {
    use base64::Engine as _;

    let image = image::load_from_memory(BACKGROUND_JPG).ok()?;
    let rgb = image.to_rgb8();
    let mut output = std::io::Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, 60);
    rgb.write_with_encoder(encoder).ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(output.into_inner()))
}

/// 渲染失败返回 None（上层回退文本）。
///
/// 数据不是对象时直接放弃（对应 Python `_render_status` 的 isinstance 检查），
/// 因此无论 feature / 浏览器可用性如何，非法数据的可观测行为都是回退文本。
pub async fn render_status_png(
    data: &Value,
    addresses: Option<&[(String, String)]>,
) -> Option<Vec<u8>> {
    if !data.is_object() {
        return None;
    }
    match StatusRenderer::new(addresses.map(|list| list.to_vec()))
        .render(data)
        .await
    {
        Ok(png) => Some(png),
        Err(err) => {
            tracing::warn!("状态图渲染失败，回退文本: {err}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 镜像 tests/test_commands.py 的 STATUS_DATA
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

    #[test]
    fn template_renders_values() {
        let html = build_status_html(&status_data(), None, None);
        // Python 测试断言的全部内容
        assert!(html.contains("game.example.com"), "默认地址应来自 ADDRESSES");
        assert!(html.contains("服务端A"));
        assert!(html.contains("玩家A"));
        assert!(html.contains("linear-gradient"), "没背景图时用渐变兜底");
        assert!(!html.contains("{routes}"));
        assert!(!html.contains("{servers}"));
        // 路由 / 服务器行内容
        assert!(html.contains("节点A"));
        assert!(html.contains(r#"<div class="item-name">节点A</div>"#));
        assert!(html.contains("延迟: 21.50ms"));
        assert!(html.contains("丢包: 0.0%"));
        assert!(html.contains("✅ 在线"));
        assert!(html.contains("❌ 离线"));
        // 玩家名清洗后的节点
        assert!(html.contains("<div class='player'>玩家A</div>"));
        // 离线服务器没有玩家列表、在线服务器有
        assert!(!html.contains("(无)"));
        assert!(html.contains("<div class='players'>"));
    }

    #[test]
    fn doubled_css_braces_collapsed_to_single() {
        let html = build_status_html(&status_data(), None, None);
        assert!(html.contains("body {"), "双花括号应已收拢为单花括号");
        assert!(!html.contains("{{"));
        assert!(!html.contains("}}"));
        assert!(html.contains("* { margin: 0; padding: 0; box-sizing: border-box; }"));
        // 四个占位符全部被替换
        assert!(!html.contains("{background}"));
        assert!(!html.contains("{addresses}"));
        assert!(!html.contains("{routes}"));
        assert!(!html.contains("{servers}"));
    }

    #[test]
    fn default_addresses_when_none() {
        let html = build_status_html(&status_data(), None, None);
        assert!(html.contains("主IP"));
        assert!(html.contains("备用地址"));
        assert!(html.contains("backup.example.com:25565"));
        assert!(html.contains(r#"<span class="address-value">game.example.com</span>"#));
    }

    #[test]
    fn custom_addresses_override_and_empty_falls_back() {
        let addresses = vec![("测试IP".to_string(), "mc.test.dev:1234".to_string())];
        let html = build_status_html(&status_data(), None, Some(&addresses));
        assert!(html.contains("mc.test.dev:1234"));
        assert!(!html.contains("game.example.com"));

        // Python 的 `addresses or ADDRESSES`：空列表同样回退默认
        let empty: Vec<(String, String)> = Vec::new();
        let html = build_status_html(&status_data(), None, Some(empty.as_slice()));
        assert!(html.contains("game.example.com"));
    }

    #[test]
    fn background_variants() {
        let gradient = build_status_html(&status_data(), None, None);
        assert!(gradient.contains("background: linear-gradient(135deg, #1a1a2e 0%, #16213e 50%, #0f3460 100%);"));

        let with_bg = build_status_html(&status_data(), Some("QUJD"), None);
        assert!(
            with_bg.contains("background: url('data:image/jpeg;base64,QUJD') no-repeat center center;"),
            "背景 data-URI 应被注入"
        );
        // body 背景不再使用渐变（注意 .divider 的 CSS 里本就有 linear-gradient，不能整体排除）
        assert!(!with_bg.contains("background: linear-gradient(135deg, #1a1a2e"));
    }

    #[test]
    fn empty_players_placeholder() {
        let data = json!({
            "servers": [{"server_name": "空服", "online": true, "online_players": []}],
        });
        let html = build_status_html(&data, None, None);
        assert!(html.contains("空服"));
        assert!(
            html.contains("<div class='players'><div class='player'>(无)</div></div>"),
            "在线但无人时应显示 (无)"
        );
    }

    #[test]
    fn non_dict_rows_skipped_and_null_data_renders_empty_sections() {
        let data = json!({
            "network_routes": ["非法", {"route_name": "节点B", "online": false}],
            "servers": ["非法", {"server_name": "服X", "online": true, "online_players": []}],
        });
        let html = build_status_html(&data, None, None);
        assert!(html.contains("节点B"));
        assert!(html.contains("服X"));
        assert!(!html.contains("非法"));

        let html = build_status_html(&Value::Null, None, None);
        assert!(html.contains("节点状态"), "模板骨架仍在");
        assert!(!html.contains(r#"<div class="item-name">"#), "无任何数据行");
        assert!(html.contains("linear-gradient"));
    }

    #[test]
    fn embedded_assets_present() {
        assert!(TEMPLATE.contains("节点状态"));
        assert!(TEMPLATE.contains("{background}"));
        assert!(TEMPLATE.contains("{addresses}"));
        assert!(TEMPLATE.contains("{routes}"));
        assert!(TEMPLATE.contains("{servers}"));
        assert!(BACKGROUND_JPG.len() > 500_000, "背景图约 800KB");
        // JPEG 魔数
        assert_eq!(&BACKGROUND_JPG[..3], &[0xFF, 0xD8, 0xFF]);
    }

    #[test]
    fn renderer_defaults() {
        let renderer = StatusRenderer::new(None);
        assert_eq!(renderer.width, 1500);
        assert_eq!(renderer.height, 1400);
        assert!(renderer.addresses.is_none());

        let renderer = StatusRenderer::new(Some(vec![("a".into(), "b".into())]));
        assert_eq!(renderer.addresses.as_deref().map(|a| a.len()), Some(1));
    }

    // 真渲染：仅当本机确有可用的 Chrome/Chromium（CHROME_PATH 或常见系统路径）时执行，
    // 否则静默跳过——CI 没有浏览器也不能挂。
    #[cfg(feature = "status-image")]
    #[tokio::test]
    async fn render_png_when_chrome_available() {
        if find_chrome().is_none() {
            return; // 无浏览器环境：跳过
        }
        let renderer = StatusRenderer::new(None);
        let png = renderer
            .render(&status_data())
            .await
            .expect("Chrome 可用时渲染应成功");
        assert!(png.len() > 1_000);
        // PNG 魔数
        assert_eq!(&png[..4], &[0x89, b'P', b'N', b'G']);
    }
}
