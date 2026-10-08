// ============================================================
// cxu-chathub 控制台 —— 前端逻辑（原生 JS）
// 页面结构：单张 800px 表格；配置页按组合并（每组一次保存）。
// 组保存 = PUT 该组全部字段（部分提交），后端深合并。
// ============================================================

'use strict';

// ---------- 配置分区：每个分区 = 一个 fieldset + 一张表 ----------
const SECTIONS = {
  onebot: { title: 'OneBot 配置 —— NapCat 反向 WS 接入', fields: [
    { k: 'listen_host', label: '监听地址', type: 'text', size: 25 },
    { k: 'listen_port', label: '监听端口', type: 'number', size: 15 },
    { k: 'path', label: 'WS 路径', type: 'text', size: 25 },
    { k: 'access_token', label: 'access_token', type: 'secret', size: 25 },
    { k: 'self_id', label: '机器人 QQ', type: 'number', size: 15 },
  ] },
  chatroom: { title: 'Chatroom 对接 —— 服务端读写与白名单', fields: [
    { k: 'base_url', label: '服务端地址', type: 'text', size: 45 },
    { k: 'channel_id', label: '目标频道 ID', type: 'number', size: 15 },
    { k: 'forward_token', label: 'forward_token', type: 'secret', size: 25 },
    { k: 'refresh_token', label: 'refresh_token', type: 'secret', size: 25 },
    { k: 'group_ids', label: 'QQ 群白名单', type: 'intlist', size: 30 },
    { k: 'qq_sync_enabled', label: '群→chatroom 同步', type: 'bool' },
    { k: 'qq_forward_enabled', label: '!q → QQ 群', type: 'bool' },
    { k: 'qq_to_game_enabled', label: '!q → 游戏内', type: 'bool' },
    { k: 'player_join_pattern', label: '上线识别正则', type: 'text', size: 45 },
    { k: 'player_quit_pattern', label: '下线识别正则', type: 'text', size: 45 },
    { k: 'voice_api', label: '语音 API', type: 'text', size: 45 },
    { k: 'status_api', label: '状态 API', type: 'text', size: 45 },
  ] },
  chatbridge: { title: 'ChatBridge 互通 —— MC 游戏服（AES-CBC/TCP 21027）', fields: [
    { k: 'enabled', label: '启用', type: 'bool' },
    { k: 'host', label: '游戏服地址', type: 'text', size: 30 },
    { k: 'port', label: '端口', type: 'number', size: 15 },
    { k: 'name', label: '客户端标识', type: 'text', size: 25 },
    { k: 'password', label: 'password', type: 'secret', size: 25 },
    { k: 'aes_key', label: 'aes_key', type: 'secret', size: 25 },
  ] },
  commands: { title: '群命令 —— /chatroom /server 与权限', fields: [
    { k: 'group_allow_all', label: '所有群可用', type: 'bool' },
    { k: 'status_image', label: '/server 发状态图', type: 'bool' },
    { k: 'allow_from', label: '白名单群', type: 'intlist', size: 30 },
  ] },
  api: { title: 'API 服务 —— HTTP API（本控制台的数据来源）', fields: [
    { k: 'enabled', label: '启用', type: 'bool' },
    { k: 'listen_host', label: '监听地址', type: 'text', size: 25 },
    { k: 'listen_port', label: '监听端口', type: 'number', size: 15 },
    { k: 'access_token', label: 'access_token', type: 'secret', size: 25 },
  ] },
  agent: { title: 'Agent 能力 —— 检索问答 / 智能路由 / LLM', fields: [
    { k: 'enabled', label: '启用', type: 'bool' },
    { k: 'llm.api_url', label: 'LLM API 地址', type: 'text', size: 45 },
    { k: 'llm.api_key', label: 'LLM api_key', type: 'secret', size: 25 },
    { k: 'llm.model', label: 'LLM 模型', type: 'text', size: 25 },
    { k: 'llm.timeout_secs', label: 'LLM 超时(秒)', type: 'number', size: 15 },
    { k: 'llm.max_answer_chars', label: '回答长度上限', type: 'number', size: 15 },
    { k: 'routing.enabled', label: '智能路由(@bot)', type: 'bool' },
    { k: 'routing.group_ids', label: '路由灰度群', type: 'intlist', size: 30 },
  ] },
  patch: { key: 'patch_broadcast', title: '版本播报 —— Mojang 更新自动播报', fields: [
    { k: 'enabled', label: '启用', type: 'bool' },
    { k: 'feed_url', label: '官方 feed（v2）', type: 'text', size: 45 },
    { k: 'poll_interval_secs', label: '轮询间隔(秒)', type: 'number', size: 15 },
  ] },
  root: { title: '运行参数 —— 状态文件与日志', fields: [
    { k: 'state_path', label: 'state.json 路径', type: 'text', size: 45 },
    { k: 'log_level', label: '日志级别', type: 'select', options: ['DEBUG', 'INFO', 'WARNING', 'ERROR'] },
    { k: 'log_format', label: '日志格式', type: 'select', options: ['text', 'json'] },
  ] },
};

