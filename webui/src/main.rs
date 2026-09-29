// ============================================================
// cxu-chathub Web 控制台 —— 中间层后端（Rust + axum）
//
// 职责（前端永不直接接触 config.json 与真实 token）：
//   1. 读 / 写 config.json（敏感字段脱敏下发、空值保留原值、原子落盘）
//   2. 代理 cxu-chathub 的 /api/*（持有 api.access_token，不下发浏览器）
//   3. 登录鉴权（内存 session + HttpOnly Cookie + 登录失败限速）
//   4. 配置写入后 docker restart 容器并轮询健康检查
//   5. SSE 每 2 秒推送服务状态
//   6. 操作日志（不含任何敏感值）
//
// 行为与旧 Node/Express 版 server.js 保持一致，配置/环境变量契约不变。
// ============================================================

mod cfg;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use cfg::*;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::timeout;
use tower_http::services::ServeDir;

// ---------- 运行状态 ----------
struct Session {
    user: String,
    expires: i64,
}

struct LockEntry {
    count: u32,
    until: i64,
}

struct Op {
    time: String,
    user: String,
    action: String,
    detail: String,
    result: String,
}

struct AppState {
    config_path: String,
    api_url: String,
    health_url: String,
    container_name: String,
    docker_enabled: bool,
    template_path: String,
    admin_user: String,
    admin_password: String,
    session_ttl_ms: i64,
    sessions: Mutex<HashMap<String, Session>>,
    failed: Mutex<HashMap<String, LockEntry>>,
    ops: Mutex<Vec<Op>>,
    client: reqwest::Client,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// 操作日志时间：与原 Node 版一致，取 UTC 的 "YYYY-MM-DD HH:MM:SS"
fn utc_timestamp() -> String {
    use time::macros::format_description;
    let now = time::OffsetDateTime::now_utc();
    let fmt = format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    now.format(&fmt).unwrap_or_default()
}

fn log_op(state: &AppState, user: &str, action: &str, detail: &str, result: &str) {
    let mut ops = state.ops.lock().unwrap();
    ops.insert(
        0,
        Op {
            time: utc_timestamp(),
            user: if user.is_empty() {
                "-".to_string()
            } else {
                user.to_string()
            },
            action: action.to_string(),
            detail: detail.to_string(),
            result: if result.is_empty() {
                "ok".to_string()
            } else {
                result.to_string()
            },
        },
    );
    ops.truncate(200);
}

// ---------- Cookie / 鉴权 ----------
fn parse_cookies(header_value: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for part in header_value.split(';') {
        if let Some(idx) = part.find('=') {
            if idx > 0 {
                let name = part[..idx].trim().to_string();
                let value = part[idx + 1..].trim().to_string();
                out.insert(name, value);
            }
        }
    }
    out
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    parse_cookies(raw).get("cxu_session").cloned()
}

/// 校验会话并滑动续期；返回登录用户名。
fn auth_user(state: &AppState, headers: &HeaderMap) -> Option<String> {
    let token = cookie_token(headers)?;
    let now = now_ms();
    let mut sessions = state.sessions.lock().unwrap();
    match sessions.get_mut(&token) {
        Some(s) if s.expires >= now => {
            s.expires = now + state.session_ttl_ms; // 滑动续期
            Some(s.user.clone())
        }
        Some(_) => {
            sessions.remove(&token);
            None
        }
        None => None,
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "未登录或会话已过期" })),
    )
        .into_response()
}

/// 常量时间比较（与 Node 版一致：先 sha256 再逐字节比较）
fn constant_time_equal(a: &str, b: &str) -> bool {
    let ha = Sha256::digest(a.as_bytes());
    let hb = Sha256::digest(b.as_bytes());
    let mut diff = 0u8;
    for i in 0..ha.len() {
        diff |= ha[i] ^ hb[i];
    }
    diff == 0
}

// ---------- 请求体 JSON ----------
fn parse_json_body(body: &Bytes) -> Result<Value, Response> {
    let text = String::from_utf8_lossy(body);
    if text.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_str(&text).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "请求体不是合法 JSON" })),
        )
            .into_response()
    })
}

// ---------- 服务重启 + 健康轮询 ----------
async fn docker_restart(state: &AppState) -> (bool, String) {
    if !state.docker_enabled {
        return (false, "CXU_DOCKER_ENABLED=false，跳过重启".to_string());
    }
    let fut = tokio::process::Command::new("docker")
        .arg("restart")
        .arg(&state.container_name)
        .output();
    match timeout(Duration::from_millis(90_000), fut).await {
        Ok(Ok(out)) if out.status.success() => {
            (true, format!("容器 {} 已重启", state.container_name))
        }
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let msg = if !stderr.is_empty() {
                stderr
            } else {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            };
            (false, format!("docker restart 失败: {}", msg))
        }
        Ok(Err(e)) => (false, format!("docker restart 失败: {}", e)),
        Err(_) => (false, "docker restart 失败: 超时".to_string()),
    }
}

