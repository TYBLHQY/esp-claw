'use strict';
const $ = id => document.getElementById(id);
const MAX_MEMORY_FACT_BYTES = 512;
let token = sessionStorage.getItem('esp-management-code') || '';
let generation = null;
let refreshing = false;
let operating = false;
const quickPending = new Set();
let modeIntent = 0;
let lastStatus = null;
let timer = null;
let noticeTimer = null;
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

function notice(message, error = false) {
  $('notice').textContent = message;
  $('notice').classList.toggle('error', error);
  $('notice').hidden = false;
  clearTimeout(noticeTimer);
  noticeTimer = setTimeout(() => { $('notice').hidden = true; }, error ? 12000 : 5000);
}

function lock() {
  token = '';
  generation = null;
  lastStatus = null;
  sessionStorage.removeItem('esp-management-code');
  for (const id of ['management-code', 'wifi-password', 'deepseek-key', 'qq-app-secret', 'memory-fact', 'memory-fact-index']) $(id).value = '';
  $('memory-status').textContent = '尚未读取记忆状态。';
  $('memory-summary').textContent = '尚未读取摘要。';
  $('memory-facts').replaceChildren();
  $('memory-facts').hidden = true;
  $('memory-facts-empty').hidden = false;
  $('memory-facts-empty').textContent = '尚未读取记忆条目。';
  $('memory-meta').textContent = '读取后显示版本与保存情况。';
  $('memory-truncated').hidden = true;
  $('memory-operation').textContent = '等待操作';
  clearInterval(timer);
  $('manager').hidden = true;
  $('unlock').hidden = false;
  $('logout').hidden = true;
  $('connection').textContent = '尚未解锁';
  $('connection').className = 'pill';
}

async function api(path, method = 'GET', payload) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 9000);
  try {
    const response = await fetch(path, {
      method,
      headers: { Authorization: `Bearer ${token}`, ...(payload === undefined ? {} : { 'Content-Type': 'application/json' }) },
      body: payload === undefined ? undefined : JSON.stringify(payload),
      cache: 'no-store',
      credentials: 'omit',
      signal: controller.signal,
    });
    let data;
    try { data = await response.json(); } catch (_) { throw new Error('设备返回了不完整的数据，请刷新查看状态。'); }
    if (response.status === 401) { lock(); throw new Error('管理码不正确或已失效，请重新输入。'); }
    if (!response.ok || !data.ok) throw new Error(data.error || `请求失败（${response.status}）`);
    return data.result;
  } catch (error) {
    if (error.name === 'AbortError') throw new Error('等待设备超时，操作结果尚未确认；请刷新状态，不要重复提交。');
    throw error;
  } finally { clearTimeout(timeout); }
}

function formatBytes(value) {
  if (typeof value !== 'number') return '—';
  for (const [unit, scale] of [['GiB', 1073741824], ['MiB', 1048576], ['KiB', 1024]]) {
    if (value >= scale) return `${(value / scale).toFixed(1)} ${unit}`;
  }
  return `${value} B`;
}

