// ============================================================
// cxu-chathub 控制台 —— 前端逻辑（原生 JS，无框架）
// 结构：登录 → 配置面板（schema 驱动渲染）→ 状态监视（SSE + 轮询兜底）
//       → 消息列表 → relay 下发 → 操作日志
// ============================================================

'use strict';

// ---------- 配置面板的 schema（按 config.json 顶层字段分组） ----------
// type: text 文本 / number 整数 / bool 复选 / intlist 逗号分隔整数列表
//       secret 敏感字段（is_set + 修改按钮）/ select 下拉
// dotted key 支持嵌套（如 agent.llm.api_url）
const FIELD_GROUPS = [
  {
    key: 'onebot', title: 'onebot —— NapCat 反向 WS 接入', fields: [
      { k: 'listen_host', label: '监听地址', type: 'text' },
      { k: 'listen_port', label: '监听端口', type: 'number' },
      { k: 'path', label: 'WS 路径', type: 'text' },
      { k: 'access_token', label: 'access_token', type: 'secret' },
      { k: 'self_id', label: '机器人 QQ 号', type: 'number' },
    ],
  },
  {
    key: 'chatroom', title: 'chatroom —— 服务端对接', fields: [
      { k: 'base_url', label: '服务端地址', type: 'text' },
      { k: 'channel_id', label: '目标频道 ID', type: 'number' },
      { k: 'forward_token', label: 'forward_token', type: 'secret' },
      { k: 'refresh_token', label: 'refresh_token', type: 'secret' },
      { k: 'group_ids', label: 'QQ 群白名单', type: 'intlist' },
      { k: 'qq_sync_enabled', label: 'QQ 群 → chatroom 同步', type: 'bool' },
      { k: 'qq_forward_enabled', label: '!q → QQ 群', type: 'bool' },
      { k: 'qq_to_game_enabled', label: '!q → 游戏内', type: 'bool' },
      { k: 'poll_interval', label: '读轮询间隔（秒）', type: 'number' },
      { k: 'voice_api', label: '语音 API', type: 'text' },
      { k: 'status_api', label: '状态 API', type: 'text' },
      { k: 'player_join_pattern', label: '上线识别正则', type: 'text' },
      { k: 'player_quit_pattern', label: '下线识别正则', type: 'text' },
    ],
  },
  {
    key: 'chatbridge', title: 'chatbridge —— MC 游戏互通', fields: [
      { k: 'enabled', label: '启用', type: 'bool' },
      { k: 'host', label: '游戏服地址', type: 'text' },
      { k: 'port', label: '端口', type: 'number' },
      { k: 'name', label: '客户端标识', type: 'text' },
      { k: 'password', label: 'password', type: 'secret' },
      { k: 'aes_key', label: 'aes_key', type: 'secret' },
    ],
  },
  {
    key: 'commands', title: 'commands —— 群内命令', fields: [
      { k: 'group_allow_all', label: '所有群可用', type: 'bool' },
      { k: 'allow_from', label: '白名单群', type: 'intlist' },
      { k: 'status_image', label: '/server 发状态图', type: 'bool' },
    ],
  },
  {
    key: 'api', title: 'api —— HTTP API', fields: [
      { k: 'enabled', label: '启用', type: 'bool' },
      { k: 'listen_host', label: '监听地址', type: 'text' },
      { k: 'listen_port', label: '监听端口', type: 'number' },
      { k: 'access_token', label: 'access_token', type: 'secret' },
    ],
  },
  {
    key: 'agent', title: 'agent —— 检索问答与智能路由', fields: [
      { k: 'enabled', label: '启用', type: 'bool' },
      { k: 'llm.api_url', label: 'LLM API 地址', type: 'text' },
      { k: 'llm.api_key', label: 'LLM api_key', type: 'secret' },
      { k: 'llm.model', label: 'LLM 模型', type: 'text' },
      { k: 'llm.timeout_secs', label: 'LLM 超时（秒）', type: 'number' },
      { k: 'llm.max_answer_chars', label: '回答长度上限', type: 'number' },
      { k: 'routing.enabled', label: '智能路由（@bot 触发）', type: 'bool' },
      { k: 'routing.group_ids', label: '路由灰度群', type: 'intlist' },
    ],
  },
  {
    key: 'patch_broadcast', title: 'patch_broadcast —— 版本更新播报', fields: [
      { k: 'enabled', label: '启用', type: 'bool' },
      { k: 'feed_url', label: '官方 feed（v2）', type: 'text' },
      { k: 'poll_interval_secs', label: '轮询间隔（秒）', type: 'number' },
    ],
  },
  {
    key: 'root', title: '顶层 —— 状态与日志', fields: [
      { k: 'state_path', label: 'state.json 路径', type: 'text' },
      { k: 'log_level', label: '日志级别', type: 'select', options: ['DEBUG', 'INFO', 'WARNING', 'ERROR'] },
      { k: 'log_format', label: '日志格式', type: 'select', options: ['text', 'json'] },
    ],
  },
];

