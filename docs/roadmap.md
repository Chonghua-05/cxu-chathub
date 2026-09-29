# Roadmap

本仓库的定位是 **本社区的消息与服务中枢**，长期会承载多类常驻服务和有限的 Agent 能力。
下面按「已经到的 → 下一步 → 更远」排列，不承诺时间点。

## 当前待办（TODO，面向接手开发者）

> 按优先级排列。已完成/历史项见下方各版本清单。

- [ ] **v0.3 线上收尾**：代码侧已全部完成（检索评测、云端语料、智能路由，见下方
      v0.3 清单）；剩线上步骤——`!mc` 反编译源码 / `!aimc` 释读文档等本地语料放置、
      线上启用 `agent.skills` / `agent.llm`（改配置须 `docker restart`），LLM 就位后
      跑 `rust/eval/mc-source.json` 与评测 `--llm` 模式核对，并在测试群灰度验证智能路由。
- [ ] **v0.4 服务化**：子服务边界拆分、配置热重载（SIGHUP）、结构化 JSON 日志、`/metrics`。
- [ ] **v0.5（新功能）**：Mojang 版本更新播报 —— 自动抓官方更新说明 → LLM 翻译 →
      「译后 / 译前截图 + 原文链接」打包成合并转发消息发群（详见下方 v0.5 节）；
      ＋ Web UI 面板（见下方 v0.5+ 节）。
- [ ] **更远**：发布到包管理器、插件化。

## v0.1

- [x] QQ 群 ↔ chatroom 双向同步（文本 / 图片 / 引用）
- [x] chatroom ↔ MC 游戏内聊天（ChatBridge，AES-CBC over TCP）
- [x] 玩家上下线事件推送（快照比对 + 防抖）
- [x] 群内 `/chatroom`、`/server` 命令（状态图带文本回退）
- [x] 去重表 / 读游标 / refresh token 持久化（原子写、损坏自愈）
- [x] 单元测试 + 一条真实 WS 装配烟测

## v0.2 —— Rust 重写（已完成，2026-09-13 切流上线）

用 Rust 重写整套服务（`rust/`，包 `chatroom-bridge`），**行为对齐 Python 版**。
**Rust 版已于 2026-09-13 在云主机切流上线为唯一线上实现**（镜像 `cxu-chathub:rust-v0.2`、
容器 `chatroom-bridge`）；Python 版代码保留在 `src/`，仅供回滚参照。

- [x] 行为对齐：三端互通、群命令（`/chatroom` `/server`）、去重 / 游标 / refresh token
      持久化逐一对齐 Python 版
- [x] 部署件：`rust/Dockerfile` 多阶段构建（`STATUS_IMAGE` 可选 Chromium 变体）+
      `docker-compose.yml` 的 `rust` profile（与 Python 版二选一，6199 端口冲突）
- [x] 切流验收：`state.json` 与 Python 版互相兼容（`./data` 目录共用），切流后
      去重表与读游标不丢
- [x] 架构预留：agent 能力扩展点固化（`router::CommandHandler` / `agent::DocumentSource`），
      设计见 [`docs/agent-design.md`](agent-design.md)，技能实现属 v0.3
- [x] 性能优化：`/server` 状态图渲染改为常驻 Chromium 复用（+ 背景重编码缓存、
      去掉多余导航），端到端 ~9s → ~1s 级；已随 2026-09-13 切流上线（源码 `d841d7b`）

## v0.3 —— Agent 能力（顺延，原 v0.2 项）

目标：让玩家在群里/游戏里用自然语言问游戏机制，由 Agent 查证后回答。
完整设计见 [`docs/agent-design.md`](agent-design.md)。实现形态：**命令与文档源全部由
`agent.skills` 配置注册**（新增命令零代码），检索层（本地目录 / MediaWiki）+ 通用技能层 +
LLM 整理（失败自动降级摘录）均已落地并通过端到端测试。

- [x] **检索层与技能层**：`LocalDocSource`（文件/行级出处）、`MediaWikiSource`（条目 URL）、
      通用 `DocQuerySkill`（多源合并、触发边界、三端回复、LLM 整理与降级）
