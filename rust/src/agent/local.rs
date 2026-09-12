//! 本地文档/源码目录检索（[`LocalDocSource`]）。
//!
//! 供配置驱动的文档查询系统使用：命令在配置里绑定若干数据源，本模块负责
//! 本地语料（文档 / 源码副本）那一半。设计要点：
//!
//! - **惰性索引**：首次 [`DocumentSource::search`] 时经 [`tokio::sync::OnceCell`]
//!   构建一次，之后复用；刷新语料靠重启进程。目录不存在不算致命——Docker 卷
//!   可能后挂载，但重启前不会重扫（索引保持为空，检索一律返回空列表）。
//! - **内存与语料规模解耦**：索引只存 (相对路径, 起始行, 行数, 标题行, 词频向量)
//!   轻量元数据，正文不入内存；snippet 在查询时按需读文件。
//! - **打分**：查询词在分块内的词频求和，词命中标题行再 ×3；
//!   无命中即空列表——「查不到就说查不到，不编」（roadmap 非目标约束）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::sync::OnceCell;
use tracing::{debug, info, warn};

use super::{DocHit, DocumentSource};

/// 单文件大小上限 20MB：超过直接跳过，防止把构建产物 / 大二进制拖进索引。
const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;

/// 代码文件固定分块窗口：60 行，无重叠。
const CODE_WINDOW_LINES: usize = 60;

/// snippet 最多展示的行数（查询时读文件截取）。
const SNIPPET_MAX_LINES: usize = 12;

/// markdown 类扩展名：按标题行（`^#{1,6}\s`）分节；其余扩展名按固定行窗口分块。
const MARKDOWN_EXTS: &[&str] = &[".md", ".txt"];

/// `extensions` 为空时的内置默认扩展名集合。
const DEFAULT_EXTS: &[&str] = &[
    ".md",
    ".txt",
    ".java",
    ".kt",
    ".py",
    ".json",
    ".toml",
    ".yaml",
    ".yml",
    ".cfg",
    ".properties",
];

/// 索引时直接跳过的目录名（构建产物 / 依赖 / 版本库；隐藏目录另行按 `.` 前缀跳过）。
const SKIPPED_DIRS: &[&str] = &["target", "node_modules", ".git"];

/// 索引条目：只存轻量元数据，正文不入内存（snippet 查询时按需读文件）。
struct Chunk {
    /// 相对 root 的路径（统一用 `/` 分隔；Windows 的 std::fs 也接受该分隔符）。
    rel_path: String,
    /// 起始行（1-based，含）。
    start_line: usize,
    /// 结束行（1-based，含）；行数 = end - start + 1。
    end_line: usize,
    /// 标题：markdown 取标题行去掉前导 `#`，代码取文件名。
    title: String,
    /// 词频向量：分词 -> 出现次数。
    tf: HashMap<String, usize>,
}

/// 内存索引：条目列表。构建失败（根目录缺失等）时保持为空。
struct Index {
    chunks: Vec<Chunk>,
}

/// 本地文档/源码目录检索。首次 search 时惰性建索引（tokio OnceCell，只建一次，
/// 刷新靠重启进程）；索引只存 (相对路径, 起始行, 行数, 标题行, 词频向量)，
/// snippet 在查询时按需读文件——内存占用与语料规模解耦。
pub struct LocalDocSource {
    name: String,
    root: PathBuf,
    extensions: Vec<String>,
    index: OnceCell<Index>,
}

impl LocalDocSource {
    /// `extensions` 为空时用内置默认集：`.md/.txt/.java/.kt/.py/.json/.toml/
    /// .yaml/.yml/.cfg/.properties`。扩展名统一小写化，缺前导点时自动补齐
    /// （容错配置写成 `"md"` 的情况）。
    pub fn new(
        name: impl Into<String>,
        root: impl Into<std::path::PathBuf>,
        extensions: Vec<String>,
    ) -> Self {
        let extensions = if extensions.is_empty() {
            DEFAULT_EXTS.iter().map(|ext| ext.to_string()).collect()
        } else {
            extensions
                .into_iter()
                .map(|mut ext| {
                    ext.make_ascii_lowercase();
                    if !ext.starts_with('.') {
                        ext.insert(0, '.');
                    }
                    ext
                })
                .collect()
        };
        Self {
            name: name.into(),
            root: root.into(),
            extensions,
            index: OnceCell::new(),
        }
    }
}

