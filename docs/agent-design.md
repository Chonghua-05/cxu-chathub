# Agent 能力设计（命令触发式检索问答）

本文档是 Agent 能力（roadmap v0.3）的设计依据。核心形态：**玩家用固定命令提问，
Agent 检索既定文档源后带着出处回答**——不是聊天机器人，也不是全自动助手。
当前（Rust 迁移阶段）只完成架构预留（见 §1），技能实现属下一阶段，按本文档落地。

## 1. 目标与范围

| 范围 | 内容 |
|------|------|
| 做 | `!mc` / `!wiki` / `!tmc` 三个命令触发的**领域检索问答**，回答带出处 |
| 做 | 可继续注册新文档源（本地 / 云端），技能层无感 |
| 做 | 智能路由预留：未来 LLM 在自然语言请求中选取命令（§5） |
| 不做 | 通用聊天机器人（roadmap 非目标：AI 只在「需要查证的领域问答」上开放） |
| 不做 | 写游戏服（发物品 / 重启等）——只读 |
| 不做 | 修改 chatroom 服务端——本仓库只做客户端 |

**本阶段已预留的部分**（随 Rust 迁移落地，不实现技能）：

- `rust/src/agent/mod.rs`：`DocumentSource` trait 与 `DocHit` 结构（检索层接口）。
- `rust/src/router/mod.rs`：`CommandHandler` / `CommandInfo` / `CommandRouter`
  扩展点，三端入站消息统一汇入 `CommandRouter::dispatch`。
- `rust/src/router/handlers.rs`：首批 handler（斜杠命令、`!q` 中继、`!snap` 快照通知），
  同时是未来 agent 技能的参照样板。

## 2. 命令语义

| 命令 | 数据源 | 形态 | 出处形态 | 状态 |
|------|--------|------|----------|------|
| `!mc <问题>` | MC 源码 | 本地源码副本检索，回答带**文件/行级出处** | `文件:行号区间` | 下一阶段实现 |
| `!wiki <词条>` | Minecraft Wiki | 云端 MediaWiki API（如 zh.minecraft.wiki） | 条目 URL | 下一阶段实现 |
| `!tmc <问题>` | techmc wiki | 文档源（本地 / 云端待定） | `文件:行号区间` 或 URL | 下一阶段实现 |
| （规划中） | MC 源码释读文档 | 本地文档，帮助理解架构与逻辑设计 | `文件:行号区间` | 文档撰写中 |

要点：

- **可扩展**：上面只是首批四个文档源；任何新的 `DocumentSource` 实现注册后即可被
  技能引用（§4），接口不变。
- **触发解析**复用 `router::match_prefix`：整前缀大小写不敏感（`!MC` 同样命中）、
  载荷自动 trim，与既有 `!q` 行为一致。
- **前缀重叠**：`!tmc` 与 `!mc` 互为前缀，`CommandRouter` 按注册顺序匹配，
  注册时**长者在前**（`!tmc` 先注册）。
- **硬约束**：查不到就说查不到，不编（roadmap 非目标）。检索为空 / 云端失败时
  回复固定话术，绝不臆造出处。

## 3. 接入路径

每个命令 = 一个 `CommandHandler` 实现 + 一个 `DocumentSource` 实现，注册一行接入三端：

```
CommandRouter::register(Arc::new(McCommand::new(doc_source)));
```

`CommandRouter` 已被 QQ 群（OneBot WS）、游戏（ChatBridge）、chatroom（读轮询）
三端入站消息汇入，注册即全部生效，无需改适配层。

### 3.1 handler 骨架（示意，参照 router/handlers.rs 既有 handler）

```rust
struct McCommand { docs: Arc<dyn DocumentSource> }

#[async_trait]
impl CommandHandler for McCommand {
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: "mc",
            aliases: &[],
            trigger: "!mc <问题>",
            description: "检索本地 MC 源码副本，回答带文件/行级出处",
        }
    }
    fn matches(&self, msg: &InboundMessage) -> bool {
        match_prefix(&msg.text, "!mc").is_some()
    }
    async fn handle(&self, ctx: &DispatchCtx<'_>) -> bool {
        let query = match_prefix(&ctx.msg.text, "!mc").unwrap_or("");
        // 空 query / 检索为空 → 固定话术，返回 true（消息已消费）
        // 命中 → 组装 title + locator + snippet，回源应答
        // ctx.origin.send_text(...)；落库见 §6
    }
}
```

### 3.2 各端的消费语义

| 来源端 | 命中命令后的行为 |
|--------|------------------|
| QQ 群 | **消费语义**：`handle` 返回 true 后消息不进转发流水线（命令本身不写入 chatroom 频道） |
| 游戏 | 原始消息照常广播（ChatBridge 既有行为），handler 只负责自己的应答 |
| chatroom | 原始消息照常走既有转发/中继，handler 只负责自己的应答 |

`handle` 返回 false（未命中 / 主动放行）时，消息一律按既有流程走，行为与现状逐字节一致。

## 4. 文档源设计

检索层接口（`agent/mod.rs`，已定稿）：