// ---------- 页面 → 分组（导航页；组保存 = 一次 PUT 全部组内字段） ----------
const PAGES = {
  overview: { title: '系统总览' },
  access: { title: '接入与互通', sections: ['onebot', 'chatroom', 'chatbridge'] },
  features: { title: '功能开关', sections: ['commands', 'api', 'agent', 'patch'] },
  root: { title: '运行参数', sections: ['root'] },
  messages: { title: '消息与下发' },
  logs: { title: '运行日志' },
};

// ---------- 全局状态 ----------
let requestCount = 0;
let statusPaused = false;
let sseSource = null;
let pollTimer = null;
let msgTimer = null;
let logTimer = null;
let configData = null;
let localLogs = [];

const $ = (id) => document.getElementById(id);

// ---------- 状态栏 / 时钟 ----------
function setOp(text) { $('sb-op').textContent = text; }
function tickClock() {
  const now = new Date().toTimeString().slice(0, 8);
  $('sb-time').textContent = now;
  $('clock').textContent = now;
}

// ---------- 请求封装（计数 + 401 回登录） ----------
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

// ---------- 登录 / 注销（登录页保持不变） ----------
function showLogin(message) {
  stopAll();
  $('console').hidden = true;
  $('login-screen').hidden = false;
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
  $('login-screen').hidden = true;
  $('console').hidden = false;
  $('whoami').textContent = user;
  requestCount = 0;
  $('req-count').textContent = '0';
  localLogs = [];
  switchPage('overview');
  loadConfig();
  startStatusStream();
  refreshMessages();
  msgTimer = setInterval(refreshMessages, 2000);
  refreshLogs();
  logTimer = setInterval(refreshLogs, 5000);
  setOp('就绪');
}

function stopAll() {
  if (sseSource) { sseSource.close(); sseSource = null; }
  if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
  if (msgTimer) { clearInterval(msgTimer); msgTimer = null; }
  if (logTimer) { clearInterval(logTimer); logTimer = null; }
}

// ---------- 主导航：切换表格行的显隐 ----------
function switchPage(name) {
  for (const el of document.querySelectorAll('tr.page')) el.hidden = true;
  const target = $('page-' + name);
  if (target) target.hidden = false;
  for (const link of document.querySelectorAll('#navbar a')) {
    link.style.color = link.dataset.page === name ? '#a00000' : '';
  }
  setOp('浏览：' + (PAGES[name] ? PAGES[name].title : name));
  window.scrollTo(0, 0);
}

// ============================================================
// 配置表单（schema 驱动：单列从上到下，label 150 右对齐）
// ============================================================
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

// 字段的绝对配置路径（root 分区直接在顶层；分区可用 section.key 指定实际配置段名，
// 如 patch 分区对应 patch_broadcast，见 SECTIONS.patch.key）
function fieldPath(secKey, section, field) {
  const base = section.key || secKey;
  return secKey === 'root' ? field.k : base + '.' + field.k;
}

