# 架构与数据流

单进程 asyncio 服务，所有子系统跑在同一个事件循环里，共享一份内存态 + 一个 `state.json`。
没有外部数据库、没有消息队列、没有 AI 依赖。

## 进程与生命周期（`main.py`）

1. `load_config()` 读取 `config.json`（**只在启动时读一次，无热重载**）。
2. 构造 `ChatroomBridge`：装配 OneBot WS 服务端、Forward API 客户端、ChatBridge 客户端。
3. `start()` 并发拉起：
   - OneBot WS 服务（`aiohttp` Web 服务，`/ws` + `/healthz`）
   - chatroom 读方向轮询循环（`_chatroom_loop`）
   - 玩家状态轮询循环（`_player_loop`，仅在 `player_tracking_enabled` 时）
   - ChatBridge 连接与接收循环
4. `stop()` 反向关停：先停轮询，再断 WS，最后落盘 `state.json`。

## 数据流

### A. QQ 群 → chatroom

```
NapCat ──WS message.group──▶ onebot.py 解析成 GroupMessage
      ──▶ bridge.py：文本/图片/引用分类、本地去重（state.forwarded）
      ──▶ forward_api.py：POST /api/forward/channels/{id}/messages
           附件先 POST /api/forward/channels/{id}/upload → attachment_ids
      ──▶ chatroom 落库（服务端不去重，重复提交会产生新消息）
```

- 去重键为 `source_message_id`（QQ 侧消息 ID），重复提交直接丢弃。
- 图片下载后上传，≤10MB，可执行扩展名会被服务端拒绝。

### B. chatroom → QQ 群 / 游戏

```
chatroom_read.py 每 poll_interval 秒 GET /api/channels/{id}/messages
      ──▶ 与 state 中的读游标比对，取增量
      ──▶ 解析 !q <内容>：qq_forward_enabled → NapCat send_group_msg
                          qq_to_game_enabled  → chatbridge.send()
```

- 游标持久化在 `state.json`，重启不重复推送。
- `!q` 由人工触发，不做防抖。

### C. 游戏 ↔ chatroom（ChatBridge）

```
chatbridge.py ── AES-CBC over TCP(21027) ──▶ MC 服务端插件
    入向：on_game_chat(sender, author, text)
    出向：!q 触发的文本、玩家上下线事件
```

- 帧格式：4 字节大端长度前缀 + AES-CBC 密文，与旧旧框架插件协议一致。
- `name` 字段标识来源（默认 `web`），服务端据此区分客户端。

### D. 玩家上下线

```
player_tracker.py 轮询状态 API→ 玩家快照
      ──▶ 与上一快照做差集 → PlayerEvent(上线/下线)
      ──▶ 状态驱动防抖（连续 debounce_count 次快照一致才上报）
      ──▶ chatbridge 推送到 chatroom（source: "game"）
```

防抖的意义：MC 服务端统计存在抖动（假上线/假下线），快照差集必须先稳定再上报。

### E. 群命令

```
群内 "/chatroom" 或 "/server"
      ──▶ onebot.py 收到 → commands.py 解析（含 group_allow_all / allow_from 鉴权）
      ──▶ /chatroom：查询语音频道在线人数 → 文本回复
      ──▶ /server：status_render.py 渲染 HTML → PNG（Playwright/Chromium）
                    成功 → base64:// 图片；失败 → 自动回退文本
```

- 图片走 `base64://`，容器与宿主机不需要共享文件系统。
- `/status` 是 `/server` 的兼容别名。

## 状态文件（`state.py`）

`state.json` 保存三样东西：

| 键 | 用途 |
|----|------|
| `forwarded` | 已转发的 `source_message_id` 环形表（默认 2000 条） |
| `cursor` | chatroom 读方向游标（最后处理的消息 ID / 时间） |
| `refresh_token` | 用户 JWT 轮换后的最新 refresh token |

写入策略：**临时文件 + 原子 rename**；读取时若 JSON 损坏则丢弃重建，不让一个坏文件卡死启动。

## 并发与失败处理

- 每个子系统独立循环，单个循环抛异常只记录日志并退避重试，不拖垮整个进程。
- 转发失败不重试跨平台：写方向失败仅记日志（避免重复写入 chatroom）。
- ChatBridge 断线后按退避重连。
- 内存常驻约 40MB；开启状态图渲染时 Chromium 会额外占用，容器内存上限 1.2G、`shm_size` 512m。

## Rust 版分层（`rust/`，v0.2 起）

Rust 版复刻以上数据流，并把「消息如何被认领」收敛为一个统一路由层：

```
QQ 群事件 ─┐
游戏聊天 ──┼─▶ InboundMessage(source, text) ─▶ CommandRouter ─▶ CommandHandler 依次认领
chatroom ──┘                                       │   /chatroom /server  → SlashCommandAdapter
                                                   │   !q                → QqForwardRelay（跨端中继）
                                                   │   !snap             → SnapshotRelay
                                                   └─ 未认领 → 原有流水线（QQ 消息进 forwarder）
```

三个关键抽象（是未来 agent 能力的接缝，见 `docs/agent-design.md`）：

- **`ReplySink`**：回复到消息来源端（QQ 群 / 游戏广播 / chatroom 频道），handler 不感知传输。
- **`Hub`**：跨端中继出口（向所有配置群发文本、游戏广播、写 chatroom）。
- **`CommandHandler` + `CommandInfo`**：处理器与其元数据（名称 / 别名 / 触发形态 / 描述）。
  `!mc` / `!wiki` / `!tmc` 等新技能各实现一个并注册即可接入三端；
  未来智能路由（LLM 选取 handler）只替换 `CommandRouter` 的匹配策略，接口不动。

对外还留了第二个接缝：`api/` 模块提供**独立 HTTP API**（默认回环 8199，配置段 `api`），
读接口（状态 / 近期消息环形缓冲）+ token 保护的写接口（`POST /api/relay` 走 `Hub`），
供 Web UI 与社区其他网站调用，并带 CORS；设计见 `docs/api-design.md`。
三端入站在统一入口处写入 `RecentLog`（内存环形缓冲），是 API 与未来 UI 的数据源。

出站协议仍是 `adapters/` 下的五个互不感知的适配器；`service.rs` 负责装配与三个轮询循环，
与 Python 版 `main.py` 逐行为对应。

## 为什么这么切模块

`onebot.py` / `forward_api.py` / `chatbridge.py` 三个协议适配器互不感知，各自只管字节流与
协议语义；`bridge.py` 是策略层（什么该转发、怎么去重）；`main.py` 只做装配。
这样新增一个协议（例如以后的 Agent 通道）只需要新增一个适配器 + 在 `main.py` 挂上，
不用改策略层。
