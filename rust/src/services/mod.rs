//! 策略层：什么该转发、怎么去重、命令如何响应。
//! - [`forwarder`]        QQ 群 → chatroom 转发流水线（文本 / 图片 / 引用 / 本地去重）
//! - [`player_events`]    玩家上下线事件检测（ChatBridge 系统广播 + 可配置正则）
//! - [`commands`]         斜杠命令解析与格式化（/chatroom /server）
//! - [`status_render`]    /server 状态图渲染（纯 Rust：cosmic-text + resvg）
//! - [`patch_broadcast`]  Mojang 版本更新播报（轮询 feed → 翻译 → 截图 → 合并转发，v0.5）

pub mod commands;
pub mod forwarder;
pub mod patch_broadcast;
pub mod player_events;
pub mod status_render;

// --- Python 语义小工具（commands 与 status_render 共用，避免两处各抄一份） ---

use serde_json::Value;

/// Python 风格标量转字符串（str(v) / f-string 插值）：None → "None"、
/// True/False 保留 Python 大写、字符串原样、数字与 serde 表示一致。
pub(crate) fn python_str(value: &Value) -> String {
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
pub(crate) fn py_get_str(obj: &Value, key: &str, default: &str) -> String {
    match obj.get(key) {
        Some(v) => python_str(v),
        None => default.to_string(),
    }
}

/// Python truthiness（bool(v)）。
pub(crate) fn truthy(value: Option<&Value>) -> bool {
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
pub(crate) fn clean_player_name(player: &Value) -> String {
    python_str(player)
        .trim_start_matches(['•', ' '])
        .trim()
        .to_string()
}