#[async_trait]
impl DocumentSource for LocalDocSource {
    fn name(&self) -> &str {
        &self.name
    }

    async fn search(&self, query: &str, limit: usize) -> Vec<DocHit> {
        if limit == 0 {
            return Vec::new();
        }
        // 首次查询惰性建索引：阻塞的文件遍历丢进 spawn_blocking，避免卡死异步线程。
        // OnceCell 保证只建一次（哪怕结果是空索引——失败也不重试，刷新靠重启进程）。
        let index = self
            .index
            .get_or_init(|| {
                let root = self.root.clone();
                let extensions = self.extensions.clone();
                async move {
                    match tokio::task::spawn_blocking(move || build_index(&root, &extensions)).await
                    {
                        Ok(index) => index,
                        Err(err) => {
                            warn!(error = %err, "本地文档索引构建任务异常，索引保持为空（重启进程前不会重试）");
                            Index { chunks: Vec::new() }
                        }
                    }
                }
            })
            .await;

        // 查询分词：去重后逐词计分（同一词在查询里重复出现不重复计分）。
        let mut terms = tokenize(query);
        terms.sort();
        terms.dedup();
        if terms.is_empty() {
            return Vec::new();
        }

        // 打分：Σ 查询词在分块内的词频；词命中标题行（含标题文本子串）再 ×3。
        let mut scored: Vec<(u64, &Chunk)> = index
            .chunks
            .iter()
            .filter_map(|chunk| {
                let title_lower = chunk.title.to_lowercase();
                let mut score = 0u64;
                for term in &terms {
                    if let Some(tf) = chunk.tf.get(term) {
                        let weight = if title_lower.contains(term.as_str()) {
                            3
                        } else {
                            1
                        };
                        score += *tf as u64 * weight;
                    }
                }
                (score > 0).then_some((score, chunk))
            })
            .collect();
        // 排序：分数降序，同分按路径、起始行升序（保证可复现）。
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.rel_path.cmp(&b.1.rel_path))
                .then_with(|| a.1.start_line.cmp(&b.1.start_line))
        });
        scored.truncate(limit);

        scored
            .into_iter()
            .map(|(_, chunk)| DocHit {
                title: chunk.title.clone(),
                locator: format!("{}:{}-{}", chunk.rel_path, chunk.start_line, chunk.end_line),
                snippet: read_snippet(&self.root, chunk),
            })
            .collect()
    }
}

/// 查询时按需读文件取 snippet：从分块起始行起最多 [`SNIPPET_MAX_LINES`] 行。
/// 文件缺失 / 不可读 / 行数变化都退化为空串，绝不 panic。
fn read_snippet(root: &Path, chunk: &Chunk) -> String {
    let Ok(content) = std::fs::read_to_string(root.join(&chunk.rel_path)) else {
        return String::new();
    };
    let take = SNIPPET_MAX_LINES.min(chunk.end_line.saturating_sub(chunk.start_line) + 1);
    content
        .lines()
        .skip(chunk.start_line.saturating_sub(1))
        .take(take)
        .collect::<Vec<_>>()
        .join("\n")
}

/// 建索引入口：根目录缺失 / 非目录时 warn 一次并返回空索引。
fn build_index(root: &Path, extensions: &[String]) -> Index {
    if !root.is_dir() {
        warn!(
            root = %root.display(),
            "本地文档根目录不存在或不是目录，索引保持为空（目录可能后挂载；重启进程前不会重扫）"
        );
        return Index { chunks: Vec::new() };
    }
    let mut files = 0usize;
    let mut chunks = Vec::new();
    walk_dir(root, "", extensions, &mut chunks, &mut files);
    info!(
        files,
        chunks = chunks.len(),
        root = %root.display(),
        "本地文档索引构建完成"
    );
    Index { chunks }
}

