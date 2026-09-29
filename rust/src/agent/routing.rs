//! LLM 智能路由（灰度，roadmap v0.3 收尾项）：把 **@ 机器人**的不带命令前缀的
//! 自然语言消息路由到合适的技能。
//!
//! 注册位置在 [`crate::router::CommandRouter`] 的**最末**：只有显式命令全部
//! 不认领的消息才会到这里，任何显式命令行为都不受影响。门控全部满足才启用：
//!
//! - 消息 **@ 了机器人**（`at_me`，NapCat at 段 × self_id）——成本护栏：
//!   普通聊天零 LLM 开销，@ 了才进路由；
//! - `agent.routing.enabled = true` 且 `agent.llm` 已配置；
//! - 消息来自 `agent.routing.group_ids` 白名单内的 QQ 群（灰度范围；
//!   游戏 / chatroom 端无 @ 语义，不参与路由）；
//! - 消息不是命令形态（`!` / `/` 开头的是命令尝试，误路由只会白烧调用）。
//!
//! 决策：一次小 LLM 调用，只回技能名或 `NONE`。拒绝 / 失败 / 超时 / 未知输出
//! 一律返回 `false` 不消费消息——消息照常进转发流水线，对群友零感知。
//! 命中 → 合成 `{trigger} {原文}` 交给技能 handler，完整复用技能的检索、
//! 查询翻译、回答链路（被路由的消息不进转发流水线，与显式命令的消费语义一致）。
//!
//! 延迟注意：OneBot 事件消费者是顺序处理（见 `adapters/onebot.rs`），路由调用
//! 会占住队列，因此决策调用有独立短超时 [`ROUTE_TIMEOUT_SECS`]（小于 LLM 总超时）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tracing::{info, warn};

use crate::config::AgentConfig;
use crate::router::{CommandHandler, CommandInfo, DispatchCtx, InboundMessage, Source};

use super::llm::LlmClient;

/// 路由决策的独立超时（秒）：决策必须快，超时视为拒绝（不路由）。
/// 事件消费者是顺序处理，这里卡多久群消息就停多久——不能沿用 LLM 总超时（30s）。
const ROUTE_TIMEOUT_SECS: u64 = 10;

/// 路由决策提示词（system）：只输出技能名或 NONE。技能清单由构建时拼接。
const ROUTER_PROMPT: &str = "你在为游戏社区的文档问答机器人做路由。根据收到的群消息判断它是否想查询下面某个技能覆盖的资料：是 → 只输出该技能的名称；闲聊、其它事务、不确定 → 只输出 NONE。不要输出任何其他内容。\n可用技能：";

/// 一个可路由的技能：handler + 其元数据（LLM 决策的选取依据来自 info）。
pub struct RoutableSkill {
    pub handler: Arc<dyn CommandHandler>,
    pub info: CommandInfo,
}

/// 自然语言 → 技能的路由器。见模块文档的门控与决策语义。
pub struct LlmSkillRouter {
    llm: LlmClient,
    skills: Vec<RoutableSkill>,
    group_ids: Vec<i64>,
}

impl LlmSkillRouter {
    /// 从 agent 配置与已注册技能构建。Err 文案只用于日志；调用方在未启用时
    /// 静默跳过、启用但失败时告警。
    pub fn new(agent: &AgentConfig, skills: Vec<RoutableSkill>) -> Result<Self, String> {
        if !agent.routing.enabled {
            return Err("agent.routing.enabled 未开启".into());
        }
        let llm_cfg = agent
            .llm
            .as_ref()
            .filter(|cfg| !cfg.api_url.is_empty())
            .ok_or("agent.llm 未配置")?;
        let llm = LlmClient::new(llm_cfg.clone()).map_err(|err| err.to_string())?;
        if skills.is_empty() {
            return Err("没有可路由的技能（agent.skills 为空或全部无效）".into());
        }
        if agent.routing.group_ids.is_empty() {
            return Err("agent.routing.group_ids 为空（灰度必须显式指定白名单群）".into());
        }
        Ok(Self {
            llm,
            skills,
            group_ids: agent.routing.group_ids.clone(),
        })
    }

