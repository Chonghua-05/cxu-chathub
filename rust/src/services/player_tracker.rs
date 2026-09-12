//! MC 玩家上下线追踪：轮询状态 API + 状态驱动防抖。
//!
//! 从旧框架插件的 `_poll_once` 逻辑移植（对应 Python `player_tracker.py`）：
//! 只有连续 N 次轮询都看到同一变化才确认事件，避免玩家瞬断/重连造成的
//! 事件抖动。
//!
//! 防抖语义（对齐 Python `_maybe_confirm` / `poll_once`）：
//! - 同一变化要连续观察 `debounce_count` 轮才确认并回调 `on_event`；
//! - `on_event` 返回 false 或 panic → `failed+1`，pending 保留，下轮重试；
//! - 已确认玩家短暂掉线又回来（抖动）→ 撤销 pending（误报抵消）；
//! - 未确认的 pending 玩家从快照消失 → 跨轮次残留清理；
//! - 空服务端且不在本轮快照里 → 从 confirmed 里移除。

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Local, TimeZone};
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use serde_json::Value;

/// 默认状态 API 地址。
pub const STATUS_API: &str = "https://status.example.com/api/qqbot/status";

const KIND_ONLINE: &str = "online";
const KIND_OFFLINE: &str = "offline";

/// 事件类型（Python 里是 `"online" | "offline"` 字符串）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    Online,
    Offline,
}

/// 玩家上下线事件。
#[derive(Debug, Clone)]
pub struct PlayerEvent {
    pub event_type: EventType,
    pub server: String,
    pub player: String,
    pub timestamp: f64,
}

impl PlayerEvent {
    /// `🎮 {player} → {server_name or server} 上线  HH:MM:SS`
    /// （本地时区；下线为 🚪 / 下线）。
    pub fn format_message(&self, server_names: Option<&HashMap<String, String>>) -> String {
        let (icon, action) = match self.event_type {
            EventType::Online => ("🎮", "上线"),
            EventType::Offline => ("🚪", "下线"),
        };
        let display_server = server_names
            .and_then(|names| names.get(&self.server))
            .cloned()
            .unwrap_or_else(|| self.server.clone());
        format!(
            "{icon} {} → {display_server} {action}  {}",
            self.player,
            local_clock(self.timestamp)
        )
    }
}

/// 玩家事件回调：返回 true 表示推送成功（失败保留 pending，下轮重试）。
pub type OnEvent = Arc<dyn Fn(PlayerEvent) -> BoxFuture<'static, bool> + Send + Sync>;

/// 追踪计数（对齐 Python `stats` 字典）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrackerStats {
    pub online: u64,
    pub offline: u64,
    pub failed: u64,
}

/// 对比在线玩家快照，产生带防抖的上下线事件。
pub struct PlayerTracker {
    client: reqwest::Client,
    status_api: String,
    debounce: u32,
    on_event: Option<OnEvent>,
    /// 已确认在线：server -> players
    confirmed: Mutex<HashMap<String, HashSet<String>>>,
    /// 待确认变化：(player, server, kind) -> 连续观察次数
    pending: Mutex<HashMap<(String, String, String), u32>>,
    online: AtomicU64,
    offline: AtomicU64,
    failed: AtomicU64,
}

impl std::fmt::Debug for PlayerTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlayerTracker")
            .field("status_api", &self.status_api)
            .field("debounce", &self.debounce)
            .field("has_on_event", &self.on_event.is_some())
            .field("stats", &self.stats())
            .finish()
    }
}