// ---------- 全局状态 ----------
let requestCount = 0;      // 本次会话请求数
let statusPaused = false;  // 状态刷新暂停
let sseSource = null;      // EventSource
let pollTimer = null;      // 轮询兜底
let msgTimer = null;       // 消息轮询
let logTimer = null;       // 日志轮询
let configData = null;     // 最近一次加载（脱敏形态）
let localLogs = [];        // 前端本地操作记录

const $ = (id) => document.getElementById(id);

// ---------- 基础请求封装（计数 + 401 回登录页） ----------
async function api(method, url, body) {
  requestCount++;
  $('req-count').textContent = requestCount;
  const options = { method, headers: {} };
  if (body !== undefined) {
    options.headers['Content-Type'] = 'application/json';
    options.body = JSON.stringify(body);
  }
  const response = await fetch(url, options);
  if (response.status === 401) { showLogin('会话已过期，请重新登录'); throw new Error('401'); }
  const data = await response.json().catch(() => ({}));
  if (!response.ok) {
    const err = new Error(data.error || `HTTP ${response.status}`);
    err.data = data;
    throw err;
  }
  return data;
}

// ---------- 登录 / 注销 ----------
function showLogin(message) {
  stopAll();
  $('console').classList.add('hidden');
  $('login-screen').classList.remove('hidden');
  $('login-msg').textContent = message || '';
}

async function doLogin() {
  const user = $('login-user').value.trim();
  const pass = $('login-pass').value;
  $('login-msg').textContent = '正在登录……';
  try {
    const response = await fetch('/auth/login', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ username: user, password: pass }),
    });
    const data = await response.json().catch(() => ({}));
    if (!response.ok) { $('login-msg').textContent = data.error || '登录失败'; return; }
    $('login-msg').textContent = '';
    enterConsole(user);
  } catch (err) {
    $('login-msg').textContent = '服务未连接';
  }
}

async function doLogout() {
  try { await api('POST', '/auth/logout'); } catch (e) { /* 会话可能已过期 */ }
  showLogin('已注销');
}

function enterConsole(user) {
  $('login-screen').classList.add('hidden');
  $('console').classList.remove('hidden');
  $('whoami').textContent = user;
  requestCount = 0;
  $('req-count').textContent = '0';
  localLogs = [];
  loadConfig();
  startStatusStream();
  refreshMessages();
  msgTimer = setInterval(refreshMessages, 2000);
  refreshLogs();
  logTimer = setInterval(refreshLogs, 5000);
}

function stopAll() {
  if (sseSource) { sseSource.close(); sseSource = null; }
  if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
  if (msgTimer) { clearInterval(msgTimer); msgTimer = null; }
  if (logTimer) { clearInterval(logTimer); logTimer = null; }
  $('refresh-mode').textContent = '已停止';
}

// ============================================================
// 配置面板（schema 驱动）
// ============================================================

// 点分路径取值
function getPath(obj, dotted) {
  return dotted.split('.').reduce((acc, key) => (acc == null ? undefined : acc[key]), obj);
}
// 点分路径设值
function setPath(obj, dotted, value) {
  const keys = dotted.split('.');
  let cur = obj;
  for (let i = 0; i < keys.length - 1; i++) {
    if (typeof cur[keys[i]] !== 'object' || cur[keys[i]] === null) cur[keys[i]] = {};
    cur = cur[keys[i]];
  }
  cur[keys[keys.length - 1]] = value;
}