/// 递归遍历目录：跳过隐藏项（`.` 前缀）、`target`/`node_modules`/`.git` 与符号链接。
fn walk_dir(
    dir: &Path,
    rel_prefix: &str,
    extensions: &[String],
    chunks: &mut Vec<Chunk>,
    files: &mut usize,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        debug!(dir = %dir.display(), "读取目录失败，跳过");
        return;
    };
    let mut entries: Vec<std::fs::DirEntry> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name()); // 按名字稳定遍历
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // 隐藏文件 / 隐藏目录（含 .git）
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let child_rel = join_rel(rel_prefix, &name);
        if file_type.is_dir() {
            if SKIPPED_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk_dir(&entry.path(), &child_rel, extensions, chunks, files);
        } else if file_type.is_file() {
            index_file(&entry.path(), &child_rel, &name, extensions, chunks, files);
        }
        // 符号链接等其它类型：跳过（防环路，避免重复计数）
    }
}

fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/// 索引单个文件：扩展名白名单 + 20MB 上限 + UTF-8 校验，然后按类型分块。
fn index_file(
    path: &Path,
    rel_path: &str,
    file_name: &str,
    extensions: &[String],
    chunks: &mut Vec<Chunk>,
    files: &mut usize,
) {
    let lower = file_name.to_ascii_lowercase();
    if !extensions.iter().any(|ext| lower.ends_with(ext.as_str())) {
        return; // 不在扩展名白名单内
    }
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() <= MAX_FILE_BYTES => {}
        Ok(meta) => {
            debug!(file = %rel_path, size = meta.len(), "文件超过 20MB，跳过索引");
            return;
        }
        Err(_) => return,
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        debug!(file = %rel_path, "非 UTF-8 文本（多半是二进制），跳过索引");
        return;
    };
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return; // 空文件没有可检索内容
    }
    *files += 1;
    let markdown_like = MARKDOWN_EXTS.iter().any(|ext| lower.ends_with(ext));
    if markdown_like {
        // markdown / 纯文本：按标题行分节，节包含其标题行；第一个标题之前为前言。
        for (start, end, heading) in markdown_sections(&lines) {
            let title = match heading {
                Some(no) => lines[no - 1].trim_start_matches('#').trim().to_string(),
                None => file_name.to_string(), // 前言：尚无标题行，退用文件名
            };
            push_chunk(&lines[start - 1..end], rel_path, start, title, chunks);
        }
    } else {
        // 代码等其它文本：固定 60 行窗口，无重叠。
        for start in (1..=lines.len()).step_by(CODE_WINDOW_LINES) {
            let end = (start + CODE_WINDOW_LINES - 1).min(lines.len());
            push_chunk(
                &lines[start - 1..end],
                rel_path,
                start,
                file_name.to_string(),
                chunks,
            );
        }
    }
}

/// 把一段行（`lines` 的 1-based 起始行为 `start_line`）收进索引：算词频向量。
fn push_chunk(
    lines: &[&str],
    rel_path: &str,
    start_line: usize,
    title: String,
    chunks: &mut Vec<Chunk>,
) {
    let mut tf: HashMap<String, usize> = HashMap::new();
    for token in tokenize(&lines.join("\n")) {
        *tf.entry(token).or_insert(0) += 1;
    }
    chunks.push(Chunk {
        rel_path: rel_path.to_string(),
        start_line,
        end_line: start_line + lines.len() - 1,
        title,
        tf,
    });
}

/// markdown 分节：`^#{1,6}\s` 为界，节包含其标题行；返回 (起始行, 结束行, 标题行号)。
/// 第一个标题之前的前言节 heading 为 None。
fn markdown_sections(lines: &[&str]) -> Vec<(usize, usize, Option<usize>)> {
    let mut sections = Vec::new();
    let mut cur_start = 1usize;
    let mut cur_heading: Option<usize> = None;
    for (idx, line) in lines.iter().enumerate() {
        let line_no = idx + 1;
        if is_heading_line(line) {
            if line_no > cur_start {
                sections.push((cur_start, line_no - 1, cur_heading));
            }
            cur_start = line_no;
            cur_heading = Some(line_no);
        }
    }
    sections.push((cur_start, lines.len(), cur_heading));
    sections
}