// 单个字段控件（含 data-path = 绝对配置路径，供收集）
function buildFieldControl(field, dotted, value) {
  const size = field.size || 25;
  if (field.type === 'secret') {
    const isSet = !!(value && value.is_set);
    const wrap = document.createElement('span');
    const state = document.createElement('span');
    state.style.color = isSet ? 'green' : 'gray';
    state.textContent = isSet ? '已设置' : '未设置';
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.innerHTML = '<font face="SimSun,宋体" size="2">修改</font>';
    const input = document.createElement('input');
    input.type = 'password';
    input.size = size;
    input.dataset.path = dotted;
    input.dataset.kind = 'secret-new';
    input.style.display = 'none';
    btn.addEventListener('click', () => {
      const showing = input.style.display !== 'none';
      input.style.display = showing ? 'none' : 'inline-block';
      btn.innerHTML = showing ? '修改' : '<font face="SimSun,宋体" size="2">取消</font>';
      if (!showing) input.focus();
    });
    wrap.appendChild(state);
    wrap.appendChild(btn);
    wrap.appendChild(input);
    return wrap;
  }
  if (field.type === 'bool') {
    const cb = document.createElement('input');
    cb.type = 'checkbox';
    cb.dataset.path = dotted;
    cb.dataset.kind = 'bool';
    cb.checked = !!value;
    return cb;
  }
  if (field.type === 'select') {
    const select = document.createElement('select');
    select.dataset.path = dotted;
    select.dataset.kind = 'select';
    for (const option of field.options) {
      const opt = document.createElement('option');
      opt.value = option;
      opt.textContent = option;
      if (value === option) opt.selected = true;
      select.appendChild(opt);
    }
    return select;
  }
  if (field.type === 'intlist') {
    const input = document.createElement('input');
    input.type = 'text';
    input.size = size;
    input.dataset.path = dotted;
    input.dataset.kind = 'intlist';
    input.value = Array.isArray(value) ? value.join(', ') : '';
    input.placeholder = '如: 123456, 789000';
    return input;
  }
  const input = document.createElement('input');
  input.type = 'text';
  input.size = size;
  input.dataset.path = dotted;
  input.dataset.kind = field.type === 'number' ? 'number' : 'text';
  if (field.type === 'number') input.classList.add('mono');
  input.value = value === undefined || value === null ? '' : value;
  return input;
}

function renderAllConfigForms(masked) {
  configData = masked;
  for (const [secKey, section] of Object.entries(SECTIONS)) {
    const table = $('tbl-' + secKey);
    if (!table) continue;
    table.innerHTML = '';
    for (const field of section.fields) {
      const dotted = fieldPath(secKey, section, field);
      const tr = table.insertRow(-1);
      const tdLabel = tr.insertCell(-1);
      tdLabel.width = 150;
      tdLabel.align = 'right';
      tdLabel.textContent = field.label + '：';
      const tdValue = tr.insertCell(-1);
      tdValue.align = 'left';
      tdValue.appendChild(buildFieldControl(field, dotted, getPath(masked, dotted)));
    }
  }
}

// 收集某组的全部字段 → 部分配置对象（未动的敏感字段原样带回脱敏对象 = 后端保留原值）
function collectPage(pageKey) {
  const payload = {};
  for (const secKey of PAGES[pageKey].sections || []) {
    for (const el of document.querySelectorAll('#tbl-' + secKey + ' [data-path]')) {
      const dotted = el.dataset.path;   // 绝对配置路径（root 分区即顶层）
      const kind = el.dataset.kind;
      if (kind === 'bool') { setPath(payload, dotted, el.checked); continue; }
      if (kind === 'number') { setPath(payload, dotted, el.value === '' ? 0 : parseInt(el.value, 10)); continue; }
      if (kind === 'intlist') {
        setPath(payload, dotted, el.value.split(/[,，\s]+/).filter(Boolean).map((n) => parseInt(n, 10)).filter((n) => !Number.isNaN(n)));
        continue;
      }
      if (kind === 'select') { setPath(payload, dotted, el.value); continue; }
      // secret-new：点了修改 → 填了用新值，留空发空串（后端保留原值）
      setPath(payload, dotted, el.value);
    }
  }
  return payload;
}