function renderConfig(masked) {
  configData = masked;
  const form = $('config-form');
  form.innerHTML = '';
  for (const group of FIELD_GROUPS) {
    const fs = document.createElement('fieldset');
    fs.className = 'inner';
    const legend = document.createElement('legend');
    legend.textContent = group.title;
    fs.appendChild(legend);
    const table = document.createElement('table');
    table.className = 'cfg-table';
    for (const field of group.fields) {
      const tr = document.createElement('tr');
      const tdLabel = document.createElement('td');
      tdLabel.className = 'lbl';
      tdLabel.textContent = field.label + '：';
      const tdValue = document.createElement('td');
      renderFieldValue(tdValue, field, getPath(masked, group.key === 'root' ? field.k : group.key + '.' + field.k));
      tr.appendChild(tdLabel);
      tr.appendChild(tdValue);
      table.appendChild(tr);
    }
    fs.appendChild(table);
    form.appendChild(fs);
  }
}

// 字段值 → 控件（含路径前缀，收集时按同一路径写回）
function renderFieldValue(td, field, value) {
  const path = field.k;
  if (field.type === 'secret') {
    // 敏感字段：显示 已设置/未设置 + 修改按钮（勾选后才出空输入框，留空 = 保留原值）
    const isSet = !!(value && value.is_set);
    const state = document.createElement('span');
    state.className = 'secret-state ' + (isSet ? 'set' : 'unset');
    state.textContent = isSet ? '已设置' : '未设置';
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'btn95 btn-sm';
    btn.textContent = '修改';
    const input = document.createElement('input');
    input.type = 'password';
    input.dataset.path = path;
    input.dataset.kind = 'secret-new';
    input.style.display = 'none';
    input.style.width = '200px';
    btn.addEventListener('click', () => {
      const showing = input.style.display !== 'none';
      input.style.display = showing ? 'none' : 'inline-block';
      btn.textContent = showing ? '修改' : '取消';
      if (!showing) input.focus();
    });
    const hiddenKeep = document.createElement('input');
    hiddenKeep.type = 'hidden';
    hiddenKeep.dataset.path = path;
    hiddenKeep.dataset.kind = 'secret-keep';
    hiddenKeep.value = isSet ? 'keep' : '';
    td.className = 'secret-cell';
    td.appendChild(state);
    td.appendChild(btn);
    td.appendChild(input);
    td.appendChild(hiddenKeep);
    return;
  }
  if (field.type === 'bool') {
    const cb = document.createElement('input');
    cb.type = 'checkbox';
    cb.dataset.path = path;
    cb.dataset.kind = 'bool';
    cb.checked = !!value;
    td.appendChild(cb);
    return;
  }
  if (field.type === 'select') {
    const select = document.createElement('select');
    select.dataset.path = path;
    select.dataset.kind = 'select';
    for (const option of field.options) {
      const opt = document.createElement('option');
      opt.value = option;
      opt.textContent = option;
      if (value === option) opt.selected = true;
      select.appendChild(opt);
    }
    td.appendChild(select);
    return;
  }
  if (field.type === 'intlist') {
    const input = document.createElement('input');
    input.type = 'text';
    input.dataset.path = path;
    input.dataset.kind = 'intlist';
    input.value = Array.isArray(value) ? value.join(', ') : '';
    input.placeholder = '如: 123456, 789000';
    td.appendChild(input);
    return;
  }
  // text / number
  const input = document.createElement('input');
  input.type = field.type === 'number' ? 'number' : 'text';
  input.dataset.path = path;
  input.dataset.kind = field.type === 'number' ? 'number' : 'text';
  if (field.type === 'number') { input.classList.add('short'); input.step = '1'; }
  input.value = value === undefined || value === null ? '' : value;
  td.appendChild(input);
}

