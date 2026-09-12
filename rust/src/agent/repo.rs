//! GitHub 仓库文档源（[`RepoSource`]）。
//!
//! 把 GitHub 上的 markdown 文档仓库（例如 mdBook 写的
//! <https://github.com/AlexanderjFraser/MinecraftDocs>，`src/` 下是 md 页）的
//! tarball 下载到本地缓存，再委托 [`LocalDocSource`] 检索——云端语料、本地索引、
//! 出处映射回站点 URL（mdBook 干净 URL / GitHub blob 地址）。
//!
//! 缓存与降级策略：
//! - 缓存目录 = `{cache_root}/{slug(repo)}`（`owner/name` → `owner-name`，
//!   repo 是完整 URL 时对整个串做 slug），`meta.json` 记录下载时刻；
//! - 距上次下载 < [`REFRESH_SECS`] 直接用缓存；过期则重新下载——tarball 完整
//!   拿到后先清空缓存目录再解压（旧版本文件不残留），成功后写 meta.json；
//! - 下载/解压失败：缓存目录里还有 meta.json（哪怕过期）就 warn 后用旧缓存；
//!   否则数据源定局为空（重启进程才会重试下载）；
//! - 所有失败路径一律 warn + 降级（旧缓存 or 空 vec），绝不 panic、错误不扩散。

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use tar::Archive;
use tokio::sync::OnceCell;
use tracing::{info, warn};

use super::local::LocalDocSource;
use super::{DocHit, DocumentSource};

/// 缓存元数据文件名（记录下载时间，用于 24h 刷新判断）。
const META_FILE: &str = "meta.json";

/// 缓存刷新间隔。
const REFRESH_SECS: u64 = 24 * 60 * 60;

/// 下载超时。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// 委托给本地检索的文档扩展名白名单。
const DOC_EXTENSIONS: &[&str] = &[".md", ".markdown", ".mdx", ".txt"];

/// 缓存元数据（`meta.json` 内容）：只记下载时刻，用于新鲜度判断。
#[derive(Debug, Serialize, Deserialize)]
struct CacheMeta {
    /// 下载完成时刻（Unix 秒）。
    downloaded_at: u64,
}

/// GitHub 仓库文档源。`repo` 为 `owner/name`（拼
/// `https://codeload.github.com/{repo}/tar.gz/refs/heads/{branch}`）或完整
/// tarball URL（自托管/测试用）。首次 [`DocumentSource::search`] 经 [`OnceCell`]
/// 惰性准备缓存（只跑一次，成功或最终失败都定局），之后复用内层检索器。
pub struct RepoSource {
    /// 数据源标识（命中结果标注来源用）。
    name: String,
    /// "owner/name" 或完整 tarball URL。
    repo: String,
    /// 分支名（repo 为 owner/name 时拼 codeload 地址用）。
    branch: String,
    /// 仓库内子目录（如 mdBook 的 `src`）；None = 整个仓库。
    subdir: Option<String>,
    /// 文档站点地址；None = 出处用 GitHub blob 地址。
    site_url: Option<String>,
    /// 索引扩展名白名单（后缀匹配，支持 `.zh.md` 这类双后缀）；空 = [`DOC_EXTENSIONS`]。
    extensions: Vec<String>,
    /// tarball 解压缓存目录。
    cache_dir: PathBuf,
    /// 下载客户端（总超时 [`DOWNLOAD_TIMEOUT`]）。
    client: reqwest::Client,
    /// 惰性构建的内层本地检索器；None = 准备失败定局（检索一律返回空列表）。
    inner: OnceCell<Option<LocalDocSource>>,
}

