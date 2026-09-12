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

## `agent` —— 检索问答技能（Rust 版）

命令与文档源**全部由配置注册**，新增命令不需要改代码（见 [`agent-design.md`](agent-design.md)）。

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `enabled` | bool | `false` | agent 技能总开关 |
| `llm` | object / null | `null` | OpenAI 兼容 `/chat/completions`：`api_url` / `api_key` / `model` / `timeout_secs`(30) / `max_answer_chars`(1000) / `system_prompt`。未配置或调用失败时自动降级为纯检索摘录 |
| `skills[]` | list | `[]` | 命令声明：`name`（如 "mc"）、`trigger`（默认 `!{name}`）、`description`、`max_results`(5)、`sources[]` |
| `skills[].sources[]` | list | 必填 | 三种类型见下表；可多个，结果合并并标注来源 |

数据源类型：

| `type` | 字段 | 说明 |
|--------|------|------|
| `local` | `root`（目录）、`extensions`（白名单，空=内置默认集）、`name` | 本地目录检索；md 按标题分节、代码按行窗分块，IDF+路径分词+文件级聚合；出处=`文件:行号区间` |
| `mediawiki` | `api_url`（…/api.php）、`name` | MediaWiki 站点两步查询；出处=条目 URL |
| `repo` | `repo`（"owner/name" 或完整 tarball URL）、`branch`(main)、`subdir`（如 mdBook 的 "src"）、`site_url`（出处映射，如 https://minecraftdocs.dev ）、`name` | GitHub 仓库文档：tarball 下载到系统临时目录缓存（24h 刷新，失败回退旧缓存），委托本地检索；出处=站点页面 URL |

中文问题对英文语料的检索：配置了 `llm` 时自动把问题翻译成英文关键词（MC 术语用官方
英文名），原文与译文各查一遍按出处去重合并；无 LLM 时只用原文查询。

示例：`!mc` 接本地 MC 源码副本、`!wiki` 接 zh.minecraft.wiki、`!tmc` 同时接本地
techmc 文档与云端站点、`!docs` 接云端 MinecraftDocs——见 `config.example.json` 的
`agent` 段。检索质量调试工具：`cargo run --release --example doc_query -- <目录> <查询词> [扩展名]`。

## 顶层

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `state_path` | str | `/data/state.json` | 状态文件路径（去重表 / 读游标 / refresh_token） |
| `log_level` | str | `INFO` | `DEBUG` / `INFO` / `WARNING` / `ERROR` |

## 安全约定

- 所有密钥只从配置文件读取；`describe()` 输出的配置摘要会自动打码。
- `config.json` 已在 `.gitignore` 中，**不要**提交、不要贴到聊天或 issue 里。
- 修改 `config.json` 后必须**重启进程**：配置只在启动时加载一次，没有热重载接口。