// 从表单收集完整配置对象（在加载的脱敏配置上覆盖用户改过的字段；
// 敏感字段：未点修改 → 原样带回脱敏对象；点了修改但留空 → 空串（后端保留原值））
function collectConfig() {
  const out = JSON.parse(JSON.stringify(configData));
  const fields = document.querySelectorAll('#config-form [data-path]');
  const secretTouched = new Set(); // 点了「修改」的敏感字段
  document.querySelectorAll('#config-form input[data-kind="secret-new"]').forEach((input) => {
    if (input.style.display !== 'none') secretTouched.add(input.dataset.path);
  });
  for (const el of fields) {
    const path = el.dataset.path;
    const kind = el.dataset.kind;
    if (kind === 'secret-keep') continue;
    if (kind === 'secret-new') {
      // 点了修改：填了新值用新值；留空发空串（后端保留原值）
      setPath(out, path, el.value);
      continue;
    }
    if (kind === 'bool') { setPath(out, path, el.checked); continue; }
    if (kind === 'number') { setPath(out, path, el.value === '' ? 0 : parseInt(el.value, 10)); continue; }
    if (kind === 'intlist') {
      setPath(out, path, el.value.split(/[,，\s]+/).filter(Boolean).map((n) => parseInt(n, 10)).filter((n) => !Number.isNaN(n)));
      continue;
    }
    setPath(out, path, el.value);
  }
  void secretTouched;
  return out;
}

async function loadConfig() {
  $('cfg-msg').className = 'cfg-msg';
  $('cfg-msg').textContent = '正在加载配置……';
  try {
    const data = await api('GET', '/api/config');
    renderConfig(data.config);
    $('cfg-msg').textContent = '';
  } catch (err) {
    $('cfg-msg').className = 'cfg-msg err';
    $('cfg-msg').textContent = err.message === '401' ? '' : '服务未连接：' + err.message;
    $('config-form').innerHTML = '<div class="dim">服务未连接</div>';
  }
}

async function saveConfig() {
  const msg = $('cfg-msg');
  msg.className = 'cfg-msg';
  msg.textContent = '正在保存……';
  try {
    const payload = collectConfig();
    const data = await api('PUT', '/api/config', payload);
    if (data.errors && data.errors.length) {
      msg.className = 'cfg-msg err';
      msg.innerHTML = data.errors.map(esc).join('<br>');
      return;
    }
    // 成功：展示分段进度条，等重启就绪
    $('restart-progress').classList.remove('hidden');
    $('cfg-msg').textContent = '';
    const segBar = document.querySelector('.seg-bar');
    segBar.classList.add('on');
    $('restart-text').textContent = data.restarted
      ? '配置已写入，服务重启中……'
      : '配置已写入（' + (data.note || '未执行重启') + '）';
    // 轮询就绪（后端已轮询过一轮，这里再兜底 15 秒）
    const deadline = Date.now() + 15000;
    while (Date.now() < deadline) {
      try {
        const health = await (await fetch('/api/health')).json();
        if (health && health.status === 'ok') break;
      } catch (e) { /* 继续等 */ }
      await new Promise((r) => setTimeout(r, 1000));
    }
    segBar.classList.remove('on');
    $('restart-progress').classList.add('hidden');
    msg.className = 'cfg-msg ok';
    msg.textContent = data.ready === false && data.restarted
      ? '已保存，但服务 ' + 30 + ' 秒内未报告就绪，请检查容器日志'
      : '已保存并生效';
    addLocalLog('保存配置（' + (data.changed || []).join(', ') + '）');
    loadConfig();
  } catch (err) {
    if (err.message === '401') return;
    msg.className = 'cfg-msg err';
    msg.textContent = '保存失败：' + err.message;
  }
}

async function resetConfig() {
  if (!confirm('确定恢复默认配置吗？\n当前 config.json 将被内置模板覆盖，服务会重启！')) return;
  $('cfg-msg').className = 'cfg-msg';
  $('cfg-msg').textContent = '正在恢复默认……';
  try {
    const data = await api('POST', '/api/config/reset');
    $('cfg-msg').className = 'cfg-msg ok';
    $('cfg-msg').textContent = '已恢复默认' + (data.restarted ? '并重启服务' : '（' + (data.note || '') + '）');
    addLocalLog('恢复默认配置');
    loadConfig();
  } catch (err) {
    if (err.message === '401') return;
    $('cfg-msg').className = 'cfg-msg err';
    $('cfg-msg').textContent = '恢复失败：' + err.message;
  }
}

// ============================================================
// 状态监视（SSE 优先，失败回退 2 秒轮询）
// ============================================================

