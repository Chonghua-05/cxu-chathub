//! 真实联网验证（默认忽略，手动跑）：
//! `cargo test --test wiki_real -- --ignored --nocapture`
//!
//! 直连 zh.minecraft.wiki 的 MediaWiki API，验证两段式调用
//! （list=search → prop=extracts）在真实站点上的行为。

use chatroom_bridge::agent::http::MediaWikiSource;
use chatroom_bridge::agent::DocumentSource as _;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "需要真实网络；手动运行验证 Minecraft Wiki 接入"]
async fn real_zh_minecraft_wiki_search() {
    let source = MediaWikiSource::new("minecraft-wiki", "https://zh.minecraft.wiki/api.php").unwrap();

    // 问句清洗生效：整句问题收敛成检索词
    let hits = source.search("活塞是怎么工作的？", 3).await;
    println!("查询「活塞是怎么工作的？」命中 {} 条：", hits.len());
    for hit in &hits {
        let preview: String = hit.snippet.chars().take(80).collect();
        println!("  - {} [{}]\n    {preview}…", hit.title, hit.locator);
    }
    assert!(!hits.is_empty(), "活塞条目应当能查到");
    assert!(
        hits.iter()
            .all(|h| h.locator.starts_with("https://zh.minecraft.wiki/wiki/")),
        "locator 应为 zh.minecraft.wiki 条目 URL"
    );
    // 整页摘录（非 400 字摘要）：正常条目的 extract 应明显长于搜索摘要
    assert!(
        hits.iter().any(|h| h.snippet.chars().count() > 400),
        "应有条目拿到整页级摘录"
    );
}