async function loadConfig() {
  try {
    const data = await api('GET', '/api/config');
    renderAllConfigForms(data.config);
  } catch (err) {
    if (err.message === '401') return;
    setOp('配置加载失败：' + err.message);
    for (const secKey of Object.keys(SECTIONS)) {
      const table = $('tbl-' + secKey);
      if (table) table.innerHTML = '<tr><td style="color:#a00000">服务未连接</td></tr>';
    }
  }
}

async function savePage(pageKey) {
  const msg = $('msg-' + pageKey);
  msg.textContent = '配置已提交，正在写入……';
  setOp('正在保存 ' + PAGES[pageKey].title + ' …');
  try {
    const data = await api('PUT', '/api/config', collectPage(pageKey));
    if (data.errors && data.errors.length) {
      msg.style.color = 'red';
      msg.textContent = '校验失败：' + data.errors.join('；');
      setOp('保存失败（校验未通过）');
      return;
    }
    msg.style.color = 'red';
    msg.textContent = data.restarted
      ? '配置已保存，服务正在重启...'
      : '配置已保存（' + (data.note || '未执行重启') + '）';
    setOp('服务重启中……');
    const deadline = Date.now() + 15000;
    while (Date.now() < deadline) {
      try {
        const health = await (await fetch('/api/health')).json();
        if (health && health.status === 'ok') break;
      } catch (e) { /* 继续等 */ }
      await new Promise((r) => setTimeout(r, 1000));
    }
    msg.style.color = 'green';
    msg.textContent = data.restarted && data.ready === false
      ? '已保存，但服务 30 秒内未报告就绪，请检查容器日志'
      : '配置已保存并生效';
    setOp('就绪');
    addLocalLog('保存配置·' + PAGES[pageKey].title + '（' + (data.changed || []).join(', ') + '）');
    loadConfig();
  } catch (err) {
    if (err.message === '401') return;
    msg.style.color = 'red';
    msg.textContent = '保存失败：' + err.message;
    setOp('保存失败');
  }
}

async function reloadPage(pageKey) {
  await loadConfig();
  const msg = $('msg-' + pageKey);
  msg.style.color = 'black';
  msg.textContent = '已从服务器还原本组';
  setOp('还原本组：' + PAGES[pageKey].title);
}