async fn poll_health(state: &AppState) -> bool {
    for _ in 0..30 {
        if let Ok(resp) = state
            .client
            .get(&state.health_url)
            .timeout(Duration::from_millis(2000))
            .send()
            .await
        {
            if resp.status().is_success() {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
    false
}

// ---------- 代理到 cxu-chathub ----------
async fn get_service_token(state: &AppState) -> String {
    match read_config_file(&state.config_path) {
        Ok(c) => get_path(&c, "api.access_token")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

async fn proxy_to(
    state: &AppState,
    service_path: &str,
    method: reqwest::Method,
    body: Option<Value>,
) -> Response {
    let token = get_service_token(state).await;
    let mut req = state
        .client
        .request(method, format!("{}{}", state.api_url, service_path))
        .header(header::CONTENT_TYPE, "application/json")
        .timeout(Duration::from_millis(8000));
    if !token.is_empty() {
        req = req.header(header::AUTHORIZATION, format!("Bearer {}", token));
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    match req.send().await {
        Ok(resp) => {
            let code =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let text = resp.text().await.unwrap_or_default();
            (
                code,
                [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
                text,
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "服务未连接", "detail": e.to_string() })),
        )
            .into_response(),
    }
}

// ============================================================
// 处理器
// ============================================================

async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    let ip = peer.ip().to_string();
    let now = now_ms();

    // 限速：同 IP 连续失败 5 次锁定 60 秒
    {
        let failed = state.failed.lock().unwrap();
        if let Some(lock) = failed.get(&ip) {
            if lock.until > now {
                let seconds = ((lock.until - now) as f64 / 1000.0).ceil() as i64;
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({ "error": format!("失败次数过多，请 {} 秒后再试", seconds) })),
                )
                    .into_response();
            }
        }
    }

    let parsed = match parse_json_body(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let username = value_to_string(parsed.get("username"));
    let password = value_to_string(parsed.get("password"));

    if constant_time_equal(&username, &state.admin_user)
        && constant_time_equal(&password, &state.admin_password)
    {
        state.failed.lock().unwrap().remove(&ip);
        let token = random_token();
        state.sessions.lock().unwrap().insert(
            token.clone(),
            Session {
                user: username.clone(),
                expires: now + state.session_ttl_ms,
            },
        );
        log_op(&state, &username, "登录", &format!("ip={}", ip), "ok");
        let cookie = format!(
            "cxu_session={}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
            token,
            state.session_ttl_ms / 1000
        );
        return ([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response();
    }

    {
        let mut failed = state.failed.lock().unwrap();
        let entry = failed
            .entry(ip.clone())
            .or_insert(LockEntry { count: 0, until: 0 });
        entry.count += 1;
        if entry.count >= 5 {
            entry.count = 0;
            entry.until = now + 60 * 1000; // 锁 60 秒
        }
    }
    let log_user = if username.is_empty() {
        "?"
    } else {
        username.as_str()
    };
    log_op(&state, log_user, "登录失败", &format!("ip={}", ip), "拒绝");
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "用户名或密码错误" })),
    )
        .into_response()
}

async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let user = match auth_user(&state, &headers) {
        Some(u) => u,
        None => return unauthorized(),
    };
    if let Some(token) = cookie_token(&headers) {
        state.sessions.lock().unwrap().remove(&token);
    }
    log_op(&state, &user, "登出", "", "ok");
    (
        [(
            header::SET_COOKIE,
            "cxu_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        )],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

async fn get_config(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let user = match auth_user(&state, &headers) {
        Some(u) => u,
        None => return unauthorized(),
    };
    match read_config_file(&state.config_path) {
        Ok(config) => {
            log_op(&state, &user, "读取配置", "", "ok");
            Json(json!({ "config": mask_config(&config) })).into_response()
        }
        Err(err) => {
            log_op(&state, &user, "读取配置", &err, "失败");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("读取配置失败: {}", err) })),
            )
                .into_response()
        }
    }
}