    /// LLM 决策：返回命中的技能。任何失败路径都返回 None（不路由）。
    async fn decide(&self, text: &str) -> Option<&RoutableSkill> {
        let mut listing = String::new();
        for skill in &self.skills {
            listing.push_str(&format!("\n- {}: {}", skill.info.name, skill.info.description));
        }
        let prompt = format!("{ROUTER_PROMPT}{listing}");
        let reply = match tokio::time::timeout(
            Duration::from_secs(ROUTE_TIMEOUT_SECS),
            self.llm.complete(&prompt, text),
        )
        .await
        {
            Ok(Ok(reply)) => reply,
            Ok(Err(err)) => {
                warn!(error = %err, "智能路由 LLM 调用失败，消息不路由");
                return None;
            }
            Err(_) => {
                warn!(timeout_secs = ROUTE_TIMEOUT_SECS, "智能路由决策超时，消息不路由");
                return None;
            }
        };
        let index = parse_decision(&reply, &self.skills)?;
        Some(&self.skills[index])
    }
}

/// 解析路由决策：第一行 trim 后必须是**裸技能名**（大小写不敏感，容忍 `!` 前缀
/// 与引号包裹）；`NONE` / 空 / 超长 / 认不出的输出一律 None——路由宁可错过，
/// 不可错路由。
fn parse_decision(reply: &str, skills: &[RoutableSkill]) -> Option<usize> {
    let line = reply
        .lines()
        .next()?
        .trim()
        .trim_matches('"')
        .trim_start_matches('!')
        .trim()
        .to_lowercase();
    if line.is_empty() || line.eq("none") || line.eq("无") || line.len() > 32 {
        return None;
    }
    skills
        .iter()
        .position(|skill| skill.info.name.to_lowercase() == line)
        .or_else(|| {
            skills.iter().position(|skill| {
                skill.info.trigger.trim_start_matches('!').to_lowercase() == line
            })
        })
}

#[async_trait]
impl CommandHandler for LlmSkillRouter {
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: "llm-router".into(),
            aliases: vec![],
            trigger: "自然语言（无前缀）".into(),
            description: "灰度：白名单群的自然语言消息由 LLM 路由到技能（拒绝则不消费）".into(),
        }
    }

    fn matches(&self, msg: &InboundMessage) -> bool {
        // 成本护栏第一道：只有 @ 了机器人的消息才可能进路由（普通聊天零 LLM 开销）
        if !msg.at_me || !matches!(msg.source, Source::QQ) || !self.group_ids.contains(&msg.group_id)
        {
            return false;
        }
        let text = msg.text.trim();
        !text.is_empty() && !text.starts_with('!') && !text.starts_with('/')
    }

    async fn handle(&self, ctx: &DispatchCtx<'_>) -> bool {
        let Some(skill) = self.decide(&ctx.msg.text).await else {
            return false; // 拒绝 / 失败：不消费，消息照常进转发流水线
        };
        info!(
            skill = %skill.info.name,
            group = ctx.msg.group_id,
            text = %truncate_for_log(&ctx.msg.text),
            "自然语言消息已路由到技能"
        );
        // 合成显式命令消息：技能 handler 按既有链路处理（解析查询、检索、回答）
        let routed = InboundMessage {
            source: ctx.msg.source.clone(),
            text: format!("{} {}", skill.info.trigger, ctx.msg.text.trim()),
            group_id: ctx.msg.group_id,
            user_id: ctx.msg.user_id,
            display_name: ctx.msg.display_name.clone(),
            at_me: ctx.msg.at_me,
        };
        let routed_ctx = DispatchCtx {
            hub: ctx.hub,
            origin: ctx.origin,
            msg: &routed,
        };
        skill.handler.handle(&routed_ctx).await
    }
}

