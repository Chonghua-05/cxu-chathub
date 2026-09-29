// ============================================================
// 配置对象操作：脱敏 / 深合并 / 校验 / 落盘
// 逐条对应原 Node 版 server.js 中的同名逻辑，行为保持一致。
// ============================================================

use serde_json::{json, Map, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 敏感字段（点分路径；读取时脱敏、写入时空值保留原值）
pub const SENSITIVE_PATHS: [&str; 7] = [
    "onebot.access_token",
    "chatroom.forward_token",
    "chatroom.refresh_token",
    "chatbridge.password",
    "chatbridge.aes_key",
    "api.access_token",
    "agent.llm.api_key",
];

/// 内置默认配置模板（「恢复默认」用；优先级低于 CXU_TEMPLATE_PATH 文件）
pub const DEFAULT_TEMPLATE: &str = r#"{
  "onebot": { "listen_host": "0.0.0.0", "listen_port": 6199, "path": "/ws", "access_token": "", "self_id": 10000 },
  "chatroom": {
    "base_url": "https://chatroom.example.com",
    "channel_id": 1,
    "forward_token": "",
    "refresh_token": "",
    "group_ids": [],
    "qq_sync_enabled": true,
    "qq_forward_enabled": true,
    "qq_to_game_enabled": true,
    "player_join_pattern": "",
    "player_quit_pattern": "",
    "voice_api": "https://chatroom.example.com/api/voice/qqbot/get_voice_channel_people",
    "status_api": "https://status.example.com/api/qqbot/status",
    "server_addresses": [["主IP", "game.example.com"]],
    "poll_interval": 10
  },
  "chatbridge": { "enabled": true, "host": "", "port": 21027, "name": "web", "password": "", "aes_key": "" },
  "commands": { "group_allow_all": true, "allow_from": [], "status_image": true },
  "api": { "enabled": true, "listen_host": "127.0.0.1", "listen_port": 8199, "access_token": "" },
  "agent": {
    "enabled": false,
    "llm": { "api_url": "", "api_key": "", "model": "", "timeout_secs": 30, "max_answer_chars": 1000 },
    "routing": { "enabled": false, "group_ids": [] },
    "skills": []
  },
  "patch_broadcast": { "enabled": false, "poll_interval_secs": 1800 },
  "state_path": "/data/state.json",
  "log_level": "INFO",
  "log_format": "text"
}
"#;

// ---------- 点分路径取值 / 设值 / 删除 ----------
pub fn get_path<'a>(v: &'a Value, dotted: &str) -> Option<&'a Value> {
    let mut cur = v;
    for key in dotted.split('.') {
        match cur {
            Value::Object(m) => cur = m.get(key)?,
            _ => return None,
        }
    }
    Some(cur)
}

pub fn set_path(v: &mut Value, dotted: &str, val: Value) {
    let keys: Vec<&str> = dotted.split('.').collect();
    let mut cur = v;
    for k in &keys[..keys.len() - 1] {
        if !cur.is_object() {
            *cur = Value::Object(Map::new());
        }
        let m = cur.as_object_mut().unwrap();
        let next = m
            .entry(k.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !next.is_object() {
            *next = Value::Object(Map::new());
        }
        cur = next;
    }
    if let Some(m) = cur.as_object_mut() {
        m.insert(keys[keys.len() - 1].to_string(), val);
    }
}

pub fn delete_path(v: &mut Value, dotted: &str) {
    let keys: Vec<&str> = dotted.split('.').collect();
    let mut cur = v;
    for k in &keys[..keys.len() - 1] {
        match cur.as_object_mut().and_then(|m| m.get_mut(*k)) {
            Some(next) if next.is_object() => cur = next,
            _ => return,
        }
    }
    if let Some(m) = cur.as_object_mut() {
        m.remove(keys[keys.len() - 1]);
    }
}

// JS `String(v)` 的近似实现（仅敏感字段的非字符串值会走到这里）
pub fn js_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(a) => a.iter().map(js_to_string).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

// ---------- 脱敏：敏感字段替换为 { __masked__: true, is_set } ----------
pub fn mask_config(config: &Value) -> Value {
    let mut out = config.clone();
    for dotted in SENSITIVE_PATHS {
        let is_set = match get_path(&out, dotted) {
            None => continue,
            Some(v) => match v {
                Value::String(s) => !s.is_empty(),
                Value::Null => false,
                Value::Bool(b) => *b,
                Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
                _ => true,
            },
        };
        set_path(
            &mut out,
            dotted,
            json!({ "__masked__": true, "is_set": is_set }),
        );
    }
    out
}

// ---------- 深合并：对象递归合并；数组/标量整体替换；敏感字段空值保留原值 ----------
pub fn deep_merge(base: &Value, incoming: &Value) -> Value {
    let mut out = base.clone();
    let mut inc = incoming.clone();
    for dotted in SENSITIVE_PATHS {
        let iv = match get_path(&inc, dotted) {
            Some(v) => v.clone(),
            None => continue,
        };
        let keep = match &iv {
            Value::String(s) => s.is_empty(), // 空字符串 = 保留原值
            Value::Object(m) => m.get("__masked__") == Some(&Value::Bool(true)), // 原样带回的脱敏对象 = 保留
            _ => false,
        };
        if !keep {
            set_path(&mut out, dotted, Value::String(js_to_string(&iv)));
        }
        // 从 incoming 里摘掉敏感字段，后面的通用深合并不再碰它
        delete_path(&mut inc, dotted);
    }
    merge_into(&mut out, &inc);
    out
}

fn merge_into(base: &mut Value, incoming: &Value) {
    if !incoming.is_object() {
        return;
    }
    if !base.is_object() {
        *base = Value::Object(Map::new());
    }
    let bm = base.as_object_mut().unwrap();
    if let Value::Object(inc) = incoming {
        for (k, v) in inc {
            let is_deep = v.is_object() && bm.get(k).map(|x| x.is_object()).unwrap_or(false);
            if is_deep {
                merge_into(bm.get_mut(k).unwrap(), v);
            } else {
                bm.insert(k.clone(), v.clone());
            }
        }
    }
}

// ---------- 校验：按 config.json 的结构做范围 / 类型 / 必填检查 ----------
fn is_http_url(v: Option<&Value>) -> bool {
    match v {
        Some(Value::String(s)) => {
            s.is_empty() || s.starts_with("http://") || s.starts_with("https://")
        }
        _ => false,
    }
}

fn is_port(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => {
            let x = n.as_i64().unwrap_or(-1);
            (1..=65535).contains(&x)
        }
        _ => false,
    }
}