async fn put_config(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth_user(&state, &headers) {
        Some(u) => u,
        None => return unauthorized(),
    };
    let incoming = match parse_json_body(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let original = match read_config_file(&state.config_path) {
        Ok(v) => v,
        Err(err) => {
            log_op(&state, &user, "写入配置", &err, "失败");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "ok": false, "errors": [format!("写入失败: {}", err)] })),
            )
                .into_response();
        }
    };
    let merged = deep_merge(&original, &incoming);
    let errors = validate_config(&merged);
    if !errors.is_empty() {
        log_op(&state, &user, "写入配置", &errors.join("; "), "校验失败");
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "errors": errors })),
        )
            .into_response();
    }

    // 变更字段清单（只记字段名，不记值）
    let changed = changed_top_level(&original, &merged);
    let content = format!(
        "{}\n",
        serde_json::to_string_pretty(&merged).unwrap_or_default()
    );
    if let Err(err) = write_config_file(&state.config_path, &content) {
        log_op(&state, &user, "写入配置", &err, "失败");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "errors": [format!("写入失败: {}", err)] })),
        )
            .into_response();
    }
    log_op(
        &state,
        &user,
        "写入配置",
        &format!(
            "变更: {}",
            if changed.is_empty() {
                "无".to_string()
            } else {
                changed.join(", ")
            }
        ),
        "ok",
    );

    let (restarted, note) = docker_restart(&state).await;
    let ready = poll_health(&state).await;
    log_op(
        &state,
        &user,
        "服务重启",
        &note,
        if restarted {
            if ready {
                "已就绪"
            } else {
                "超时未就绪"
            }
        } else {
            "跳过"
        },
    );
    Json(json!({ "ok": true, "changed": changed, "restarted": restarted, "note": note, "ready": ready }))
        .into_response()
}

async fn reset_config(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let user = match auth_user(&state, &headers) {
        Some(u) => u,
        None => return unauthorized(),
    };
    let template = load_template(&state.template_path);
    let errors = validate_config(&template);
    if !errors.is_empty() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                json!({ "ok": false, "errors": [format!("内置模板非法: {}", errors.join("; "))] }),
            ),
        )
            .into_response();
    }
    let content = format!(
        "{}\n",
        serde_json::to_string_pretty(&template).unwrap_or_default()
    );
    if let Err(err) = write_config_file(&state.config_path, &content) {
        log_op(&state, &user, "恢复默认配置", &err, "失败");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "errors": [err] })),
        )
            .into_response();
    }
    log_op(&state, &user, "恢复默认配置", "", "ok");

    let (restarted, note) = docker_restart(&state).await;
    let ready = poll_health(&state).await;
    log_op(
        &state,
        &user,
        "服务重启",
        &note,
        if restarted {
            if ready {
                "已就绪"
            } else {
                "超时未就绪"
            }
        } else {
            "跳过"
        },
    );
    Json(json!({ "ok": true, "restarted": restarted, "note": note, "ready": ready }))
        .into_response()
}

async fn proxy_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if auth_user(&state, &headers).is_none() {
        return unauthorized();
    }
    proxy_to(&state, "/api/status", reqwest::Method::GET, None).await
}

async fn proxy_health(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if auth_user(&state, &headers).is_none() {
        return unauthorized();
    }
    proxy_to(&state, "/api/health", reqwest::Method::GET, None).await
}

async fn proxy_messages(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if auth_user(&state, &headers).is_none() {
        return unauthorized();
    }
    proxy_to(&state, "/api/messages", reqwest::Method::GET, None).await
}

async fn relay(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let user = match auth_user(&state, &headers) {
        Some(u) => u,
        None => return unauthorized(),
    };
    let parsed = match parse_json_body(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let target = match parsed.get("target") {
        Some(v) if !v.is_null() => value_to_string(Some(v)),
        _ => "?".to_string(),
    };
    log_op(
        &state,
        &user,
        "relay 发送",
        &format!("target={}", target),
        "已转发",
    );
    proxy_to(&state, "/api/relay", reqwest::Method::POST, Some(parsed)).await
}

async fn status_stream(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if auth_user(&state, &headers).is_none() {
        return unauthorized();
    }
    let st = state.clone();
    let stream = async_stream::stream! {
        yield Ok::<Event, Infallible>(Event::default().comment("connected"));
        loop {
            let data = sse_push(&st).await;
            yield Ok::<Event, Infallible>(Event::default().data(data));
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    };
    Sse::new(stream).into_response()
}

async fn sse_push(state: &AppState) -> String {
    match state
        .client
        .get(format!("{}/api/status", state.api_url))
        .timeout(Duration::from_millis(1500))
        .send()
        .await
    {
        Ok(resp) => resp.text().await.unwrap_or_default().replace('\n', " "),
        Err(_) => json!({ "service_connected": false }).to_string(),
    }
}

async fn logs(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if auth_user(&state, &headers).is_none() {
        return unauthorized();
    }
    let ops = state.ops.lock().unwrap();
    let list: Vec<Value> = ops
        .iter()
        .take(200)
        .map(|o| {
            json!({
                "time": o.time,
                "user": o.user,
                "action": o.action,
                "detail": o.detail,
                "result": o.result,
            })
        })
        .collect();
    Json(json!({ "logs": list })).into_response()
}

async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response()
}

// ---------- 小工具 ----------
fn value_to_string(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(other) => cfg::js_to_string(other),
        None => "undefined".to_string(),
    }
}

