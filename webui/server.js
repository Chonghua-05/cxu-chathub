// ============================================================
// cxu-chathub Web 控制台 —— 中间层后端
//
// 职责（前端永不直接接触 config.json 与真实 token）：
//   1. 读 / 写 config.json（敏感字段脱敏下发、空值保留原值、原子落盘）
//   2. 代理 cxu-chathub 的 /api/*（持有 api.access_token，不下发浏览器）
//   3. 登录鉴权（内存 session + HttpOnly Cookie + 登录失败限速）
//   4. 配置写入后 docker restart 容器并轮询健康检查
//   5. SSE 每 2 秒推送服务状态
//   6. 操作日志（不含任何敏感值）
//
// 依赖：express（唯一）；Node >= 18（原生 fetch）。
// ============================================================

const express = require('express');
const crypto = require('crypto');
const fs = require('fs');
const fsp = fs.promises;
const path = require('path');
const { exec } = require('child_process');

// ---------- 环境变量（配置路径写死在服务端，防路径穿越） ----------
const PORT = parseInt(process.env.PORT || '9090', 10);
const CONFIG_PATH = process.env.CXU_CONFIG_PATH || '';
const API_URL = (process.env.CXU_API_URL || 'http://127.0.0.1:8199').replace(/\/+$/, '');
const HEALTH_URL = process.env.CXU_HEALTH_URL || 'http://127.0.0.1:6199/healthz';
const CONTAINER_NAME = process.env.CXU_CONTAINER_NAME || 'chatroom-bridge-rust';
const DOCKER_ENABLED = (process.env.CXU_DOCKER_ENABLED || 'true') === 'true';
const TEMPLATE_PATH = process.env.CXU_TEMPLATE_PATH || '';
const ADMIN_USER = process.env.CXU_ADMIN_USER || 'admin';
const ADMIN_PASSWORD = process.env.CXU_ADMIN_PASSWORD || 'admin';
const SESSION_TTL_MS = 8 * 60 * 60 * 1000; // 会话 8 小时

if (!CONFIG_PATH) {
  console.error('[启动失败] 缺少环境变量 CXU_CONFIG_PATH（config.json 的绝对路径）');
  process.exit(1);
}

// ---------- 敏感字段（点分路径；读取时脱敏、写入时空值保留原值） ----------
const SENSITIVE_PATHS = [
  'onebot.access_token',
  'chatroom.forward_token',
  'chatroom.refresh_token',
  'chatbridge.password',
  'chatbridge.aes_key',
  'api.access_token',
  'agent.llm.api_key',
];

// ---------- 内置默认配置模板（「恢复默认」用；优先级低于 CXU_TEMPLATE_PATH 文件） ----------
const DEFAULT_TEMPLATE = {
  onebot: { listen_host: '0.0.0.0', listen_port: 6199, path: '/ws', access_token: '', self_id: 10000 },
  chatroom: {
    base_url: 'https://chatroom.example.com',
    channel_id: 1,
    forward_token: '',
    refresh_token: '',
    group_ids: [],
    qq_sync_enabled: true,
    qq_forward_enabled: true,
    qq_to_game_enabled: true,
    player_join_pattern: '',
    player_quit_pattern: '',
    voice_api: 'https://chatroom.example.com/api/voice/qqbot/get_voice_channel_people',
    status_api: 'https://status.example.com/api/qqbot/status',
    server_addresses: [['主IP', 'game.example.com']],
    poll_interval: 10,
  },
  chatbridge: { enabled: true, host: '', port: 21027, name: 'web', password: '', aes_key: '' },
  commands: { group_allow_all: true, allow_from: [], status_image: true },
  api: { enabled: true, listen_host: '127.0.0.1', listen_port: 8199, access_token: '' },
  agent: {
    enabled: false,
    llm: { api_url: '', api_key: '', model: '', timeout_secs: 30, max_answer_chars: 1000 },
    routing: { enabled: false, group_ids: [] },
    skills: [],
  },
  patch_broadcast: { enabled: false, poll_interval_secs: 1800 },
  state_path: '/data/state.json',
  log_level: 'INFO',
  log_format: 'text',
};

