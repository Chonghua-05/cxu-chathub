# Roadmap

本仓库的定位是 **本社区的消息与服务中枢**，长期会承载多类常驻服务和有限的 Agent 能力。
下面按「已经到的 → 下一步 → 更远」排列，不承诺时间点。

## v0.1

- [x] QQ 群 ↔ chatroom 双向同步（文本 / 图片 / 引用）
- [x] chatroom ↔ MC 游戏内聊天（ChatBridge，AES-CBC over TCP）
- [x] 玩家上下线事件推送（快照比对 + 防抖）
- [x] 群内 `/chatroom`、`/server` 命令（状态图带文本回退）
- [x] 去重表 / 读游标 / refresh token 持久化（原子写、损坏自愈）
- [x] 单元测试 + 一条真实 WS 装配烟测

## v0.2 —— Rust 重写（进行中）

当前工作重心：用 Rust 重写整套服务（`rust/`，包 `chatroom-bridge`），**行为对齐 Python 版**。
Python 版仍是线上实现；Rust 版验收通过后才切流，此前的条目均为待验收状态。

- [ ] 行为对齐：三端互通、群命令（`/chatroom` `/server`）、去重 / 游标 / refresh token
      持久化逐一对齐 Python 版
- [ ] 部署件：`rust/Dockerfile` 多阶段构建（`STATUS_IMAGE` 可选 Chromium 变体）+
      `docker-compose.yml` 的 `rust` profile（与 Python 版二选一，6199 端口冲突）
- [ ] 切流验收：`state.json` 与 Python 版互相兼容（`./data` 目录共用），切流后
      去重表与读游标不丢
- [ ] 架构预留：agent 能力扩展点固化（`router::CommandHandler` / `agent::DocumentSource`），
      设计见 [`docs/agent-design.md`](agent-design.md)，技能实现属 v0.3

## v0.3 —— Agent 能力（顺延，原 v0.2 项）

目标：让玩家在群里/游戏里用自然语言问游戏机制，由 Agent 查证后回答。
完整设计（命令 `!mc` / `!wiki` / `!tmc`、文档源与配置、智能路由预留）见
[`docs/agent-design.md`](agent-design.md)。

- [ ] **MC 源码检索**：按玩家问题定位 Minecraft（及服务端插件）源码位置并摘要，
      回答必须带文件/行级出处
- [ ] **Wiki 检索**：从权威 wiki 取机制说明，区分「官方行为」与「社区经验」
- [ ] 只读沙箱：Agent 只能读仓库/源码副本，不能改游戏服务端
- [ ] 回答落库：把问答对写回 chatroom，便于回溯与纠错
- [ ] 明确的能力边界与失败话术（查不到就说查不到，不编）

## v0.4 —— 服务化（顺延，原 v0.2）

- [ ] 拆出「桥接 / 玩家状态 / 命令响应」子服务边界，统一生命周期与健康检查
- [ ] 统一配置加载与校验，支持热重载（先做「SIGHUP 重读」这一最小形态）
- [ ] 结构化日志（JSON），便于以后接监控
- [ ] `/metrics`（Prometheus 文本格式）：转发计数、失败计数、连接状态

## 更远

- [ ] 多频道 / 多服务器支持（当前频道与群号是白名单硬配置）
- [ ] Publish 到 PyPI，支持 `pip install` 后以库的形式嵌入其他服务
- [ ] 插件化：其他社区服务以子进程 / 插件方式挂到本仓库的运行时

## 非目标

- 不做通用聊天机器人：AI 部分只在「需要查证的领域问答」上开放。
- 不接管游戏服管理（重启、发物品等）——只读。
- 不修改 chatroom 服务端（本仓库只做客户端）。