- [ ] **语料接入**：云端语料已全部接入并评测（`!tmc` GTMC 文章库 / `!doc` RMS-Docs /
      `!docs` MinecraftDocs（英文，含 `exclude` 路径排除）/ `!wiki`）；剩本地语料——
      MC 源码副本（`!mc`）与源码释读文档（`!aimc`）放置到配置目录并线上启用
- [x] **检索质量评测**：固定抽样问题集核对带出处的准确率——`rust/eval/` 四份问题集 +
      `examples/eval_retrieval` 运行器（通过率/MRR，`--min-rate` 门禁），云端语料实测
      gtmc 11/11、rms-docs 7/7、mc-wiki 8/8（2026-09-28）；顺带修正路径 CJK 分词与
      正文不一致的问题（`tokenize_path` 补二元组拆分，带回归测试）
- [x] **回答落库**：chatroom 端提问的回答经 Forward API 以 bot 身份写回频道
- [x] 只读沙箱：检索与 LLM 均只读，不触碰游戏服与 chatroom 服务端
- [x] 明确的能力边界与失败话术（查不到就说查不到，检索与 LLM 双层遵循）
- [x] **LLM 智能路由灰度**：`agent/routing.rs` `LlmSkillRouter`——注册在路由最末
      （不影响任何显式命令）、**仅 @ 机器人的消息进入路由**（成本护栏：普通聊天零
      LLM 开销）+ 白名单群灰度门控、LLM 拒绝/失败/超时不消费消息（照常转发）、
      决策独立 10s 短超时；默认关闭，设计与验证步骤见 `docs/agent-design.md` §4

## v0.4 —— 服务化（顺延，原 v0.2）

- [ ] 拆出「桥接 / 玩家状态 / 命令响应」子服务边界，统一生命周期与健康检查
- [ ] 统一配置加载与校验，支持热重载（先做「SIGHUP 重读」这一最小形态）
- [ ] 结构化日志（JSON），便于以后接监控
- [ ] `/metrics`（Prometheus 文本格式）：转发计数、失败计数、连接状态

## v0.5 —— Mojang 版本更新播报（新功能，需求已定，未实现）

目标：把 Minecraft（Java 版）新版本的官方更新说明自动播报到 QQ 群。

- [ ] **数据源**：轮询 Mojang 官方 `https://launchercontent.mojang.com/javaPatchNotes.json`
      （字段 title / type / version / image / body(HTML) / contentPath），检测到新版本即触发。
      注：Mojang **无推送/订阅 API**，只能轮询；判断逻辑可与 `snapshot-updater`
      （`Chonghua-05/snapshot-updater`，已在每 30 分钟拉版本清单）复用。
- [ ] **翻译**：复用 cxu 已有的 LLM 配置，把正文译为中文。
- [ ] **截图**：渲染两张长图——翻译后 / 翻译前（复用 `status_render` 的常驻 Chromium，HTML→PNG）。
- [ ] **打包**：以「译后截图 + 译前截图 + 原文链接」组成一条**合并转发聊天记录**
      （OneBot `send_group_forward_msg`，节点含图片与文本）。
- [ ] **发送**：自动发到配置的 `group_ids` 白名单群。

选型已定：翻译＝现有 LLM；目标群＝`group_ids` 白名单；触发＝轮询自动检测新版本。
待定：是否需要手动命令（如 `/patch`）重新发送最近一期。

## v0.5+ —— Web UI（2026-09-28 用户提出，未设计）

- [ ] 基于 HTTP API 底座做社区状态与消息查看面板：读接口 `/api/status`
      （含 capabilities 能力清单）、`/api/messages`（近期消息环形缓冲）已就绪，
      写接口 `/api/relay`（token 保护）可用于发送消息；CORS 已全路由放开。
      部署与接入路径（反代 `/api/*` → 8199、token 不下发浏览器）见
      `docs/api-design.md`；实时刷新预留 SSE（`/api/events`，数据源 RecentLog）。

## 更远

- [ ] Publish 到 PyPI，支持 `pip install` 后以库的形式嵌入其他服务
- [ ] 插件化：其他社区服务以子进程 / 插件方式挂到本仓库的运行时

## 非目标

- 不做通用聊天机器人：AI 部分只在「需要查证的领域问答」上开放。
- 不接管游戏服管理（重启、发物品等）——只读。
- 不修改 chatroom 服务端（本仓库只做客户端）。