```rust
pub struct DocHit {
    pub title: String,     // 条目标题，如 "PlayerListEvent#handle"
    pub locator: String,   // "src/path.rs:120-145" 或 "https://zh.minecraft.wiki/w/…"
    pub snippet: String,   // 命中片段（截断后随回答给出）
}

#[async_trait]
pub trait DocumentSource: Send + Sync {
    fn name(&self) -> &'static str;                        // "local-mc-source" / "minecraft-wiki" / …
    async fn search(&self, query: &str, limit: usize) -> Vec<DocHit>;
}
```

`locator` 是出处保障：本地源给「文件:行号区间」，云端源给 URL，回答必须原样带出，
保证玩家（和 Agent 自己）能回去核对。检索失败返回空列表——技能层据此走
「查不到」话术，而不是编答案。

### 4.1 本地源（!mc / techmc 的本地形态 / 释读文档）

- **采集**：目录扫描 `markdown` / `txt` 文件（MC 源码副本、释读文档放在挂载卷里，
  不打进镜像）。
- **分块**：按标题层级分节，单节过长再按行数二次分块；每块记录来源文件与行号区间，
  供 `locator` 使用。
- **检索**：起步用**词频 + CJK bigram 评分**（对中文问题和英文标识符都够用、零外部依赖）；
  `search` 签名不变，后续可整体替换为向量检索，技能层无感。
- 索引常驻内存（源码副本量级在几百 MB 内可接受），启动时扫描一次，命令手动触发重建。

### 4.2 云端源（!wiki，及 techmc 的云端形态）

- **协议**：MediaWiki `api.php?action=query&list=search&srsearch=<query>&format=json`
  （zh.minecraft.wiki），再取条目摘要组装 `DocHit`，`locator` = 条目 URL。
- **超时**：reqwest client 显式设置请求超时（秒级），不拖住 dispatcher。
- **TTL 缓存**：同 query 命中缓存直接返回（TTL 小时级），避免打爆对方站点、也避免慢查询反复阻塞。
- **失败降级**：超时 / 非 200 / 解析失败一律返回空列表 → 走「查不到」话术，不重试不编造。

### 4.3 配置

`config.json` 新增可选 `agent` 段，**缺省关闭**（不配置 = 完全不注册技能，行为与现在一致）：

```json
{
  "agent": {
    "enabled": false,
    "mc":   { "doc_root": "/data/docs/mc-source" },
    "wiki": { "api_url": "https://zh.minecraft.wiki/api.php", "timeout_secs": 8, "cache_ttl_secs": 3600 },
    "tmc":  { "doc_root": "/data/docs/techmc" }
  }
}
```

- 实现沿用 `config.rs` 既有 `lenient` 反序列化：段缺失 / 类型不符退回默认值并告警，
  老配置文件零改动。
- `enabled: false`（或缺省）时技能不注册；`doc_root` 指向的目录建议放 `./data`
  挂载卷（如 `/data/docs/`），与 `state.json` 同卷，更新文档不需要改镜像。

## 5. 智能路由预留

现状：`CommandRouter::dispatch` 按注册顺序显式匹配——`handler.matches()` 首个命中且
`handle` 返回 true 即消费。确定性强、零 AI 依赖，命令触发场景够用。

预留路径（下一阶段之后）：

```
入站消息 ──▶ [前置 LLM 路由器] ──▶ 选中 CommandHandler ──▶ DispatchCtx（原样）
                │
                └─ 工具集 = 遍历 CommandRouter::handlers() 取各 CommandInfo
                   （name / aliases / trigger / description 元数据，专为选取而设计）
```

- LLM 路由器读取全部 `CommandHandler::info()` 元数据做**工具选择**，把自然语言请求
  （「末影人怕什么」→ `!wiki`，`!mc 爆炸伤害怎么算` 之外的口语化问法 → `!mc`）
  路由到最合适的 handler；选不中则退回按注册顺序的显式匹配，或直接放行。
- **接口零改动**：`CommandHandler`、`ReplySink`、`Hub`、`DocumentSource` 都不动，
  只替换 `dispatch` 之前的匹配策略；未启用 LLM 路由时走纯命令匹配。
- **边界不变**：失败话术沿用 roadmap 约束（查不到就说查不到，不编）；只读边界——
  不写游戏服、不改 chatroom 服务端，回答与快照一样经既有出站通道（`Hub::chatroom_post`）落库。

## 6. 里程碑清单

按序推进，每步可独立验收：

1. **技能实现**：三个 `DocumentSource`（本地 MC 源码 / MediaWiki / techmc）+
   三个 `CommandHandler`（`!mc` / `!wiki` / `!tmc`）注册进 `CommandRouter`，
   三端可触发，回答带 `locator`。
2. **检索质量评测**：固定抽样问题集，人工核对**带出处的准确率**（locator 指到的
   源码行 / wiki 条目确实回答了问题），不达标先调分块与评分，不加功能。
3. **LLM 路由灰度**：仅在部分群 / 端启用自然语言路由（§5），命令触发保持直通，
   灰度期对比两者的路由正确率。
4. **回答落库**：问答对经 `Hub::chatroom_post` 写回 chatroom，便于回溯与纠错
   （roadmap v0.3 既有项，接口已就位）。
