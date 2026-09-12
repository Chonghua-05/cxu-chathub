//! 真实联网验证（默认忽略，手动跑）：
//! `cargo test --test repo_real -- --ignored --nocapture`
//!
//! 从 GitHub 拉取真实语料仓库 → 缓存 → 检索，验证云端源的端到端行为与 locator 映射。
//! 覆盖：techmc-wiki/articles（GTMC 文章库，!tmc 的语料）与 Conflux-Union/RMS-Docs（!doc）。

use chatroom_bridge::agent::repo::RepoSource;
use chatroom_bridge::agent::DocumentSource as _;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "需要真实网络；手动运行验证云端源"]
async fn real_gtmc_articles_repo_search() {
    let cache_root = tempfile::tempdir().unwrap();
    // site_url 留空 → 出处用 GitHub blob 地址（文件路径完整保留）
    let source = RepoSource::new(
        "gtmc-articles",
        "techmc-wiki/articles",
        "main",
        "",
        "",
        vec![".zh.md".to_string(), ".md".to_string()],
        cache_root.path(),
    )
    .unwrap();

    // 中文查询：GTMC 文章是双语（.zh.md / .en.md），CJK bigram 直接命中中文版
    let hits = source.search("方块更新", 5).await;
    println!("中文查询「方块更新」命中 {} 条：", hits.len());
    for hit in &hits {
        let preview: String = hit.snippet.chars().take(60).collect();
        println!("  - {} [{}]\n    {preview}…", hit.title, hit.locator);
    }
    assert!(!hits.is_empty(), "「方块更新」应命中 BlockUpdate 相关文章");
    assert!(
        hits.iter().all(|h| h
            .locator
            .starts_with("https://github.com/techmc-wiki/articles/blob/main/")),
        "locator 应为 GitHub blob 文件地址"
    );

    // 二次查询走缓存（不重复下载），只验证不 panic 且结果稳定
    let again = source.search("更新抑制", 5).await;
    let _ = again;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "需要真实网络；手动运行验证云端源"]
async fn real_rms_docs_repo_search() {
    let cache_root = tempfile::tempdir().unwrap();
    let source = RepoSource::new(
        "rms-docs",
        "Conflux-Union/RMS-Docs",
        "master",
        "",
        "https://docs.rms.net.cn",
        vec![],
        cache_root.path(),
    )
    .unwrap();

    // 中文语料：CJK bigram 直接命中（RMS-Docs 是中文文档，无需翻译层）
    let hits = source.search("服务器 规定", 5).await;
    println!("中文查询「服务器 规定」命中 {} 条：", hits.len());
    for hit in &hits {
        println!("  - {} [{}]", hit.title, hit.locator);
    }
    assert!(!hits.is_empty(), "中文查询应有命中");
    // 文件名带空格：出处必须百分号编码成站点可访问 URL
    assert!(
        hits.iter()
            .all(|h| h.locator.starts_with("https://docs.rms.net.cn/")
                && !h.locator.contains(' ')),
        "locator 应映射到 docs.rms.net.cn 且空格已编码：{:?}",
        hits.iter().map(|h| &h.locator).collect::<Vec<_>>()
    );
}