fn is_nonneg_int(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => {
            n.as_i64().map(|x| x >= 0).unwrap_or(false)
        }
        _ => false,
    }
}

fn is_int_ge(v: Option<&Value>, min: i64) -> bool {
    match v {
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => {
            n.as_i64().map(|x| x >= min).unwrap_or(false)
        }
        _ => false,
    }
}

fn get<'a>(cfg: &'a Value, key: &str) -> Option<&'a Value> {
    cfg.get(key)
}

pub fn validate_config(cfg: &Value) -> Vec<String> {
    let mut e: Vec<String> = Vec::new();
    let plain = |v: Option<&Value>| v.map(|x| x.is_object()).unwrap_or(false);

    // onebot
    let onebot = get(cfg, "onebot");
    if !plain(onebot) {
        e.push("onebot 段缺失或不是对象".to_string());
    } else {
        let o = onebot.unwrap();
        if !is_port(o.get("listen_port")) {
            e.push("onebot.listen_port 必须是 1-65535 的整数".to_string());
        }
        match o.get("path") {
            Some(Value::String(s)) if s.starts_with('/') => {}
            _ => e.push("onebot.path 必须以 / 开头".to_string()),
        }
        if !is_nonneg_int(o.get("self_id")) {
            e.push("onebot.self_id 必须是非负整数".to_string());
        }
    }

    // chatroom
    let chatroom = get(cfg, "chatroom");
    if !plain(chatroom) {
        e.push("chatroom 段缺失或不是对象".to_string());
    } else {
        let c = chatroom.unwrap();
        if !is_http_url(c.get("base_url")) {
            e.push("chatroom.base_url 必须以 http(s):// 开头".to_string());
        }
        if !is_nonneg_int(c.get("channel_id")) {
            e.push("chatroom.channel_id 必须是非负整数".to_string());
        }
        let gids_ok = match c.get("group_ids") {
            Some(Value::Array(a)) => a.iter().all(|g| g.is_i64() || g.is_u64()),
            _ => false,
        };
        if !gids_ok {
            e.push("chatroom.group_ids 必须是整数数组".to_string());
        }
        if !is_int_ge(c.get("poll_interval"), 1) {
            e.push("chatroom.poll_interval 必须 >= 1".to_string());
        }
        for key in ["voice_api", "status_api"] {
            if !is_http_url(c.get(key)) {
                e.push(format!("chatroom.{} 必须以 http(s):// 开头", key));
            }
        }
    }

    // chatbridge
    let chatbridge = get(cfg, "chatbridge");
    if !plain(chatbridge) {
        e.push("chatbridge 段缺失或不是对象".to_string());
    } else if !is_port(chatbridge.unwrap().get("port")) {
        e.push("chatbridge.port 必须是 1-65535 的整数".to_string());
    }

    // commands
    let commands = get(cfg, "commands");
    if !plain(commands) {
        e.push("commands 段缺失或不是对象".to_string());
    } else if !commands
        .unwrap()
        .get("allow_from")
        .map(|v| v.is_array())
        .unwrap_or(false)
    {
        e.push("commands.allow_from 必须是数组".to_string());
    }

    // api
    let api = get(cfg, "api");
    if !plain(api) {
        e.push("api 段缺失或不是对象".to_string());
    } else if !is_port(api.unwrap().get("listen_port")) {
        e.push("api.listen_port 必须是 1-65535 的整数".to_string());
    }

    // agent
    let agent = get(cfg, "agent");
    if !plain(agent) {
        e.push("agent 段缺失或不是对象".to_string());
    } else if let Some(llm) = agent.unwrap().get("llm") {
        if !llm.is_null() {
            if !llm.is_object() {
                e.push("agent.llm 必须是对象".to_string());
            } else {
                if !is_http_url(llm.get("api_url")) {
                    e.push("agent.llm.api_url 必须以 http(s):// 开头".to_string());
                }
                if !is_int_ge(llm.get("timeout_secs"), 1) {
                    e.push("agent.llm.timeout_secs 必须 >= 1".to_string());
                }
                if !is_int_ge(llm.get("max_answer_chars"), 1) {
                    e.push("agent.llm.max_answer_chars 必须 >= 1".to_string());
                }
            }
        }
    }

    // patch_broadcast
    let pb = get(cfg, "patch_broadcast");
    if !plain(pb) {
        e.push("patch_broadcast 段缺失或不是对象".to_string());
    } else if !is_int_ge(pb.unwrap().get("poll_interval_secs"), 10) {
        e.push("patch_broadcast.poll_interval_secs 必须 >= 10".to_string());
    }

    // 顶层
    match cfg.get("state_path") {
        Some(Value::String(s)) if !s.is_empty() => {}
        _ => e.push("state_path 必须是非空字符串".to_string()),
    }
    match cfg.get("log_level") {
        Some(Value::String(s)) if ["DEBUG", "INFO", "WARNING", "ERROR"].contains(&s.as_str()) => {}
        _ => e.push("log_level 必须是 DEBUG / INFO / WARNING / ERROR 之一".to_string()),
    }
    match cfg.get("log_format") {
        Some(Value::String(s)) if ["text", "json"].contains(&s.as_str()) => {}
        _ => e.push("log_format 必须是 text 或 json".to_string()),
    }

    e
}