// ============================================================
// 状态监视（SSE 优先，断线回退 2 秒轮询）
// ============================================================
function startStatusStream() {
  try {
    sseSource = new EventSource('/api/status/stream');
    sseSource.onmessage = (event) => {
      if (statusPaused) return;
      try { renderStatus(JSON.parse(event.data)); } catch (e) { /* 忽略半包 */ }
    };
    sseSource.onerror = () => {
      if (sseSource) { sseSource.close(); sseSource = null; }
      if (!pollTimer && !statusPaused) {
        pollTimer = setInterval(pollStatusOnce, 2000);
        pollStatusOnce();
      }
    };
  } catch (e) {
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

// LED 用表格单元格的 bgcolor 表达（无 emoji、无现代 CSS）
function setLed(id, ok) {
  $(id).setAttribute('bgcolor', ok ? '#00FF00' : '#FF0000');
}

function renderStatus(st) {
  const connected = st.service_connected !== false;
  setLed('led-onebot', connected && st.onebot && st.onebot.connected);
  setLed('led-chatbridge', connected && st.chatbridge && st.chatbridge.enabled && st.chatbridge.connected);
  setLed('led-api', connected);
  const marquee = $('marquee');
  if (!connected) {
    marquee.innerHTML = '<font color="#a00000"><b>⚠ 服务未连接 —— 请检查 cxu-chathub 容器是否在运行</b></font>';
  } else {
    const parts = [];
    parts.push('OneBot ' + (st.onebot.connected ? '已连接（self_id=' + st.onebot.self_id + '）' : '等待 NapCat 连入'));
    parts.push('ChatBridge ' + (st.chatbridge.enabled ? (st.chatbridge.connected ? '已连接' : '未连接') : '未启用'));
    if (st.patch_broadcast) parts.push('版本播报 ' + (st.patch_broadcast.enabled ? '已启用' : '未启用'));
    marquee.innerHTML = '<font color="#006000">' + parts.map(esc).join('　◆　') + '</font>';
  }
  const list = $('subsystem-list');
  if (Array.isArray(st.subsystems)) {
    // 灰网格嵌套表：cellspacing=1 + bgcolor=#808080 形成经典格线
    list.innerHTML =
      '<table border="1" cellpadding="4" cellspacing="1" bgcolor="#808080" width="100%">' +
      st.subsystems.map((s) =>
        '<tr>' +
        `<td width="12" height="12" bgcolor="${s.healthy ? '#00FF00' : '#FF0000'}" style="border:1px solid #000"></td>` +
        `<td bgcolor="#C0C0C0"><b>${esc(s.name)}</b>　${esc(s.detail)}</td>` +
        '</tr>'
      ).join('') +
      '</table>';
  } else {
    list.innerHTML = '<font color="#666666">服务未连接</font>';
  }
  if (connected) {
    $('info-version').textContent = 'v' + st.version;
    $('info-uptime').textContent = formatUptime(st.uptime_secs);
    $('info-fwd').textContent = st.forwarder ? st.forwarder.forwarded : '-';
    $('info-fail').textContent = st.forwarder ? st.forwarder.failed : '-';
    $('info-dup').textContent = st.forwarder ? st.forwarder.skipped_duplicate : '-';
    $('info-state').textContent = st.state ? st.state.forwarded_count + ' 条' : '-';
  }
  $('last-refresh').textContent = new Date().toTimeString().slice(0, 8);
  const mode = sseSource ? 'SSE' : (pollTimer ? '轮询' : '-');
  $('refresh-mode').textContent = mode;
}

function formatUptime(secs) {
  if (secs == null) return '-';
  const m = Math.floor(secs / 60);
  if (m < 60) return m + ' 分钟';
  return Math.floor(m / 60) + ' 小时 ' + (m % 60) + ' 分';
}

function togglePause() {
  statusPaused = !statusPaused;
  setOp(statusPaused ? '状态刷新已暂停' : '就绪');
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
      return `<tr><td class="mono">${esc(time)}</td><td><font color="#000080" size="1"><b>${esc(m.source)}</b></font></td>` +
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

// ---------- 运行日志（后端 + 本地合并，最新在最上） ----------
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
  }
  renderLogs(backendLogs);
}

function renderLogs(backendLogs) {
  const tbody = $('log-table').querySelector('tbody');
  const all = backendLogs ? backendLogs.concat(localLogs) : localLogs;
  const rows = all.slice(0, 200);
  if (!rows.length) {
    tbody.innerHTML = '<tr><td colspan="5" class="dim">暂无日志</td></tr>';
    return;
  }
  tbody.innerHTML = rows.map((l) =>
    `<tr><td class="mono">${esc(l.time)}</td><td>${esc(l.user)}</td><td>${esc(l.action)}</td>` +
    `<td>${esc(l.detail)}</td><td>${String(l.result).includes('失败') || String(l.result).includes('拒绝') ? `<font color="#a00000"><b>${esc(l.result)}</b></font>` : `<font color="#006000">${esc(l.result)}</font>`}</td></tr>`
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

  for (const link of document.querySelectorAll('#navbar a.navlink, .foot a.navlink')) {
    link.addEventListener('click', (e) => { e.preventDefault(); switchPage(link.dataset.page); });
  }

  for (const btn of document.querySelectorAll('[data-save]')) {
    btn.addEventListener('click', () => savePage(btn.dataset.save));
  }
  for (const btn of document.querySelectorAll('[data-reload]')) {
    btn.addEventListener('click', () => reloadPage(btn.dataset.reload));
  }

  $('logout-link').addEventListener('click', (e) => { e.preventDefault(); doLogout(); });
  $('pause-link').addEventListener('click', (e) => { e.preventDefault(); togglePause(); });
  $('relay-send').addEventListener('click', relaySend);
  $('log-clear').addEventListener('click', () => { localLogs = []; renderLogs(); });

  tickClock();
  setInterval(tickClock, 1000);

  fetch('/api/health')
    .then((r) => (r.ok ? enterConsole('admin') : showLogin('')))
    .catch(() => showLogin('服务未连接'));
});
