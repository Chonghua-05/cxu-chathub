//! 语料检索调试工具：对任意本地目录跑 LocalDocSource 检索，打印命中与出处。
//! 用途：接入真实语料前的质量评测（agent-design.md §6 里程碑）。
//!
//! 用法：cargo run --release --example doc_query -- <语料目录> <查询词> [扩展名白名单，如 ".java,.md"]

use chatroom_bridge::agent::local::LocalDocSource;
use chatroom_bridge::agent::DocumentSource as _;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("用法: doc_query <语料目录> <查询词> [扩展名白名单，如 .java,.md]");
        std::process::exit(2);
    }
    let root = &args[1];
    let query = &args[2];
    let extensions: Vec<String> = args
        .get(3)
        .map(|raw| raw.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();

    let started = std::time::Instant::now();
    let source = LocalDocSource::new("corpus", root, extensions);
    let hits = source.search(query, 5).await;
    let elapsed = started.elapsed();

    println!("查询「{query}」于 {root} —— {} 个命中，耗时 {elapsed:.1?}\n", hits.len());
    for (index, hit) in hits.iter().enumerate() {
        println!("{}. {}  [{}]", index + 1, hit.title, hit.locator);
        for line in hit.snippet.lines().take(6) {
            println!("    {line}");
        }
        println!();
    }
}
