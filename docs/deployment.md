# 部署（云主机 Docker）

目标形态：云主机上 `docker compose` 起单个容器，监听 `127.0.0.1:6199`，由同机 NapCat 的
「NapCat 客户端」客户端以反向 WS 连入。**不新增 NapCat 客户端条目，不额外放行端口。**

## 0. 前置

| 项 | 要求 |
|----|------|
| Docker / compose | 已装（`docker compose version`） |
| 端口 | `6199` 空闲（**旧框架容器会占用，必须先停掉旧框架**） |
| 配置 | 仓库目录下有 `config.json`（从 `config.example.json` 复制） |

## 1. 拉代码与配置

```bash
cd /opt && git clone <repo-url> cxu-chathub && cd cxu-chathub
cp config.example.json config.json
vi config.json      # 填 chatroom.forward_token / chatbridge.* / onebot.access_token
```

必填项：

- `chatroom.forward_token` —— chatroom 官方给 bot 的静态 token（唯一必需）
- `chatbridge.password`、`chatbridge.aes_key` —— 服务端分配（可从旧旧框架插件配置搬）
- `onebot.access_token` —— 与 NapCat 客户端的 token 一致
- `commands.status_image` —— `true` 表示 `/server` 发图（构建时需带 Chromium，镜像大约 1GB）

## 2. 构建并启动

```bash
docker compose up -d --build
curl -s http://127.0.0.1:6199/healthz     # 期望 onebot_connected 字段
docker compose logs -f --tail 50
```

`docker-compose.yml` 关键点：

- 端口映射写成 `127.0.0.1:6199:6199` —— 只绑回环，公网不可达。
- `config.json` 以 **只读** 挂载；`./data` 为可写卷，存放 `state.json`。
- `mem_limit: 1200m` / `shm_size: 512m`：给 Chromium 渲染留余量；容器空闲常驻约 40MB。

## 3. 切流顺序（避免重复转发）

1. 停掉旧框架容器（释放 6199，同时避免同一 `source_message_id` 被两边各转一次）。
2. `docker compose up -d`（本服务开始监听 `127.0.0.1:6199`）。
3. 把 NapCat 客户端的 `enable` 改成 `true`（并让 NapCat 重载配置）→ 桥接开始收到群事件。
4. 验证：群里发 `/chatroom` 应回在线人数；群里正常聊天应出现在 chatroom 的目标频道。
5. 最后移除 AI 服务 侧 `chatroom 插件`、`server-status 插件` 插件与对应 ACL 条目。

## 4. 日常运维

```bash
docker compose ps
docker compose logs -f --tail 100
docker compose restart chatroom-bridge     # 改完 config.json 必须重启
docker stats --no-stream chatroom-bridge
```

**配置改了不会生效**：`config.json` 只在进程启动时读一次，且以只读方式挂载，必须 `restart`。

`state.json` 在 `./data` 卷里，重启不丢去重表与读游标。

## 5. 更新

```bash
cd /opt/cxu-chathub
git pull
docker compose up -d --build
curl -s http://127.0.0.1:6199/healthz
```

## 6. 故障速查

| 现象 | 先看什么 |
|------|----------|
| `/healthz` 里 `onebot_connected: false` | NapCat 里「NapCat 客户端」客户端是否 `enable`、token 是否一致 |
| 群消息没进 chatroom | 日志里 `转发到 chatroom 失败` 的 HTTP 码；若返回 **200 + HTML** 说明服务端接口路径已变（前端 SPA 兜底），不是 token 问题 |
| `/server` 不出图 | `commands.status_image` 是否为 true、镜像是否带 Chromium；渲染失败会自动回退文本 |
| 启动即退出 | `config.json` 的 JSON 语法；`docker compose logs` 里的 `ConfigError` |
| 端口占用 | `docker ps --filter publish=6199` —— 大概率是旧框架还在跑 |

## 7. 安全清单

- [ ] `config.json` 不在 git 工作区外泄（`.gitignore` 已覆盖，提交前 `git status` 复核）
- [ ] 端口映射始终是 `127.0.0.1:...`
- [ ] 不在日志、issue、聊天里粘贴 token