/// 标题行判定：行首 1..=6 个 `#` 后跟空白（等价正则 `^#{1,6}\s`）。
fn is_heading_line(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut hashes = 0;
    while hashes < 6 && hashes < bytes.len() && bytes[hashes] == b'#' {
        hashes += 1;
    }
    hashes > 0 && hashes < bytes.len() && (bytes[hashes] == b' ' || bytes[hashes] == b'\t')
}

/// 是否 CJK 表意文字（汉字主平面 + 扩展A + 兼容区）。
fn is_cjk(ch: char) -> bool {
    matches!(ch, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' | '\u{F900}'..='\u{FAFF}')
}

/// 分词：小写化后，连续 ASCII 字母数字为词；连续 CJK 字符切重叠二元组（bigram），
/// 孤立单字（前后都不是 CJK）按单字成词。无停用词表——保持简单。
/// 注意：不做子词切分，camelCase 标识符（`retractArm`）是整词 `retractarm`。
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut word = String::new(); // ASCII 词缓冲
    let mut cjk: Vec<char> = Vec::new(); // CJK 连续段缓冲
    let lower = text.to_lowercase();
    for ch in lower.chars() {
        if ch.is_ascii_alphanumeric() {
            flush_cjk(&mut cjk, &mut tokens);
            word.push(ch);
        } else if is_cjk(ch) {
            flush_word(&mut word, &mut tokens);
            cjk.push(ch);
        } else {
            flush_word(&mut word, &mut tokens);
            flush_cjk(&mut cjk, &mut tokens);
        }
    }
    flush_word(&mut word, &mut tokens);
    flush_cjk(&mut cjk, &mut tokens);
    tokens
}

fn flush_word(word: &mut String, tokens: &mut Vec<String>) {
    if !word.is_empty() {
        tokens.push(std::mem::take(word));
    }
}

