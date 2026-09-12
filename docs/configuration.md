# 配置项参考

配置文件默认路径：容器内 `/app/config.json`（`--config` 可覆盖）。
从 [`config.example.json`](../config.example.json) 复制一份开始。

**token / 密钥类字段留空即表示不启用对应能力，代码不会回退到任何内置默认值。**

## `onebot` —— 接入 NapCat

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `listen_host` | str | `0.0.0.0` | 监听地址。对外只应通过 `-p 127.0.0.1:6199:6199` 暴露 |
| `listen_port` | int | `6199` | 反向 WS 端口，需与 NapCat 客户端配置一致 |
| `path` | str | `/ws` | WS 路径 |
| `access_token` | str | `""` | 必须与 NapCat 中该客户端的 token 一致 |
| `self_id` | int | `0` | 机器人 QQ 号，用于过滤自身消息 |

## `chatroom` —— chatroom 服务端

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `base_url` | str | `https://chatroom.example.com` | 服务端根地址 |
| `channel_id` | int | `1` | 目标频道，必须是 `FORWARD` 类型 |
| `forward_token` | str | `""` | **唯一必需的 token**，写方向 `/api/forward/*` 用 |
| `refresh_token` | str | `""` | 用户 JWT 的 refresh token；轮换后自动写回 `state.json` |
| `voice_api` | str | `https://chatroom.example.com/api/voice/qqbot/get_voice_channel_people` | `/chatroom` 查询语音频道在线人数用的接口 |
| `status_api` | str | `https://status.example.com/api/qqbot/status` | `/server` 与玩家追踪用的状态接口 |
| `server_addresses` | [label, value][] | `[["主IP", "game.example.com"]]` | 状态图里展示的服务器地址列表 |
| `group_ids` | int[] | `[]` | 允许同步的 QQ 群号列表（白名单） |
| `poll_interval` | int | `10` | 读方向轮询间隔（秒） |
| `debounce_count` | int | `2` | 同一条消息需连续出现在几次快照中才上报（防抖） |
| `qq_sync_enabled` | bool | `true` | QQ 群 → chatroom 总开关 |
| `qq_forward_enabled` | bool | `true` | chatroom → QQ 群（`!q`）开关 |
| `qq_to_game_enabled` | bool | `true` | `!q` 同时转发到游戏内 |
| `player_tracking_enabled` | bool | `false` | 玩家上下线推送开关 |
| `snapshot_sender` | str | `snapshot` | 快照消息的发送者标识 |
| `snapshot_prefix` | str | `!snap` | 快照消息前缀 |

## `chatbridge` —— 与 MC 服务端通信

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `enabled` | bool | `true` | 是否连接 ChatBridge |
| `host` / `port` | str / int | `""` / `21027` | 游戏侧地址 |
| `name` | str | `web` | 客户端标识（服务端用于区分来源） |
| `password` | str | `""` | 登录口令 |
| `aes_key` | str | `""` | AES-CBC 密钥，需与服务端一致 |

## `commands` —— 群内命令

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `group_allow_all` | bool | `true` | `true` 时所有群可用；`false` 时仅 `allow_from` |
| `allow_from` | int[] | `[]` | 允许使用命令的群号 |
| `status_image` | bool | `false` | `/server` 是否发图（需构建镜像时 `STATUS_IMAGE=true`） |

## `api` —— HTTP API（Rust 版，Web UI / 外部站点调用）

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `enabled` | bool | `false` | 启动独立 HTTP API（与 OneBot 的 6199 隔离） |
| `listen_host` | str | `127.0.0.1` | 默认只绑回环；对外暴露前务必先配 `access_token` |
| `listen_port` | int | `8199` | API 端口 |
| `access_token` | str | `""` | 写接口（`POST /api/relay`）必需；为空时写接口一律 403。端点与鉴权见 [`api-design.md`](api-design.md) |

## 顶层

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `state_path` | str | `/data/state.json` | 状态文件路径（去重表 / 读游标 / refresh_token） |
| `log_level` | str | `INFO` | `DEBUG` / `INFO` / `WARNING` / `ERROR` |

## 安全约定

- 所有密钥只从配置文件读取；`describe()` 输出的配置摘要会自动打码。
- `config.json` 已在 `.gitignore` 中，**不要**提交、不要贴到聊天或 issue 里。
- 修改 `config.json` 后必须**重启进程**：配置只在启动时加载一次，没有热重载接口。