// ---------- 内存状态 ----------
const sessions = new Map(); // token -> { user, expires }
const failedLogins = new Map(); // ip -> { count, until }
const ops = []; // 操作日志（最新在前，上限 200）

function logOp(user, action, detail, result) {
  ops.unshift({
    time: new Date().toISOString().replace('T', ' ').slice(0, 19),
    user: user || '-',
    action,
    detail: detail || '',
    result: result || 'ok',
  });
  if (ops.length > 200) ops.length = 200;
}

// ---------- 工具：点分路径取值 / 设值 ----------
function getPath(obj, dotted) {
  return dotted.split('.').reduce((acc, key) => (acc == null ? undefined : acc[key]), obj);
}
function setPath(obj, dotted, value) {
  const keys = dotted.split('.');
  let cur = obj;
  for (let i = 0; i < keys.length - 1; i++) {
    if (typeof cur[keys[i]] !== 'object' || cur[keys[i]] === null) cur[keys[i]] = {};
    cur = cur[keys[i]];
  }
  cur[keys[keys.length - 1]] = value;
}
function isPlainObject(v) {
  return v !== null && typeof v === 'object' && !Array.isArray(v);
}

// ---------- 脱敏：敏感字段替换为 { __masked__: true, is_set } ----------
function maskConfig(config) {
  const out = JSON.parse(JSON.stringify(config));
  for (const dotted of SENSITIVE_PATHS) {
    const v = getPath(out, dotted);
    if (v === undefined) continue;
    setPath(out, dotted, { __masked__: true, is_set: typeof v === 'string' ? v.length > 0 : !!v });
  }
  return out;
}

// ---------- 深合并：对象递归合并；数组/标量整体替换；敏感字段空值保留原值 ----------
function deepMerge(base, incoming, user) {
  const out = JSON.parse(JSON.stringify(base));
  for (const dotted of SENSITIVE_PATHS) {
    const incomingValue = getPath(incoming, dotted);
    if (incomingValue === undefined) continue;
    const keep =
      incomingValue === '' || // 空字符串 = 保留原值
      (isPlainObject(incomingValue) && incomingValue.__masked__ === true); // 原样带回的脱敏对象 = 保留
    if (!keep) {
      setPath(out, dotted, String(incomingValue));
    }
    // 从 incoming 里摘掉敏感字段，后面的通用深合并不再碰它
    deletePath(incoming, dotted);
  }
  mergeInto(out, incoming);
  void user;
  return out;
}
function deletePath(obj, dotted) {
  const keys = dotted.split('.');
  let cur = obj;
  for (let i = 0; i < keys.length - 1; i++) {
    if (!isPlainObject(cur?.[keys[i]])) return;
    cur = cur[keys[i]];
  }
  delete cur[keys[keys.length - 1]];
}
function mergeInto(base, incoming) {
  for (const [key, value] of Object.entries(incoming)) {
    if (isPlainObject(value) && isPlainObject(base[key])) {
      mergeInto(base[key], value);
    } else {
      base[key] = value;
    }
  }
}