impl RepoSource {
    /// 构建失败仅可能是 reqwest 客户端初始化失败（极少见）。
    pub fn new(
        name: impl Into<String>,
        repo: impl Into<String>,
        branch: impl Into<String>,
        subdir: impl Into<String>,      // 空串 = 整个仓库
        site_url: impl Into<String>,    // 空串 = 用 GitHub blob 地址
        extensions: Vec<String>,        // 空 = 内置默认集（.md/.markdown/.mdx/.txt）
        cache_root: impl Into<PathBuf>,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(DOWNLOAD_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            // 自报身份；部分 CDN/网关对空 UA 拒绝
            .user_agent(concat!("cxu-chathub/", env!("CARGO_PKG_VERSION"), " (community doc bot)"))
            .build()?;
        let repo = repo.into();
        let subdir = subdir.into();
        let site_url = site_url.into();
        Ok(Self {
            name: name.into(),
            branch: branch.into(),
            subdir: (!subdir.is_empty()).then_some(subdir),
            site_url: (!site_url.is_empty()).then_some(site_url),
            extensions,
            cache_dir: cache_root.into().join(repo_slug(&repo)),
            client,
            repo,
            inner: OnceCell::new(),
        })
    }

    /// tarball 下载地址：repo 本身是 URL 时原样使用；否则拼 codeload 分支地址。
    fn tarball_url(&self) -> String {
        if self.repo.contains("://") {
            self.repo.clone()
        } else {
            format!(
                "https://codeload.github.com/{}/tar.gz/refs/heads/{}",
                self.repo, self.branch
            )
        }
    }

    /// 确保缓存可用并构建内层检索器。由 [`OnceCell`] 保证进程生命周期内至多执行
    /// 一次：成功与最终失败都定局，重启进程才重试。
    async fn prepare(&self) -> Option<LocalDocSource> {
        // 1) 缓存目录存在且 meta.json 的 downloaded_at 距今 < REFRESH_SECS → 直接用缓存。
        if self.cache_fresh() {
            return Some(self.build_inner());
        }
        // 2) 过期 / 无缓存 → 下载 → 清空缓存目录 → 解压 → 写 meta.json。
        match self.download_and_extract().await {
            Ok(()) => {
                self.write_meta();
                info!(
                    source = %self.name,
                    repo = %self.repo,
                    cache = %self.cache_dir.display(),
                    "GitHub 仓库语料缓存已刷新"
                );
                Some(self.build_inner())
            }
            // 3) 失败降级：还有旧缓存（含 meta.json，哪怕过期）就先用旧缓存；
            //    否则定局为 None（检索一律空列表，重启进程才会重试）。
            Err(err) => {
                if self.has_cached_snapshot() {
                    warn!(
                        source = %self.name,
                        repo = %self.repo,
                        error = %err,
                        "tarball 下载失败，降级使用旧缓存（可能过期；重启进程前不会重试）"
                    );
                    Some(self.build_inner())
                } else {
                    warn!(
                        source = %self.name,
                        repo = %self.repo,
                        error = %err,
                        "tarball 下载失败且无本地缓存，数据源保持为空（重启进程才会重试）"
                    );
                    None
                }
            }
        }
    }

    /// 下载 tarball 并解压到缓存目录。网络 / HTTP 非 2xx / 解压失败都收敛为
    /// `Err(String)`，由调用方 warn + 降级。清空缓存目录发生在 tarball 完整拿到
    /// 之后——网络与 HTTP 失败不会破坏旧缓存。
    async fn download_and_extract(&self) -> Result<(), String> {
        let url = self.tarball_url();
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|err| format!("请求失败: {err}"))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|err| format!("读取响应体失败: {err}"))?;
        // 解压是阻塞的磁盘活：tarball 已在内存，丢进 spawn_blocking 不卡异步线程。
        let cache_dir = self.cache_dir.clone();
        let extracted = tokio::task::spawn_blocking(move || extract_tarball(&bytes, &cache_dir))
            .await
            .map_err(|err| format!("解压任务异常结束: {err}"))?;
        extracted.map_err(|err| format!("解压失败: {err}"))
    }

    /// 读 meta.json；缺失 / 损坏一律 None。
    fn read_meta(&self) -> Option<CacheMeta> {
        let bytes = std::fs::read(self.cache_dir.join(META_FILE)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// 缓存是否新鲜：meta.json 可解析且 downloaded_at 距今 < REFRESH_SECS。
    fn cache_fresh(&self) -> bool {
        let Some(meta) = self.read_meta() else {
            return false;
        };
        unix_now().saturating_sub(meta.downloaded_at) < REFRESH_SECS
    }

    /// 是否还有可降级的旧缓存（缓存目录里有 meta.json，哪怕过期）。
    fn has_cached_snapshot(&self) -> bool {
        self.cache_dir.join(META_FILE).is_file()
    }

    /// 写 meta.json；失败只 warn（下次查询会当作过期重新下载，不影响本次结果）。
    fn write_meta(&self) {
        let meta = CacheMeta {
            downloaded_at: unix_now(),
        };
        match serde_json::to_string(&meta) {
            Ok(json) => {
                if let Err(err) = std::fs::write(self.cache_dir.join(META_FILE), json) {
                    warn!(
                        source = %self.name,
                        cache = %self.cache_dir.display(),
                        error = %err,
                        "缓存元数据写入失败，下次查询会重新下载"
                    );
                }
            }
            Err(err) => warn!(source = %self.name, error = %err, "缓存元数据序列化失败"),
        }
    }

    /// 用当前缓存目录构建内层本地检索器（subdir 非空时根 = cache_dir/subdir）。
    fn build_inner(&self) -> LocalDocSource {
        let root = match &self.subdir {
            Some(sub) => self.cache_dir.join(sub),
            None => self.cache_dir.clone(),
        };
        let extensions = if self.extensions.is_empty() {
            DOC_EXTENSIONS.iter().map(|ext| ext.to_string()).collect()
        } else {
            self.extensions.clone()
        };
        LocalDocSource::new(self.name.as_str(), root, extensions)
    }

    /// 把内层 [`LocalDocSource`] 的 `"{rel}:{start}-{end}"` 出处映射为可访问的
    /// 站点 URL：路径分隔符统一规范成 `/`，`.md`/`.markdown` 后缀去掉，行号后缀
    /// 丢弃，再对路径做百分号编码（空格 → `%20` 等——文件名可能含空格或中文，
    /// 如 RMS-Docs 的 `0. Home.md` → `https://docs.rms.net.cn/0.%20Home`）。
    /// site_url 非空 → `{site 去尾斜杠}/{rel}`；否则 → GitHub blob 地址。
    /// 没有 `:` 的 locator 原样映射路径部分。
    fn map_locator(&self, locator: &str) -> String {
        let normalized = locator.replace('\\', "/");
        let path_part = match normalized.split_once(':') {
            Some((path, _lines)) => path,
            None => normalized.as_str(),
        };
        let base = match &self.site_url {
            Some(site) => site.trim_end_matches('/').to_string(),
            // blob 地址是文件路径：保留分支名与完整文件名（含 `.zh.md` 这类双后缀）
            None => {
                return format!(
                    "https://github.com/{}/blob/{}/{}",
                    self.repo,
                    self.branch,
                    percent_encode_path(path_part)
                );
            }
        };
        let rel = strip_doc_extension(path_part);
        format!("{base}/{}", percent_encode_path(rel))
    }
}

/// 路径百分号编码：保留 RFC 3986 未保留字符与 `/` 分隔符，其余按 UTF-8 字节转义。
/// 文件名里的空格、中文等都能得到合法 URL（`0. Home` → `0.%20Home`）。
fn percent_encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                encoded.push(byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

#[async_trait]
impl DocumentSource for RepoSource {
    fn name(&self) -> &str {
        &self.name
    }

    async fn search(&self, query: &str, limit: usize) -> Vec<DocHit> {
        // OnceCell 保证缓存准备逻辑只跑一次（成功或最终失败都定局，重启进程才重试）。
        let Some(inner) = self.inner.get_or_init(|| self.prepare()).await else {
            return Vec::new();
        };
        // 检索委托内层本地源，出处映射回站点 URL（保证可溯源）。
        inner
            .search(query, limit)
            .await
            .into_iter()
            .map(|hit| DocHit {
                locator: self.map_locator(&hit.locator),
                title: hit.title,
                snippet: hit.snippet,
            })
            .collect()
    }
}

/// 解压 GitHub tarball 到 `dest`：路径 ≥2 层深度的剥掉首段（GitHub tarball 首层
/// 是 `{repo}-{短sha}/`）后 unpack；0/1 层深度的目录项跳过；链接等非常规项与含
/// `..` 的路径（防穿越）也跳过。解压前清空 `dest`（旧版本文件不能残留）。
fn extract_tarball(bytes: &[u8], dest: &Path) -> std::io::Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    std::fs::create_dir_all(dest)?;

    let gz = GzDecoder::new(bytes);
    let mut archive = Archive::new(gz);
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !matches!(
            entry.header().entry_type(),
            tar::EntryType::Regular | tar::EntryType::Directory | tar::EntryType::GNUSparse
        ) {
            continue; // 链接 / 设备等：文档仓库用不到，跳过
        }
        let path = entry.path()?.to_path_buf();
        if path.components().any(|c| matches!(c, Component::ParentDir)) {
            continue; // 防 tar 路径穿越
        }
        // 只看真实层级：忽略根目录 / `./` 前缀等非 Normal 组件
        let components: Vec<Component<'_>> = path
            .components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .collect();
        if components.len() < 2 {
            continue; // 0/1 层深度：tarball 首层目录本身
        }
        let target = dest.join(
            components[1..]
                .iter()
                .map(|c| c.as_os_str())
                .collect::<PathBuf>(),
        );
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entry.unpack(&target)?;
    }
    Ok(())
}

