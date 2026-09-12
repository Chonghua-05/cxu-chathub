# Agent 能力设计（配置注册式检索问答）

本文档是 Agent 能力（roadmap v0.3）的设计依据与现状说明。核心形态：**玩家用固定命令提问，
Agent 检索配置指定的文档源后带着出处回答**——不是聊天机器人，也不是全自动助手。

## 1. 核心机制：命令与文档全部由配置注册

**代码不为任何具体命令写死实现。** 代码只提供三种可复用的积木：

| 积木 | 位置 | 职责 |
|------|------|------|
| `LocalDocSource` | `agent/local.rs` | 本地目录检索（md 按标题分节、代码按行窗分块；IDF 权重 + 路径 camelCase 分词 + import 降噪 + 文件级聚合），出处=`文件:行号区间` |
| `MediaWikiSource` | `agent/http.rs` | MediaWiki 站点 `api.php`，三段式（对齐 astrbot minecraft_wiki 插件）：问句清洗 → **标题直达**（`redirects=1` 逐词试查，"活塞是"→"活塞" 命中整页）→ 全文搜索兜底；出处=条目 URL |
| `RepoSource` | `agent/repo.rs` | GitHub 仓库文档源（如 mdBook 站点）：tarball 下载→本地缓存（24h 刷新，失败回退旧缓存）→委托本地检索，出处=站点页面 URL（路径百分号编码，兼容含空格/中文的文件名） |
| `DocQuerySkill` | `agent/skill.rs` | 通用命令处理器：触发（边界安全，`!mc` 不吃 `!mcs`）→ 并发查所有源 → LLM 整理（可配）或摘录降级 → 回复到来源端 |

一个命令 = `config.json` 里 `agent.skills` 的一条声明（名字、触发前缀、任意多个数据源）。
**新增 `!xxx` 命令 = 改一行配置重启，零代码。** 触发约定：全部走**显式命令强制触发**
（`!mc` / `!aimc` / `!wiki` / `!tmc` / `!doc`），不做任何自动/意图路由——那是智能路由
阶段的事。命令语义一一对应、不混源：`!mc` 只查源码，`!wiki` 只查 Wiki。

```json
"agent": {
  "enabled": true,
  "llm": { "api_url": "https://llm.example.com/v1/chat/completions", "api_key": "...", "model": "..." },
  "skills": [
    { "name": "mc",   "description": "MC 源码查询", "max_results": 5,
      "sources": [ {"type": "local", "root": "/data/docs/mc-source", "extensions": [".java"]} ] },
    { "name": "aimc", "description": "MC 源码释读（架构与逻辑设计解读文档）",
      "sources": [ {"type": "local", "root": "/data/docs/aimc", "extensions": [".md"]} ] },
    { "name": "tmc",  "sources": [ {"type": "repo", "repo": "techmc-wiki/articles", "branch": "main",
                                    "site_url": "", "extensions": [".zh.md", ".md"]} ] },
    { "name": "doc",  "sources": [ {"type": "repo", "repo": "Conflux-Union/RMS-Docs", "branch": "master",
                                    "site_url": "https://docs.rms.net.cn"} ] },
    { "name": "wiki", "sources": [ {"type": "mediawiki", "api_url": "https://zh.minecraft.wiki/api.php"} ] }
  ]
}
```

- `!tmc` 的语料是 GTMC 文章库（techmc.wiki 的源仓库，双语 `.zh.md`/`.en.md`）；
  `site_url` 留空 → 出处用 GitHub blob 文件地址（保留完整路径）；`extensions`
  用后缀匹配，可写 `.zh.md` 只收中文版（默认全收）。
- 命令语义一一对应（**不混源**）：`!mc` 只查源码、`!wiki` 只查 Wiki、`!aimc` 只查
  源码释读文档（语料就位即生效，目录缺失时检索返回空并告警）。

- 多源合并：结果按源交错、标注来源名、总量 ≤ `max_results`。
- 单个源无效（如目录不存在）只跳过该源；全部无效则该技能不注册。
- 索引惰性构建（首次查询时），只存词频向量不存正文，snippet 查询时按需读文件；语料更新靠重启。

## 2. 答案生成：LLM 整理 + 双重降级

- 配置了 `agent.llm`（OpenAI 兼容 `/chat/completions`）：检索片段（编号 + 出处）连同问题
  交给 LLM，system prompt 强制「只依据片段回答、引用编号出处、查不到就明说、禁止编造」。
- **中文问题 × 英文语料**（MC 源码 / MinecraftDocs 都是英文）：LLM 自动把问题翻译成英文
  检索关键词（守卫者→Guardian、刷怪→mob spawning），原文与译文各查一遍、按出处去重合并；
  无 LLM 或翻译失败只用原文（中文查英文语料会查不到——摘录模式请用英文关键词）。
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

- [x] 检索层：`LocalDocSource` / `MediaWikiSource` / `RepoSource`（IDF+路径分词+文件级聚合、
      两步 MediaWiki 查询、tarball 缓存）
- [x] 技能层：`DocQuerySkill`（多源合并、触发边界、LLM 整理与降级、中文查询自动翻译、三端回复）
- [x] 装配：`agent::build_skills()` 配置 → 注册；端到端集成测试（`tests/agent_flow.rs`）；
      真实联网测试（`tests/repo_real.rs`，`cargo test --test repo_real -- --ignored`）
- [x] 真实语料验证：MC 1.17.1 反编译源码（4144 个 .java，`spawner` → NaturalSpawner.java 排第一、
      `guardian spawn water` → Guardian.java 排第一）；MinecraftDocs 云端接入
      （"mob spawning" → mob tick / entity lifecycle / mob caps 等页面，出处映射 minecraftdocs.dev）
- [x] 检索质量调试工具：`cargo run --release --example doc_query -- <目录> <查询词> [扩展名]`
- [ ] 检索质量评测常态化：固定抽样问题集核对带出处的准确率，不达标先调分块与评分
- [ ] LLM 智能路由灰度（自然语言 → 技能选择）
