//! 本地调试：对 config.json 里注册的 agent 技能跑一次问答（检索 + LLM），打印回答。
//! 不经 QQ / 路由，直接构造 DispatchCtx 调 handler，等价于群里发 `!<skill> <问题>`。
//!
//! 用法：cargo run --release --example query_skill -- <config.json> <skill> <问题> [<skill> <问题> ...]
//! 例：  ... -- config.json wiki "守卫者是什么" docs "活塞最多能推动多少方块"

use std::sync::Arc;

use async_trait::async_trait;
use chatroom_bridge::agent::build_skills;
use chatroom_bridge::config::AgentConfig;
use chatroom_bridge::router::{
    CommandHandler, DispatchCtx, Hub, InboundMessage, ReplySink, Source,
};

struct PrintSink;
#[async_trait]
impl ReplySink for PrintSink {
    async fn send_text(&self, text: &str) -> bool {
        println!("\n---- 回答 ----\n{text}\n");
        true
    }
}

struct NoHub;
#[async_trait]
impl Hub for NoHub {
    async fn qq_send_text(&self, _: Option<i64>, _: &str) -> bool {
        false
    }
    async fn game_broadcast(&self, _: &str) -> bool {
        false
    }
    async fn chatroom_post(&self, _: &str, _: &str, _: &str, _: &str) -> bool {
        false
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let Some(cfg_path) = args.next() else {
        eprintln!("用法: query_skill <config.json> <skill> <问题> [...]");
        std::process::exit(2);
    };
    let raw = std::fs::read_to_string(&cfg_path).expect("读配置失败");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("配置不是合法 JSON");
    let agent: AgentConfig =
        serde_json::from_value(value["agent"].clone()).expect("解析 agent 段失败");

    let skills: Vec<Arc<dyn CommandHandler>> = build_skills(&agent)
        .into_iter()
        .map(|s| Arc::new(s) as Arc<dyn CommandHandler>)
        .collect();
    if skills.is_empty() {
        eprintln!("没有注册任何技能（agent.skills 为空或全部源无效）");
        std::process::exit(1);
    }
    eprintln!("已注册技能: {:?}", skills.iter().map(|s| s.info().name).collect::<Vec<_>>());

    let pairs: Vec<(String, String)> = args
        .collect::<Vec<_>>()
        .chunks(2)
        .filter_map(|c| (c.len() == 2).then(|| (c[0].clone(), c[1].clone())))
        .collect();

    let hub = NoHub;
    let sink = PrintSink;
    for (skill_name, q) in pairs {
        let Some(skill) = skills.iter().find(|s| s.info().name == skill_name) else {
            eprintln!("跳过：没有技能 {skill_name}");
            continue;
        };
        let msg = InboundMessage {
            source: Source::QQ,
            text: format!("!{skill_name} {q}"),
            group_id: 533106020,
            user_id: 0,
            at_me: true,
        };
        println!("\n>>>> !{} {} <<<<", skill_name, q);
        let ctx = DispatchCtx { hub: &hub, origin: &sink, msg: &msg };
        skill.handle(&ctx).await;
    }
}