// ---------- 校验：按 config.json 的结构做范围 / 类型 / 必填检查 ----------
function validateConfig(cfg) {
  const errors = [];
  const isPort = (v) => Number.isInteger(v) && v >= 1 && v <= 65535;
  const isHttpUrl = (v) => v === '' || /^https?:\/\//.test(v);

  if (!isPlainObject(cfg.onebot)) errors.push('onebot 段缺失或不是对象');
  else {
    if (!isPort(cfg.onebot.listen_port)) errors.push('onebot.listen_port 必须是 1-65535 的整数');
    if (typeof cfg.onebot.path !== 'string' || !cfg.onebot.path.startsWith('/'))
      errors.push('onebot.path 必须以 / 开头');
    if (!Number.isInteger(cfg.onebot.self_id) || cfg.onebot.self_id < 0)
      errors.push('onebot.self_id 必须是非负整数');
  }
  if (!isPlainObject(cfg.chatroom)) errors.push('chatroom 段缺失或不是对象');
  else {
    if (!isHttpUrl(cfg.chatroom.base_url)) errors.push('chatroom.base_url 必须以 http(s):// 开头');
    if (!Number.isInteger(cfg.chatroom.channel_id) || cfg.chatroom.channel_id < 0)
      errors.push('chatroom.channel_id 必须是非负整数');
    if (!Array.isArray(cfg.chatroom.group_ids) || cfg.chatroom.group_ids.some((g) => !Number.isInteger(g)))
      errors.push('chatroom.group_ids 必须是整数数组');
    if (!Number.isInteger(cfg.chatroom.poll_interval) || cfg.chatroom.poll_interval < 1)
      errors.push('chatroom.poll_interval 必须 >= 1');
    for (const key of ['voice_api', 'status_api']) {
      if (!isHttpUrl(cfg.chatroom[key])) errors.push(`chatroom.${key} 必须以 http(s):// 开头`);
    }
  }
  if (!isPlainObject(cfg.chatbridge)) errors.push('chatbridge 段缺失或不是对象');
  else if (!isPort(cfg.chatbridge.port)) errors.push('chatbridge.port 必须是 1-65535 的整数');
  if (!isPlainObject(cfg.commands)) errors.push('commands 段缺失或不是对象');
  else if (!Array.isArray(cfg.commands.allow_from))
    errors.push('commands.allow_from 必须是数组');
  if (!isPlainObject(cfg.api)) errors.push('api 段缺失或不是对象');
  else if (!isPort(cfg.api.listen_port)) errors.push('api.listen_port 必须是 1-65535 的整数');
  if (!isPlainObject(cfg.agent)) errors.push('agent 段缺失或不是对象');
  else if (cfg.agent.llm !== undefined && cfg.agent.llm !== null) {
    if (!isPlainObject(cfg.agent.llm)) errors.push('agent.llm 必须是对象');
    else {
      if (!isHttpUrl(cfg.agent.llm.api_url)) errors.push('agent.llm.api_url 必须以 http(s):// 开头');
      if (!Number.isInteger(cfg.agent.llm.timeout_secs) || cfg.agent.llm.timeout_secs < 1)
        errors.push('agent.llm.timeout_secs 必须 >= 1');
      if (!Number.isInteger(cfg.agent.llm.max_answer_chars) || cfg.agent.llm.max_answer_chars < 1)
        errors.push('agent.llm.max_answer_chars 必须 >= 1');
    }
  }
  if (!isPlainObject(cfg.patch_broadcast)) errors.push('patch_broadcast 段缺失或不是对象');
  else if (!Number.isInteger(cfg.patch_broadcast.poll_interval_secs) || cfg.patch_broadcast.poll_interval_secs < 10)
    errors.push('patch_broadcast.poll_interval_secs 必须 >= 10');
  if (typeof cfg.state_path !== 'string' || !cfg.state_path)
    errors.push('state_path 必须是非空字符串');
  if (!['DEBUG', 'INFO', 'WARNING', 'ERROR'].includes(cfg.log_level))
    errors.push('log_level 必须是 DEBUG / INFO / WARNING / ERROR 之一');
  if (!['text', 'json'].includes(cfg.log_format)) errors.push('log_format 必须是 text 或 json');
  return errors;
}

// ---------- 落盘：挂载点检测 + 原子写 ----------
// 单文件 bind mount（线上 config.json 的挂法）下，rename 会替换 inode，
// 容器里仍指向旧文件、restart 也读不到新值——因此检测到挂载点时改为原地写
// （先写临时文件 + fsync，再 r+ 覆盖目标并 fsync，尽量缩短非原子窗口）。
function isMountpoint(target) {
  try {
    const mounts = fs.readFileSync('/proc/self/mounts', 'utf8');
    return mounts
      .split('\n')
      .some((line) => {
        const parts = line.split(' ');
        return parts.length >= 2 && decodeMountPath(parts[1]) === path.resolve(target);
      });
  } catch {
    return false; // 非 Linux（本机开发）按普通文件处理
  }
}
function decodeMountPath(p) {
  return p.replace(/\\040/g, ' ').replace(/\\011/g, '\t');
}

