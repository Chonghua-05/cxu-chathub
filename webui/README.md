# cxu-chathub Web 控制台

Windows 95/98 风格的配置管理与状态监视控制台。前端为**纯 HTML（零 CSS、零框架、零构建）**：布局靠 `<table>` 嵌套，配色靠 `bgcolor`/`<font>`/`<body link>` 属性，按钮与输入框用浏览器原生控件样式。

```
浏览器（Win95 风格前端）
   │  登录 Cookie（HttpOnly）
   ▼
中间层后端（本目录，Node.js + Express）
   │                    │
   │ 读写               │ 代理 /api/*（注入 api.access_token）
   ▼                    ▼
config.json        cxu-chathub（/api/status、/api/messages、/api/relay）
（写入后 docker restart + 轮询健康检查）
```

## 它能做什么

| 功能 | 说明 |
|------|------|
| 配置管理 | 读写 `config.json`：敏感字段（token/密钥）**脱敏下发**（只返回是否已设置），保存时**空值保留原值**；写入前按 config 结构校验（端口范围 / URL 形态 / 数组类型等） |
| 服务重启 | 配置写入后 `docker restart` 目标容器，并轮询健康检查直到就绪 |
| 状态监视 | SSE 每 2 秒推送 `/api/status`（断线自动回退 2 秒轮询）：LED 状态灯、子系统健康、转发计数 |
| 消息监视 | 轮询 `/api/messages`（最近 50 条） |
| 消息下发 | `/api/relay`（target=qq/game/chatroom），token 由后端注入，不下发浏览器 |
| 操作日志 | 时间 / 用户 / 操作 / 变更字段（不含敏感值）/ 结果，保留 200 条 |

## 安全设计

- **敏感值绝不进浏览器**：`forward_token` / `aes_key` / `access_token` / `password` /
  `api_key` 读取时只返回 `{ is_set: true/false }`；保存时空值或原样带回 = 保留原值。
- `config.json` 路径只从环境变量 `CXU_CONFIG_PATH` 读取，前端无法指定路径。
- 所有 `/api/*` 与 `/auth/logout` 需要登录（HttpOnly Cookie 会话，8 小时滑动续期）。
- 登录失败限速：同 IP 连续失败 5 次锁定 60 秒。
- 操作日志不记录任何敏感值。

## 快速开始（本机调试）

```bash
cd webui
npm install

# 指向一个本地 cxu-chathub 实例（API 需已启用）
export CXU_CONFIG_PATH=/tmp/demo-config.json
export CXU_API_URL=http://127.0.0.1:18199
export CXU_HEALTH_URL=http://127.0.0.1:16199/healthz
export CXU_DOCKER_ENABLED=false          # 本机无 docker.sock 时关闭重启
export CXU_ADMIN_PASSWORD=dev-password
node server.js
# 打开 http://127.0.0.1:9090
```

## 线上部署（docker compose）

**第 0 步（一次性，改主 `docker-compose.yml`）**：给主服务容器补一行端口映射，
这是 `docs/api-design.md` 既定的 Web UI 接入路径：

```yaml
    ports:
      - "127.0.0.1:6199:6199"
      - "127.0.0.1:8199:8199"   # ← 新增：Web 控制台 / 外部站点调用
```

**第 1 步**：准备 `webui/.env`（从 `.env.example` 复制，改掉密码）：

```ini
CXU_ADMIN_USER=admin
CXU_ADMIN_PASSWORD=换成强密码
CXU_API_URL=http://127.0.0.1:8199
CXU_HEALTH_URL=http://127.0.0.1:6199/healthz
CXU_CONTAINER_NAME=chatroom-bridge-rust
```

**第 2 步**：把 `webui/docker-compose.yml` 里 volumes 的配置文件路径改成线上实际
路径（默认 `/opt/cxu-chathub/config.json`），然后：

```bash
cd webui && docker compose up -d --build
curl -s http://127.0.0.1:9090/ -o /dev/null -w '%{http_code}\n'   # 200
```

浏览器打开 `http://127.0.0.1:9090`（本机或经 SSH 端口转发），登录后即可使用。

### config.json 的写入方式（重要）

线上 `config.json` 是**单文件 bind mount**（`./config.json:/app/config.json:ro`）。
这种挂法绑定的是 inode：如果控制台用「临时文件 + rename」替换它，主容器重启后
读到的仍是旧文件。控制台会自动检测（读 `/proc/self/mounts`）：

- **检测到挂载点** → 原地覆盖（r+ 写入 + fsync，保持 inode），`docker restart`
  后主容器能读到新配置；
- **普通文件**（本机开发）→ 临时文件 + fsync + rename 原子替换。

两种方式都先写临时文件并 fsync，尽量缩短异常窗口。

### 端到端数据流

```
保存配置 → 深合并（敏感空值保留原值）→ 校验 → 落盘 → docker restart
        → 轮询 /healthz（最多 30 秒）→ 前端提示"已保存并生效"
```

## 接口一览（全部需要登录，除 /auth/login 与静态页）

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/auth/login` | 登录（设置 HttpOnly 会话 Cookie） |
| POST | `/auth/logout` | 注销 |
| GET | `/api/config` | 读 config.json（敏感字段脱敏为 `{is_set}`） |
| PUT | `/api/config` | 深合并写入 → 校验 → 原子落盘 → 重启 → 健康轮询 |
| POST | `/api/config/reset` | 恢复默认模板（同样触发重启） |
| GET | `/api/status` `/api/health` `/api/messages` | 代理 cxu-chathub 读接口 |
| POST | `/api/relay` | 代理写接口（后端注入 `api.access_token`） |
| GET | `/api/status/stream` | SSE，每 2 秒推送状态 |
| POST | — | `/api/service/restart` 功能已并入配置保存流程（写入即重启） |
| GET | `/api/logs` | 后端操作日志（最新 200 条） |

## 目录结构

```
webui/
├── server.js          # 中间层后端（Express，唯一依赖）
├── public/
│   ├── index.html     # 登录页 + 控制台骨架（纯 table 嵌套布局，含全部样式属性）
│   └── app.js         # 前端逻辑（schema 驱动的配置面板、SSE 状态、消息、日志）
├── Dockerfile
├── docker-compose.yml
├── .env.example
└── README.md
```
