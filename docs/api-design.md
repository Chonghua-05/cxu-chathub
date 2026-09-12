# HTTP API 设计（Web UI / 外部站点调用）

`rust/src/api/` 提供一个**独立于 OneBot WS（6199）的 HTTP API 服务**，是 Web UI
和社区其他网站调用本服务的唯一入口。默认只绑回环 `127.0.0.1:8199`，配置段 `api`
可开关（`enabled` 默认 `false`，需显式开启）。

## 端点

| 方法 | 路径 | 鉴权 | 说明 |
|------|------|------|------|
| GET | `/api/health` | 无 | 存活检查：`{status, version, uptime_secs}` |
| GET | `/api/status` | 无 | 全量状态：OneBot/ChatBridge 连接、各子系统统计、state 快照、**capabilities 能力清单** |
| GET | `/api/messages?limit=N` | 无 | 近期消息环形缓冲（默认 50，上限 200，仅内存、重启清空） |
| POST | `/api/relay` | **token** | 按目标端下发消息，`{"target": "qq"\|"game"\|"chatroom", "text": "...", "group_id": 可选}` |

`POST /api/relay` 返回 `{"ok": bool}`（200）或 `400`（参数错误）/`403`（鉴权失败）。
`target=qq` 且缺省 `group_id` 时发给所有配置群；投递失败（如 QQ 未连接）是 `ok:false`，
不是 HTTP 错误——方便 UI 轮询展示。

## 鉴权模型

- **读接口无鉴权**（与 `/healthz` 同级，只暴露状态与近期消息，无群号/token 等敏感信息）。
- **写接口必须 token**：`Authorization: Bearer <token>` 或 `X-API-Token: <token>`；
  `api.access_token` 为空时写接口一律 403 并在启动日志提示。
- 默认回环绑定 + 空 token = 只读模式，零配置安全；要跨机调用时：配置 token →
  评估是否真要公网暴露（优先用反代加 TLS 与更严格的源限制）。

## CORS

全部路由带 `Access-Control-Allow-Origin: *`（读接口对浏览器直接开放；写接口由 token
保护）。**建议**：Web UI 的前端不要把 relay token 下发到浏览器，而是让 UI 的后端
（或反代）持有 token 转发调用。

## 与其他扩展点的关系

- `/api/status` 的 `capabilities` 字段来自 `router::CommandHandler::info()` 元数据——
  与未来 agent 智能路由共用同一份能力发现（见 `docs/agent-design.md`）。
- `/api/relay` 走 `Hub` 出站抽象，与 `!q`/快照中继同一条路：新增目标端时 API 自动可用。
- `/api/messages` 的数据源是 `service.rs` 的 `RecentLog`（三端入站消息在统一入口处记录）；
  将来做消息持久化/全文检索时在此处替换或扩展。

## 未来 Web UI 的接入路径

1. UI 独立开发（任意前端栈），开发期直接跨域调 `127.0.0.1:8199`；
2. 部署期两种形态：反代（nginx/caddy）把 `/api/*` 指到 8199，或 UI 静态文件由
   反代托管、API 同域反代——都不需要改本服务；
3. 若需要服务端推送（实时刷新），后续在 `api/` 增加 `GET /api/events`（SSE），
   数据源同为 `RecentLog`，接口契约不破坏。

## 测试

`rust/tests/api_http.rs`：health/status/messages 读接口、relay 的 403/400/ok 语义、
CORS 预检、token 未配置时的强制只读。