function startStatusStream() {
  $('refresh-mode').textContent = 'SSE';
  try {
    sseSource = new EventSource('/api/status/stream');
    sseSource.onmessage = (event) => {
      if (statusPaused) return;
      try { renderStatus(JSON.parse(event.data)); } catch (e) { /* 忽略半包 */ }
    };
    sseSource.onerror = () => {
      // SSE 不可用（代理断开等）→ 回退轮询
      if (sseSource) { sseSource.close(); sseSource = null; }
      if (!pollTimer && !statusPaused) {
        $('refresh-mode').textContent = '轮询';
        pollTimer = setInterval(pollStatusOnce, 2000);
        pollStatusOnce();
      }
    };
  } catch (e) {
    $('refresh-mode').textContent = '轮询';
    pollTimer = setInterval(pollStatusOnce, 2000);
  }
}

async function pollStatusOnce() {
  try {
    const st = await api('GET', '/api/status');
    renderStatus(st);
  } catch (err) {
    if (err.message === '401') return;
    renderStatus({ service_connected: false });
  }
}

function renderStatus(st) {
  const connected = st.service_connected !== false;
  // LED
  setLed('led-onebot', connected && st.onebot && st.onebot.connected);
  setLed('led-chatbridge', connected && st.chatbridge && st.chatbridge.enabled && st.chatbridge.connected);
  setLed('led-api', connected);
  // 公告栏
  const marquee = $('marquee');
  if (!connected) {
    marquee.textContent = '⚠ 服务未连接 —— 请检查 cxu-chathub 容器是否在运行';
    marquee.className = 'down';
  } else {
    const parts = [];
    parts.push('OneBot ' + (st.onebot.connected ? '已连接（self_id=' + st.onebot.self_id + '）' : '等待 NapCat 连入'));
    parts.push('ChatBridge ' + (st.chatbridge.enabled ? (st.chatbridge.connected ? '已连接' : '未连接') : '未启用'));
    if (st.patch_broadcast) parts.push('版本播报 ' + (st.patch_broadcast.enabled ? '已启用' : '未启用'));
    marquee.textContent = parts.join('　◆　');
    marquee.className = 'ok';
  }
  // 子系统
  if (Array.isArray(st.subsystems)) {
    $('subsystem-list').innerHTML = st.subsystems.map((s) =>
      `<div class="subsys-row"><span class="led ${s.healthy ? 'green' : 'red'}"></span>` +
      `<span class="sname">${esc(s.name)}</span><span class="sdetail">${esc(s.detail)}</span></div>`
    ).join('');
  } else {
    $('subsystem-list').innerHTML = '<div class="dim">服务未连接</div>';
  }
  // 系统信息
  if (connected) {
    $('info-version').textContent = 'v' + st.version;
    $('info-uptime').textContent = formatUptime(st.uptime_secs);
    $('info-fwd').textContent = st.forwarder ? st.forwarder.forwarded : '-';
    $('info-fail').textContent = st.forwarder ? st.forwarder.failed : '-';
    $('info-dup').textContent = st.forwarder ? st.forwarder.skipped_duplicate : '-';
    $('info-state').textContent = st.state ? st.state.forwarded_count + ' 条' : '-';
  }
  $('last-refresh').textContent = new Date().toTimeString().slice(0, 8);
  $('sb-conn').textContent = connected ? '服务：已连接' : '服务：未连接';
}

function setLed(id, ok) {
  const led = $(id);
  led.className = 'led ' + (ok ? 'green' : 'red');
}

function formatUptime(secs) {
  if (secs == null) return '-';
  const m = Math.floor(secs / 60);
  if (m < 60) return m + ' 分钟';
  return Math.floor(m / 60) + ' 小时 ' + (m % 60) + ' 分';
}

function togglePause() {
  statusPaused = !statusPaused;
  $('pause-btn').textContent = statusPaused ? '继续刷新' : '暂停刷新';
  $('sb-hint').textContent = statusPaused ? '状态刷新已暂停' : '就绪';
}

// ============================================================
// 消息列表 / relay / 日志
// ============================================================