function render(status) {
  lastStatus = status;
  const wifi = status.wifi || {};
  const services = status.services || {};
  const bot = services.bot || {};
  const hardware = status.hardware || {};
  const sd = status.sd || status.sdcard || {};
  $('wifi-state').textContent = wifi.online ? (wifi.time_synchronized ? '已连接' : '正在校时') : '未连接';
  $('wifi-address').textContent = wifi.online ? (wifi.ip || '') : (wifi.fault || '等待连接');
  $('bot-state').textContent = bot.gateway_ready ? (bot.mode === 'DeepSeek' ? 'DeepSeek 在线' : '连接测试在线') : (bot.active_workers ? '正在连接' : '已停止');
  $('bot-counts').textContent = `接收 ${bot.received || 0} · 发送 ${bot.sent || 0} · 错误 ${bot.errors || 0}`;
  $('heap-free').textContent = formatBytes(hardware.internal_free);
  $('heap-min').textContent = `最低可用 ${formatBytes(hardware.internal_minimum)}`;
  const sdState = typeof sd === 'string' ? sd : (sd.phase || sd.state || sd.status || '');
  const names = { available: '可用', mounted: '可用', ready: '可用', mounting: '正在连接', working: '正在操作', disabled: '未启用', failed: '连接失败', mount_failed: '连接失败', read_failed: '读取失败', write_failed: '写入失败', cleanup_failed: '断开失败', operation_timed_out: '操作超时', state_failed: '状态异常', error: '连接失败', write_failure: '写入失败', unavailable: '不可用', idle: '尚未连接' };
  $('sd-state').textContent = names[sdState] || (sdState ? String(sdState) : '尚未连接');
  $('sd-detail').textContent = sd.fault ? (typeof sd.fault === 'object' ? `${sd.fault.stage || '存储卡错误'}（${sd.fault.code ?? '—'}）` : String(sd.fault)) : (sd.error || (sd.capacity_bytes ? `容量 ${formatBytes(sd.capacity_bytes)} · 可用 ${formatBytes(sd.free_bytes)}` : '聊天不依赖存储卡'));
  $('connection').textContent = '设备已连接';
  $('connection').className = 'pill online';
  $('updated').textContent = `更新于 ${new Date().toLocaleTimeString()}`;
  const applied = status.led && status.led.driver_applied;
  $('led-state').textContent = Array.isArray(applied) ? `当前设置：红 ${applied[0]} · 绿 ${applied[1]} · 蓝 ${applied[2]}` : '灯控状态暂不可用';
  $('health').textContent = `${status.watchdog_registered ? '运行监测已启用' : '运行监测未就绪'} · Flash ${formatBytes(hardware.flash_bytes)} · PSRAM ${formatBytes(hardware.psram_bytes)}`;
  const details = [bot.recent_errors, bot.error_history].find(hasInformation) || null;
  const errors = { hardware: status.hardware_error, storage: status.nvs_error, wifi: status.wifi_error || wifi.fault, service: services.fault, recent: bot.last_error || null, details };
  const present = Object.fromEntries(Object.entries(errors).filter(([, value]) => hasInformation(value)));
  $('error-summary').textContent = Object.keys(present).length ? JSON.stringify(present, null, 2) : '当前没有错误信息。';
}

function hasInformation(value) {
  if (value === null || value === undefined || value === '') return false;
  if (Array.isArray(value)) return value.length > 0;
  if (typeof value === 'object') return Object.keys(value).length > 0;
  return true;
}

async function refresh(silent = false) {
  if (!token || refreshing) return;
  refreshing = true;
  try { render(await api('/api/status')); }
  catch (error) {
    $('connection').textContent = '连接中断';
    $('connection').className = 'pill offline';
    if (!silent) notice(error.message, true);
    throw error;
  } finally { refreshing = false; }
}

async function loadConfig() {
  const config = await api('/api/config');
  generation = Number.isInteger(config.generation) ? config.generation : null;
  for (const [field, key] of [['wifi-ssid', 'wifi_ssid'], ['deepseek-model', 'deepseek_model'], ['qq-app-id', 'qq_app_id']]) $(field).value = config[key] || '';
  $('allow-users').value = (config.allow_users || []).join('\n');
  $('allow-groups').value = (config.allow_groups || []).join('\n');
  for (const field of ['wifi-password', 'deepseek-key', 'qq-app-secret']) $(field).value = '';
  $('wifi-secret-state').textContent = config.wifi_password_present ? '设备已保存 Wi-Fi 密码。' : '设备未保存 Wi-Fi 密码。';
  $('deepseek-secret-state').textContent = config.deepseek_configured ? '设备已保存 DeepSeek 配置。' : '尚未配置 DeepSeek。';
  $('config-generation').textContent = generation === null ? '等待读取配置版本' : `当前配置版本 ${generation}`;
}

