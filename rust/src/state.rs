//! 持久化状态：去重表、读游标、refresh_token、已播报的版本更新。
//! 写入策略：临时文件 + fsync + 原子 rename；读取损坏时丢弃重建，不让坏文件卡死启动。
//! `state.json` 格式与 Python 版逐字节兼容（v0.5 新增字段向后兼容，缺省为空）。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use tracing::{error, warn};

/// 已播报版本条目的保留上限（防 state.json 无限膨胀；远超实际更新频率）。
const MAX_ANNOUNCED_PATCHES: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateSnapshot {
    pub forwarded_count: usize,
    pub last_read_message_id: i64,
    pub has_refresh_token: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct StateFile {
    #[serde(default)]
    forwarded: IndexMap<String, i64>,
    #[serde(default)]
    last_read_message_id: i64,
    #[serde(default)]
    refresh_token: String,
    /// v0.5 版本更新播报：已播报的补丁 id → 标题（判断「新版本」的依据）
    #[serde(default)]
    announced_patches: IndexMap<String, String>,
}

struct Inner {
    forwarded: IndexMap<String, i64>,
    last_read_message_id: i64,
    refresh_token: String,
    announced_patches: IndexMap<String, String>,
}

/// 去重表 + 读游标 + refresh_token 的小型 JSON 存储。
///
/// 去重表是必需的：chatroom 服务端明确不去重，同一 source_message_id
/// 重复提交会产生新消息。
pub struct StateStore {
    path: PathBuf,
    max_forwarded: usize,
    inner: Mutex<Inner>,
}

impl StateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::with_max_forwarded(path, 2000)
    }

    pub fn with_max_forwarded(path: impl Into<PathBuf>, max_forwarded: usize) -> Self {
        let path = path.into();
        let mut inner = Inner {
            forwarded: IndexMap::new(),
            last_read_message_id: 0,
            refresh_token: String::new(),
            announced_patches: IndexMap::new(),
        };
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<StateFile>(&bytes) {
                Ok(file) => {
                    inner.forwarded = file.forwarded;
                    inner.last_read_message_id = file.last_read_message_id;
                    inner.refresh_token = file.refresh_token;
                    inner.announced_patches = file.announced_patches;
                }
                Err(err) => warn!("状态文件损坏，已重建: {} ({err})", path.display()),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => warn!("状态文件读取失败，已重建: {} ({err})", path.display()),
        }
        Self {
            path,
            max_forwarded,
            inner: Mutex::new(inner),
        }
    }

    /// 临时文件 + fsync + 原子 rename，避免半截文件。
    fn flush(&self, state: &StateFile) {
        let directory = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if let Err(err) = std::fs::create_dir_all(directory) {
            error!("状态目录创建失败 {}: {err}", directory.display());
            return;
        }
        let result = tempfile::Builder::new()
            .prefix(".state-")
            .suffix(".tmp")
            .tempfile_in(directory)
            .and_then(|mut tmp| {
                use std::io::Write as _;
                let payload = serde_json::to_string_pretty(state)
                    .map_err(|err| std::io::Error::other(err.to_string()))?;
                tmp.write_all(payload.as_bytes())?;
                tmp.as_file().sync_all()?;
                tmp.persist(&self.path)
                    .map_err(|err| std::io::Error::other(err.to_string()))
            });
        if let Err(err) = result {
            error!("状态文件写入失败 {}: {err}", self.path.display());
        }
    }

    /// 从内存态拼出待落盘的 StateFile（各 setter 共用）。
    fn snapshot_file(guard: &Inner) -> StateFile {
        StateFile {
            forwarded: guard.forwarded.clone(),
            last_read_message_id: guard.last_read_message_id,
            refresh_token: guard.refresh_token.clone(),
            announced_patches: guard.announced_patches.clone(),
        }
    }

    // --- 去重表 ---
    pub fn forwarded_id(&self, source_message_id: &str) -> Option<i64> {
        self.inner
            .lock()
            .ok()?
            .forwarded
            .get(source_message_id)
            .copied()
    }

    pub fn already_forwarded(&self, source_message_id: &str) -> bool {
        self.forwarded_id(source_message_id).is_some()
    }

    pub fn mark_forwarded(&self, source_message_id: &str, chatroom_message_id: i64) {
        let mut guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        guard
            .forwarded
            .insert(source_message_id.to_string(), chatroom_message_id);
        while guard.forwarded.len() > self.max_forwarded {
            guard.forwarded.shift_remove_index(0);
        }
        self.flush(&Self::snapshot_file(&guard));
    }

    // --- 读游标 ---
    pub fn last_read_message_id(&self) -> i64 {
        self.inner
            .lock()
            .map(|guard| guard.last_read_message_id)
            .unwrap_or(0)
    }

    pub fn set_last_read_message_id(&self, message_id: i64) {
        let state = match self.inner.lock() {
            Ok(mut guard) => {
                guard.last_read_message_id = message_id;
                Self::snapshot_file(&guard)
            }
            Err(_) => return,
        };
        self.flush(&state);
    }

    // --- refresh_token ---
    pub fn refresh_token(&self) -> String {
        self.inner
            .lock()
            .map(|guard| guard.refresh_token.clone())
            .unwrap_or_default()
    }

    pub fn set_refresh_token(&self, token: String) {
        let state = match self.inner.lock() {
            Ok(mut guard) => {
                guard.refresh_token = token;
                Self::snapshot_file(&guard)
            }
            Err(_) => return,
        };
        self.flush(&state);
    }

    // --- 已播报的版本更新（v0.5 播报去重） ---
    pub fn patch_announced(&self, patch_id: &str) -> bool {
        self.inner
            .lock()
            .map(|guard| guard.announced_patches.contains_key(patch_id))
            .unwrap_or(false)
    }

    pub fn has_announced_patches(&self) -> bool {
        self.inner
            .lock()
            .map(|guard| !guard.announced_patches.is_empty())
            .unwrap_or(false)
    }

    /// 标记一个补丁条目已播报（含标题便于排查）；超过保留上限时淘汰最早的。
    pub fn mark_patch_announced(&self, patch_id: &str, title: &str) {
        let state = match self.inner.lock() {
            Ok(mut guard) => {
                guard
                    .announced_patches
                    .insert(patch_id.to_string(), title.to_string());
                while guard.announced_patches.len() > MAX_ANNOUNCED_PATCHES {
                    guard.announced_patches.shift_remove_index(0);
                }
                Self::snapshot_file(&guard)
            }
            Err(_) => return,
        };
        self.flush(&state);
    }

    pub fn snapshot(&self) -> StateSnapshot {
        match self.inner.lock() {
            Ok(guard) => StateSnapshot {
                forwarded_count: guard.forwarded.len(),
                last_read_message_id: guard.last_read_message_id,
                has_refresh_token: !guard.refresh_token.is_empty(),
            },
            Err(_) => StateSnapshot {
                forwarded_count: 0,
                last_read_message_id: 0,
                has_refresh_token: false,
            },
        }
    }
}