/// 缓存目录名 slug：字母数字与 `-`/`_` 保留，其余（`/`、`:`、`.` 等非法字符）
/// 替换为 `-`。`owner/name` → `owner-name`；repo 是完整 URL 时对整个串做 slug。
fn repo_slug(repo: &str) -> String {
    let slug: String = repo
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if slug.is_empty() {
        "repo".to_string() // 防御：空 repo 不把 cache_root 本身当缓存目录
    } else {
        slug
    }
}

/// 去掉文档扩展名后缀（`.markdown` / `.md`），无该后缀原样返回。
fn strip_doc_extension(path: &str) -> &str {
    path.strip_suffix(".markdown")
        .or_else(|| path.strip_suffix(".md"))
        .unwrap_or(path)
}

/// 当前 Unix 秒（时钟异常时退化为 0，只会多触发一次刷新，不出错）。
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::http::{header, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::Router;

    /// 测试语料：introduction.md 含 spawning，nested.md 含 villager（两词不重叠，
    /// 便于按词断言命中哪个文件）。
    const INTRO_MD: &str = "# Introduction\nSpawning rules for mobs live here.\n";
    const NESTED_MD: &str = "# Deep Page\nVillager trading hall notes live here.\n";

    /// mock 行为模式。
    #[derive(Clone, Copy)]
    enum MockMode {
        /// 返回真 tar.gz。
        Ok,
        /// 直接 500。
        ServerError,
    }

    struct MockState {
        mode: Mutex<MockMode>,
        /// /tar.gz 被请求的次数（断言是否真的触发下载）。
        hits: AtomicUsize,
        tarball: Vec<u8>,
    }

    impl MockState {
        fn set_mode(&self, mode: MockMode) {
            *self.mode.lock().unwrap() = mode;
        }
    }

    /// GET /tar.gz：计数 + 按模式应答（模拟 codeload）。
    async fn handle_tar_gz(State(state): State<Arc<MockState>>) -> Response {
        state.hits.fetch_add(1, Ordering::SeqCst);
        let mode = *state.mode.lock().unwrap();
        match mode {
            MockMode::Ok => (
                [(header::CONTENT_TYPE, "application/gzip")],
                state.tarball.clone(),
            )
                .into_response(),
            MockMode::ServerError => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }

    /// 起一个随机端口的 axum mock 当 codeload，返回基址与状态。
    async fn spawn_mock(mode: MockMode) -> (String, Arc<MockState>) {
        let state = Arc::new(MockState {
            mode: Mutex::new(mode),
            hits: AtomicUsize::new(0),
            tarball: build_tar_gz(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/tar.gz", axum::routing::get(handle_tar_gz))
            .with_state(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), state)
    }

    /// 现场构造真 tar.gz：首层目录 RepoXyz123（模拟 GitHub 的 `{repo}-{短sha}/`），
    /// 里面是 mdBook 风格的 src/ 页面。
    fn build_tar_gz() -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        let mut add = |path: &str, content: &str| {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(content.len() as u64);
            header.set_cksum();
            tar.append_data(&mut header, path, content.as_bytes()).unwrap();
        };
        add("RepoXyz123/src/introduction.md", INTRO_MD);
        add("RepoXyz123/src/deep/nested.md", NESTED_MD);
        let gz = tar.into_inner().unwrap();
        gz.finish().unwrap()
    }

    /// 建 RepoSource：repo 直接填 mock tarball 完整 URL（合约允许），site_url =
    /// mock 基址，subdir 按需（"src" / ""）。同一 cache_root 两次构建共享缓存目录。
    fn make_source(base: &str, subdir: &str, cache_root: &Path) -> RepoSource {
        RepoSource::new(
            "mc-docs",
            format!("{base}/tar.gz"),
            "main",
            subdir,
            base,
            vec![],
            cache_root,
        )
        .unwrap()
    }

    /// 手写一份过期 meta.json（downloaded_at = 25h 前）。
    fn write_stale_meta(cache_dir: &Path) {
        let stale = unix_now() - REFRESH_SECS - 60 * 60;
        let json = serde_json::json!({ "downloaded_at": stale }).to_string();
        std::fs::create_dir_all(cache_dir).unwrap();
        std::fs::write(cache_dir.join(META_FILE), json).unwrap();
    }

    /// 下载 → 解压（剥首层目录名）→ 委托检索 → 出处映射回站点 URL。
    #[tokio::test]
    async fn repo_downloads_extracts_and_maps_site_url() {
        let (base, state) = spawn_mock(MockMode::Ok).await;
        let cache = tempfile::tempdir().unwrap();
        let source = make_source(&base, "src", cache.path());

        let hits = source.search("spawning", 10).await;

        assert_eq!(state.hits.load(Ordering::SeqCst), 1); // 真的下载过一次
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Introduction"); // 标题取 markdown 标题行
        assert_eq!(hits[0].locator, format!("{base}/introduction")); // mdBook 干净 URL
        assert!(hits[0].snippet.contains("Spawning"));
        assert!(source.cache_dir.join(META_FILE).is_file()); // meta 已写
    }

    /// subdir 生效：内层根 = cache/src，src 下的 nested 页也能查到且出处正确。
    #[tokio::test]
    async fn repo_subdir_scopes_inner_root() {
        let (base, _state) = spawn_mock(MockMode::Ok).await;
        let cache = tempfile::tempdir().unwrap();
        let source = make_source(&base, "src", cache.path());

        let nested = source.search("villager", 10).await;
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].locator, format!("{base}/deep/nested"));

        let intro = source.search("spawning", 10).await;
        assert_eq!(intro.len(), 1);
        assert_eq!(intro[0].locator, format!("{base}/introduction"));
    }

    /// 缓存复用：meta 新鲜时后续检索不再触发下载（mock 计数不涨）。
    #[tokio::test]
    async fn repo_fresh_cache_reuses_without_download() {
        let (base, state) = spawn_mock(MockMode::Ok).await;
        let cache = tempfile::tempdir().unwrap();
        let source = make_source(&base, "src", cache.path());

        assert!(!source.search("spawning", 10).await.is_empty());
        assert!(!source.search("villager", 10).await.is_empty());
        assert!(!source.search("introduction", 10).await.is_empty());
        assert_eq!(state.hits.load(Ordering::SeqCst), 1); // 只有第一次触发下载
    }

    /// 过期刷新：meta 超 24h → 重新下载（计数涨）；刷新后 meta 新鲜 → 再用缓存。
    #[tokio::test]
    async fn repo_stale_meta_triggers_redownload() {
        let (base, state) = spawn_mock(MockMode::Ok).await;
        let cache = tempfile::tempdir().unwrap();
        let source = make_source(&base, "src", cache.path());

        // 预置过期 meta：缓存目录存在但超 24h → 触发重新下载
        write_stale_meta(&source.cache_dir);
        assert!(!source.search("spawning", 10).await.is_empty());
        assert_eq!(state.hits.load(Ordering::SeqCst), 1);

        // 刷新后 meta 新鲜：再建一个实例走缓存，不再下载
        let second = make_source(&base, "src", cache.path());
        assert!(!second.search("spawning", 10).await.is_empty());
        assert_eq!(state.hits.load(Ordering::SeqCst), 1);
    }

    /// 500 + 有旧缓存（meta.json 哪怕过期）→ warn 后用旧缓存，仍返回命中。
    #[tokio::test]
    async fn repo_server_error_falls_back_to_stale_cache() {
        let (base, state) = spawn_mock(MockMode::Ok).await;
        let cache = tempfile::tempdir().unwrap();
        let first = make_source(&base, "src", cache.path());
        assert!(!first.search("spawning", 10).await.is_empty());
        assert_eq!(state.hits.load(Ordering::SeqCst), 1);

        // 服务端开始 500，并把 meta 时间戳改过期 → 走「下载失败 → 旧缓存降级」
        state.set_mode(MockMode::ServerError);
        write_stale_meta(&first.cache_dir);
        let second = make_source(&base, "src", cache.path());
        let hits = second.search("spawning", 10).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].locator, format!("{base}/introduction"));
        assert_eq!(state.hits.load(Ordering::SeqCst), 2); // 试过下载但失败
    }

    /// 500 + 无缓存 → 空 vec，不 panic；失败定局（OnceCell），重启进程前不重试。
    #[tokio::test]
    async fn repo_server_error_without_cache_returns_empty() {
        let (base, state) = spawn_mock(MockMode::ServerError).await;
        let cache = tempfile::tempdir().unwrap();
        let source = make_source(&base, "src", cache.path());

        assert!(source.search("spawning", 10).await.is_empty());
        assert!(source.search("villager", 10).await.is_empty());
        assert_eq!(state.hits.load(Ordering::SeqCst), 1); // 只试了一次
    }

    /// map_locator 纯函数：site_url 版 / GitHub blob 版 / 反斜杠路径 / 无行号后缀。
    #[test]
    fn repo_map_locator_variants() {
        // site_url 版：去尾斜杠 + 去 md/markdown 后缀（mdBook 干净 URL）
        let site = RepoSource::new(
            "docs",
            "owner/name",
            "main",
            "",
            "https://minecraftdocs.dev/",
            vec![],
            std::env::temp_dir(),
        )
        .unwrap();
        assert_eq!(
            site.map_locator("introduction.md:1-9"),
            "https://minecraftdocs.dev/introduction"
        );
        assert_eq!(
            site.map_locator("deep/nested.markdown:2-3"),
            "https://minecraftdocs.dev/deep/nested"
        );

        // GitHub blob 版（site_url 空）：文件路径完整保留（分支名 + 扩展名）
        let blob = RepoSource::new(
            "docs",
            "owner/name",
            "main",
            "",
            "",
            vec![],
            std::env::temp_dir(),
        )
        .unwrap();
        assert_eq!(
            blob.map_locator("src/deep/nested.md:2-3"),
            "https://github.com/owner/name/blob/main/src/deep/nested.md"
        );

        // 反斜杠路径（Windows 内层产物）：统一成 / 再映射
        assert_eq!(
            blob.map_locator("src\\deep\\nested.md:4-6"),
            "https://github.com/owner/name/blob/main/src/deep/nested.md"
        );

        // 双后缀文件名（techmc-wiki/articles 的双语文件）：blob 路径原样保留
        assert_eq!(
            blob.map_locator("BlockUpdate/更新的概念.zh.md:1-9"),
            format!(
                "https://github.com/owner/name/blob/main/BlockUpdate/{}.zh.md",
                "%E6%9B%B4%E6%96%B0%E7%9A%84%E6%A6%82%E5%BF%B5"
            )
        );

        // 无行号后缀：整串当路径映射，扩展名照常去掉
        assert_eq!(
            site.map_locator("readme"),
            "https://minecraftdocs.dev/readme"
        );
        assert_eq!(
            site.map_locator("readme.md"),
            "https://minecraftdocs.dev/readme"
        );

        // 路径百分号编码：文件名含空格（RMS-Docs 的 "0. Home.md"）→ %20
        assert_eq!(
            site.map_locator("0. Home.md:1-9"),
            "https://minecraftdocs.dev/0.%20Home"
        );
        // 非 ASCII（中文文件名）按 UTF-8 字节编码，'/' 分隔符保留
        assert_eq!(
            site.map_locator("教程/第一章.md:1-2"),
            "https://minecraftdocs.dev/%E6%95%99%E7%A8%8B/%E7%AC%AC%E4%B8%80%E7%AB%A0"
        );
    }
}