async function unlock(event) {
  if (event) event.preventDefault();
  const entered = $('management-code').value.trim();
  if (entered) token = entered;
  if (!token) { notice('请输入管理码。', true); return; }
  try {
    render(await api('/api/status'));
    sessionStorage.setItem('esp-management-code', token);
    $('management-code').value = '';
    $('unlock').hidden = true;
    $('manager').hidden = false;
    $('logout').hidden = false;
    await loadConfig();
    clearInterval(timer);
    timer = setInterval(() => { if (!document.hidden && !operating) refresh(true).catch(() => {}); }, 5000);
  } catch (error) { notice(error.message, true); }
}

async function execute(command) {
  return api('/api/command', 'POST', { command });
}

function showResult(value) {
  $('result').textContent = typeof value === 'string' ? value : JSON.stringify(value, null, 2);
}

async function finishDiagnostic(initial, command = '') {
  if (!initial || initial.started !== true) return initial;
  const end = Date.now() + 150000;
  while (Date.now() < end) {
    await sleep(1500);
    const status = await api('/api/status');
    render(status);
    if (command.startsWith('sd ')) {
      const sd = status.sd || status.sdcard || {};
      if (sd.phase === 'operation_timed_out' || (sd.fault && (!sd.busy || sd.io_disabled))) {
        const fault = sd.fault || {};
        throw new Error(`存储卡操作未完成（${fault.stage || sd.phase}，${fault.code ?? '—'}）。请查看存储卡状态。`);
      }
      if (!sd.busy) {
        if (initial.sequence !== undefined && sd.sequence !== initial.sequence) {
          throw new Error('存储卡状态已变化，无法确认该操作结果；请刷新查看状态。');
        }
        return sd.last_result || sd;
      }
      continue;
    }
    const services = status.services || {};
    if (!services.diagnostic_running) {
      const result = services.diagnostic_result;
      if (result && result.ok === false) throw new Error(result.error || '操作未完成。');
      return result && result.result !== undefined ? result.result : (result || initial);
    }
  }
  throw new Error('后台操作仍未完成，请稍后刷新查看结果，不要重复发起。');
}

function checkMemoryAccess(expectedToken) {
  if (!token || token !== expectedToken || $('manager').hidden) throw new Error('管理已锁定，请解锁后重新读取记忆。');
}

async function readMemoryStatus(expectedToken) {
  const status = await execute('memory status');
  checkMemoryAccess(expectedToken);
  // This endpoint returns storage/client metadata only, never chat text or credentials.
  $('memory-status').textContent = JSON.stringify(status, null, 2);
}

function renderMemoryPreview(preview) {
  if (!preview || typeof preview.summary !== 'string' || !Array.isArray(preview.facts) || !preview.facts.every(fact => typeof fact === 'string')) {
    throw new Error('记忆预览格式异常，请重新读取后再操作。');
  }
  const memory = preview;
  const facts = memory.facts;
  $('memory-summary').textContent = memory.summary || '当前没有对话摘要。';
  $('memory-facts').replaceChildren();
  for (const fact of facts) {
    const item = document.createElement('li');
    item.textContent = fact;
    $('memory-facts').append(item);
  }
  $('memory-facts').hidden = facts.length === 0;
  $('memory-facts-empty').hidden = facts.length > 0;
  $('memory-facts-empty').textContent = '当前没有长期记忆条目。';
  $('memory-truncated').hidden = memory.truncated !== true;
  const revision = Number.isSafeInteger(memory.revision) ? memory.revision : '—';
  const turns = Number.isSafeInteger(memory.turn_count) ? memory.turn_count : '—';
  const summarized = Number.isSafeInteger(memory.summarized_turns) ? memory.summarized_turns : '—';
  const saved = memory.persisted === true ? '已保存到 SD' : (memory.persisted === false ? '尚未确认保存到 SD' : '保存状态未知');
  $('memory-meta').textContent = `版本 ${revision} · ${saved} · 近期 ${turns} 轮 · 已摘要 ${summarized} 轮`;
}

async function readMemoryPreview(expectedToken) {
  const preview = await finishDiagnostic(await execute('memory show'), 'memory show');
  checkMemoryAccess(expectedToken);
  renderMemoryPreview(preview);
}

