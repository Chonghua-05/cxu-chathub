# cxu-chathub

> 本社区消息中枢：让 **QQ 群**、**chatroom 频道** 与 **MC 游戏内聊天** 三端互通，
> 并为后续的社区服务与 Agent 能力提供常驻底座。

前身是某个聊天机器人框架的 chatroom 追踪插件。现已独立为**无 AI 依赖**的单进程
asyncio 服务，跑在云主机 Docker 上，与旧框架和 AI 服务完全解耦。

- 仓库名：`cxu-chathub`
- Python 包名：`chatroom_bridge`，容器名 `chatroom-bridge`（暂未改名，避免打断线上部署）
- 版本：`0.1.0`

---

## 它能做什么

| 方向 | 说明 |
|------|------|
| QQ 群 → chatroom | 群消息（文本 / 图片 / 引用）经官方 Forward Bot API 写入 FORWARD 频道 |
| chatroom → QQ 群 | 频道内 `!q xxx` 转发到 QQ 群；游戏内 `!q xxx` 同样转发 |
| chatroom ↔ 游戏 | ChatBridge（AES-CBC over TCP，端口 21027）双向同步 |
| MC 玩家上下线 | 轮询状态 API（可配置），带防抖地把上下线事件推送到 chatroom |
| QQ 命令 | `/chatroom`（语音频道在线）、`/server`（状态图，渲染失败自动回退文本；`/status` 为兼容别名） |

设计要点：

- **零 AI 依赖**：纯 aiohttp + pycryptodome，常驻内存约 40MB（开启状态图渲染时容器上限 1.2G）。
- **单进程、无状态外部依赖**：去重表与读游标落在 `state.json`（原子写、损坏自愈）。
- **复用已有链路**：直接监听 `127.0.0.1:6199`，即 NapCat 中「NapCat 客户端」客户端原本指向的地址，
  切换时不需要动 NapCat 配置、不需要放行新端口。

---

## 架构

```
        ┌────────────┐        ┌──────────────┐
        │  QQ 群      │        │ NapCat 客户端 │
        └─────┬──────┘        └──────┬───────┘
              │ 群事件                │ OneBot v11 反向 WS
              │                      │ 127.0.0.1:6199/ws
              ▼                      ▼
        ┌───────────────────────────────────────────┐
        │            cxu-chathub（本服务）            │
        │  onebot.py ── bridge.py ── forward_api.py  │
        │  chatroom_read.py ─ chatroom_auth.py       │
        │  chatbridge.py ─ player_tracker.py         │
        │  commands.py ─ status_render.py ─ state.py │
        └──────┬──────────────────────┬──────────────┘
               │ Forward Bot API       │ ChatBridge (AES/TCP 21027)
               ▼                       ▼
        ┌─────────────┐         ┌─────────────┐
        │  chatroom    │         │  MC 服务端   │
        │  目标频道     │         │  (游戏内聊天) │
        └─────────────┘         └─────────────┘
```

详见 [`docs/architecture.md`](docs/architecture.md)。

---

## 快速开始

```bash
git clone <repo-url> cxu-chathub && cd cxu-chathub

# 1) 配置（token 由你自己填，不要提交进 git）
cp config.example.json config.json
#    chatroom.forward_token        ← chatroom 官方给 bot 的静态 token（唯一必需）
#    chatbridge.password / aes_key ← 服务端分配
#    onebot.access_token           ← 与 NapCat 客户端的 token 一致
#    commands.status_image         ← true 表示 /server 发图（构建时需带 Chromium）
#    chatroom.voice_api / status_api / server_addresses
#                                  ← 你自己的服务地址（示例值必须覆盖）

# 2) 本地开发 / 测试
uv venv .venv && uv pip install --python .venv/bin/python aiohttp pillow pycryptodome pytest
PYTHONPATH=src .venv/bin/python -m pytest -q tests

# 3) 运行（容器内路径为 /app/config.json）
PYTHONPATH=src .venv/bin/python -m chatroom_bridge.main --config ./config.json
```

健康检查：`curl -s http://127.0.0.1:6199/healthz`

Docker 部署与切流顺序见 [`docs/deployment.md`](docs/deployment.md)；
全部配置项见 [`docs/configuration.md`](docs/configuration.md)。

---

## Rust 版（`rust/`，当前开发重心）

v0.2 起本服务用 **Rust 重写**（省内存：常驻约 10MB 级 vs Python 的 40MB + Chromium），
与 Python 版**行为逐一对齐**，`config.json` / `state.json` 格式完全兼容，切换时
NapCat 与数据目录零改动。Python 版保留至切流验收完成（见 `docs/roadmap.md`）。