fn random_token() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

fn changed_top_level(original: &Value, merged: &Value) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    if let Value::Object(m) = original {
        for k in m.keys() {
            if !keys.contains(k) {
                keys.push(k.clone());
            }
        }
    }
    if let Value::Object(m) = merged {
        for k in m.keys() {
            if !keys.contains(k) {
                keys.push(k.clone());
            }
        }
    }
    let mut changed = Vec::new();
    for k in keys {
        let a = original.get(&k).map(|v| v.to_string());
        let b = merged.get(&k).map(|v| v.to_string());
        if a != b {
            changed.push(k);
        }
    }
    changed
}

// ============================================================
// 入口
// ============================================================

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9090);
    let config_path = std::env::var("CXU_CONFIG_PATH").unwrap_or_default();
    let api_url = trim_trailing_slash(
        &std::env::var("CXU_API_URL").unwrap_or_else(|_| "http://127.0.0.1:8199".to_string()),
    );
    let health_url = std::env::var("CXU_HEALTH_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:6199/healthz".to_string());
    let container_name =
        std::env::var("CXU_CONTAINER_NAME").unwrap_or_else(|_| "chatroom-bridge-rust".to_string());
    let docker_enabled = std::env::var("CXU_DOCKER_ENABLED")
        .map(|v| v == "true")
        .unwrap_or(true);
    let template_path = std::env::var("CXU_TEMPLATE_PATH").unwrap_or_default();
    let admin_user = std::env::var("CXU_ADMIN_USER").unwrap_or_else(|_| "admin".to_string());
    let admin_password =
        std::env::var("CXU_ADMIN_PASSWORD").unwrap_or_else(|_| "admin".to_string());
    let public_dir = std::env::var("CXU_PUBLIC_DIR").unwrap_or_else(|_| "public".to_string());

    if config_path.is_empty() {
        eprintln!("[启动失败] 缺少环境变量 CXU_CONFIG_PATH（config.json 的绝对路径）");
        std::process::exit(1);
    }

    let state = Arc::new(AppState {
        config_path: config_path.clone(),
        api_url: api_url.clone(),
        health_url: health_url.clone(),
        container_name: container_name.clone(),
        docker_enabled,
        template_path,
        admin_user,
        admin_password: admin_password.clone(),
        session_ttl_ms: 8 * 60 * 60 * 1000,
        sessions: Mutex::new(HashMap::new()),
        failed: Mutex::new(HashMap::new()),
        ops: Mutex::new(Vec::new()),
        client: reqwest::Client::new(),
    });

    let app = Router::new()
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/config/reset", post(reset_config))
        .route("/api/status", get(proxy_status))
        .route("/api/health", get(proxy_health))
        .route("/api/messages", get(proxy_messages))
        .route("/api/relay", post(relay))
        .route("/api/status/stream", get(status_stream))
        .route("/api/logs", get(logs))
        .fallback_service(ServeDir::new(public_dir).not_found_service(get(not_found)))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(state);

    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[启动失败] 无法监听 0.0.0.0:{} —— {}", port, e);
            std::process::exit(1);
        }
    };

    println!("[cxu-chathub 控制台] http://0.0.0.0:{}", port);
    println!(
        "  配置文件: {}{}",
        config_path,
        if is_mountpoint(&config_path) {
            "（检测到挂载点，写入将原地覆盖）"
        } else {
            ""
        }
    );
    println!(
        "  服务 API: {} · 健康检查: {} · 重启目标: {}",
        api_url, health_url, container_name
    );
    if admin_password == "admin" {
        eprintln!("  ⚠ 当前使用默认密码 admin，生产环境请务必设置 CXU_ADMIN_PASSWORD！");
    }

    if let Err(e) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    {
        eprintln!("[服务异常退出] {}", e);
    }
}

fn trim_trailing_slash(s: &str) -> String {
    s.trim_end_matches('/').to_string()
}
