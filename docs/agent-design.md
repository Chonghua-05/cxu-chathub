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
（`!mc` / `!tmc` / `!docs` / `!wiki`），不做任何自动/意图路由——那是智能路由阶段的事。
命令语义一一对应：`!wiki` 只查 Wiki、`!docs` 只查 RMS 文档；`!mc` 是一个**两段**技能——
先查 MinecraftDocs 机制文档定位、再翻 15 版源码（见下「分段检索」）。

```json
"agent": {
  "enabled": true,
  "llm": { "api_url": "https://llm.example.com/v1/chat/completions", "api_key": "...", "model": "..." },
  "skills": [
    { "name": "mc", "max_results": 6, "sources": [
        {"type": "repo", "repo": "AlexanderjFraser/MinecraftDocs", "branch": "main", "subdir": "src",
         "site_url": "https://minecraftdocs.dev", "extensions": [".md"],
         "exclude": ["summary.md", "reference/class-index", "generated/", "maps/", "figures/", "lectures.md"], "name": "minecraftdocs", "stage": 1},
        {"type": "local", "root": "/data/docs/mc-source", "extensions": [".java"],
         "name": "mc-source", "stage": 2} ] },
    { "name": "tmc",  "sources": [ {"type": "repo", "repo": "techmc-wiki/articles", "branch": "main",
                                    "site_url": "https://techmc.wiki", "extensions": [".zh.md", ".md"]} ] },
    { "name": "docs", "sources": [ {"type": "repo", "repo": "Conflux-Union/RMS-Docs", "branch": "master",
                                    "site_url": "https://docs.cxu.org.cn"} ] },
    { "name": "wiki", "sources": [ {"type": "mediawiki", "api_url": "https://zh.minecraft.wiki/api.php"} ] }
  ]
}
```

- `!tmc` 的语料是 GTMC 文章库（techmc.wiki 的源仓库，双语 `.zh.md`/`.en.md`）；
  `extensions` 用后缀匹配，可写 `.zh.md` 只收中文版（默认全收）。
- **分段检索（`stage`）**：数据源可带 `stage`（缺省 1）；同一技能内按 stage 升序**逐段**
  检索，后段把前段命中里榨出的标识符（反引号标识符 / 驼峰类名）补进查询词。`!mc` 的
  「先查 MinecraftDocs 机制文档定位、再翻源码」即由此实现；单段（缺省）行为不变。

- 多源合并：同段内结果按源交错、标注来源名，各段按 stage 顺序拼接，总量 ≤ `max_results`。
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

## 4. 智能路由（灰度，已实现）

`agent/routing.rs` 的 `LlmSkillRouter` 注册在 `CommandRouter` **最末**：只有显式命令
全部不认领的消息才进入路由，任何显式命令行为都不受影响。

- **决策**：一次小 LLM 调用（system = 路由提示词 + 技能清单，清单来自各 handler 的
  `CommandInfo` 元数据——与 `/api/status` 的 `capabilities` 共用同一份能力发现），
  只回技能名或 `NONE`。解析只认**裸技能名**（第一行、大小写不敏感、容忍 `!` 前缀）；
  `NONE` / 空 / 超长 / 认不出的输出一律不路由——**宁可错过，不可错路由**。
- **命中**：合成 `{trigger} {原文}` 交给技能 handler，完整复用技能的检索、查询翻译、
  回答链路；消息被消费（与显式命令一致，不进转发流水线）。
- **拒绝 / 失败 / 超时**：返回 false 不消费，消息照常进转发流水线，对群友零感知。
  决策调用有独立 10s 短超时——OneBot 事件消费者是顺序处理，决策卡多久群消息就停多久，
  不能沿用 LLM 总超时（30s）。