async function writeConfigFile(target, content) {
  const dir = path.dirname(target);
  const tmp = path.join(dir, `.config.${process.pid}.${Date.now()}.tmp`);
  const handle = await fsp.open(tmp, 'w');
  try {
    await handle.writeFile(content, 'utf8');
    await handle.sync();
  } finally {
    await handle.close();
  }
  if (isMountpoint(target)) {
    // 原地覆盖（保持 inode，单文件 bind mount 容器可见）
    const fh = await fsp.open(target, 'r+');
    try {
      await fh.writeFile(content, 'utf8');
      await fh.sync();
      await fh.truncate(Buffer.byteLength(content, 'utf8'));
    } finally {
      await fh.close();
    }
    await fsp.unlink(tmp);
  } else {
    await fsp.rename(tmp, target); // 原子替换
  }
}

async function readConfigFile() {
  const raw = await fsp.readFile(CONFIG_PATH, 'utf8');
  return JSON.parse(raw.replace(/^\uFEFF/, ''));
}

async function loadTemplate() {
  if (TEMPLATE_PATH) {
    try {
      const raw = await fsp.readFile(TEMPLATE_PATH, 'utf8');
      return JSON.parse(raw.replace(/^\uFEFF/, ''));
    } catch (err) {
      console.warn(`[模板] 读取 ${TEMPLATE_PATH} 失败，使用内置默认模板: ${err.message}`);
    }
  }
  return JSON.parse(JSON.stringify(DEFAULT_TEMPLATE));
}

// ---------- 服务重启 + 健康轮询 ----------
function dockerRestart() {
  return new Promise((resolve) => {
    if (!DOCKER_ENABLED) return resolve({ restarted: false, note: 'CXU_DOCKER_ENABLED=false，跳过重启' });
    exec(`docker restart ${JSON.stringify(CONTAINER_NAME)}`, { timeout: 90000 }, (err, stdout, stderr) => {
      if (err) {
        resolve({ restarted: false, note: `docker restart 失败: ${(stderr || err.message).trim()}` });
      } else {
        resolve({ restarted: true, note: `容器 ${CONTAINER_NAME} 已重启` });
      }
    });
  });
}

async function pollHealth(maxTries = 30, intervalMs = 1000) {
  for (let i = 0; i < maxTries; i++) {
    try {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), 2000);
      const response = await fetch(HEALTH_URL, { signal: controller.signal });
      clearTimeout(timer);
      if (response.ok) return true;
    } catch {
      // 未就绪，继续等
    }
    await new Promise((r) => setTimeout(r, intervalMs));
  }
  return false;
}

// ---------- 拿服务端的 api.access_token（代理注入用；绝不下发浏览器） ----------
async function getServiceToken() {
  try {
    const config = await readConfigFile();
    const token = getPath(config, 'api.access_token');
    return typeof token === 'string' && token ? token : '';
  } catch {
    return '';
  }
}

async function proxyToService(req, res, servicePath, options = {}) {
  try {
    const token = await getServiceToken();
    const headers = { 'Content-Type': 'application/json' };
    if (token) headers.Authorization = `Bearer ${token}`;
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 8000);
    const response = await fetch(`${API_URL}${servicePath}`, {
      method: options.method || 'GET',
      headers,
      body: options.body ? JSON.stringify(options.body) : undefined,
      signal: controller.signal,
    });
    clearTimeout(timer);
    const text = await response.text();
    res.status(response.status).type('json').send(text);
  } catch (err) {
    res.status(502).json({ error: '服务未连接', detail: err.message });
  }
}

// ---------- 会话与鉴权 ----------
function parseCookies(req) {
  const out = {};
  const header = req.headers.cookie;
  if (!header) return out;
  for (const part of header.split(';')) {
    const idx = part.indexOf('=');
    if (idx > 0) out[part.slice(0, idx).trim()] = decodeURIComponent(part.slice(idx + 1).trim());
  }
  return out;
}

