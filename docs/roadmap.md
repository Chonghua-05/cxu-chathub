# Roadmap

本仓库的定位是 **本社区的消息与服务中枢**，长期会承载多类常驻服务和有限的 Agent 能力。
下面按「已经到的 → 下一步 → 更远」排列，不承诺时间点。

## 当前待办（TODO，面向接手开发者）

> 按优先级排列。已完成/历史项见下方各版本清单。

- [ ] **v0.3 线上收尾**：代码侧已全部完成（检索评测、云端语料、智能路由，见下方
      v0.3 清单）；剩线上步骤——`!mc` 反编译源码 / `!aimc` 释读文档等本地语料放置、
      线上启用 `agent.skills` / `agent.llm`（改配置须 `docker restart`），LLM 就位后
      跑 `rust/eval/mc-source.json` 与评测 `--llm` 模式核对，并在测试群灰度验证智能路由。
- [ ] **v0.5 线上启用与验证**：代码侧已完成（见下方 v0.5 清单）——`config.json`
      开 `patch_broadcast.enabled` + 配 `agent.llm`（状态图/长图渲染已内置），
      测试群验证合并转发的实际显示效果（本机无 NapCat，wire 格式仅单测覆盖）。
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
- [x] 部署件：`rust/Dockerfile` 多阶段构建 +
      `docker-compose.yml` 的 `rust` profile（与 Python 版二选一，6199 端口冲突）
- [x] 切流验收：`state.json` 与 Python 版互相兼容（`./data` 目录共用），切流后
      去重表与读游标不丢
- [x] 架构预留：agent 能力扩展点固化（`router::CommandHandler` / `agent::DocumentSource`），
      设计见 [`docs/agent-design.md`](agent-design.md)，技能实现属 v0.3
- [x] 性能优化：`/server` 状态图渲染改为常驻复用（当时为 Chromium；v0.6 已整体替换
      为纯 Rust 渲染，见下），端到端 ~9s → ~1s 级；已随 2026-09-13 切流上线（源码 `d841d7b`）

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

## v0.4 —— 服务化（已完成，2026-09-29）

- [x] **子服务边界**：`service.rs` 单体拆为 `service/` 目录（`mod` 装配与生命周期 +
      `qq` / `chatroom` / `game` 消息路径分区），新增 `subsystem.rs` 的
      `Subsystem` trait（start/stop/health）——六个子服务（qq-bridge / chatroom-sync /
      game-link / http-api / player-events / command-responder）顺序 start、逆序 stop，
      健康快照汇入 `/healthz` 与 `/api/status` 的 `subsystems` 数组；行为零变化
- [x] **配置热重载（最小形态）**：SIGHUP → 重读配置 → `chatroom.group_ids` 白名单
      与 `log_level` 热生效（tracing reload 过滤层；RUST_LOG 优先时不覆盖）；
      其余差异用打码后的 describe 摘要做字段级 diff 并记日志「重启生效」；
      重载失败（JSON 非法等）不影响运行中的服务
- [x] **结构化日志**：`log_format: "json"`（默认 `text` 不变），tracing-subscriber
      JSON 格式，便于接入日志采集
- [x] **`/metrics`**：Prometheus 文本格式，与 `/healthz` 同在 6199 回环端口——
      转发/失败计数（qq_forwarded / game_forwarded / skipped_duplicate 等）、
      连接状态 gauge（onebot / chatbridge 的 enabled 与 connected 分开）、
      按命令名的响应计数

## v0.5 —— Mojang 版本更新播报（代码已完成，2026-09-29；线上启用待办）

- [x] **数据源**：轮询官方 feed → 检测新版本。**实测修正**：需求原定的
      `launchercontent.mojang.com/javaPatchNotes.json`（v1）2024 年起已冻结
      （最新停在 1.20.4-rc1），改用 **v2 端点** `/v2/javaPatchNotes.json` 为默认
      （`feed_url` 可配置）；v2 列表条目无正文（只有 shortText），检测到新版本后
      按 contentPath 从 feed 同目录按需拉取；响应可能带 UTF-8 BOM 已容忍。
      判定：列表最新在前，前缀中未播报的条目即新版本；**首启只记基线不播报**
      （防上线风暴）；按从旧到新播报；发送失败不标记、下轮重试（保持时间顺序）。
      播报状态持久化在 `state.json` 的 `announced_patches`（保留 50 条）