impl PlayerTracker {
    pub fn new(
        debounce_count: u32,
        on_event: Option<OnEvent>,
        status_api: impl Into<String>,
    ) -> Result<Self, reqwest::Error> {
        // Python: aiohttp.ClientTimeout(total=15, sock_connect=5)
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self {
            client,
            status_api: status_api.into(),
            // Python: max(1, int(debounce_count))
            debounce: debounce_count.max(1),
            on_event,
            confirmed: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            online: AtomicU64::new(0),
            offline: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        })
    }

    pub fn stats(&self) -> TrackerStats {
        TrackerStats {
            online: self.online.load(Ordering::Relaxed),
            offline: self.offline.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
        }
    }

    /// 拉取当前在线玩家 {server: {player}}；失败返回 None。
    pub async fn fetch_raw(&self) -> Option<HashMap<String, HashSet<String>>> {
        let response = match self.client.get(&self.status_api).send().await {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!("状态 API请求失败: {err}");
                return None;
            }
        };
        let status = response.status().as_u16();
        if status != 200 {
            tracing::warn!("状态 API返回 HTTP {status}");
            return None;
        }
        // Python: resp.json(content_type=None) —— 不校验 Content-Type
        let data: Value = match response.json().await {
            Ok(data) => data,
            Err(err) => {
                tracing::warn!("状态 API响应不是合法 JSON: {err}");
                return None;
            }
        };
        if !data.is_object() {
            return None;
        }
        let mut result = HashMap::new();
        let servers = data
            .get("servers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for server in &servers {
            let Some(entry) = server.as_object() else {
                continue; // 非 dict 条目
            };
            // name = str(server.get("server_name") or "unknown")
            let name = entry
                .get("server_name")
                .filter(|value| is_truthy(value))
                .map_or_else(|| "unknown".to_string(), stringify);
            // players = {str(p) for p in (server.get("online_players") or []) if p}
            let players: HashSet<String> = entry
                .get("online_players")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter(|player| is_truthy(player))
                        .map(stringify)
                        .collect()
                })
                .unwrap_or_default();
            if !players.is_empty() {
                // 无玩家的服务端跳过；同名服务端后者覆盖前者（Python dict 语义）
                result.insert(name, players);
            }
        }
        Some(result)
    }

    /// 跑一轮比对，返回本轮确认的事件。
    pub async fn poll_once(&self) -> Vec<PlayerEvent> {
        let Some(current) = self.fetch_raw().await else {
            return Vec::new();
        };
        self.diff_once(current).await
    }

    /// 快照差分 + 状态驱动防抖（对齐 Python `poll_once` 的逐服务端流程）。
    async fn diff_once(&self, current: HashMap<String, HashSet<String>>) -> Vec<PlayerEvent> {
        let mut events = Vec::new();

        // all_servers = current ∪ confirmed（排序保证跨服务端事件顺序确定）
        let mut all_servers: Vec<String> = current.keys().cloned().collect();
        all_servers.extend(self.lock_confirmed().keys().cloned());
        all_servers.sort();
        all_servers.dedup();

        for server in all_servers {
            let players_now = current.get(&server).cloned().unwrap_or_default();
            let confirmed = self.lock_confirmed().get(&server).cloned().unwrap_or_default();

            for player in sorted_difference(&players_now, &confirmed) {
                if let Some(event) =
                    self.maybe_confirm(EventType::Online, &server, player).await
                {
                    events.push(event);
                }
            }
            for player in sorted_difference(&confirmed, &players_now) {
                if let Some(event) =
                    self.maybe_confirm(EventType::Offline, &server, player).await
                {
                    events.push(event);
                }
            }

            // 误报抵消 + 清理跨轮次残留
            let mut pending = self.lock_pending();
            for player in players_now.intersection(&confirmed) {
                pending.remove(&(player.clone(), server.clone(), KIND_OFFLINE.to_string()));
            }
            for player in confirmed.difference(&players_now) {
                pending.remove(&(player.clone(), server.clone(), KIND_ONLINE.to_string()));
            }
            let stale: Vec<(String, String, String)> = pending
                .keys()
                .filter(|key| key.1 == server)
                .cloned()
                .collect();
            for key in stale {
                let (player, _, kind) = &key;
                let remove = match kind.as_str() {
                    KIND_ONLINE => !players_now.contains(player),
                    _ => !confirmed.contains(player),
                };
                if remove {
                    pending.remove(&key);
                }
            }
        }

        // 空服务端且本轮快照里也没有 → 从 confirmed 里移除
        self.lock_confirmed()
            .retain(|server, players| !players.is_empty() || current.contains_key(server));

        events
    }

    /// 状态驱动防抖：连续 `debounce` 轮看到同一变化才确认（对齐 Python
    /// `_maybe_confirm`）。
    async fn maybe_confirm(
        &self,
        event_type: EventType,
        server: &str,
        player: String,
    ) -> Option<PlayerEvent> {
        let kind = match event_type {
            EventType::Online => KIND_ONLINE,
            EventType::Offline => KIND_OFFLINE,
        };
        let key = (player.clone(), server.to_string(), kind.to_string());
        let count = {
            let mut pending = self.lock_pending();
            let count = pending.entry(key.clone()).or_insert(0);
            *count += 1;
            *count
        };
        if count < self.debounce {
            return None;
        }

        let event = PlayerEvent {
            event_type,
            server: server.to_string(),
            player,
            timestamp: unix_now(),
        };
        if let Some(on_event) = &self.on_event {
            // 回调 panic 视同失败：保留 pending，下轮再试
            let pushed = AssertUnwindSafe(on_event(event.clone()))
                .catch_unwind()
                .await
                .unwrap_or(false);
            if !pushed {
                self.failed.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    "推送玩家事件失败（保留待重试）: {} {} @ {}",
                    kind,
                    event.player,
                    event.server
                );
                return None;
            }
        }

        self.lock_pending().remove(&key);
        match event_type {
            EventType::Online => {
                self.lock_confirmed()
                    .entry(server.to_string())
                    .or_default()
                    .insert(event.player.clone());
                self.online.fetch_add(1, Ordering::Relaxed);
            }
            EventType::Offline => {
                // Python: self._confirmed.get(server, set()).discard(player)
                // （服务器不存在时是空集合，弃置即可，不新建条目）
                if let Some(players) = self.lock_confirmed().get_mut(server) {
                    players.remove(&event.player);
                }
                self.offline.fetch_add(1, Ordering::Relaxed);
            }
        }
        Some(event)
    }

    /// 当前确认的在线快照：{server: sorted players}。
    pub fn snapshot(&self) -> HashMap<String, Vec<String>> {
        self.lock_confirmed()
            .iter()
            .map(|(server, players)| {
                let mut list: Vec<String> = players.iter().cloned().collect();
                list.sort();
                (server.clone(), list)
            })
            .collect()
    }

    /// 宽松恢复（stringify 键值，容忍垃圾）——对齐 Python `restore`：
    /// 快照不是对象时静默忽略；值是列表时逐元素字符串化（null 跳过），
    /// 其它形状按空集合容忍。整体替换现有 confirmed。
    pub fn restore(&self, snapshot: &Value) {
        let Some(map) = snapshot.as_object() else {
            return; // Python: if isinstance(snapshot, dict)
        };
        let mut confirmed = HashMap::new();
        for (key, value) in map {
            let players = match value {
                Value::Array(items) => items
                    .iter()
                    .filter(|item| !matches!(item, Value::Null))
                    .map(stringify)
                    .collect(),
                _ => HashSet::new(),
            };
            confirmed.insert(key.clone(), players);
        }
        *self.lock_confirmed() = confirmed;
    }

    fn lock_confirmed(&self) -> MutexGuard<'_, HashMap<String, HashSet<String>>> {
        self.confirmed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_pending(&self) -> MutexGuard<'_, HashMap<(String, String, String), u32>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Python `sorted(left - right)`。