- **灰度门控**（全部满足才生效）：
  1. **消息 @ 了机器人**（`at_me`，NapCat at 段 × self_id 判定）——**成本护栏**：
     普通聊天零 LLM 开销，@ 了才进路由；
  2. `agent.routing.enabled = true` 且 `agent.llm` 已配置；
  3. 消息来自 `agent.routing.group_ids` 白名单群；
  4. 消息非命令形态（`!` / `/` 开头不路由）。

  游戏 / chatroom 端无 @ 语义，不参与路由（避免每条玩家消息都打 LLM）。
- **配置**（默认全关；`group_ids` 为空视为未启用）：

```json
"agent": { "enabled": true, "llm": { ... },
           "routing": { "enabled": false, "group_ids": [] }, "skills": [ ... ] }
```

真实验证（LLM 接入后，测试群进行）：白名单群里 **@ 机器人**问「活塞怎么防冲水」
应路由到 `!mc` 并带回出处的回答；未 @ 的闲聊照常同步 chatroom、零 LLM 开销。

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
- [x] 检索质量评测常态化：固定抽样问题集核对带出处的准确率（见下方 §7）
- [ ] LLM 智能路由灰度（自然语言 → 技能选择）

## 7. 检索质量评测（常态化）

固定抽样问题集 + 通过率/MRR 统计，评测对象是**检索层**（`DocumentSource::search`，
与线上技能同一条路径、不含 LLM 翻译层，离线可复现）。实现：`agent/eval.rs`（评测核心，
带单测）+ 运行器 `examples/eval_retrieval.rs`；问题集在 `rust/eval/*.json`，
`corpus` 段与 `agent.skills[].sources[]` 完全同构（建源走同一 `build_source`）。

```bash
cargo run --release --example eval_retrieval -- rust/eval/gtmc-articles.json
cargo run --release --example eval_retrieval -- rust/eval/mc-source.json --root <源码目录>
cargo run --release --example eval_retrieval -- rust/eval/minecraftdocs.json --llm <config.json>
```

- 用例两种形态：`query`（关键词，确定性基线，始终执行）与 `question`（自然语言，
  如中文问句——需 `--llm` 提供 LLM，按技能层同款流程翻译成英文关键词后双语检索；
  无 LLM 时记 **SKIP** 不计入通过率）。期望 = `expect`（出处子串 any-of，对
  locator+标题匹配、percent 解码）+ `max_rank`；指标 = 通过率 + MRR；通过率低于
  `--min-rate`（默认 1.0）时退出码 1，可作常态化门禁。`--llm` 的配置文件认
  应用 config.json（agent.llm 段）、`{"llm":{...}}` 或裸 LlmConfig 三种形态。
- 五份问题集（2026-09-28 联网实测）：`gtmc-articles` 11/11（MRR 0.909）、
  `minecraftdocs` 11/11 关键词题 + 3 道 nl 题待 `--llm`（MRR 1.000）、
  `rms-docs` 7/7（MRR 1.000）、`mc-wiki` 8/8（MRR 1.000）；`mc-source` 待语料就位后
  首轮运行核对期望（前两题为 4c0342f 实测锚点，其余按官方映射类名出题）。
- 实测结论（调分块与评分的依据）：
  - 路径 CJK 段必须与正文同规则拆二元组（`tokenize_path`），否则中文文件名对
    中文查询完全无感（已修，带回归测试）；
  - 文件级聚合会让「高频词 × 大文件多节」压过专属条目（「区块互换」→《区块存储管理器》
    第 1，其正文并无该词）——专属条目类期望放宽 top-2 是合理校准；
  - 索引页/目录页（如 mdBook 的 SUMMARY.md、全站类名索引）什么查询都命中但永远
    不是答案，靠打分压不住——新增 `exclude` 路径排除配置（local/repo 源通用）；
  - 分词无词干还原：英文语料注意单复数（「block entity」查不中，「block entities」
    目标页第 1）；中文语料检索用单词关键词：双词会被高频词稀释（「实体 碰撞」
    目标排第 4，「碰撞」排第 1）。
