# cxu-chathub Web 控制台

Windows 95/98 风格的配置管理与状态监视控制台。前端为**纯 HTML（零 CSS、零框架、零构建）**：布局靠 `<table>` 嵌套，配色靠 `bgcolor`/`<font>`/`<body link>` 属性，按钮与输入框用浏览器原生控件样式。

中间层后端为 **Rust + axum**（原 Node/Express 版已下线，见文末「从 Node 版迁移」）。

```
浏览器（Win95 风格前端）
   │  登录 Cookie（HttpOnly）
   ▼
中间层后端（本目录，Rust + axum，单静态二进制）
   │                    │
   │ 读写               │ 代理 /api/*（注入 api.access_token）
   ▼                    ▼
config.json        cxu-chathub（/api/status、/api/messages、/api/relay）
（写入后 docker restart + 轮询健康检查）
```

**体积 / 内存**：静态二进制约 2.9 MB；空闲常驻内存约 4–5 MB（旧 Node 版约 70 MB）。

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

## 环境变量

| 变量 | 默认 | 说明 |
|------|------|------|
| `PORT` | `9090` | 监听端口（绑 `0.0.0.0`，对外务必走反代 + TLS） |
| `CXU_CONFIG_PATH` | 必填 | `config.json` 绝对路径（缺失则拒绝启动） |
| `CXU_API_URL` | `http://127.0.0.1:8199` | cxu-chathub HTTP API 地址 |
| `CXU_HEALTH_URL` | `http://127.0.0.1:6199/healthz` | 重启后轮询的健康检查地址 |
| `CXU_CONTAINER_NAME` | `chatroom-bridge-rust` | 要 `docker restart` 的容器名 |
| `CXU_DOCKER_ENABLED` | `true` | 设 `false` 则跳过重启（本机调试用） |
| `CXU_TEMPLATE_PATH` | 空 | 「恢复默认」模板文件；留空用内置默认模板 |
| `CXU_ADMIN_USER` / `CXU_ADMIN_PASSWORD` | `admin` / `admin` | 登录账号（生产务必改密码） |
| `CXU_PUBLIC_DIR` | `public` | 前端静态目录（相对进程工作目录） |

## 快速开始（本机调试）

```bash
cd webui
cargo build --release          # 需要 Rust stable

# 指向一个本地 cxu-chathub 实例（API 需已启用）
export CXU_CONFIG_PATH=/tmp/demo-config.json
export CXU_API_URL=http://127.0.0.1:18199
export CXU_HEALTH_URL=http://127.0.0.1:16199/healthz
export CXU_DOCKER_ENABLED=false          # 本机无 docker.sock 时关闭重启
export CXU_ADMIN_PASSWORD=dev-password
./target/release/cxu-console
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

### 构建加速（可选）

Dockerfile 的 builder 阶段在容器内 `cargo build`。国内网络慢时，在 `builder` 阶段
加一行 crates 镜像即可（示例用 rsproxy）：

```dockerfile
RUN mkdir -p /root/.cargo && printf '[source.crates-io]\nreplace-with="rsproxy"\n\
[source.rsproxy]\nregistry="sparse+https://rsproxy.cn/index/"\n' > /root/.cargo/config.toml
```

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
├── Cargo.toml / Cargo.lock    # Rust 工程（bin: cxu-console）
├── src/
│   ├── main.rs                # axum 路由 / 处理器 / 鉴权 / SSE / 代理
│   └── cfg.rs                 # 配置脱敏 / 深合并 / 校验 / 落盘
├── public/
│   ├── index.html             # 登录页 + 控制台骨架（纯 table 嵌套布局）
│   └── app.js                 # 前端逻辑（schema 驱动配置面板、SSE 状态、消息、日志）
├── Dockerfile
├── docker-compose.yml
├── .env.example
└── README.md
```

## 从 Node 版迁移

原 `server.js`（Express）已由等价 Rust 实现替换，**接口、环境变量、前端均不变**，
仅运行时从 Node 换为 Rust（内存占用大降）。行为对齐项（与 Node 版逐条一致）：

- 请求/响应 JSON 字段、中文错误文案、HTTP 状态码；
- 脱敏规则、深合并与「空值保留原值」、配置校验规则；
- 单文件 bind mount 的原地写入、`docker restart` + `/healthz` 轮询；
- 登录失败限速（5 次 / 60 秒）、8 小时滑动会话、常量时间口令比较。

已知保留的原版细节：操作日志时间戳沿用 Node 版的 **UTC**（`toISOString`）格式，
与前端本地时间（`toTimeString`）不同源；如需改为本地时间，改 `src/main.rs` 的
`utc_timestamp()` 即可。