fn sorted_difference(left: &HashSet<String>, right: &HashSet<String>) -> Vec<String> {
    let mut items: Vec<String> = left.difference(right).cloned().collect();
    items.sort();
    items
}

/// Python 的 falsy 判定（None / False / 0 / "" / 空容器 → false）。
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `str(v)` 的 JSON 文本化近似：字符串原样，其余按 JSON 文本。
fn stringify(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 本地时区 HH:MM:SS（对齐 `datetime.fromtimestamp(ts).strftime("%H:%M:%S")`）。
fn local_clock(timestamp: f64) -> String {
    let secs = timestamp.trunc() as i64;
    let nanos = ((timestamp - timestamp.trunc()) * 1_000_000_000.0).clamp(0.0, 999_999_999.0) as u32;
    Local
        .timestamp_opt(secs, nanos)
        .single()
        .map(|time| time.format("%H:%M:%S").to_string())
        .unwrap_or_default()
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    #[derive(Clone)]
    struct StatusMock {
        status: Arc<Mutex<Value>>,
    }

    impl StatusMock {
        fn set(&self, value: Value) {
            *self.status.lock().unwrap() = value;
        }
    }

    async fn status_handler(State(mock): State<StatusMock>) -> Response {
        Json(mock.status.lock().unwrap().clone()).into_response()
    }

    async fn spawn_status_mock() -> (String, StatusMock) {
        let mock = StatusMock {
            status: Arc::new(Mutex::new(json!({ "servers": [] }))),
        };
        let app = Router::new()
            .route("/api/qqbot/status", get(status_handler))
            .route("/fail", get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }))
            .route("/garbage", get(|| async { Json(json!([1, 2, 3])) }))
            .route("/junk", get(|| async { "不是 JSON" }))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), mock)
    }

    fn tracker(status_api: &str, debounce: u32, on_event: Option<OnEvent>) -> PlayerTracker {
        PlayerTracker::new(debounce, on_event, status_api.to_string()).unwrap()
    }

    fn server(name: &str, players: &[&str]) -> Value {
        json!({
            "server_name": name,
            "online_players": players.iter().map(|player| json!(player)).collect::<Vec<_>>(),
        })
    }

    fn described(events: &[PlayerEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| format!("{:?}:{}", event.event_type, event.player))
            .collect()
    }

    // ---------- fetch_raw ----------

    #[tokio::test]
    async fn fetch_raw_parses_and_skips_empty() {
        let (base, mock) = spawn_status_mock().await;
        let t = tracker(&format!("{base}/api/qqbot/status"), 2, None);
        mock.set(json!({ "servers": [
            { "server_name": "生存", "online_players": ["alice", "bob", "", 0, null] },
            { "server_name": "", "online_players": ["x"] },
            { "online_players": ["y"] },                    // 无名字 → unknown（覆盖前一个 unknown）
            { "server_name": "空服", "online_players": [] }, // 无玩家 → 跳过
            "junk",                                         // 非 dict → 跳过
            { "server_name": "空服2" },                     // 无玩家字段 → 跳过
        ]}));

        let raw = t.fetch_raw().await.unwrap();
        assert_eq!(raw.len(), 2);
        let expected: HashSet<String> = ["alice", "bob"].iter().map(|s| s.to_string()).collect();
        assert_eq!(raw["生存"], expected);
        let unknown: HashSet<String> = ["y".to_string()].into_iter().collect();
        assert_eq!(raw["unknown"], unknown);
    }

    #[tokio::test]
    async fn fetch_raw_failure_paths_return_none() {
        let (base, _mock) = spawn_status_mock().await;
        // HTTP 500
        let t = tracker(&format!("{base}/fail"), 2, None);
        assert!(t.fetch_raw().await.is_none());
        // JSON 数组：不是 dict
        let t = tracker(&format!("{base}/garbage"), 2, None);
        assert!(t.fetch_raw().await.is_none());
        // 非 JSON
        let t = tracker(&format!("{base}/junk"), 2, None);
        assert!(t.fetch_raw().await.is_none());
        assert!(t.fetch_raw().await.is_none());
    }

    // ---------- 差分与防抖 ----------

    #[tokio::test]
    async fn diff_produces_online_then_offline_in_order() {
        let (base, mock) = spawn_status_mock().await;
        let status = format!("{base}/api/qqbot/status");
        let t = tracker(&status, 1, None);

        mock.set(json!({ "servers": [server("srv-b", &["p2", "p1"]), server("srv-a", &["q1"])] }));
        let events = t.poll_once().await;
        // 服务端按名字排序，玩家按名字排序
        assert_eq!(described(&events), ["Online:q1", "Online:p1", "Online:p2"]);
        assert_eq!(
            t.stats(),
            TrackerStats {
                online: 3,
                offline: 0,
                failed: 0
            }
        );

        mock.set(json!({ "servers": [server("srv-a", &[]), server("srv-b", &["p1"])] }));
        let events = t.poll_once().await;
        assert_eq!(described(&events), ["Offline:q1", "Offline:p2"]);
        assert_eq!(
            t.stats(),
            TrackerStats {
                online: 3,
                offline: 2,
                failed: 0
            }
        );
        // srv-a 空了且 fetch_raw 会丢弃无玩家的服务端（不在 current）→ 条目被清理
        let mut expected = HashMap::new();
        expected.insert("srv-b".to_string(), vec!["p1".to_string()]);
        assert_eq!(t.snapshot(), expected);
    }

    #[tokio::test]
    async fn debounce_requires_consecutive_polls() {
        let (base, mock) = spawn_status_mock().await;
        let t = tracker(&format!("{base}/api/qqbot/status"), 2, None);

        mock.set(json!({ "servers": [server("srv", &["p"])] }));
        let events = t.poll_once().await;
        assert!(events.is_empty());
        assert!(t.snapshot().is_empty());
        assert_eq!(t.stats(), TrackerStats::default());

        // 第二次连续观察到同一变化 → 确认
        let events = t.poll_once().await;
        assert_eq!(described(&events), ["Online:p"]);
        assert_eq!(t.snapshot()["srv"], vec!["p".to_string()]);
        assert_eq!(t.stats().online, 1);
    }

    #[tokio::test]
    async fn blip_cancels_pending_transition() {
        let (base, mock) = spawn_status_mock().await;
        let t = tracker(&format!("{base}/api/qqbot/status"), 2, None);
        t.restore(&json!({ "srv": ["p"] }));

        // p 掉线：pending offline 计 1，未确认
        mock.set(json!({ "servers": [server("srv", &[])] }));
        assert!(t.poll_once().await.is_empty());
        assert_eq!(t.snapshot()["srv"], vec!["p".to_string()]);
        assert_eq!(t.stats().offline, 0);

        // p 回来（抖动）→ 撤销 pending
        mock.set(json!({ "servers": [server("srv", &["p"])] }));
        assert!(t.poll_once().await.is_empty());
        assert_eq!(t.snapshot()["srv"], vec!["p".to_string()]);
        assert_eq!(t.stats().offline, 0);

        // 再次掉线：重新计数，仍需第二观察
        mock.set(json!({ "servers": [server("srv", &[])] }));
        assert!(t.poll_once().await.is_empty());
        assert_eq!(t.snapshot()["srv"], vec!["p".to_string()]);

        // 第二次连续观察 → 确认下线；空服务端且不在快照 → 从 confirmed 清除
        mock.set(json!({ "servers": [server("srv", &[])] }));
        let events = t.poll_once().await;
        assert_eq!(described(&events), ["Offline:p"]);
        assert!(t.snapshot().is_empty());
        assert_eq!(t.stats().offline, 1);
    }

    #[tokio::test]
    async fn empty_servers_are_cleaned_from_confirmed() {
        let (base, mock) = spawn_status_mock().await;
        let t = tracker(&format!("{base}/api/qqbot/status"), 1, None);
        t.restore(&json!({ "srv-1": ["p"], "srv-2": ["q"] }));

        // srv-1 从快照消失 → 全员下线后条目被清理；srv-2 仍在快照 → 保留
        mock.set(json!({ "servers": [server("srv-2", &["q"])] }));
        let events = t.poll_once().await;
        assert_eq!(described(&events), ["Offline:p"]);

        let snapshot = t.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot["srv-2"], vec!["q".to_string()]);
        assert_eq!(t.stats().offline, 1);
    }

    // ---------- on_event 回调 ----------

    #[tokio::test]
    async fn on_event_false_keeps_pending_for_retry() {
        let (base, mock) = spawn_status_mock().await;
        let flag = Arc::new(AtomicBool::new(false));
        let flag_for_event = flag.clone();
        let on_event: OnEvent = Arc::new(move |_event: PlayerEvent| {
            let flag = flag_for_event.clone();
            Box::pin(async move { flag.load(AtomicOrdering::SeqCst) }) as BoxFuture<'static, bool>
        });
        let t = tracker(&format!("{base}/api/qqbot/status"), 1, Some(on_event));

        mock.set(json!({ "servers": [server("srv", &["p"])] }));

        // 推送失败：pending 保留、failed+1、不写 confirmed
        let events = t.poll_once().await;
        assert!(events.is_empty());
        assert!(t.snapshot().is_empty());
        assert_eq!(
            t.stats(),
            TrackerStats {
                online: 0,
                offline: 0,
                failed: 1
            }
        );

        // 下一轮重试成功
        flag.store(true, AtomicOrdering::SeqCst);
        let events = t.poll_once().await;
        assert_eq!(described(&events), ["Online:p"]);
        assert_eq!(
            t.stats(),
            TrackerStats {
                online: 1,
                offline: 0,
                failed: 1
            }
        );
        assert_eq!(t.snapshot()["srv"], vec!["p".to_string()]);
    }

    #[tokio::test]
    async fn on_event_panic_counts_as_failure() {
        let (base, mock) = spawn_status_mock().await;
        let flag = Arc::new(AtomicBool::new(false));
        let flag_for_event = flag.clone();
        let on_event: OnEvent = Arc::new(move |_event: PlayerEvent| {
            let flag = flag_for_event.clone();
            Box::pin(async move {
                if !flag.load(AtomicOrdering::SeqCst) {
                    panic!("推送炸了");
                }
                true
            }) as BoxFuture<'static, bool>
        });
        let t = tracker(&format!("{base}/api/qqbot/status"), 1, Some(on_event));

        mock.set(json!({ "servers": [server("srv", &["p"])] }));
        assert!(t.poll_once().await.is_empty());
        assert_eq!(t.stats().failed, 1);
        assert!(t.snapshot().is_empty());

        flag.store(true, AtomicOrdering::SeqCst);
        let events = t.poll_once().await;
        assert_eq!(described(&events), ["Online:p"]);
        assert_eq!(t.stats().online, 1);
    }

    // ---------- snapshot / restore ----------

    #[tokio::test]
    async fn snapshot_restore_roundtrip() {
        let (base, mock) = spawn_status_mock().await;
        let t = tracker(&format!("{base}/api/qqbot/status"), 1, None);
        mock.set(json!({ "servers": [server("生存", &["alice", "bob"]), server("空岛", &["carol"])] }));
        t.poll_once().await;

        let snapshot = t.snapshot();
        let value = serde_json::to_value(&snapshot).unwrap();
        let restored = tracker(&format!("{base}/api/qqbot/status"), 1, None);
        restored.restore(&value);
        assert_eq!(restored.snapshot(), snapshot);
    }

    #[test]
    fn restore_is_loose_and_replaces() {
        let t = tracker("http://127.0.0.1:1/none", 2, None);
        t.restore(&json!({
            "srv": ["a", 5, null, true],
            "bad": 42,      // 非列表 → 空集合容忍
            "worse": null,  // null → 空集合
            "empty": [],
        }));
        let snapshot = t.snapshot();
        assert_eq!(snapshot.len(), 4);
        assert_eq!(
            snapshot["srv"],
            ["5".to_string(), "a".to_string(), "true".to_string()]
        );
        assert!(snapshot["bad"].is_empty());
        assert!(snapshot["worse"].is_empty());
        assert!(snapshot["empty"].is_empty());

        // 整体替换，不是合并
        t.restore(&json!({ "other": ["x"] }));
        let snapshot = t.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot["other"], ["x".to_string()]);

        // 快照不是对象 → 忽略
        t.restore(&json!([1, 2, 3]));
        assert_eq!(t.snapshot().len(), 1);
    }

    // ---------- format_message ----------

    #[test]
    fn format_message_renders_icon_action_and_clock() {
        let online = PlayerEvent {
            event_type: EventType::Online,
            server: "s1".to_string(),
            player: "Alice".to_string(),
            timestamp: 0.0,
        };
        let message = online.format_message(None);
        assert!(message.contains("🎮"), "{message}");
        assert!(message.contains("上线"), "{message}");
        assert!(message.contains("Alice → s1"), "{message}");
        let clock = &message[message.len() - 8..];
        assert_clock(clock);

        let offline = PlayerEvent {
            event_type: EventType::Offline,
            server: "s1".to_string(),
            player: "Bob".to_string(),
            timestamp: 86_399.9,
        };
        let mut names = HashMap::new();
        names.insert("s1".to_string(), "生存服".to_string());
        let message = offline.format_message(Some(&names));
        assert!(message.contains("🚪"), "{message}");
        assert!(message.contains("下线"), "{message}");
        assert!(message.contains("Bob → 生存服"), "{message}");
        assert!(!message.contains("Alice"));
        assert_clock(&message[message.len() - 8..]);

        // 未命中的服务端名回落原始名
        let message = offline.format_message(None);
        assert!(message.contains("Bob → s1"), "{message}");
    }

    fn assert_clock(clock: &str) {
        let bytes = clock.as_bytes();
        assert_eq!(clock.len(), 8, "{clock}");
        assert_eq!(bytes[2], b':', "{clock}");
        assert_eq!(bytes[5], b':', "{clock}");
        for (index, byte) in bytes.iter().enumerate() {
            if index != 2 && index != 5 {
                assert!(byte.is_ascii_digit(), "{clock}");
            }
        }
    }
}
