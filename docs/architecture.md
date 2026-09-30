# 架构与数据流

单进程 Rust 服务（tokio 异步运行时），所有子系统跑在同一运行时里，共享一份内存态 +
一个 `state.json`。没有外部数据库、没有消息队列、没有 AI 依赖。

## 进程与生命周期（`main.rs` / `service/`）

1. `load_config()` 读取 `config.json`（**只在启动时读一次，仅 `group_ids` / `log_level`
   支持 SIGHUP 热重载**）。
2. `BridgeService::new()` 装配全部子系统：OneBot WS 服务端、Forward API 客户端、
   ChatBridge 客户端、chatroom 读轮询、命令行、HTTP API、播报子系统。
3. `start()` 顺序拉起注册进 `subsystem.rs` 的 `Subsystem` 清单（顺序 start、逆序 stop）：
   - OneBot 反向 WS 服务（`adapters/onebot.rs`，axum，`/ws` + `/healthz` + `/metrics`）
   - chatroom 读方向轮询（`service/mod.rs` 的 `ChatroomPollSubsystem`）
   - ChatBridge 连接与接收循环（`adapters/chatbridge.rs`）
   - 玩家上下线推送、命令响应、独立 HTTP API（`api/`）
   - Mojang 版本更新播报（`services/patch_broadcast.rs`）
4. `stop()` 逆序关停，最后落盘 `state.json`。

健康快照由各子服务汇入 `/healthz` 与 `/api/status` 的 `subsystems` 数组。

## 数据流

### A. QQ 群 → chatroom

```
NapCat ──WS message.group──▶ adapters/onebot.rs 解析成 GroupMessage
      ──▶ services/forwarder.rs：文本/图片/引用分类、本地去重（state.forwarded）
      ──▶ adapters/forward_api.rs：POST /api/forward/channels/{id}/messages
           附件先 POST /api/forward/channels/{id}/upload → attachment_ids
      ──▶ chatroom 落库（服务端不去重，重复提交会产生新消息）
```

- 去重键为 `source_message_id`（QQ 侧消息 ID），重复提交直接丢弃。
- 图片下载后上传，≤10MB，可执行扩展名会被服务端拒绝。

### B. chatroom → QQ 群 / 游戏

```
adapters/chatroom_read.rs 每 poll_interval 秒 GET /api/channels/{id}/messages
      ──▶ 与 state 中的读游标比对，取增量
      ──▶ 解析 !q <内容>：qq_forward_enabled → NapCat send_group_msg
                          qq_to_game_enabled  → chatbridge.send()
```

- 游标持久化在 `state.json`，重启不重复推送。
- `!q` 由人工触发，不做防抖。

### C. 游戏 ↔ chatroom（ChatBridge）

```
adapters/chatbridge.rs ── AES-CBC over TCP(21027) ──▶ MC 服务端插件
    入向：on_game_chat(sender, author, text)
    出向：!q 触发的文本、玩家上下线事件
```

- 帧格式：4 字节大端长度前缀 + AES-CBC 密文，与旧插件协议逐字节对齐。
- `name` 字段标识来源（默认 `web`），服务端据此区分客户端。

### D. 玩家上下线（ChatBridge 事件驱动）

```
MC 服务端插件 ──ChatBridge 系统广播──▶ on_game_chat（author 为空）
      ──▶ services/player_events.rs 正则识别（player_join_pattern / player_quit_pattern）
      ──▶ 提取玩家名 → QQ 群推送「🎮 xx 上线 / 🚪 xx 下线」
```

为什么不用状态网站轮询做差分：轮询天然可能丢掉单次事件（两次轮询之间上线又下线），
而上下线推送恰恰要求每一条都不丢——事件源就在 ChatBridge，直接用它。
防伪造：只认系统广播（author 为空）或玩家自报（author == 玩家名），
他人冒充「xx 加入了游戏」不会触发推送。`/server` 命令的数据仍来自状态网站——
那边只关心当前状态，不怕丢单次事件。

### E. 群命令

```
群内 "/chatroom" 或 "/server"
      ──▶ router：SlashCommandAdapter 认领 → services/commands.rs 解析
           （含 group_allow_all / allow_from 鉴权）
      ──▶ /chatroom：查询语音频道在线人数 → 文本回复
      ──▶ /server：services/status_render.rs 纯 Rust 渲染 → PNG
                    （cosmic-text 整形 → SVG → resvg 光栅化）
                    成功 → base64:// 图片；失败 → 自动回退文本
```