/// 连续 CJK 段落盘：长度 1 时收单字，否则收全部重叠二元组。
fn flush_cjk(cjk: &mut Vec<char>, tokens: &mut Vec<String>) {
    if cjk.len() == 1 {
        tokens.push(cjk[0].to_string());
    }
    for pair in cjk.windows(2) {
        tokens.push(pair.iter().collect());
    }
    cjk.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建测试语料：
    /// - `guide.md`：两个标题节（第一节标题与正文含「活塞」，第二节含「末影」）
    /// - `src/PistonBlock.java`：代码文件，方法跨行包含检索词 retract
    /// - `.hidden/secret.md`：隐藏目录，必须被跳过
    /// - `notes.rst`：非白名单扩展名，必须被跳过
    /// - `target/debug/junk.md`：被排除目录，必须被跳过
    fn write_file(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write_file(
            &root.join("guide.md"),
            "# 活塞与红石\n活塞推动方块向上运动。\n\n## 末地传送门\n末影之眼可以定位要塞。\n",
        );
        write_file(
            &root.join("src/PistonBlock.java"),
            "public class PistonBlock {\n    /** 活塞推动方块后回缩 (retract the arm). */\n    \
             public void pushBlocks() {\n        extendArm();\n        retractArm();\n    }\n}\n",
        );
        write_file(&root.join(".hidden/secret.md"), "活塞的秘密文档。\n");
        write_file(&root.join("notes.rst"), "活塞标题\n=====\n");
        write_file(&root.join("target/debug/junk.md"), "活塞构建产物。\n");
        (dir, root)
    }

    /// markdown 按标题分节：分数排序（标题 ×3 加成）、locator 行区间（1-based）、
    /// 标题取标题行、snippet 含命中行。
    #[tokio::test]
    async fn local_source_md_heading_chunks_score_and_locator() {
        let (_dir, root) = fixture();
        let source = LocalDocSource::new("local-docs", &root, vec![".md".into(), ".java".into()]);
        let hits = source.search("活塞 末影", 10).await;

        // 命中 3 块：guide.md 第一节（活塞 tf=2 ×3 标题加成 = 6）、
        // guide.md 第二节（末影 tf=1）、PistonBlock.java 注释（活塞 tf=1）。
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].title, "活塞与红石"); // 标题来自标题行，去掉 #
        assert_eq!(hits[0].locator, "guide.md:1-3"); // 1-based 行区间
        assert!(hits[0].snippet.starts_with("# 活塞与红石"));
        assert!(hits[0].snippet.contains("活塞推动方块向上运动。"));

        // 同分（1 = 1）时按路径升序：guide.md 在 src/ 之前。
        assert_eq!(hits[1].title, "末地传送门");
        assert_eq!(hits[1].locator, "guide.md:4-5");
        assert!(hits[1].snippet.contains("末影之眼可以定位要塞。"));

        assert_eq!(hits[2].title, "PistonBlock.java");
        assert_eq!(hits[2].locator, "src/PistonBlock.java:1-7");
    }

    /// 代码文件：标题 = 文件名，不足 60 行整文件一个窗口，snippet 含命中行。
    #[tokio::test]
    async fn local_source_code_window_title_and_snippet() {
        let (_dir, root) = fixture();
        let source =
            LocalDocSource::new("local-mc-source", &root, vec![".md".into(), ".java".into()]);
        let hits = source.search("retract", 10).await;

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "PistonBlock.java"); // 代码块标题 = 文件名
        assert_eq!(hits[0].locator, "src/PistonBlock.java:1-7"); // 单窗口，1-based
        assert!(hits[0].snippet.contains("retractArm();")); // 命中行在 snippet 里
    }

    /// CJK 二元组：查询「活塞」命中含「活塞推动方块」的行。
    #[tokio::test]
    async fn local_source_cjk_bigram_matches() {
        let (_dir, root) = fixture();
        let source = LocalDocSource::new("docs", &root, vec![".md".into()]);
        let hits = source.search("活塞", 10).await;

        assert_eq!(hits.len(), 1); // 第二节与 .java 均无「活塞」或不在白名单
        assert!(hits[0].snippet.contains("活塞推动方块"));
    }

    /// 隐藏目录、target 目录、非白名单扩展名都必须被跳过。
    #[tokio::test]
    async fn local_source_skips_hidden_and_unlisted_extensions() {
        let (_dir, root) = fixture();
        let source = LocalDocSource::new("docs", &root, vec![".md".into(), ".java".into()]);
        let hits = source.search("活塞", 10).await;

        assert_eq!(hits.len(), 2); // guide.md 第一节 + java 注释
        for hit in &hits {
            assert!(
                !hit.locator.contains("secret.md"),
                "隐藏目录必须跳过: {}",
                hit.locator
            );
            assert!(
                !hit.locator.contains("junk.md"),
                "target 目录必须跳过: {}",
                hit.locator
            );
            assert!(
                !hit.locator.contains("notes.rst"),
                "非白名单扩展必须跳过: {}",
                hit.locator
            );
        }
    }

    /// limit 生效；无命中的查询返回空列表。
    #[tokio::test]
    async fn local_source_limit_and_no_match() {
        let (_dir, root) = fixture();
        let source = LocalDocSource::new("docs", &root, vec![".md".into(), ".java".into()]);

        let hits = source.search("活塞", 1).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].locator, "guide.md:1-3"); // 标题加成的最高分优先

        assert!(source.search("完全不存在的词汇", 10).await.is_empty());
    }

    /// 根目录不存在：不 panic，返回空列表（索引保持为空，重启前不会重扫）。
    #[tokio::test]
    async fn local_source_missing_root_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("not-mounted-yet");
        let source = LocalDocSource::new("later", &missing, vec![]);
        assert!(source.search("活塞", 10).await.is_empty());
    }

    /// extensions 为空 → 启用内置默认集（含 .cfg）。
    #[tokio::test]
    async fn local_source_default_extensions_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write_file(&root.join("server.cfg"), "motd=活塞服务器欢迎你\n");
        let source = LocalDocSource::new("cfg", &root, vec![]);
        let hits = source.search("活塞", 10).await;

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "server.cfg");
        assert_eq!(hits[0].locator, "server.cfg:1-1");
    }
}