- [x] **翻译**：复用 `agent.llm`，提示词强制保留 HTML 结构、术语用官方中文译名；
      未配置 / 失败 → 只发原文（不阻塞播报）
- [x] **长图**：译后 / 译前两张长图，纯 Rust 渲染（cosmic-text 整形 → SVG → resvg，
      黑底白字，900px 宽）；HTML 粗剥离为文本块，渲染失败 → 降级为纯文本摘录节点
- [x] **打包**：合并转发聊天记录（`send_group_forward_msg`，节点 = 标题信息 /
      译后 / 原文 / 官方链接，节点昵称区分内容）
- [x] **发送**：发到 `chatroom.group_ids` 白名单群（复用现有白名单，零新增配置）；
      作为 `patch-broadcast` 子服务接入 v0.4 的统一生命周期与健康检查
- [ ] **线上验证**：测试群实际显示效果（合并转发节点、图片长图、发送频率）
- 待定项维持：`/patch` 手动重发命令未实现；单篇原文直链未做（minecraft.net
  文章 slug 不可稳定推导，链接节点统一指向官方总览页）

## v0.5+ —— Web 控制台（2026-09-29 完成首版：`webui/`）

- [x] **中间层后端**（Node.js + Express，`webui/server.js`）：登录鉴权（内存会话 +
      登录限速）、config.json 读写（敏感字段脱敏下发 `{is_set}`、保存空值保留原值、
      按结构校验、挂载点检测 + 原子落盘——单文件 bind mount 自动原地写保证
      restart 可见）、/api/status|health|messages|relay 代理（token 后端注入）、
      SSE 状态推送（2s）、操作日志（不含敏感值，200 条）
- [x] **前端**（原生 HTML/CSS/JS，无框架）：Win95/98 风格控制台——配置面板
      （按 config 顶层字段分组、敏感字段「已设置/未设置 + 修改」）、状态监视
      （LED + 子系统健康 + 计数，SSE 断线回退轮询）、消息列表、relay 下发、
      操作日志；800px 定宽居中
- [ ] **线上部署**：主 compose 补 `127.0.0.1:8199:8199` 映射 + `webui/docker-compose.yml`
      起 console（挂 config.json 读写 + docker.sock），步骤见 `webui/README.md`
- [ ] **可选增强**：消息实时推送（SSE 数据源换主服务 `/api/events`）、反代 TLS、
      多用户审计

## v0.6.0 —— 渲染去 Chromium（已完成，2026-09-30）

接续 v0.5：`/server`、`/status` 状态图与 v0.5 播报长图原本都走 headless Chromium
截图，本轮整体迁移到**纯 Rust 渲染管线**（`cosmic-text` 整形 → glyph path → SVG →
`resvg` 光栅化），彻底移除 Chromium 常驻。

- [x] **状态图**：`services/status_render.rs` 重写——文本用 cosmic-text 完整整形后
      逐 glyph 转 `<path>`（测量与渲染同源），毛玻璃改为面板内局部高斯模糊
      （1/4 降采样、面板外保留原图）；宽度复刻 CSS `fit-content`
- [x] **播报长图**：改黑底白字极简排版（900px 宽、高度自适应）；HTML 粗剥离为文本块
      （不引解析库），渲染失败自动降级为纯文本节点，不丢消息
- [x] **依赖**：删除 `headless_chrome` 与 `status-image` feature；新增
      `cosmic-text` / `resvg` / `imageproc`
- [x] **资源**：容器内存上限 1.2G → 256M；进程空载 **70MB → 8MB**、渲染峰值
      **105MB**、二进制 **16.9MB**；运行镜像只需中文字体（`fonts-noto-cjk`），
      不再需要 Chromium
- [x] **文档**：清理 Chromium / `status-image` 残留；包版本号 `0.2.0` → `0.6.0`

## 更远

- [ ] Publish 到 PyPI，支持 `pip install` 后以库的形式嵌入其他服务
- [ ] 插件化：其他社区服务以子进程 / 插件方式挂到本仓库的运行时

## 非目标

- 不做通用聊天机器人：AI 部分只在「需要查证的领域问答」上开放。
- 不接管游戏服管理（重启、发物品等）——只读。
- 不修改 chatroom 服务端（本仓库只做客户端）。