function requireAuth(req, res, next) {
  const token = parseCookies(req).cxu_session;
  const session = token && sessions.get(token);
  if (!session || session.expires < Date.now()) {
    if (token) sessions.delete(token);
    return res.status(401).json({ error: '未登录或会话已过期' });
  }
  session.expires = Date.now() + SESSION_TTL_MS; // 滑动续期
  req.user = session.user;
  next();
}

function constantTimeEqual(a, b) {
  const ha = crypto.createHash('sha256').update(String(a)).digest();
  const hb = crypto.createHash('sha256').update(String(b)).digest();
  return crypto.timingSafeEqual(ha, hb);
}

// ---------- Express 应用 ----------
const app = express();
app.disable('x-powered-by');
app.use(express.json({ limit: '1mb' }));

// 静态资源（登录页 + 控制台；登录态由前端按 /api/health 的 401 判断）
app.use(express.static(path.join(__dirname, 'public')));

// ---- 登录 / 登出 ----
app.post('/auth/login', (req, res) => {
  const ip = req.socket.remoteAddress || '?';
  const lock = failedLogins.get(ip);
  if (lock && lock.until > Date.now()) {
    const seconds = Math.ceil((lock.until - Date.now()) / 1000);
    return res.status(429).json({ error: `失败次数过多，请 ${seconds} 秒后再试` });
  }
  const { username, password } = req.body || {};
  if (constantTimeEqual(username, ADMIN_USER) && constantTimeEqual(password, ADMIN_PASSWORD)) {
    failedLogins.delete(ip);
    const token = crypto.randomBytes(32).toString('hex');
    sessions.set(token, { user: username, expires: Date.now() + SESSION_TTL_MS });
    res.setHeader(
      'Set-Cookie',
      `cxu_session=${token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=${SESSION_TTL_MS / 1000}`,
    );
    logOp(username, '登录', `ip=${ip}`, 'ok');
    return res.json({ ok: true });
  }
  const entry = failedLogins.get(ip) || { count: 0, until: 0 };
  entry.count += 1;
  if (entry.count >= 5) {
    entry.count = 0;
    entry.until = Date.now() + 60 * 1000; // 锁 60 秒
  }
  failedLogins.set(ip, entry);
  logOp(username || '?', '登录失败', `ip=${ip}`, '拒绝');
  res.status(401).json({ error: '用户名或密码错误' });
});

app.post('/auth/logout', requireAuth, (req, res) => {
  const token = parseCookies(req).cxu_session;
  sessions.delete(token);
  logOp(req.user, '登出', '', 'ok');
  res.setHeader('Set-Cookie', 'cxu_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0');
  res.json({ ok: true });
});

// ---- 配置读写 ----
app.get('/api/config', requireAuth, async (req, res) => {
  try {
    const config = await readConfigFile();
    logOp(req.user, '读取配置', '', 'ok');
    res.json({ config: maskConfig(config) });
  } catch (err) {
    logOp(req.user, '读取配置', err.message, '失败');
    res.status(500).json({ error: `读取配置失败: ${err.message}` });
  }
});

app.put('/api/config', requireAuth, async (req, res) => {
  try {
    const original = await readConfigFile();
    const merged = deepMerge(original, JSON.parse(JSON.stringify(req.body || {})), req.user);
    const errors = validateConfig(merged);
    if (errors.length) {
      logOp(req.user, '写入配置', errors.join('; '), '校验失败');
      return res.status(400).json({ ok: false, errors });
    }
    // 变更字段清单（只记字段名，不记值）
    const changed = [];
    for (const key of new Set([...Object.keys(original), ...Object.keys(merged)])) {
      if (JSON.stringify(original[key]) !== JSON.stringify(merged[key])) changed.push(key);
    }
    await writeConfigFile(CONFIG_PATH, JSON.stringify(merged, null, 2) + '\n');
    logOp(req.user, '写入配置', `变更: ${changed.join(', ') || '无'}`, 'ok');

    const restart = await dockerRestart();
    const ready = await pollHealth();
    logOp(req.user, '服务重启', restart.note, restart.restarted ? (ready ? '已就绪' : '超时未就绪') : '跳过');
    res.json({ ok: true, changed, ...restart, ready });
  } catch (err) {
    logOp(req.user, '写入配置', err.message, '失败');
    res.status(500).json({ ok: false, errors: [`写入失败: ${err.message}`] });
  }
});