async function memoryAction(command, title, onSuccess) {
  return action(async () => {
    const expectedToken = token;
    let mutationConfirmed = false;
    $('memory-operation').textContent = title;
    try {
      if (command !== 'memory status' && command !== 'memory show') {
        await finishDiagnostic(await execute(command), command);
        mutationConfirmed = true;
        checkMemoryAccess(expectedToken);
        if (onSuccess) onSuccess();
      }
      await readMemoryStatus(expectedToken);
      if (command !== 'memory status') await readMemoryPreview(expectedToken);
      $('memory-operation').textContent = '已完成';
      // Memory content stays in its own authenticated view, not the tools result pane.
    } catch (error) {
      $('memory-operation').textContent = mutationConfirmed ? '操作已完成，重新读取未完成' : '未完成';
      if (mutationConfirmed) throw new Error(`记忆操作已完成，但重新读取未完成；请先读取记忆，不要重复提交。${error.message}`);
      throw error;
    }
  }, title);
}

async function action(work, title = '正在处理…') {
  if (operating) { notice('请等待当前操作完成。', true); return; }
  if (quickPending.has('bot-stop')) { notice('请等待停止请求完成。', true); return; }
  operating = true;
  updateControls();
  $('operation-state').textContent = title;
  try {
    const value = await work();
    if (value !== undefined) showResult(value);
    $('operation-state').textContent = '已完成';
    notice('操作已完成。');
    await refresh(true);
  } catch (error) {
    $('operation-state').textContent = '未完成';
    showResult(error.message);
    notice(error.message, true);
  } finally {
    operating = false;
    updateControls();
  }
}

function updateControls() {
  document.querySelectorAll('#manager button').forEach(button => {
    const command = button.dataset.command || '';
    if (button.dataset.tab || button.id === 'refresh') button.disabled = false;
    else if (button.id === 'bot-stop') button.disabled = quickPending.has('bot-stop');
    else if (button.id === 'led-apply' || command.startsWith('led ')) button.disabled = quickPending.has('led');
    else button.disabled = operating || quickPending.has('bot-stop');
  });
  $('bot-mode').disabled = operating || quickPending.has('bot-stop');
}

// Fast controls have their own busy flags and never overwrite a running tool's result.
async function quickAction(key, work, message) {
  if (quickPending.has(key)) return;
  quickPending.add(key);
  updateControls();
  try {
    await work();
    notice(message);
    await refresh(true);
  } catch (error) {
    notice(error.message, true);
  } finally {
    quickPending.delete(key);
    updateControls();
  }
}

async function switchMode() {
  const intent = ++modeIntent;
  const mode = $('bot-mode').value;
  await execute('bot stop');
  let status = lastStatus;
  const end = Date.now() + 45000;
  while (Date.now() < end) {
    if (intent !== modeIntent) throw new Error('模式切换已取消。');
    status = await api('/api/status');
    render(status);
    const bot = (status.services || {}).bot;
    if (intent !== modeIntent) throw new Error('模式切换已取消。');
    if (!bot || bot.active_workers === 0) return execute(`bot start ${mode}`);
    await sleep(800);
  }
  throw new Error('旧服务尚未停止，请稍后刷新状态再启用。');
}

function readIds(id) {
  return $(id).value.split(/[\n,]/).map(value => value.trim()).filter(Boolean);
}

