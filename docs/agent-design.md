# Agent 能力设计（配置注册式检索问答）

本文档是 Agent 能力（roadmap v0.3）的设计依据与现状说明。核心形态：**玩家用固定命令提问，
Agent 检索配置指定的文档源后带着出处回答**——不是聊天机器人，也不是全自动助手。

## 1. 核心机制：命令与文档全部由配置注册

**代码不为任何具体命令写死实现。** 代码只提供三种可复用的积木：

| 积木 | 位置 | 职责 |
|------|------|------|
| `LocalDocSource` | `agent/local.rs` | 本地目录检索（md 按标题分节、代码按行窗分块，词频+CJK bigram 评分），出处=`文件:行号区间` |
| `MediaWikiSource` | `agent/http.rs` | MediaWiki 站点 `api.php`（list=search → prop=extracts），出处=条目 URL |
| `DocQuerySkill` | `agent/skill.rs` | 通用命令处理器：触发（边界安全，`!mc` 不吃 `!mcs`）→ 并发查所有源 → LLM 整理（可配）或摘录降级 → 回复到来源端 |

一个命令 = `config.json` 里 `agent.skills` 的一条声明（名字、触发前缀、任意多个数据源）。
**新增 `!xxx` 命令 = 改一行配置重启，零代码。** `!mc` / `!wiki` / `!tmc` 只是三个配置实例，
`!mc` 接本地源码目录还是云端站点，同样是配置说了算。

```json
"agent": {
  "enabled": true,
  "llm": { "api_url": "https://llm.example.com/v1/chat/completions", "api_key": "...", "model": "..." },
  "skills": [
    { "name": "mc",   "description": "MC 源码查询", "max_results": 5,
      "sources": [ {"type": "local", "root": "/data/docs/mc-source", "extensions": [".java", ".md"]} ] },
    { "name": "wiki", "sources": [ {"type": "mediawiki", "api_url": "https://zh.minecraft.wiki/api.php"} ] },
    { "name": "tmc",  "sources": [ {"type": "local", "root": "/data/docs/techmc"},
                                    {"type": "mediawiki", "api_url": "https://techmc.example.com/api.php"} ] }
  ]
}
```

- 多源合并：结果按源交错、标注来源名、总量 ≤ `max_results`。
- 单个源无效（如目录不存在）只跳过该源；全部无效则该技能不注册。
- 索引惰性构建（首次查询时），只存词频向量不存正文，snippet 查询时按需读文件；语料更新靠重启。

## 2. 答案生成：LLM 整理 + 双重降级

- 配置了 `agent.llm`（OpenAI 兼容 `/chat/completions`）：检索片段（编号 + 出处）连同问题
  交给 LLM，system prompt 强制「只依据片段回答、引用编号出处、查不到就明说、禁止编造」。
- **LLM 未配置 → 自动降级为纯摘录**（编号列表：标题 + 来源 · 出处 + 片段）；
  **LLM 调用失败 → 同样降级**并记日志。技能永远不会因为 LLM 挂掉而无响应。
- 答案按 `max_answer_chars` 截断（UTF-8 安全）。

## 3. 端到端行为

三端（QQ 群 / 游戏内 / chatroom）均可触发：

| 端 | 语义 |
|----|------|
| QQ 群 | 消费语义：命令消息不进转发流水线，回答回群里 |
| 游戏 | 原始消息照常转发 chatroom，回答经 ChatBridge 广播回游戏 |
| chatroom | 原始消息照常广播游戏，回答经 Forward API 以 bot 身份写回频道（回答落库） |

命中格式中的出处：本地源 `redstone.md:1-3`（文件:行号区间），云端源
`https://zh.minecraft.wiki/wiki/活塞`（URL）——满足 roadmap「回答必须带出处」的要求。

## 4. 智能路由预留（下一迭代）

- `/api/status` 的 `capabilities` 字段实时枚举所有已注册命令（含配置注册的技能），
  人、Web UI、LLM 路由器共用同一份能力发现。
- 未来在 `CommandRouter::dispatch` 前置 LLM 路由器：读取各 handler 的
  `CommandInfo` 元数据（name/trigger/description），把**不带命令前缀的自然语言请求**
  路由到最合适的技能。技能与检索层接口无需任何改动。

## 5. 边界（沿用 roadmap 非目标）

- 不做通用聊天机器人：AI 只在「命令触发的领域问答」上开放。
- 只读：Agent 不写游戏服、不改 chatroom 服务端。
- 失败话术：查不到就说查不到；检索与 LLM 双层都遵循「不编」。

## 6. 里程碑

- [x] 检索层：`LocalDocSource` / `MediaWikiSource`（词频+CJK bigram 评分、两步 MediaWiki 查询）
- [x] 技能层：`DocQuerySkill`（多源合并、触发边界、LLM 整理与降级、三端回复）
- [x] 装配：`agent::build_skills()` 配置 → 注册；端到端集成测试（`tests/agent_flow.rs`）
- [ ] 语料接入：MC 源码副本 / techmc wiki 导出放置到配置目录（部署侧操作）
- [ ] 检索质量评测（带出处的准确率）与评分调优
- [ ] LLM 智能路由灰度（自然语言 → 技能选择）