async function refreshMessages() {
  try {
    const data = await api('GET', '/api/messages?limit=50');
    const tbody = $('msg-table').querySelector('tbody');
    const messages = (data.messages || []).slice().reverse();
    if (!messages.length) {
      tbody.innerHTML = '<tr><td colspan="4" class="dim">暂无消息</td></tr>';
      return;
    }
    tbody.innerHTML = messages.map((m) => {
      const time = m.timestamp ? new Date(m.timestamp).toTimeString().slice(0, 8) : '-';
      return `<tr><td class="mono">${esc(time)}</td><td><span class="src-tag">${esc(m.source)}</span></td>` +
        `<td>${esc(m.from)}</td><td>${esc(m.text)}</td></tr>`;
    }).join('');
  } catch (err) {
    if (err.message === '401') return;
    $('msg-table').querySelector('tbody').innerHTML =
      '<tr><td colspan="4" style="color:#a00000">服务未连接</td></tr>';
  }
}

async function relaySend() {
  const result = $('relay-result');
  const target = $('relay-target').value;
  const text = $('relay-text').value.trim();
  const groupIdRaw = $('relay-group').value.trim();
  if (!text) { result.className = 'relay-result err'; result.textContent = '内容不能为空'; return; }
  const body = { target, text };
  if (groupIdRaw) {
    const gid = parseInt(groupIdRaw, 10);
    if (Number.isNaN(gid)) { result.className = 'relay-result err'; result.textContent = '群号必须是数字'; return; }
    body.group_id = gid;
  }
  result.className = 'relay-result';
  result.textContent = '发送中……';
  try {
    const data = await api('POST', '/api/relay', body);
    if (data.ok) {
      result.className = 'relay-result ok';
      result.textContent = '已送达';
      $('relay-text').value = '';
    } else {
      result.className = 'relay-result err';
      result.textContent = '投递失败（' + (data.error || '目标端未就绪') + '）';
    }
  } catch (err) {
    if (err.message === '401') return;
    result.className = 'relay-result err';
    result.textContent = '服务未连接';
  }
}

// ---------- 操作日志（后端 /api/logs + 本地动作合并，最新在最上） ----------
function addLocalLog(text) {
  localLogs.unshift({ time: new Date().toTimeString().slice(0, 8), user: $('whoami').textContent, action: '本地', detail: text, result: 'ok' });
  renderLogs();
}

async function refreshLogs() {
  let backendLogs = [];
  try {
    const data = await api('GET', '/api/logs');
    backendLogs = data.logs || [];
  } catch (err) {
    if (err.message === '401') return;
    // 服务未连接：保留本地日志
  }
  renderLogs(backendLogs);
}

function renderLogs(backendLogs) {
  const tbody = $('log-table').querySelector('tbody');
  const all = backendLogs ? backendLogs.concat(localLogs) : localLogs.concat(localLogs);
  const rows = all.slice(0, 200);
  if (!rows.length) {
    tbody.innerHTML = '<tr><td colspan="5" class="dim">暂无日志</td></tr>';
    return;
  }
  tbody.innerHTML = rows.map((l) =>
    `<tr><td class="mono">${esc(l.time)}</td><td>${esc(l.user)}</td><td>${esc(l.action)}</td>` +
    `<td>${esc(l.detail)}</td><td class="${String(l.result).includes('失败') || String(l.result).includes('拒绝') ? 'log-result-err' : 'log-result-ok'}">${esc(l.result)}</td></tr>`
  ).join('');
}

// ---------- 小工具 ----------
function esc(s) {
  return String(s ?? '').replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}

// ---------- 事件绑定与启动 ----------
document.addEventListener('DOMContentLoaded', () => {
  $('login-btn').addEventListener('click', doLogin);
  $('login-reset').addEventListener('click', () => { $('login-user').value = 'admin'; $('login-pass').value = ''; $('login-msg').textContent = ''; });
  $('login-pass').addEventListener('keydown', (e) => { if (e.key === 'Enter') doLogin(); });

  $('logout-btn').addEventListener('click', doLogout);
  $('pause-btn').addEventListener('click', togglePause);
  $('cfg-save').addEventListener('click', saveConfig);
  $('cfg-reload').addEventListener('click', loadConfig);
  $('cfg-reset').addEventListener('click', resetConfig);
  $('relay-send').addEventListener('click', relaySend);
  $('log-clear').addEventListener('click', () => { localLogs = []; renderLogs(); });

  // 启动：探测登录态（/api/health 401 → 登录页；200 → 进控制台）
  fetch('/api/health')
    .then((r) => (r.ok ? enterConsole('admin') : showLogin('')))
    .catch(() => showLogin('服务未连接'));
});