app.post('/api/config/reset', requireAuth, async (req, res) => {
  try {
    const template = await loadTemplate();
    const errors = validateConfig(template);
    if (errors.length) return res.status(500).json({ ok: false, errors: ['内置模板非法: ' + errors.join('; ')] });
    await writeConfigFile(CONFIG_PATH, JSON.stringify(template, null, 2) + '\n');
    logOp(req.user, '恢复默认配置', '', 'ok');
    const restart = await dockerRestart();
    const ready = await pollHealth();
    logOp(req.user, '服务重启', restart.note, restart.restarted ? (ready ? '已就绪' : '超时未就绪') : '跳过');
    res.json({ ok: true, ...restart, ready });
  } catch (err) {
    logOp(req.user, '恢复默认配置', err.message, '失败');
    res.status(500).json({ ok: false, errors: [err.message] });
  }
});

// ---- 服务代理 ----
app.get('/api/status', requireAuth, (req, res) => proxyToService(req, res, '/api/status'));
app.get('/api/health', requireAuth, (req, res) => proxyToService(req, res, '/api/health'));
app.get('/api/messages', requireAuth, (req, res) => proxyToService(req, res, '/api/messages'));
app.post('/api/relay', requireAuth, (req, res) => {
  logOp(req.user, 'relay 发送', `target=${req.body?.target ?? '?'}`, '已转发');
  proxyToService(req, res, '/api/relay', { method: 'POST', body: req.body });
});

// ---- SSE 状态推送（每 2 秒）----
app.get('/api/status/stream', requireAuth, async (req, res) => {
  res.writeHead(200, {
    'Content-Type': 'text/event-stream',
    'Cache-Control': 'no-cache',
    Connection: 'keep-alive',
  });
  res.write(': connected\n\n');
  let closed = false;
  req.on('close', () => {
    closed = true;
  });
  const push = async () => {
    if (closed) return;
    try {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), 1500);
      const response = await fetch(`${API_URL}/api/status`, { signal: controller.signal });
      clearTimeout(timer);
      const text = await response.text();
      if (!closed) res.write(`data: ${text.replace(/\n/g, ' ')}\n\n`);
    } catch {
      if (!closed) res.write(`data: ${JSON.stringify({ service_connected: false })}\n\n`);
    }
  };
  await push();
  const interval = setInterval(push, 2000);
  req.on('close', () => clearInterval(interval));
});

// ---- 操作日志 ----
app.get('/api/logs', requireAuth, (req, res) => {
  res.json({ logs: ops.slice(0, 200) });
});

// ---- 兜底 ----
app.use((err, req, res, next) => {
  if (err.type === 'entity.parse.failed') return res.status(400).json({ error: '请求体不是合法 JSON' });
  console.error(err);
  res.status(500).json({ error: '内部错误' });
});
app.use((req, res) => res.status(404).json({ error: 'not found' }));

app.listen(PORT, () => {
  console.log(`[cxu-chathub 控制台] http://0.0.0.0:${PORT}`);
  console.log(`  配置文件: ${CONFIG_PATH}${isMountpoint(CONFIG_PATH) ? '（检测到挂载点，写入将原地覆盖）' : ''}`);
  console.log(`  服务 API: ${API_URL} · 健康检查: ${HEALTH_URL} · 重启目标: ${CONTAINER_NAME}`);
  if (ADMIN_PASSWORD === 'admin') {
    console.warn('  ⚠ 当前使用默认密码 admin，生产环境请务必设置 CXU_ADMIN_PASSWORD！');
  }
});
