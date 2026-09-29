// ============================================================
// cxu-chathub 控制台 —— 前端逻辑（原生 JS）
// 页面 = 单张 800px 表格的行；导航切换行的显隐。
// 表单按 schema 渲染：短字段两两一行（label 120 右对齐 + 固定 size 输入框），
// 长字段独占一行（colspan=3）。保存 = PUT 部分字段，后端深合并。
// ============================================================

'use strict';

// ---------- 配置 schema：一个分组 = 导航的一页 ----------
// half: true = 可两个并排一行；否则独占一行（colspan=3）
const FIELD_GROUPS = {
  onebot: { title: 'OneBot配置', fields: [
    { k: 'listen_host', label: '监听地址', type: 'text', half: true },
    { k: 'listen_port', label: '监听端口', type: 'number', half: true },
    { k: 'path', label: 'WS 路径', type: 'text', half: true },
    { k: 'self_id', label: '机器人 QQ', type: 'number', half: true },
    { k: 'access_token', label: 'access_token', type: 'secret' },
  ] },
  chatroom: { title: 'Chatroom对接', fields: [
    { k: 'base_url', label: '服务端地址', type: 'text' },
    { k: 'channel_id', label: '目标频道 ID', type: 'number', half: true },
    { k: 'poll_interval', label: '读轮询间隔(秒)', type: 'number', half: true },
    { k: 'forward_token', label: 'forward_token', type: 'secret' },
    { k: 'refresh_token', label: 'refresh_token', type: 'secret' },
    { k: 'group_ids', label: 'QQ 群白名单', type: 'intlist' },
    { k: 'qq_sync_enabled', label: '群→chatroom 同步', type: 'bool', half: true },
    { k: 'qq_forward_enabled', label: '!q → QQ 群', type: 'bool', half: true },
    { k: 'qq_to_game_enabled', label: '!q → 游戏内', type: 'bool', half: true },
    { k: 'player_join_pattern', label: '上线识别正则', type: 'text' },
    { k: 'player_quit_pattern', label: '下线识别正则', type: 'text' },
    { k: 'voice_api', label: '语音 API', type: 'text' },
    { k: 'status_api', label: '状态 API', type: 'text' },
  ] },
  chatbridge: { title: 'ChatBridge互通', fields: [
    { k: 'enabled', label: '启用', type: 'bool', half: true },
    { k: 'port', label: '端口', type: 'number', half: true },
    { k: 'host', label: '游戏服地址', type: 'text' },
    { k: 'name', label: '客户端标识', type: 'text', half: true },
    { k: 'password', label: 'password', type: 'secret' },
    { k: 'aes_key', label: 'aes_key', type: 'secret' },
  ] },
  commands: { title: '群命令', fields: [
    { k: 'group_allow_all', label: '所有群可用', type: 'bool', half: true },
    { k: 'status_image', label: '/server 发状态图', type: 'bool', half: true },
    { k: 'allow_from', label: '白名单群', type: 'intlist' },
  ] },
  api: { title: 'API服务', fields: [
    { k: 'enabled', label: '启用', type: 'bool', half: true },
    { k: 'listen_port', label: '监听端口', type: 'number', half: true },
    { k: 'listen_host', label: '监听地址', type: 'text', half: true },
    { k: 'access_token', label: 'access_token', type: 'secret' },
  ] },
  agent: { title: 'Agent能力', fields: [
    { k: 'enabled', label: '启用', type: 'bool', half: true },
    { k: 'llm.timeout_secs', label: 'LLM 超时(秒)', type: 'number', half: true },
    { k: 'llm.api_url', label: 'LLM API 地址', type: 'text' },
    { k: 'llm.api_key', label: 'LLM api_key', type: 'secret' },
    { k: 'llm.model', label: 'LLM 模型', type: 'text', half: true },
    { k: 'llm.max_answer_chars', label: '回答长度上限', type: 'number', half: true },
    { k: 'routing.enabled', label: '智能路由(@bot)', type: 'bool', half: true },
    { k: 'routing.group_ids', label: '路由灰度群', type: 'intlist' },
  ] },
  patch: { key: 'patch_broadcast', title: '版本播报', fields: [
    { k: 'enabled', label: '启用', type: 'bool', half: true },
    { k: 'poll_interval_secs', label: '轮询间隔(秒)', type: 'number', half: true },
    { k: 'feed_url', label: '官方 feed（v2）', type: 'text' },
  ] },
  root: { title: '运行参数', fields: [
    { k: 'state_path', label: 'state.json 路径', type: 'text' },
    { k: 'log_level', label: '日志级别', type: 'select', options: ['DEBUG', 'INFO', 'WARNING', 'ERROR'], half: true },
    { k: 'log_format', label: '日志格式', type: 'select', options: ['text', 'json'], half: true },
  ] },
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

// ---------- 主导航：切换表格行的显隐（整体切换感） ----------
function switchPage(name) {
  for (const el of document.querySelectorAll('tr.page')) el.classList.add('hidden');
  const target = $('page-' + name);
  if (target) target.classList.remove('hidden');
  for (const link of document.querySelectorAll('#navbar a')) {
    link.classList.toggle('current', link.dataset.page === name);
  }
  setOp('浏览：' + (FIELD_GROUPS[name] ? FIELD_GROUPS[name].title : navTitle(name)));
  window.scrollTo(0, 0);
}
function navTitle(name) {
  return { overview: '系统总览', messages: '消息与下发', logs: '运行日志' }[name] || name;
}

// ============================================================
// 配置表单（schema 驱动：短字段两两一行，长字段独占一行 colspan=3）
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

// 单个字段 → 控件（含 data-path 供收集）
function buildFieldControl(field, dotted, value) {
  if (field.type === 'secret') {
    const isSet = !!(value && value.is_set);
    const wrap = document.createElement('span');
    const state = document.createElement('span');
    state.style.color = isSet ? 'green' : 'gray';
    state.textContent = isSet ? '已设置' : '未设置';
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.textContent = '修改';
    const input = document.createElement('input');
    input.type = 'password';
    input.size = 25;
    input.dataset.path = dotted;
    input.dataset.kind = 'secret-new';
    input.style.display = 'none';
    btn.addEventListener('click', () => {
      const showing = input.style.display !== 'none';
      input.style.display = showing ? 'none' : 'inline-block';
      btn.textContent = showing ? '修改' : '取消';
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
    input.size = 25;
    input.dataset.path = dotted;
    input.dataset.kind = 'intlist';
    input.value = Array.isArray(value) ? value.join(', ') : '';
    input.placeholder = '如: 123456, 789000';
    return input;
  }
  const input = document.createElement('input');
  input.type = field.type === 'number' ? 'text' : 'text';   // 数字也用 text + size（90s 形态）
  input.size = field.type === 'number' ? 15 : 25;
  input.dataset.path = dotted;
  input.dataset.kind = field.type === 'number' ? 'number' : 'text';
  if (field.type === 'number') input.classList.add('mono');
  input.value = value === undefined || value === null ? '' : value;
  return input;
}

// 一行：label(120 右对齐) + 控件；half 配对时两对同一行
function appendFieldRow(table, field, dotted, value, pairWith) {
  const tr = table.insertRow(-1);
  if (pairWith) {
    const td1 = tr.insertCell(-1);
    td1.width = 120; td1.align = 'right'; td1.textContent = pairWith.field.label + '：';
    const td2 = tr.insertCell(-1);
    td2.appendChild(buildFieldControl(pairWith.field, pairWith.dotted, pairWith.value));
    const td3 = tr.insertCell(-1);
    td3.width = 120; td3.align = 'right'; td3.textContent = field.label + '：';
    const td4 = tr.insertCell(-1);
    td4.appendChild(buildFieldControl(field, dotted, value));
  } else {
    const td1 = tr.insertCell(-1);
    td1.width = 120; td1.align = 'right'; td1.textContent = field.label + '：';
    const td2 = tr.insertCell(-1);
    td2.colSpan = 3;
    td2.appendChild(buildFieldControl(field, dotted, value));
  }
}

function renderAllConfigForms(masked) {
  configData = masked;
  for (const [pageKey, group] of Object.entries(FIELD_GROUPS)) {
    const table = $('tbl-' + pageKey);
    if (!table) continue;
    const sectionKey = group.key || pageKey;
    table.innerHTML = '';
    let pendingHalf = null;   // 等待配对的半行字段
    for (const field of group.fields) {
      const dotted = sectionKey === 'root' ? field.k : sectionKey + '.' + field.k;
      const value = getPath(masked, dotted);
      if (field.half) {
        if (pendingHalf) {
          appendFieldRow(table, field, dotted, value, pendingHalf);
          pendingHalf = null;
        } else {
          pendingHalf = { field, dotted, value };
        }
      } else {
        if (pendingHalf) {   // 半行字段后面跟了整行字段：先把半行按整行放出
          appendFieldRow(table, pendingHalf.field, pendingHalf.dotted, pendingHalf.value, null);
          pendingHalf = null;
        }
        appendFieldRow(table, field, dotted, value, null);
      }
    }
    if (pendingHalf) appendFieldRow(table, pendingHalf.field, pendingHalf.dotted, pendingHalf.value, null);
  }
}

// 收集某页的字段 → 部分配置对象（未动的敏感字段原样带回脱敏对象 = 后端保留原值）
function collectPage(pageKey) {
  const group = FIELD_GROUPS[pageKey];
  const sectionKey = group.key || pageKey;
  const payload = sectionKey === 'root' ? {} : { [sectionKey]: {} };
  for (const el of document.querySelectorAll('#tbl-' + pageKey + ' [data-path]')) {
    const dotted = el.dataset.path;
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
  return payload;
}

async function loadConfig() {
  try {
    const data = await api('GET', '/api/config');
    renderAllConfigForms(data.config);
  } catch (err) {
    if (err.message === '401') return;
    setOp('配置加载失败：' + err.message);
    for (const [pageKey] of Object.entries(FIELD_GROUPS)) {
      const table = $('tbl-' + pageKey);
      if (table) table.innerHTML = '<tr><td style="color:#a00000">服务未连接</td></tr>';
    }
  }
}

async function savePage(pageKey) {
  const msg = $('msg-' + pageKey);
  const progress = $('progress-' + pageKey);
  const setMsg = (text) => { msg.textContent = text; };
  setMsg('配置已提交，正在写入……');
  setOp('正在保存 ' + FIELD_GROUPS[pageKey].title + ' …');
  try {
    const data = await api('PUT', '/api/config', collectPage(pageKey));
    if (data.errors && data.errors.length) {
      msg.style.color = 'red';
      setMsg('校验失败：' + data.errors.join('；'));
      setOp('保存失败（校验未通过）');
      return;
    }
    msg.style.color = 'red';
    progress.classList.remove('hidden');
    progress.querySelector('.seg-bar').classList.add('on');
    progress.querySelector('.seg-text').textContent = data.restarted
      ? '配置已保存，服务正在重启...'
      : '配置已保存（' + (data.note || '未执行重启') + '）';
    setMsg('配置已保存，服务正在重启...');
    setOp('服务重启中……');
    const deadline = Date.now() + 15000;
    while (Date.now() < deadline) {
      try {
        const health = await (await fetch('/api/health')).json();
        if (health && health.status === 'ok') break;
      } catch (e) { /* 继续等 */ }
      await new Promise((r) => setTimeout(r, 1000));
    }
    progress.querySelector('.seg-bar').classList.remove('on');
    progress.classList.add('hidden');
    msg.style.color = 'green';
    setMsg(data.restarted && data.ready === false
      ? '已保存，但服务 30 秒内未报告就绪，请检查容器日志'
      : '配置已保存并生效');
    setOp('就绪');
    addLocalLog('保存配置·' + FIELD_GROUPS[pageKey].title + '（' + (data.changed || []).join(', ') + '）');
    loadConfig();
  } catch (err) {
    if (err.message === '401') return;
    msg.style.color = 'red';
    setMsg('保存失败：' + err.message);
    setOp('保存失败');
  }
}

async function reloadPage(pageKey) {
  await loadConfig();
  const msg = $('msg-' + pageKey);
  msg.style.color = 'black';
  msg.textContent = '已从服务器还原本页';
  setOp('还原本页：' + FIELD_GROUPS[pageKey].title);
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

function renderStatus(st) {
  const connected = st.service_connected !== false;
  setLed('led-onebot', connected && st.onebot && st.onebot.connected);
  setLed('led-chatbridge', connected && st.chatbridge && st.chatbridge.enabled && st.chatbridge.connected);
  setLed('led-api', connected);
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
  if (Array.isArray(st.subsystems)) {
    $('subsystem-list').innerHTML = st.subsystems.map((s) =>
      `<div class="subsys-row"><span class="led ${s.healthy ? 'green' : 'red'}"></span> ` +
      `<span class="sname">${esc(s.name)}</span>　<span class="sdetail">${esc(s.detail)}</span></div>`
    ).join('');
  } else {
    $('subsystem-list').innerHTML = '<span class="dim">服务未连接</span>';
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

function setLed(id, ok) {
  $(id).className = 'led ' + (ok ? 'green' : 'red');
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