$('unlock-form').addEventListener('submit', unlock);
$('logout').addEventListener('click', lock);
$('refresh').addEventListener('click', () => refresh().catch(() => {}));
$('load-config').addEventListener('click', () => action(loadConfig, '正在读取配置…'));
$('bot-start').addEventListener('click', () => action(switchMode, '正在切换机器人模式…'));
$('bot-stop').addEventListener('click', () => {
  ++modeIntent;
  quickAction('bot-stop', () => execute('bot stop'), '已请求停止机器人。');
});
$('led-apply').addEventListener('click', () => quickAction('led', () => {
  const values = ['led-r', 'led-g', 'led-b'].map(id => Number($(id).value));
  if (!values.every(value => Number.isInteger(value) && value >= 0 && value <= 32)) throw new Error('颜色通道必须是 0 到 32 的整数。');
  return execute(`led rgb ${values.join(' ')}`);
}, '彩灯颜色已设置。'));
document.querySelectorAll('[data-command]').forEach(button => {
  button.addEventListener('click', () => {
    const command = button.dataset.command;
    if (command.startsWith('led ')) quickAction('led', () => execute(command), '彩灯设置已更新。');
    else action(async () => finishDiagnostic(await execute(command), command));
  });
});
document.querySelectorAll('[data-tab]').forEach(button => {
  button.addEventListener('click', () => {
    document.querySelectorAll('.tab-page').forEach(page => { page.hidden = page.id !== button.dataset.tab; });
    document.querySelectorAll('[data-tab]').forEach(tab => { tab.classList.toggle('selected', tab === button); });
  });
});
$('config-form').addEventListener('submit', event => {
  event.preventDefault();
  action(async () => {
    if (generation === null) throw new Error('请先读取当前配置。');
    const patch = { wifi_ssid: $('wifi-ssid').value, deepseek_model: $('deepseek-model').value.trim(), qq_app_id: $('qq-app-id').value.trim(), allow_users: readIds('allow-users'), allow_groups: readIds('allow-groups') };
    for (const [field, key] of [['wifi-password', 'wifi_password'], ['deepseek-key', 'deepseek_key'], ['qq-app-secret', 'qq_app_secret']]) {
      const value = $(field).value;
      if (value !== '') patch[key] = value;
    }
    const result = await api('/api/config', 'PUT', { expected_generation: generation, patch });
    for (const field of ['wifi-password', 'deepseek-key', 'qq-app-secret']) $(field).value = '';
    await loadConfig();
    return result;
  }, '正在保存配置…');
});
$('search-form').addEventListener('submit', event => {
  event.preventDefault();
  action(async () => finishDiagnostic(await execute(`deepseek search ${$('search-query').value.trim()}`)), '正在搜索网页…');
});
$('fetch-form').addEventListener('submit', event => {
  event.preventDefault();
  action(async () => finishDiagnostic(await execute(`deepseek fetch ${$('fetch-url').value.trim()}`)), '正在读取网页…');
});
$('memory-refresh').addEventListener('click', () => memoryAction('memory status', '正在读取记忆状态…'));
$('memory-load').addEventListener('click', () => memoryAction('memory show', '正在读取私聊记忆…'));
$('memory-compact').addEventListener('click', () => memoryAction('memory compact', '正在整理对话摘要…'));
$('memory-remember-form').addEventListener('submit', event => {
  event.preventDefault();
  const fact = $('memory-fact').value.trim();
  if (!fact || /[\u0000-\u001f\u007f-\u009f]/.test(fact) || new TextEncoder().encode(fact).byteLength > MAX_MEMORY_FACT_BYTES) {
    notice('请输入单行记忆，长度不超过 512 个 UTF-8 字节。', true);
    return;
  }
  memoryAction(`memory remember ${fact}`, '正在保存长期记忆…', () => {
    if ($('memory-fact').value.trim() === fact) $('memory-fact').value = '';
  });
});
$('memory-forget-form').addEventListener('submit', event => {
  event.preventDefault();
  const index = Number($('memory-fact-index').value);
  if (!Number.isInteger(index) || index < 1 || index > 65535) { notice('请输入有效的记忆条目编号。', true); return; }
  if (operating || quickPending.has('bot-stop')) { notice('请等待当前操作完成。', true); return; }
  if (!window.confirm(`删除第 ${index} 条长期记忆？此操作无法撤销。`)) return;
  memoryAction(`memory forget ${index}`, '正在删除指定记忆…');
});
$('memory-clear').addEventListener('click', () => {
  if (!window.confirm('清空当前私聊的对话摘要和近期对话？长期记忆条目会保留。此操作无法撤销。')) return;
  memoryAction('memory clear', '正在清空对话上下文…');
});
$('memory-forget-all').addEventListener('click', () => {
  if (!window.confirm('忘记当前私聊的全部长期记忆、摘要和近期对话？此操作无法撤销。')) return;
  memoryAction('memory forget all', '正在清除全部记忆…');
});
if (token) unlock();