| 模块 | 作用 |
|------|------|
| `adapters/onebot.rs` | OneBot v11 反向 WS 服务端（axum）：token 校验、echo 动作调用、事件队列 + 独立消费者、`/healthz` |
| `adapters/forward_api.rs` | Forward Bot API 客户端（reqwest，HTTP 201 校验、上传黑名单与大小限制） |
| `adapters/chatroom_auth.rs` | 用户 JWT 刷新与轮换持久化 |
| `adapters/chatroom_read.rs` | 读方向轮询、读游标、`!q` 解析 |
| `adapters/chatbridge.rs` | ChatBridge 客户端（4 字节长度前缀 + AES-CBC；与 Python 版逐字节对拍） |
| `services/forwarder.rs` | QQ 群 → chatroom 流水线（去重 / 图片压缩 / 引用回填） |
| `services/player_events.rs` | 玩家上下线推送（ChatBridge 系统广播 + 可配置正则，事件驱动不丢条目） |
| `services/commands.rs` / `status_render.rs` | `/chatroom` `/server` 命令与状态图（Chromium 渲染由 `status-image` feature 门控，自动回退文本） |
| `router/` | **统一消息路由**：三端入站汇入 `CommandRouter`，`/命令`、`!q`、`!snap` 均为注册其上的 `CommandHandler` |
| `agent/` | **agent 能力（已实现）**：命令与文档源全部由 `agent.skills` 配置注册——`LocalDocSource`（本地目录，文件:行号出处）+ `MediaWikiSource`（云端 api.php）+ 通用 `DocQuerySkill`（LLM 整理、失败自动降级摘录）；设计见 [`docs/agent-design.md`](docs/agent-design.md) |
| `api/` | **独立 HTTP API**（默认 `127.0.0.1:8199`）：状态/近期消息读接口 + token 保护的 `/api/relay` 写接口，带 CORS——Web UI 与其他站点调用的入口（见 [`docs/api-design.md`](docs/api-design.md)） |
| `service.rs` / `main.rs` | 服务装配与生命周期（`--config`） |

```bash
cd rust
cargo test                 # 122 个测试（单元 + WS 集成 + e2e 冒烟）
cargo build --release
./target/release/chatroom-bridge --config ./config.json
# 状态图渲染变体：cargo build --release --features status-image（需系统 Chromium）
```

Agent 能力（`!mc` / `!wiki` / `!tmc`，下一阶段）的扩展点设计见
[`docs/agent-design.md`](docs/agent-design.md)：新技能 = 一个 `CommandHandler` +
一个 `DocumentSource`，注册进路由即可接入三端；未来智能路由（LLM）只替换匹配策略，
不动 handler 与出站接口。

---

## 模块一览（Python 版，切流前保留）

| 文件 | 作用 |
|------|------|
| `onebot.py` | OneBot v11 反向 WS 服务端：token 校验、群事件解析、动作调用（`send_group_msg` 等）、`/healthz` |
| `forward_api.py` | 官方 Forward Bot API 客户端（Bearer 静态 token，写方向） |
| `chatroom_auth.py` | 用户 JWT：`/api/auth/refresh` 换 token，轮换后持久化，解析 exp / user_id |
| `chatroom_read.py` | 读方向轮询 `/api/channels/{id}/messages`，维护读游标，解析 `!q` |
| `chatbridge.py` | ChatBridge 客户端（4 字节长度前缀 + AES-CBC，与旧插件协议一致） |
| `player_tracker.py` | 在线玩家快照比对 + 状态驱动防抖，产生上下线事件 |
| `bridge.py` | QQ 群 → chatroom 流水线（文本 / 图片上传 / 引用 / 本地去重） |
| `commands.py` | 命令解析与格式化（`/chatroom`、`/server`） |
| `status_render.py` | `/server` 状态图的 HTML→PNG 渲染（Playwright/Chromium） |
| `state.py` | 去重表、读游标、refresh_token 持久化（原子写，损坏自愈） |
| `config.py` | 配置加载与校验（token 只从文件读，绝不写日志） |
| `main.py` | 服务装配与生命周期（`--config`） |

---

## 已知约束

- chatroom 服务端**不去重**：同一 `source_message_id` 重复提交会产生新消息，去重由本服务负责
  （`state.json` 的 `forwarded` 表，默认保留 2000 条）。
- 目标频道必须已转成 `FORWARD` 类型；该类型下普通用户只读，写入只能走 `/api/forward/*`。
- 附件先上传（≤10MB，可执行扩展名被拒）再用 `attachment_ids` 挂到消息上，不挂会留孤儿附件。
- 作者映射由服务端决定：QQ 号按 SSO 邮箱 `<qq>@qq.com` 匹配平台账号；匹配不到则以 bot 账号发布并显示
  `昵称(未知用户)`。游戏侧用 `sender.username` 匹配。
- 游戏内聊天与玩家上下线事件按 `source: "game"` 提交，属性归属由服务端映射决定
  （与旧插件「以 bot 身份发文本」不同，属于有意变更）。
- `/status` 图片以 `base64://` 交给 NapCat 发送，容器与宿主机无需共享文件系统。

---

## 测试

```bash
PYTHONPATH=src .venv/bin/python -m pytest -q tests
```

覆盖：配置解析、状态持久化（含损坏恢复）、OneBot 事件解析、Forward API 请求形状与错误处理、
转发去重、玩家事件防抖、读游标与 `!q` 解析、命令与状态图模板，以及一条真实 WS 的端到端装配烟测。

---

## 安全

- `forward_token` / `refresh_token` / `chatbridge.password` / `aes_key` / `onebot.access_token`
  只从 `config.json` 读取：**不写日志、不进 git、不出现在聊天里**（`describe()` 打摘要时自动打码）。
- `config.json`、`state.json`、`data/` 已在 `.gitignore` 中；提交前请再确认一次（`git status`）。
- HTTP 服务只绑宿主机回环 `127.0.0.1:6199`，不对外暴露端口。
- 端口 `6199` 若被旧框架容器占用，需先停掉旧框架（见部署文档）。

---

## Roadmap

本仓库会逐步长成 本社区 的社区服务底座（多服务 + 有限 Agent 能力），规划见
[`docs/roadmap.md`](docs/roadmap.md)。近期方向：

1. 服务拆分：桥接 / 玩家状态 / 命令响应各自独立，统一生命周期与健康检查。
2. Agent 能力：按玩家需求检索 MC 源码与 Wiki、回答游戏机制问题。
3. 统一配置与鉴权：一份配置驱动所有子服务，密钥只在启动时读取。

---

## License

MIT，见 [`LICENSE`](LICENSE)。