/// 日志用截断（决策日志不落全文，控制在 60 字符内）。
fn truncate_for_log(text: &str) -> String {
    match text.char_indices().nth(60) {
        None => text.to_string(),
        Some((cut, _)) => format!("{}…", &text[..cut]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use axum::Json;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::Router;

    use crate::router::{Hub, ReplySink};

    fn agent_config(api_url: &str, group_ids: Vec<i64>, enabled: bool) -> AgentConfig {
        serde_json::from_str(&format!(
            r#"{{
                "enabled": true,
                "llm": {{ "api_url": "{api_url}", "model": "test" }},
                "routing": {{ "enabled": {enabled}, "group_ids": {group_ids:?} }}
            }}"#
        ))
        .unwrap()
    }

    fn fake_skill() -> (RoutableSkill, Arc<FakeSkill>) {
        let skill = Arc::new(FakeSkill::default());
        let routable = RoutableSkill {
            handler: skill.clone(),
            info: CommandInfo {
                name: "mc".into(),
                aliases: vec![],
                trigger: "!mc".into(),
                description: "MC 源码查询".into(),
            },
        };
        (routable, skill)
    }

    fn router_for_test(api_url: &str, group_ids: Vec<i64>) -> LlmSkillRouter {
        let (skill, _) = fake_skill();
        LlmSkillRouter::new(&agent_config(api_url, group_ids, true), vec![skill]).unwrap()
    }

    fn qq_msg(group_id: i64, text: &str) -> InboundMessage {
        InboundMessage {
            source: Source::QQ,
            text: text.into(),
            group_id,
            user_id: 2,
            display_name: "tester".into(),
            at_me: true,
        }
    }

    fn qq_plain_msg(group_id: i64, text: &str) -> InboundMessage {
        InboundMessage {
            at_me: false,
            ..qq_msg(group_id, text)
        }
    }

    // ---------- 测试桩 ----------

    #[derive(Default)]
    struct FakeSkill {
        handled: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl CommandHandler for FakeSkill {
        fn info(&self) -> CommandInfo {
            CommandInfo {
                name: "mc".into(),
                aliases: vec![],
                trigger: "!mc".into(),
                description: "测试技能".into(),
            }
        }
        fn matches(&self, _msg: &InboundMessage) -> bool {
            true
        }
        async fn handle(&self, ctx: &DispatchCtx<'_>) -> bool {
            self.handled.lock().unwrap().push(ctx.msg.text.clone());
            true
        }
    }

    struct NoopHub;
    #[async_trait]
    impl Hub for NoopHub {
        async fn qq_send_text(&self, _: Option<i64>, _: &str) -> bool { true }
        async fn qq_send_image(&self, _: i64, _: &[u8]) -> bool { false }
        async fn game_broadcast(&self, _: &str) -> bool { false }
        async fn chatroom_post(&self, _: &str, _: &str, _: &str, _: &str) -> bool { false }
    }

    #[derive(Default)]
    struct FakeSink {
        texts: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl ReplySink for FakeSink {
        async fn send_text(&self, text: &str) -> bool {
            self.texts.lock().unwrap().push(text.to_string());
            true
        }
    }

    // ---------- LLM mock（与 skill.rs / eval.rs 测试同款套路） ----------

    struct MockState {
        bodies: Mutex<Vec<serde_json::Value>>,
        content: &'static str,
    }

    async fn handler(
        State(st): State<Arc<MockState>>,
        Json(body): Json<serde_json::Value>,
    ) -> Response {
        st.bodies.lock().unwrap().push(body);
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "choices": [ { "message": { "role": "assistant", "content": st.content } } ]
            })),
        )
            .into_response()
    }

    async fn spawn_mock(content: &'static str) -> (String, Arc<MockState>) {
        let state = Arc::new(MockState { bodies: Mutex::new(Vec::new()), content });
        let app = Router::new()
            .route("/v1/chat/completions", post(handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/v1/chat/completions"), state)
    }

    // ---------- 用例 ----------

    /// 决策解析：裸技能名（大小写 / `!` 前缀 / 引号容忍）；NONE、多余输出、
    /// 未知技能、空串都拒绝。
    #[test]
    fn parse_decision_accepts_bare_skill_name_only() {
        let (skill, _) = fake_skill();
        let skills = vec![skill];

        assert_eq!(parse_decision("mc", &skills), Some(0));
        assert_eq!(parse_decision("MC", &skills), Some(0));
        assert_eq!(parse_decision("!mc", &skills), Some(0));
        assert_eq!(parse_decision(" \"mc\" ", &skills), Some(0));
        assert_eq!(parse_decision("NONE", &skills), None);
        assert_eq!(parse_decision("无", &skills), None);
        assert_eq!(parse_decision("", &skills), None);
        assert_eq!(parse_decision("我觉得应该查 mc 吧", &skills), None);
        assert_eq!(parse_decision("mc wiki", &skills), None);
        assert_eq!(parse_decision("wiki", &skills), None); // 不存在的技能
    }

    /// 构建门控：未启用 / 缺 LLM / 无技能 / 空白名单都拒绝构建。
    #[test]
    fn router_build_gates() {
        // 未启用
        let (s, _) = fake_skill();
        assert!(LlmSkillRouter::new(&agent_config("", vec![1], false), vec![s]).is_err());
        // 缺 LLM
        let (s, _) = fake_skill();
        assert!(LlmSkillRouter::new(&agent_config("", vec![1], true), vec![s]).is_err());
        // 无技能
        assert!(LlmSkillRouter::new(&agent_config("http://x", vec![1], true), vec![]).is_err());
        // 空白名单
        let (s, _) = fake_skill();
        assert!(LlmSkillRouter::new(&agent_config("http://x", vec![], true), vec![s]).is_err());
        // 全齐 → Ok
        let (s, _) = fake_skill();
        assert!(LlmSkillRouter::new(&agent_config("http://x", vec![1], true), vec![s]).is_ok());
        // routing 段缺省（serde default）→ 未启用
        let no_routing: AgentConfig = serde_json::from_str(r#"{ "enabled": true }"#).unwrap();
        assert!(!no_routing.routing.enabled);
    }

    /// matches 门控：仅白名单 QQ 群内 @ 机器人的非命令文本；未 @、`!` / `/`
    /// 开头、其它来源都不路由。
    #[tokio::test]
    async fn router_matches_gates() {
        let router = router_for_test("http://unused", vec![100]);

        assert!(router.matches(&qq_msg(100, "活塞怎么防冲水")));
        assert!(!router.matches(&qq_plain_msg(100, "活塞怎么防冲水"))); // 未 @ 机器人
        assert!(!router.matches(&qq_msg(200, "活塞怎么防冲水"))); // @ 了但非白名单群
        assert!(!router.matches(&qq_msg(100, "!mc 活塞"))); // 命令形态
        assert!(!router.matches(&qq_msg(100, "/server"))); // 斜杠命令
        assert!(!router.matches(&qq_msg(100, "  "))); // 空消息
        assert!(!router.matches(&InboundMessage {
            source: Source::Game { sender: "web".into(), author: "bob".into() },
            text: "活塞怎么防冲水".into(),
            group_id: 0,
            user_id: 0,
            display_name: "bob".into(),
            at_me: false,
        })); // 游戏来源不参与路由
    }

    /// 端到端：LLM 回技能名 → 合成 `{trigger} {原文}` 交给技能并消费；
    /// LLM 回 NONE → 不消费、技能未被调用。
    #[tokio::test]
    async fn router_routes_or_declines() {
        // 命中路径
        let (url, state) = spawn_mock("mc").await;
        let (skill, fake) = fake_skill();
        let router = LlmSkillRouter::new(&agent_config(&url, vec![100], true), vec![skill]).unwrap();

        let sink = FakeSink::default();
        let hub = NoopHub;
        let msg = qq_msg(100, "活塞怎么防冲水");
        let ctx = DispatchCtx { hub: &hub, origin: &sink, msg: &msg };
        assert!(router.handle(&ctx).await);
        assert_eq!(
            fake.handled.lock().unwrap().as_slice(),
            ["!mc 活塞怎么防冲水"] // 合成显式命令消息
        );
        let prompt_user = state.bodies.lock().unwrap()[0]["messages"][1]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(prompt_user, "活塞怎么防冲水");

        // 拒绝路径：NONE → 不消费
        let (url2, _) = spawn_mock("NONE").await;
        let (skill2, fake2) = fake_skill();
        let router2 =
            LlmSkillRouter::new(&agent_config(&url2, vec![100], true), vec![skill2]).unwrap();
        let ctx2 = DispatchCtx { hub: &hub, origin: &sink, msg: &msg };
        assert!(!router2.handle(&ctx2).await);
        assert!(fake2.handled.lock().unwrap().is_empty());

        // LLM 挂掉 → 不消费（技能永远不会因 LLM 失败而误吞消息）
        let (skill3, fake3) = fake_skill();
        let router3 = LlmSkillRouter::new(
            &agent_config("http://127.0.0.1:9/v1/chat/completions", vec![100], true),
            vec![skill3],
        )
        .unwrap();
        let ctx3 = DispatchCtx { hub: &hub, origin: &sink, msg: &msg };
        assert!(!router3.handle(&ctx3).await);
        assert!(fake3.handled.lock().unwrap().is_empty());
    }
}