- 图片走 `base64://`，容器与宿主机不需要共享文件系统。
- `/status` 是 `/server` 的兼容别名。

## 状态文件（`state.rs`）

`state.json` 保存：

| 键 | 用途 |
|----|------|
| `forwarded` | 已转发的 `source_message_id` 环形表（默认 2000 条） |
| `cursor` | chatroom 读方向游标（最后处理的消息 ID / 时间） |
| `refresh_token` | 用户 JWT 轮换后的最新 refresh token |
| `announced_patches` | v0.5 版本播报已记录条目（保留 50 条） |

写入策略：**临时文件 + 原子 rename**；读取时若 JSON 损坏则丢弃重建，不让一个坏文件卡死启动。

## 并发与失败处理

- 每个子系统独立任务，单个任务抛异常只记录日志并退避重试，不拖垮整个进程。
- 转发失败不重试跨平台：写方向失败仅记日志（避免重复写入 chatroom）。
- ChatBridge 断线后按退避重连。
- 内存：空载常驻约 8MB、渲染峰值约 105MB；容器内存上限 256M。渲染为纯 Rust
  （无 Chromium），仅依赖中文字体 `fonts-noto-cjk`。

## 统一路由层与出站适配器

三端入站汇入统一路由，把「消息如何被认领」收敛为 `router` 一层：

```
QQ 群事件 ─┐
游戏聊天 ──┼─▶ InboundMessage(source, text) ─▶ CommandRouter ─▶ CommandHandler 依次认领
chatroom ──┘                                       │   /chatroom /server  → SlashCommandAdapter
                                                   │   !q                → QqForwardRelay（跨端中继）
                                                   │   !snap             → SnapshotRelay
                                                   │   !mc/!wiki/!tmc…   → agent 技能（配置注册）
                                                   └─ 未认领 → 原有流水线（QQ 消息进 forwarder）
```

三个关键抽象（是 agent 能力的接缝，见 `docs/agent-design.md`）：

- **`ReplySink`**：回复到消息来源端（QQ 群 / 游戏广播 / chatroom 频道），handler 不感知传输。
- **`Hub`**：跨端中继出口（向所有配置群发文本、游戏广播、写 chatroom）。
- **`CommandHandler` + `CommandInfo`**：处理器与其元数据（名称 / 别名 / 触发形态 / 描述）。
  `!mc` / `!wiki` / `!tmc` 等技能各实现一个并注册即可接入三端；
  智能路由（LLM 选取 handler）只替换 `CommandRouter` 的匹配策略，接口不动。

对外还留了第二个接缝：`api/` 模块提供**独立 HTTP API**（默认回环 8199，配置段 `api`），
读接口（状态 / 近期消息环形缓冲）+ token 保护的写接口（`POST /api/relay` 走 `Hub`），
供 Web UI 与社区其他网站调用，并带 CORS；设计见 `docs/api-design.md`。
三端入站在统一入口处写入 `RecentLog`（内存环形缓冲），是 API 与未来 UI 的数据源。

出站协议是 `adapters/` 下五个互不感知的适配器（onebot / forward_api / chatroom_auth /
chatroom_read / chatbridge）；`service/`（目录：`mod.rs` 装配与生命周期 +
`qq.rs` / `chatroom.rs` / `game.rs` 消息路径分区）负责装配与轮询循环。
生命周期与健康检查统一走 `subsystem.rs` 的 `Subsystem` trait：各子服务
（qq-bridge / chatroom-sync / game-link / http-api / player-events /
command-responder / patch-broadcast）顺序 start、逆序 stop，健康快照汇入 `/healthz`
与 `/api/status` 的 `subsystems` 数组；SIGHUP 热重载（group_ids / log_level）
与 `/metrics`（Prometheus 文本，6199 端口）也由这一层支撑。

## 为什么这么切模块

`adapters/onebot.rs` / `forward_api.rs` / `chatbridge.rs` 三个协议适配器互不感知，
各自只管字节流与协议语义；`services/forwarder.rs` 是策略层（什么该转发、怎么去重）；
`service/mod.rs` 只做装配。这样新增一个协议（例如以后的 Agent 通道）只需要新增一个
适配器 + 在装配处挂上，不用改策略层。