// ---------- 读取 ----------
pub fn read_config_file(path: &str) -> Result<Value, String> {
    let raw = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let raw = raw.strip_prefix('\u{FEFF}').unwrap_or(&raw);
    serde_json::from_str(raw).map_err(|e| e.to_string())
}

pub fn load_template(template_path: &str) -> Value {
    if !template_path.is_empty() {
        if let Ok(raw) = fs::read_to_string(template_path) {
            let raw = raw.strip_prefix('\u{FEFF}').unwrap_or(&raw);
            if let Ok(v) = serde_json::from_str::<Value>(raw) {
                return v;
            }
            eprintln!("[模板] 读取 {} 失败，使用内置默认模板", template_path);
        } else {
            eprintln!("[模板] 读取 {} 失败，使用内置默认模板", template_path);
        }
    }
    serde_json::from_str(DEFAULT_TEMPLATE).expect("内置模板必须是合法 JSON")
}

// ---------- 落盘：挂载点检测 + 原子写 ----------
// 单文件 bind mount（线上 config.json 的挂法）下，rename 会替换 inode，
// 容器里仍指向旧文件、restart 也读不到新值——因此检测到挂载点时改为原地写。
pub fn is_mountpoint(target: &str) -> bool {
    let mounts = match fs::read_to_string("/proc/self/mounts") {
        Ok(m) => m,
        Err(_) => return false, // 非 Linux（本机开发）按普通文件处理
    };
    let abs = absolute(target);
    for line in mounts.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && decode_mount_path(parts[1]) == abs {
            return true;
        }
    }
    false
}

fn decode_mount_path(p: &str) -> String {
    p.replace("\\040", " ").replace("\\011", "\t")
}

fn absolute(p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

pub fn write_config_file(target: &str, content: &str) -> Result<(), String> {
    let target_path = absolute(target);
    let dir = target_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let pid = std::process::id();
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let tmp = dir.join(format!(".config.{}.{}.tmp", pid, ms));

    // 先写临时文件 + fsync
    {
        let mut f = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(content.as_bytes()).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }

    if is_mountpoint(target) {
        // 原地覆盖（保持 inode，单文件 bind mount 容器可见）
        let mut f = fs::OpenOptions::new()
            .write(true)
            .open(&target_path)
            .map_err(|e| e.to_string())?;
        f.write_all(content.as_bytes()).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        f.set_len(content.len() as u64).map_err(|e| e.to_string())?;
        let _ = fs::remove_file(&tmp);
    } else {
        fs::rename(&tmp, &target_path).map_err(|e| e.to_string())?; // 原子替换
    }
    Ok(())
}
