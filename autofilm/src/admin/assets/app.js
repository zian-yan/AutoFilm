'use strict';

/**
 * AutoFilm 管理控制台前端。
 *
 * 数据流仍是「读取整份配置 → 本地编辑 → 整份提交」：提交时携带 base_version 做
 * 乐观并发控制，后端校验通过后写盘并实时应用。字段结构全部来自 /api/state
 * 返回的 schema，前端不硬编码字段，避免与 Rust 结构体漂移。
 *
 * 配置分成两份互不影响的文档，两者都是保存即生效、无需重启：
 * - state.config / state.version：业务配置（config.yaml），热重载调度器；
 * - state.web / state.webVersion：面板配置（web.yaml），热切换监听与认证。
 *
 * 保存按钮放在编辑弹窗（列表分区）与单值卡片里：点保存立即落盘生效，不再有
 * 顶部「保存并应用 / 重新加载」。正在运行的任务会被锁定——只有那一行不能编辑
 * 或删除，其它分区与其它任务照常修改；任务停止后锁定自动解除。
 */

const SVG_NS = 'http://www.w3.org/2000/svg';
const THEME_KEY = 'autofilm-theme';
const POLL_INTERVAL = 3000;
/** 日志弹层的刷新频率：500ms 轮询，兼顾准实时观感与本机请求开销。 */
const TASK_LOG_POLL_INTERVAL = 500;
/** 单次日志请求超时：避免请求挂住后在途标记永不释放。 */
const TASK_LOG_TIMEOUT = 4000;
/** 目录浏览请求超时；后端 /api/browse 自身有 10s 上限，这里略放宽。 */
const BROWSE_TIMEOUT = 12000;

const state = {
  app: null,
  schema: null,
  config: null,
  version: 0,
  web: null,
  webVersion: 0,
  runtime: {},
  // 由 applyState 依据地址栏 hash 决定，空 hash 落到首个配置分区。
  active: '',
  dirty: false,
  saving: false,
  /** 顶部保存状态指示：saved / dirty / saving / error / invalid。 */
  saveState: 'saved',
  savedBusiness: '',
  savedWeb: '',
  theme: 'dark',
  /** 打开弹窗时条目的快照：关闭弹窗即丢弃未保存的改动。 */
  editorSnapshot: null,
  /** 当前打开的配置编辑弹窗键（最多一个），格式为 `分区#下标`。 */
  expanded: new Set(),
  /** 刚点过「立即运行」、等后端确认的任务键，用于即时反馈。 */
  pending: new Set(),
  pollTimer: null,
  pollKick: null,
};

/* ---------------- 工具函数 ---------------- */

const byId = (id) => document.getElementById(id);

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = text;
  return node;
}

function deepClone(value) {
  return JSON.parse(JSON.stringify(value));
}

function emptyToNull(value) {
  const text = String(value == null ? '' : value).trim();
  return text === '' ? null : text;
}

function getPath(target, path) {
  return path.split('.').reduce((cursor, key) => (cursor == null ? undefined : cursor[key]), target);
}

function setPath(target, path, value) {
  const keys = path.split('.');
  let cursor = target;
  for (let i = 0; i < keys.length - 1; i += 1) {
    const key = keys[i];
    if (cursor[key] == null || typeof cursor[key] !== 'object') cursor[key] = {};
    cursor = cursor[key];
  }
  cursor[keys[keys.length - 1]] = value;
}

function fieldPath(section, index, key) {
  return index >= 0 ? `${section.key}[${index}].${key}` : `${section.key}.${key}`;
}

/* ---------------- 图标（自绘线性 SVG，不使用 Jellyfin 图标） ---------------- */

const ICON_PATHS = {
  play: ['M8.5 5.6v12.8L19 12z'],
  pencil: ['M4 20h4l10.4-10.4-4-4L4 16v4z', 'M14.4 5.6l4 4'],
  trash: ['M4 7h16', 'M9 7V4.6h6V7', 'M6.6 7l1 12.4h8.8l1-12.4', 'M10 11v5.4', 'M14 11v5.4'],
  plus: ['M12 5v14', 'M5 12h14'],
  bolt: ['M13.2 3 5.5 14h5l-1 7 8-11h-5l.7-7z'],
  refresh: ['M20 12a8 8 0 1 1-2.5-5.8', 'M19.6 4.2v5h-5'],
  save: ['M5 4.6h11l3.4 3.4v11.4H5z', 'M8.6 4.6v5h6v-5', 'M8.6 19.4v-5.2h6.8v5.2'],
  sun: [
    'M12 8.6a3.4 3.4 0 1 0 0 6.8 3.4 3.4 0 0 0 0-6.8z',
    'M12 3v2',
    'M12 19v2',
    'M3 12h2',
    'M19 12h2',
    'M5.6 5.6 7 7',
    'M17 17l1.4 1.4',
    'M18.4 5.6 17 7',
    'M7 17l-1.4 1.4',
  ],
  moon: ['M20 14.2A8.2 8.2 0 0 1 9.8 4a8.2 8.2 0 1 0 10.2 10.2z'],
  check: ['M5 12.5l4.5 4.5L19 7'],
  'chevron-up': ['M6 14.5l6-6 6 6'],
  chevron: ['M9 6l6 6-6 6'],
  folder: ['M3.5 6.5h6.2l2 2.5h8.8v10h-17z'],
  'file-text': ['M7 3.5h7l4 4v13H7z', 'M14 3.5v4h4', 'M10 12h6', 'M10 15.5h6'],
  close: ['M6 6l12 12', 'M18 6 6 18'],
  logout: ['M14 4.5H5.5v15H14', 'M11.5 12h9', 'M17.5 8.5 21 12l-3.5 3.5'],
};

function icon(name, extraClass) {
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('class', extraClass ? `icon ${extraClass}` : 'icon');
  svg.setAttribute('aria-hidden', 'true');
  for (const d of ICON_PATHS[name] || []) {
    const path = document.createElementNS(SVG_NS, 'path');
    path.setAttribute('d', d);
    svg.appendChild(path);
  }
  return svg;
}

function button(className, label, iconName, onClick) {
  const node = element('button', className);
  node.type = 'button';
  if (iconName) node.appendChild(icon(iconName, 'icon-sm'));
  if (label) node.appendChild(document.createTextNode(label));
  if (onClick) node.addEventListener('click', onClick);
  return node;
}

function iconButton(className, iconName, title, onClick) {
  const node = button(`icon-btn ${className}`, null, iconName, onClick);
  node.title = title;
  node.setAttribute('aria-label', title);
  return node;
}

/* ---------------- 主题 ---------------- */

function applyTheme(theme) {
  state.theme = theme === 'light' ? 'light' : 'dark';
  document.documentElement.dataset.theme = state.theme;
  const toggle = byId('btn-theme');
  if (toggle) {
    toggle.textContent = '';
    toggle.appendChild(icon(state.theme === 'dark' ? 'sun' : 'moon'));
    toggle.title = state.theme === 'dark' ? '切换到浅色主题' : '切换到深色主题';
    toggle.setAttribute('aria-label', toggle.title);
  }
  try {
    window.localStorage.setItem(THEME_KEY, state.theme);
  } catch (err) {
    /* 隐私模式下不可写，忽略即可 */
  }
}

function loadTheme() {
  // 允许用 ?theme=light / ?theme=dark 强制主题（便于嵌入或投屏），否则依次回落到
  // 本地保存的选择与系统偏好。
  let forced = null;
  try {
    forced = new URLSearchParams(window.location.search).get('theme');
  } catch (err) {
    forced = null;
  }
  if (forced === 'light' || forced === 'dark') {
    applyTheme(forced);
    return;
  }

  let saved = null;
  try {
    saved = window.localStorage.getItem(THEME_KEY);
  } catch (err) {
    saved = null;
  }
  if (saved) {
    applyTheme(saved);
    return;
  }
  // 没有手动选过时跟随系统：默认深色是 Jellyfin 的经典观感，浅色系统则跟随浅色。
  const light = window.matchMedia && window.matchMedia('(prefers-color-scheme: light)').matches;
  applyTheme(light ? 'light' : 'dark');
}

function toggleTheme() {
  applyTheme(state.theme === 'dark' ? 'light' : 'dark');
}

/* ---------------- 登录 / 退出 ---------------- */

async function logout() {
  stopPolling();
  try {
    await fetch('/api/logout', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'X-AutoFilm-Request': '1' },
      body: '{}',
    });
  } catch (err) {
    /* 请求失败也照常跳转：会话状态以后端为准，登录页会再拦一次 */
  }
  window.location.replace('/login');
}

/* ---------------- 分区与数据源 ---------------- */

// 业务分区 + 独立的面板分区。面板分区标记了 detached，数据来自 state.web。
function allSections() {
  const sections = Array.isArray(state.schema.sections) ? state.schema.sections.slice() : [];
  if (state.schema.web_section) sections.push(state.schema.web_section);
  return sections;
}

function businessSections() {
  return allSections().filter((section) => !section.detached);
}

// 导航只展示顶层分区；带 parent 的分区并入宿主页面下方一起渲染。
function topLevelSections() {
  return allSections().filter((section) => !section.parent);
}

// 单页要渲染的分区：宿主在前，归属它的子分区按 schema 顺序跟在后面。
function pageSections(section) {
  if (!section) return [];
  return [section, ...allSections().filter((item) => item.parent === section.key)];
}

function sectionData(section) {
  if (!section) return null;
  if (section.detached) {
    if (!state.web || typeof state.web !== 'object') state.web = {};
    return state.web;
  }
  return state.config ? state.config[section.key] : null;
}

/* ---------------- 规范化 ---------------- */

function normalizeBusiness(config) {
  const next = config || {};
  for (const section of businessSections()) {
    const value = next[section.key];
    if (section.single) {
      next[section.key] = value && typeof value === 'object' ? value : {};
    } else {
      next[section.key] = Array.isArray(value) ? value : [];
    }
  }

  // ani2alist 的 update 是 serde 的 internally-tagged 枚举，
  // 编辑期统一展开成 { mode, template, keyword } 三个字段。
  for (const task of next.ani2alist_tasks || []) {
    const update = task.update || {};
    task.update = {
      mode: update.mode || 'rss',
      template: update.template == null ? null : update.template,
      keyword: update.keyword == null ? null : update.keyword,
    };
  }
  return next;
}

function normalizeWeb(web) {
  const next = web && typeof web === 'object' ? web : {};
  // 面板配置只有一份，字段缺失时补成对应类型的空值，避免输入框渲染成 undefined。
  const section = state.schema && state.schema.web_section;
  for (const field of (section && section.fields) || []) {
    if (next[field.key] != null) continue;
    next[field.key] = field.kind === 'bool' ? false : field.kind === 'number' ? 0 : '';
  }
  if (next.password == null) next.password = '';
  return next;
}

function serializeOut(config) {
  const out = deepClone(config);
  for (const task of out.ani2alist_tasks || []) {
    const draft = task.update || {};
    const mode = draft.mode || 'rss';
    if (mode === 'latest') {
      task.update = { mode, template: emptyToNull(draft.template) };
    } else if (mode === 'keyword') {
      task.update = { mode, keyword: String(draft.keyword == null ? '' : draft.keyword).trim() };
    } else {
      task.update = { mode: 'rss' };
    }
  }
  return out;
}

function businessPayload() {
  return serializeOut(state.config);
}

function webPayload() {
  const out = deepClone(state.web || {});
  if (out.password == null) out.password = '';
  return out;
}

/* ---------------- 提示 ---------------- */

function toast(kind, message) {
  const node = element('div', `toast toast-${kind}`, message);
  byId('toasts').appendChild(node);
  window.setTimeout(() => {
    node.remove();
  }, kind === 'error' ? 8000 : 5000);
}

function showAlert(kind, message, details) {
  const alert = byId('alert');
  alert.className = `alert alert-${kind}`;
  alert.textContent = message;
  if (details && details.length) {
    const list = element('ul');
    for (const detail of details) list.appendChild(element('li', null, detail));
    alert.appendChild(list);
  }
}

function clearAlert() {
  const alert = byId('alert');
  alert.className = 'alert is-hidden';
  alert.textContent = '';
}

/* ---------------- 接口 ---------------- */

/**
 * 会话失效（401）时停止轮询并跳到登录页。
 *
 * 返回 true 表示已经处理，调用方不必再处理响应体。
 */
function redirectIfUnauthorized(response) {
  if (response.status !== 401) return false;
  stopPolling();
  window.location.replace('/login');
  return true;
}

async function requestState() {
  const response = await fetch('/api/state', {
    headers: { Accept: 'application/json' },
    cache: 'no-store',
  });
  if (redirectIfUnauthorized(response)) throw new Error('登录状态已失效，正在跳转到登录页…');
  if (!response.ok) throw new Error(`加载面板数据失败（HTTP ${response.status}）`);
  return response.json();
}

async function requestSave(payload) {
  const response = await fetch('/api/config', {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json', 'X-AutoFilm-Request': '1' },
    body: JSON.stringify(payload),
  });
  if (redirectIfUnauthorized(response)) return { response, data: {} };
  const data = await response.json().catch(() => ({}));
  return { response, data };
}

async function requestSaveWeb(payload) {
  const response = await fetch('/api/web', {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json', 'X-AutoFilm-Request': '1' },
    body: JSON.stringify(payload),
  });
  if (redirectIfUnauthorized(response)) return { response, data: {} };
  const data = await response.json().catch(() => ({}));
  return { response, data };
}

/** MediaWarp：预览生成文件（只读）。 */
async function requestMediaWarpPreview() {
  const response = await fetch('/api/mediawarp/preview', {
    headers: { Accept: 'application/json' },
    cache: 'no-store',
  });
  if (redirectIfUnauthorized(response)) return { response, data: {} };
  const data = await response.json().catch(() => ({}));
  return { response, data };
}

async function requestMediaWarpAction(action) {
  const response = await fetch(`/api/mediawarp/${action}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-AutoFilm-Request': '1' },
    body: '{}',
    cache: 'no-store',
  });
  if (redirectIfUnauthorized(response)) return { response, data: {} };
  const data = await response.json().catch(() => ({}));
  return { response, data };
}

async function requestRun(module, id) {
  const response = await fetch('/api/run', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-AutoFilm-Request': '1' },
    body: JSON.stringify({ module, id }),
  });
  if (redirectIfUnauthorized(response)) return { response, data: {} };
  const data = await response.json().catch(() => ({}));
  return { response, data };
}

async function requestRunAll(module) {
  const response = await fetch('/api/run-all', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-AutoFilm-Request': '1' },
    body: JSON.stringify({ module }),
  });
  if (redirectIfUnauthorized(response)) return { response, data: {} };
  const data = await response.json().catch(() => ({}));
  return { response, data };
}

/** 只读取一个任务的日志；后端按「模块:任务 ID」隔离，按时间顺序返回。 */
async function requestTaskLog(module, id, externalSignal) {
  const controller = new AbortController();
  const timer = window.setTimeout(() => controller.abort(), TASK_LOG_TIMEOUT);
  // 弹层关闭时外部 signal 会 abort，让在途请求立刻结束，而不是空等到超时。
  const onExternalAbort = () => controller.abort();
  if (externalSignal) {
    if (externalSignal.aborted) controller.abort();
    else externalSignal.addEventListener('abort', onExternalAbort, { once: true });
  }
  try {
    const response = await fetch('/api/task-log', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'X-AutoFilm-Request': '1' },
      body: JSON.stringify({ module, id }),
      cache: 'no-store',
      signal: controller.signal,
    });
    if (redirectIfUnauthorized(response)) return { response, data: {} };
    const data = await response.json().catch(() => ({}));
    return { response, data };
  } finally {
    window.clearTimeout(timer);
    if (externalSignal) externalSignal.removeEventListener('abort', onExternalAbort);
  }
}

/* ---------------- 状态装载 ---------------- */

function applyState(data) {
  state.app = data.app || {};
  state.schema = data.schema;
  state.version = data.version || 0;
  state.webVersion = data.web_version || 0;
  state.runtime = data.runtime || {};
  state.config = normalizeBusiness(data.config);
  state.web = normalizeWeb(data.web);
  state.savedBusiness = JSON.stringify(businessPayload());
  state.savedWeb = JSON.stringify(webPayload());
  state.dirty = false;
  state.active = activeFromHash();
  clearAlert();
  byId('app-version').textContent = state.app.version ? `v${state.app.version}` : '-';
  // 未启用面板密码时没有登录态，退出按钮也就没有意义。
  byId('btn-logout').classList.toggle('is-hidden', !state.app.auth_enabled);
  renderNav();
  render();
  setSaveStatus('saved');
  // 轮询常驻：任务开始/结束会实时改变配置锁定状态，不能只在运行时才轮询。
  startPolling();
}

async function refresh() {
  try {
    applyState(await requestState());
  } catch (err) {
    showAlert('error', err.message);
  }
}

/* ---------------- 渲染：导航与框架 ---------------- */

function renderNav() {
  const nav = byId('nav');
  nav.textContent = '';
  let grouped = false;
  for (const section of topLevelSections()) {
    // detached 分区是面板自身配置（web.yaml），在导航里单独分组，
    // 与「同步全局设置」等业务分区一眼可分；两者仍是各自独立的页面，
    // 不做任何合并，点击行为保持不变。
    if (section.detached && !grouped) {
      const group = element('div', 'nav-group');
      group.appendChild(element('span', 'nav-group-label', '面板自身'));
      nav.appendChild(group);
      grouped = true;
    }
    nav.appendChild(navButton(section.key, section.title));
  }
}

function navButton(key, label) {
  const node = element('button', `nav-item${state.active === key ? ' is-active' : ''}`, label);
  node.type = 'button';
  node.addEventListener('click', () => setActive(key));
  return node;
}

/** 无独立首页：hash 为空或非法时一律落到首个顶层配置分区。 */
function defaultSectionKey() {
  const sections = topLevelSections();
  return sections.length ? sections[0].key : '';
}

/** 切换分区：同步到地址栏 hash，刷新或分享链接后仍停留在同一分区。 */
function setActive(key) {
  state.active = key;
  closeEditor(false);
  const hash = key === defaultSectionKey() ? '' : `#${encodeURIComponent(key)}`;
  if (window.location.hash !== hash) {
    window.history.replaceState(null, '', hash || window.location.pathname);
  }
  clearAlert();
  renderNav();
  render();
  window.scrollTo({ top: 0, behavior: 'smooth' });
}

/** 从地址栏 hash 还原当前分区；已并入的子分区回落到宿主页，非法值回到首个分区。 */
function activeFromHash() {
  const key = decodeURIComponent((window.location.hash || '').replace(/^#/, '')).trim();
  if (!key) return defaultSectionKey();
  const section = allSections().find((item) => item.key === key);
  if (!section) return defaultSectionKey();
  return section.parent || key;
}

function currentSection() {
  const sections = topLevelSections();
  return sections.find((section) => section.key === state.active) || sections[0] || null;
}

function updateChrome() {
  const section = currentSection();
  byId('section-title').textContent = section ? section.title : '';
  byId('section-subtitle').textContent = section ? section.subtitle || '' : '';
}

function render() {
  if (!state.schema) return;
  const content = byId('content');
  content.textContent = '';

  // 宿主页依次渲染自身卡片与下方子分区卡片；保存动作在各卡片 / 弹窗内完成。
  pageSections(currentSection()).forEach((section, index) => {
    renderSection(content, section, index > 0 ? section.title : null);
  });
  updateChrome();
  renderEditorModal();
  syncLocks();
}

function renderSection(container, section, heading) {
  if (section.widget === 'mediawarp') {
    renderMediaWarpSection(container, section, heading);
  } else if (section.single) {
    renderSingleSection(container, section, heading);
  } else {
    renderListSection(container, section, heading);
  }
}

/* ---------------- 渲染：卡片 ---------------- */

function cardHead(title, subtitle, actions) {
  const head = element('header', 'card-head');
  const titles = element('div');
  titles.appendChild(element('h2', null, title));
  if (subtitle) titles.appendChild(element('p', 'card-sub', subtitle));
  head.appendChild(titles);
  if (actions) head.appendChild(actions);
  return head;
}

/* ---------------- 渲染：单一设置分区 ---------------- */

function renderSingleSection(container, section, heading) {
  const wrapper = element('section', 'card');
  const actions = element('div', 'card-actions');
  if (section.realtime) actions.appendChild(element('span', 'pill pill-live', '实时生效'));
  actions.appendChild(button('btn btn-primary btn-small', '保存', 'save', () => save()));
  wrapper.appendChild(cardHead(heading || section.item_label, '', actions));

  const body = element('div', 'card-body padded');
  const grid = element('div', 'editor-grid');
  for (const field of section.fields || []) {
    const node = renderField(section, sectionData(section), -1, field);
    if (node) grid.appendChild(node);
  }
  body.appendChild(grid);
  wrapper.appendChild(body);
  container.appendChild(wrapper);
}

/* ---------------- MediaWarp 专属页面 ---------------- */

const MEDIAWARP_PHASES = {
  disabled: { label: '未托管', kind: 'neutral' },
  stopped: { label: '已停止', kind: 'warn' },
  starting: { label: '重启中…', kind: 'warn' },
  running: { label: '运行中', kind: 'ok' },
  failed: { label: '异常', kind: 'danger' },
};
const MEDIAWARP_MANAGEMENT_KEYS = [
  'enable',
  'config_path',
  'binary',
  'extra_args',
  'startup_timeout_secs',
  'stop_timeout_secs',
];
let mediawarpModal = null;

/** 运行状态卡片：只就地刷新内容，不整页重绘，避免打断正在输入的表单。 */
function syncMediaWarpStatus() {
  const host = byId('mediawarp-status');
  if (!host) return;
  const info = (state.runtime && state.runtime.mediawarp) || {};
  const phase = MEDIAWARP_PHASES[info.phase] || MEDIAWARP_PHASES.disabled;

  host.textContent = '';
  const head = element('div', 'mw-status-head');
  head.appendChild(
    element(
      'span',
      phase.kind === 'neutral' ? 'pill' : `pill pill-mw-${phase.kind}`,
      phase.label,
    ),
  );
  head.appendChild(
    element(
      'span',
      'mw-status-caption',
      info.enabled ? `监听端口 ${info.port || '—'}` : '托管未启用：保存只写配置文件，不会自动重启',
    ),
  );
  host.appendChild(head);

  const rows = [
    ['进程 PID', info.pid == null ? '—' : String(info.pid)],
    ['可执行文件', info.binary || '—'],
    ['端口健康检查', info.healthy ? '通过' : '未通过'],
    ['累计重启', String(info.restart_count || 0)],
    ['上次重启', info.last_restart_at || '—'],
    ['生成文件', info.config_path || '—'],
    ['文件与面板一致', info.file_in_sync ? '是' : '否'],
  ];
  const grid = element('div', 'mw-status-grid');
  for (const [label, value] of rows) {
    const cell = element('div', 'mw-status-item');
    cell.appendChild(element('span', 'mw-status-label', label));
    const valueNode = element('span', 'mw-status-value', value);
    if (label === '生成文件' || label === '可执行文件') valueNode.classList.add('mono');
    cell.appendChild(valueNode);
    grid.appendChild(cell);
  }
  host.appendChild(grid);

  if (info.last_error) {
    host.appendChild(element('p', 'mw-status-error', info.last_error));
  }

  const restart = byId('mw-restart');
  if (restart) {
    restart.disabled = !info.enabled || info.phase === 'starting';
    restart.title = info.enabled ? '按当前已生效的配置强制重启代理服务' : '请先启用托管并保存';
  }
}

function renderMediaWarpSection(container, section, heading) {
  const statusCard = element('section', 'card');
  statusCard.appendChild(
    cardHead('服务状态', '每 3 秒自动刷新；配置保存后自动写入文件、重启服务并做端口健康检查', null),
  );
  const statusBody = element('div', 'card-body padded');
  const statusHost = element('div', 'mw-status');
  statusHost.id = 'mediawarp-status';
  statusBody.appendChild(statusHost);
  statusCard.appendChild(statusBody);
  container.appendChild(statusCard);

  const wrapper = element('section', 'card');
  const actions = element('div', 'card-actions');
  const restart = button('btn btn-ghost btn-small', '立即重启', 'refresh', () =>
    restartMediaWarp(),
  );
  restart.id = 'mw-restart';
  actions.appendChild(restart);
  actions.appendChild(
    button('btn btn-ghost btn-small', '预览配置', 'file-text', () => previewMediaWarp()),
  );
  actions.appendChild(
    button('btn btn-ghost btn-small', '从文件导入', 'folder', () => importMediaWarp()),
  );
  actions.appendChild(
    button('btn btn-primary btn-small', '保存并重启', 'save', () => save()),
  );
  wrapper.appendChild(cardHead(heading || section.item_label, '', actions));

  const body = element('div', 'card-body padded');
  const data = sectionData(section) || {};
  for (const group of section.groups || []) {
    const block = element('div', 'mw-group');
    block.appendChild(element('h3', 'mw-group-title', group.title));
    if (group.subtitle) block.appendChild(element('p', 'mw-group-sub', group.subtitle));
    const grid = element('div', 'editor-grid');
    for (const field of group.fields || []) {
      const node = renderField(section, data, -1, field);
      if (node) grid.appendChild(node);
    }
    block.appendChild(grid);
    body.appendChild(block);
  }
  wrapper.appendChild(body);
  container.appendChild(wrapper);
  syncMediaWarpStatus();
}

async function restartMediaWarp() {
  if (state.saving) return;
  const restart = byId('mw-restart');
  if (restart) restart.disabled = true;
  try {
    const { response, data } = await requestMediaWarpAction('restart');
    if (data.status) state.runtime.mediawarp = data.status;
    if (response.ok) {
      toast('ok', data.message || 'MediaWarp 已重启');
      clearAlert();
    } else {
      const message = data.message || `重启失败（HTTP ${response.status}）`;
      showAlert('error', message);
      toast('error', message);
    }
  } catch (err) {
    toast('error', `重启失败：${err.message}`);
  }
  syncMediaWarpStatus();
}

async function previewMediaWarp() {
  try {
    const { response, data } = await requestMediaWarpPreview();
    if (!response.ok) {
      toast('error', data.message || `读取失败（HTTP ${response.status}）`);
      return;
    }
    openMediaWarpModal('生成的 MediaWarp 配置', data.path || '', data.yaml || '');
  } catch (err) {
    toast('error', `读取失败：${err.message}`);
  }
}

function openMediaWarpModal(title, path, yaml) {
  closeMediaWarpModal();
  const mask = element('div', 'modal-mask');
  const modal = element('div', 'modal modal-editor');

  const head = element('div', 'modal-head');
  head.appendChild(element('span', 'modal-title', title));
  head.appendChild(iconButton('', 'close', '关闭', () => closeMediaWarpModal()));
  modal.appendChild(head);

  const body = element('div', 'modal-body');
  body.appendChild(element('p', 'mw-modal-path mono', path));
  body.appendChild(element('pre', 'mw-code mono', yaml));
  modal.appendChild(body);
  mask.appendChild(modal);
  document.body.appendChild(mask);

  const onKeydown = (event) => {
    if (event.key === 'Escape') closeMediaWarpModal();
  };
  window.addEventListener('keydown', onKeydown);
  mask.addEventListener('click', (event) => {
    if (event.target === mask) closeMediaWarpModal();
  });
  mediawarpModal = { mask, onKeydown };
}

function closeMediaWarpModal() {
  if (!mediawarpModal) return;
  window.removeEventListener('keydown', mediawarpModal.onKeydown);
  mediawarpModal.mask.remove();
  mediawarpModal = null;
}

async function importMediaWarp() {
  if (
    state.dirty &&
    !window.confirm('当前有未保存的改动，导入会覆盖 MediaWarp 表单（其它分区不受影响）。继续？')
  ) {
    return;
  }
  try {
    const { response, data } = await requestMediaWarpAction('import');
    if (!response.ok) {
      const message = data.message || `导入失败（HTTP ${response.status}）`;
      showAlert('error', message);
      toast('error', message);
      return;
    }
    if (!state.config) return;
    const target = state.config.mediawarp || (state.config.mediawarp = {});
    // 托管参数属于 AutoFilm，不能被外部文件覆盖；只导入 MediaWarp 原生字段。
    for (const [key, value] of Object.entries(data.config || {})) {
      if (!MEDIAWARP_MANAGEMENT_KEYS.includes(key)) target[key] = value;
    }
    markDirty();
    render();
    toast('ok', `已从 ${data.path || '生成文件'} 导入，请确认后点击「保存并重启」`);
  } catch (err) {
    toast('error', `导入失败：${err.message}`);
  }
}

/* ---------------- 渲染：列表分区（表格 + 展开编辑） ---------------- */

function columnsOf(section) {
  const columns = ((section.table && section.table.columns) || []).map((column) => ({
    key: column.key,
    label: column.label,
    kind: column.kind || 'text',
    map: column.map || null,
  }));
  if (section.next_run) columns.push({ key: '__next_run', label: '下次运行', kind: 'next_run' });
  return columns;
}

let editorModal = null;

function isExpanded(section, index) {
  return state.expanded.has(`${section.key}#${index}`);
}

/** 丢弃弹窗内未保存的改动，恢复打开时的条目快照。 */
function discardEditorDraft() {
  const snapshot = state.editorSnapshot;
  if (!snapshot) return;
  const items = state.config[snapshot.sectionKey];
  if (!items) return;
  if (snapshot.created) {
    items.splice(snapshot.index, 1);
  } else if (items[snapshot.index]) {
    items[snapshot.index] = snapshot.item;
  }
  syncDirty();
}

/** 关闭配置编辑弹窗。默认视为「取消」，未点保存的改动一律丢弃。 */
function closeEditor(sync = true, keepDraft = false) {
  if (!editorModal && state.expanded.size === 0 && !state.editorSnapshot) return;
  closeAlistBrowser();
  if (!keepDraft) discardEditorDraft();
  state.editorSnapshot = null;
  if (editorModal) {
    window.removeEventListener('keydown', editorModal.onKeydown);
    editorModal.mask.remove();
    editorModal = null;
  }
  state.expanded.clear();
  if (sync) render();
}

/** 把一行的配置编辑放进弹窗；created 表示这是刚新增、尚未保存的条目。 */
function openEditor(section, index, created = false) {
  const items = state.config[section.key] || [];
  if (!items[index]) return;
  closeTaskLog();
  closeEditor(false);
  state.editorSnapshot = {
    sectionKey: section.key,
    index,
    item: deepClone(items[index]),
    created,
  };
  state.expanded.add(`${section.key}#${index}`);

  const mask = element('div', 'modal-mask');
  const modal = element('div', 'modal modal-editor');

  const head = element('div', 'modal-head');
  head.appendChild(element('span', 'modal-title', '配置编辑'));
  head.appendChild(iconButton('', 'close', '关闭', () => closeEditor()));
  modal.appendChild(head);

  const body = element('div', 'modal-body editor-modal-body');
  const foot = element('div', 'modal-foot');
  modal.appendChild(body);
  modal.appendChild(foot);
  mask.appendChild(modal);
  document.body.appendChild(mask);

  const onKeydown = (event) => {
    if (event.key === 'Escape') closeEditor();
  };
  const onMaskClick = (event) => {
    if (event.target === mask) closeEditor();
  };
  window.addEventListener('keydown', onKeydown);
  mask.addEventListener('click', onMaskClick);

  editorModal = { mask, head, body, foot, onKeydown };
  render();
}

/** 每次重绘都按当前数据重建弹窗内容，字段联动与校验定位继续生效。 */
function renderEditorModal() {
  if (!editorModal) return;
  const key = [...state.expanded][0];
  const [sectionKey, indexText] = (key || '').split('#');
  const index = Number(indexText);
  const section = allSections().find((item) => item.key === sectionKey);
  const items = section ? state.config[sectionKey] || [] : [];
  const item = items[index];
  if (!section || !item) {
    closeEditor(false);
    return;
  }

  editorModal.head.querySelector('.modal-title').textContent =
    `编辑：${item.id ? item.id : `未命名${section.item_label}`}`;
  if (section.module) editorModal.mask.dataset.lockKey = `${section.module}:${item.id || ''}`;

  editorModal.body.textContent = '';
  if (section.module) {
    editorModal.body.appendChild(
      element(
        'p',
        'lock-note lock-banner is-hidden',
        '该任务正在运行，配置已锁定；请等待运行结束后再修改。',
      ),
    );
  }
  const grid = element('div', 'editor-grid');
  for (const field of section.fields || []) {
    const node = renderField(section, item, index, field);
    if (node) grid.appendChild(node);
  }
  editorModal.body.appendChild(grid);

  editorModal.foot.textContent = '';
  if (section.removable) {
    const remove = button('btn btn-danger btn-small', '删除', 'trash', () =>
      removeItem(section, index, item),
    );
    remove.classList.add('lock-sensitive');
    editorModal.foot.appendChild(remove);
  }
  if (section.module) {
    editorModal.foot.appendChild(
      button('btn btn-ghost btn-small', '立即运行', 'play', () => runTask(section, item)),
    );
  }
  editorModal.foot.appendChild(
    button('btn btn-ghost btn-small', '取消', 'close', () => closeEditor()),
  );
  const saveButton = button('btn btn-primary btn-small', '保存', 'save', () => saveEditor());
  saveButton.classList.add('lock-sensitive');
  editorModal.foot.appendChild(saveButton);
}

function toggleEditor(section, index) {
  if (isExpanded(section, index)) closeEditor();
  else openEditor(section, index);
}

function renderListSection(container, section, heading) {
  const items = Array.isArray(state.config[section.key]) ? state.config[section.key] : [];
  const columns = columnsOf(section);

  const wrapper = element('section', 'card');

  const actions = element('div', 'card-actions');
  if (section.module) {
    // 「运行所有」是分区级次要动作，用次级按钮；主按钮留给「添加」，一屏只有
    // 一个实心蓝，避免两个同权重色块互相稀释。
    actions.appendChild(button('btn btn-ghost', '运行所有', 'bolt', () => runAll(section)));
  }
  if (section.addable) {
    actions.appendChild(
      button('btn btn-primary', `添加${section.item_label}`, 'plus', () => addItem(section)),
    );
  }
  wrapper.appendChild(cardHead(heading || `${items.length} 个${section.item_label}`, '', actions));

  const body = element('div', 'card-body');
  if (!items.length) {
    body.appendChild(
      element(
        'p',
        'empty',
        `还没有${section.item_label}，点击右上角「添加${section.item_label}」开始配置`,
      ),
    );
    wrapper.appendChild(body);
    container.appendChild(wrapper);
    return;
  }

  const scroller = element('div', 'table-wrap');
  const table = element('table', 'data-table');

  const thead = element('thead');
  const headRow = element('tr');
  for (const column of columns) headRow.appendChild(element('th', null, column.label));
  const actionHead = element('th', 'col-actions', '操作');
  headRow.appendChild(actionHead);
  thead.appendChild(headRow);
  table.appendChild(thead);

  const tbody = element('tbody');
  items.forEach((item, index) => {
    tbody.appendChild(renderRow(section, item, index, columns));
  });
  table.appendChild(tbody);
  scroller.appendChild(table);
  body.appendChild(scroller);
  wrapper.appendChild(body);
  container.appendChild(wrapper);
}

function renderRow(section, item, index, columns) {
  const row = element('tr', `row${isExpanded(section, index) ? ' is-editing' : ''}`);
  row.dataset.rowKey = `${section.key}#${index}`;
  const key = section.module ? `${section.module}:${item.id || ''}` : '';
  if (section.module) row.dataset.lockKey = key;

  columns.forEach((column, columnIndex) => {
    const cell = element('td');
    if (columnIndex === 0) cell.classList.add('cell-name');
    cell.appendChild(cellContent(section, item, column));
    if (columnIndex === 0 && section.module) {
      cell.appendChild(element('span', 'lock-note is-hidden', '运行中 · 配置已锁定'));
    }
    row.appendChild(cell);
  });

  const actionCell = element('td', 'col-actions');
  const group = element('div', 'cell-actions');

  if (section.module) {
    const running = isRunning(key);
    const play = iconButton(
      'is-play',
      running ? 'refresh' : 'play',
      running ? '正在运行…' : '立即运行',
      () => runTask(section, item),
    );
    play.dataset.runKey = key;
    play.dataset.runTitle = '立即运行（按当前生效的配置执行）';
    play.disabled = running || !item.id;
    if (running) play.classList.add('is-running');
    if (!item.id) play.title = '请先填写任务 ID';
    group.appendChild(play);
    group.appendChild(
      iconButton('is-log', 'file-text', '查看运行日志', () => openTaskLog(section, item)),
    );
  }

  const edit = iconButton(
    'is-edit',
    'pencil',
    isExpanded(section, index) ? '关闭编辑' : '编辑配置',
    () => toggleEditor(section, index),
  );
  edit.classList.add('lock-sensitive');
  group.appendChild(edit);

  if (section.removable) {
    const remove = iconButton('is-delete', 'trash', '删除', () => removeItem(section, index, item));
    remove.classList.add('lock-sensitive');
    group.appendChild(remove);
  }

  actionCell.appendChild(group);
  row.appendChild(actionCell);
  return row;
}

function cellContent(section, item, column) {
  if (column.kind === 'next_run') return nextRunCell(section, item);

  const raw = getPath(item, column.key);
  switch (column.kind) {
    case 'ref': {
      const text = raw == null ? '' : String(raw);
      if (!text) return element('span', 'cell-dim', '—');
      return element('span', 'chip-ref', text);
    }
    case 'cron': {
      const text = raw == null ? '' : String(raw).trim();
      if (!text) return element('span', 'cell-dim', '未设置');
      return element('code', 'mono', text);
    }
    case 'bool': {
      if (raw) return element('span', 'pill pill-live', '是');
      return element('span', 'cell-dim', '否');
    }
    case 'count': {
      const size = Array.isArray(raw) ? raw.length : 0;
      return element('span', size ? null : 'cell-dim', `${size} 项`);
    }
    case 'map': {
      const text = raw == null ? '' : String(raw);
      if (!text) return element('span', 'cell-dim', '—');
      return element('span', null, (column.map && column.map[text]) || text);
    }
    default: {
      const text = raw == null ? '' : String(raw);
      if (!text) return element('span', 'cell-dim', '—');
      return element('span', null, text);
    }
  }
}

function nextRunCell(section, item) {
  if (!section.module || !item.id) return element('span', 'cell-dim', '—');
  if (!item.cron || !String(item.cron).trim()) return element('span', 'cell-dim', '未设置');

  const cell = element('span', 'mono');
  cell.dataset.nextRun = `${section.module}:${item.id}`;
  const text = nextRunText(section.module, item.id);
  cell.textContent = text || '—';
  if (!text) cell.classList.add('cell-dim');
  return cell;
}

function nextRunText(module, id) {
  const map = (state.runtime && state.runtime.next_runs) || {};
  return map[`${module}:${id}`] || '';
}

function addItem(section) {
  const items = Array.isArray(state.config[section.key]) ? state.config[section.key] : [];
  state.config[section.key] = items;
  items.push(deepClone(section.template || {}));
  // 新条目直接打开编辑弹窗，填完点「保存」才落盘；取消则整条移除。
  openEditor(section, items.length - 1, true);
  markDirty();
}

function removeItem(section, index, item) {
  if (isLocked(section, item)) {
    toast('warn', '该任务正在运行，配置已锁定，请等任务结束后再删除');
    return;
  }
  const label = item && item.id ? item.id : `第 ${index + 1} 个${section.item_label}`;
  if (!window.confirm(`确认删除「${label}」？删除后立即保存并生效。`)) return;
  // 删除是确认动作，不能被「取消编辑」回滚。
  state.editorSnapshot = null;
  closeEditor(false);
  const items = state.config[section.key] || [];
  items.splice(index, 1);
  markDirty();
  render();
  save();
}

/* ---------------- 表单字段 ---------------- */

function renderField(section, item, index, field) {
  if (field.visible_when) {
    const value = getPath(item, field.visible_when.field);
    if (value !== field.visible_when.equals) return null;
  }

  const path = fieldPath(section, index, field.key);
  const controlId = `field-${path.replace(/[^a-zA-Z0-9_-]/g, '-')}`;

  const wrapper = element('div', `field field-${field.kind}`);
  if (field.kind === 'libraries' || field.kind === 'list') wrapper.classList.add('field-wide');

  const label = element('label', 'field-label', field.label);
  label.htmlFor = controlId;
  if (field.required) label.appendChild(element('span', 'badge', '必填'));
  wrapper.appendChild(label);

  controlFor(section, item, field, controlId, wrapper);

  if (field.help) wrapper.appendChild(element('p', 'field-help', field.help));
  wrapper.dataset.field = path;
  // 弹窗内所有控件统一标记，任务运行时由 syncLocks 就地禁用（不重绘、不丢焦点）。
  for (const control of wrapper.querySelectorAll('input, select, textarea, button')) {
    control.classList.add('lock-sensitive');
  }
  return wrapper;
}

function controlFor(section, item, field, controlId, wrapper) {
  const value = getPath(item, field.key);
  const commit = (next) => {
    setPath(item, field.key, next);
    markDirty();
  };

  switch (field.kind) {
    case 'bool': {
      const label = element('label', 'checkbox');
      const input = document.createElement('input');
      input.type = 'checkbox';
      input.id = controlId;
      input.checked = Boolean(value);
      const caption = element('span', null, input.checked ? '已启用' : '已关闭');
      input.addEventListener('change', () => {
        caption.textContent = input.checked ? '已启用' : '已关闭';
        commit(input.checked);
      });
      label.appendChild(input);
      label.appendChild(caption);
      wrapper.appendChild(label);
      return;
    }
    case 'number': {
      const input = textInput(field, controlId, value, 'number');
      input.addEventListener('input', () => {
        if (input.value.trim() === '') {
          commit(0);
          return;
        }
        const parsed = Number(input.value);
        if (Number.isFinite(parsed)) commit(parsed);
      });
      wrapper.appendChild(input);
      return;
    }
    case 'select': {
      const select = document.createElement('select');
      select.className = 'select';
      select.id = controlId;
      if (!field.required) {
        const option = document.createElement('option');
        option.value = '';
        option.textContent = '（未选择）';
        select.appendChild(option);
      }
      for (const option of optionsOf(field)) {
        const node = document.createElement('option');
        node.value = option.value;
        node.textContent = option.label;
        select.appendChild(node);
      }
      select.value = value == null ? '' : String(value);
      select.addEventListener('change', () => {
        commit(select.value === '' ? null : select.value);
        if (hasDependents(section, field.key)) render();
      });
      wrapper.appendChild(select);
      return;
    }
    case 'secret': {
      const row = element('div', 'input-row');
      const input = textInput(field, controlId, value, 'password');
      const toggle = element('button', 'btn btn-ghost btn-small', '显示');
      toggle.type = 'button';
      toggle.addEventListener('click', () => {
        const hidden = input.type === 'password';
        input.type = hidden ? 'text' : 'password';
        toggle.textContent = hidden ? '隐藏' : '显示';
      });
      // MediaWarp 的 server.auth 是非空字符串字段，空值必须写成 "" 而不是 null。
      input.addEventListener('input', () =>
        commit(field.keep_empty ? input.value.trim() : emptyToNull(input.value)),
      );
      row.appendChild(input);
      row.appendChild(toggle);
      wrapper.appendChild(row);
      return;
    }
    case 'cron': {
      // 定时字段：保留手输 6 段表达式，同时提供「随机」在每天 00:00–23:59 内取一个时刻。
      const row = element('div', 'input-row');
      const input = textInput(field, controlId, value, 'text');
      input.addEventListener('input', () => {
        commit(field.nullable ? emptyToNull(input.value) : input.value.trim());
      });
      const dice = element('button', 'btn btn-ghost btn-small', '随机');
      dice.type = 'button';
      dice.title = '生成每天 00:00–23:59 内随机时刻的表达式';
      dice.addEventListener('click', () => {
        const next = randomDailyCron(input.value);
        input.value = next;
        commit(next);
        toast('ok', `已生成随机定时：${next}`);
      });
      row.appendChild(input);
      row.appendChild(dice);
      wrapper.appendChild(row);
      return;
    }
    case 'list': {
      const input = textInput(field, controlId, (Array.isArray(value) ? value : []).join(', '), 'text');
      input.addEventListener('input', () => {
        const list = input.value
          .split(',')
          .map((entry) => entry.trim())
          .filter((entry) => entry !== '');
        commit(list);
      });
      wrapper.appendChild(input);
      return;
    }
    case 'alist_path': {
      // 路径类字段：保留手输能力，同时提供「浏览」从 AList 上已有的目录里选。
      const row = element('div', 'input-row');
      const input = textInput(field, controlId, value, 'text');
      input.addEventListener('input', () => {
        commit(input.value.trim());
      });
      row.appendChild(input);
      row.appendChild(
        button('btn btn-ghost btn-small', '浏览', 'folder', () =>
          openAlistBrowser(item, field, input),
        ),
      );
      wrapper.appendChild(row);
      return;
    }
    case 'libraries': {
      wrapper.appendChild(librariesControl(item, field));
      return;
    }
    case 'resolution': {
      wrapper.appendChild(resolutionControl(item, field, controlId));
      return;
    }
    default: {
      const input = textInput(field, controlId, value, 'text');
      input.addEventListener('input', () => {
        commit(field.nullable ? emptyToNull(input.value) : input.value.trim());
      });
      wrapper.appendChild(input);
    }
  }
}

/**
 * 生成 6 段 cron（秒固定 0，分与时随机），即「每天 00:00–23:59 内的随机时刻」。
 * 传入当前值时会重试，尽量不生成与原来完全相同的表达式。
 */
function randomDailyCron(current) {
  const existing = current == null ? '' : String(current).trim();
  const pick = () => `0 ${Math.floor(Math.random() * 60)} ${Math.floor(Math.random() * 24)} * * *`;
  for (let attempt = 0; attempt < 8; attempt += 1) {
    const next = pick();
    if (next !== existing) return next;
  }
  return pick();
}

function textInput(field, controlId, value, type) {
  const input = document.createElement('input');
  input.className = 'input';
  input.id = controlId;
  input.type = type;
  if (field.placeholder) input.placeholder = field.placeholder;
  if (field.min != null) input.min = field.min;
  if (field.max != null) input.max = field.max;
  if (field.step != null) input.step = field.step;
  input.value = value == null ? '' : String(value);
  return input;
}

function optionsOf(field) {
  if (Array.isArray(field.options)) return field.options;
  if (field.options_from) {
    const items = state.config[field.options_from] || [];
    return items.map((entry) => ({
      value: entry.id || '',
      label: entry.id || '（未命名）',
    }));
  }
  return [];
}

function hasDependents(section, key) {
  return (section.fields || []).some((field) => field.visible_when && field.visible_when.field === key);
}

function librariesControl(item, field) {
  const wrapper = element('div');
  const fields = state.schema.library_fields || [];
  const rows = Array.isArray(item[field.key]) ? item[field.key] : [];
  if (!Array.isArray(item[field.key])) item[field.key] = rows;

  const table = document.createElement('table');
  table.className = 'lib-table';

  const thead = document.createElement('thead');
  const headRow = document.createElement('tr');
  for (const libField of fields) headRow.appendChild(element('th', null, libField.label));
  headRow.appendChild(element('th', null, '操作'));
  thead.appendChild(headRow);
  table.appendChild(thead);

  const tbody = document.createElement('tbody');
  rows.forEach((row, rowIndex) => {
    const tr = document.createElement('tr');
    for (const libField of fields) {
      const td = document.createElement('td');
      if (libField.kind === 'select') {
        const select = document.createElement('select');
        select.className = 'select';
        for (const option of libField.options || []) {
          const node = document.createElement('option');
          node.value = option.value;
          node.textContent = option.label;
          select.appendChild(node);
        }
        select.value = row[libField.key] || (libField.options && libField.options[0] ? libField.options[0].value : '');
        select.addEventListener('change', () => {
          row[libField.key] = select.value;
          markDirty();
        });
        td.appendChild(select);
      } else {
        const input = textInput(libField, '', row[libField.key] == null ? '' : row[libField.key], 'text');
        input.addEventListener('input', () => {
          row[libField.key] = input.value;
          markDirty();
        });
        td.appendChild(input);
      }
      tr.appendChild(td);
    }

    const actionCell = document.createElement('td');
    actionCell.appendChild(
      button('btn btn-danger btn-small', '移除', null, () => {
        item[field.key].splice(rowIndex, 1);
        markDirty();
        render();
      }),
    );
    tr.appendChild(actionCell);
    tbody.appendChild(tr);
  });
  table.appendChild(tbody);
  wrapper.appendChild(table);

  const actions = element('div', 'lib-actions');
  actions.appendChild(
    button('btn btn-ghost btn-small', '新增媒体库', 'plus', () => {
      item[field.key].push({ name: '', title: '', subtitle: '', sort: 'date_last_content_added' });
      markDirty();
      render();
    }),
  );
  wrapper.appendChild(actions);
  return wrapper;
}

function resolutionControl(item, field, controlId) {
  const wrapper = element('div', 'resolution');
  const current = getPath(item, field.key);
  const custom = current != null && typeof current === 'object';
  const presets = state.schema.resolution_presets || [];

  const select = document.createElement('select');
  select.className = 'select';
  select.id = controlId;
  for (const preset of presets) {
    const node = document.createElement('option');
    node.value = preset.value;
    node.textContent = preset.label;
    select.appendChild(node);
  }
  const customOption = document.createElement('option');
  customOption.value = 'custom';
  customOption.textContent = '自定义尺寸';
  select.appendChild(customOption);
  select.value = custom ? 'custom' : typeof current === 'string' ? current : '1080p';

  const dims = element('div', `resolution-dims${custom ? '' : ' is-hidden'}`);
  const widthInput = textInput({ min: 1 }, '', custom ? current.width : 1920, 'number');
  const heightInput = textInput({ min: 1 }, '', custom ? current.height : 1080, 'number');
  const writeDimensions = () => {
    const width = Number(widthInput.value);
    const height = Number(heightInput.value);
    if (!Number.isFinite(width) || !Number.isFinite(height) || width <= 0 || height <= 0) return;
    setPath(item, field.key, { width: Math.round(width), height: Math.round(height) });
    markDirty();
  };
  widthInput.addEventListener('input', writeDimensions);
  heightInput.addEventListener('input', writeDimensions);
  dims.appendChild(widthInput);
  dims.appendChild(element('span', 'muted', '×'));
  dims.appendChild(heightInput);

  select.addEventListener('change', () => {
    if (select.value === 'custom') {
      const base = custom ? current : { width: 1920, height: 1080 };
      setPath(item, field.key, { width: base.width, height: base.height });
    } else {
      setPath(item, field.key, select.value);
    }
    markDirty();
    render();
  });

  wrapper.appendChild(select);
  wrapper.appendChild(dims);
  return wrapper;
}

/* ---------------- AList 目录浏览 ---------------- */

let browserClose = null;

/** 关闭已打开的目录选择器（如果有）。 */
function closeAlistBrowser() {
  if (!browserClose) return;
  const dispose = browserClose;
  browserClose = null;
  dispose();
}

/**
 * 打开 AList 目录选择器。
 *
 * 选用的是任务上已经配置好的 AList 客户端，目录由后端用该客户端的登录态
 * 实时列出，因此用户不需要手打路径，也不会因为路径拼错导致任务扫描失败。
 */
function openAlistBrowser(item, field, input) {
  const alistId = String(getPath(item, field.alist_ref || 'alist') || '').trim();
  if (!alistId) {
    toast('warn', '请先在上面选择 AList 客户端，再来浏览目录');
    return;
  }

  closeAlistBrowser();

  let disposed = false;
  const pendingRequests = new Set();

  let currentPath = String(input.value || '').trim() || '/';
  let parentPath = null;

  const mask = element('div', 'modal-mask');
  const modal = element('div', 'modal');

  const head = element('div', 'modal-head');
  head.appendChild(element('span', 'modal-title', '选择 AList 目录'));
  const pathLabel = element('span', 'modal-path', '/');
  head.appendChild(pathLabel);
  head.appendChild(iconButton('', 'close', '关闭', () => closeAlistBrowser()));
  modal.appendChild(head);

  const body = element('div', 'modal-body');
  modal.appendChild(body);

  const foot = element('div', 'modal-foot');
  const up = button('btn btn-ghost btn-small', '上一级', 'chevron-up', () => {
    if (parentPath) load(parentPath);
  });
  up.disabled = true;
  foot.appendChild(up);
  foot.appendChild(button('btn btn-ghost btn-small', '取消', null, () => closeAlistBrowser()));
  foot.appendChild(
    button('btn btn-primary btn-small', '选择当前目录', 'check', () => {
      input.value = currentPath;
      input.dispatchEvent(new Event('input'));
      closeAlistBrowser();
    }),
  );
  modal.appendChild(foot);

  mask.appendChild(modal);
  document.body.appendChild(mask);

  const onKeydown = (event) => {
    if (event.key === 'Escape') closeAlistBrowser();
  };
  const onMaskClick = (event) => {
    if (event.target === mask) closeAlistBrowser();
  };

  window.addEventListener('keydown', onKeydown);
  mask.addEventListener('click', onMaskClick);

  browserClose = () => {
    disposed = true;
    for (const controller of pendingRequests) controller.abort();
    pendingRequests.clear();
    window.removeEventListener('keydown', onKeydown);
    mask.remove();
  };

  let loading = false;

  async function load(path) {
    if (disposed || loading) return;
    loading = true;
    const controller = new AbortController();
    pendingRequests.add(controller);
    const timer = window.setTimeout(() => controller.abort(), BROWSE_TIMEOUT);

    body.textContent = '';
    body.appendChild(element('p', 'modal-hint', '正在读取目录…'));

    try {
      const response = await fetch('/api/browse', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', 'X-AutoFilm-Request': '1' },
        body: JSON.stringify({ alist: alistId, path }),
        signal: controller.signal,
      });
      const data = await response.json().catch(() => ({}));
      if (disposed) return;
      if (redirectIfUnauthorized(response)) return;
      if (!response.ok) throw new Error(data.message || `读取目录失败（HTTP ${response.status}）`);

      currentPath = data.path || path;
      parentPath = data.parent || null;
      pathLabel.textContent = currentPath;
      pathLabel.title = currentPath;
      up.disabled = !parentPath;

      body.textContent = '';
      const entries = Array.isArray(data.entries) ? data.entries : [];
      if (!entries.length) {
        body.appendChild(element('p', 'modal-hint', '该目录下没有子目录'));
        return;
      }
      for (const entry of entries) {
        const node = element('button', 'dir-item');
        node.type = 'button';
        node.appendChild(icon('folder', 'icon-sm'));
        node.appendChild(element('span', null, entry.name));
        node.addEventListener('click', () => load(entry.path));
        body.appendChild(node);
      }
    } catch (err) {
      if (disposed) return;
      body.textContent = '';
      body.appendChild(
        element(
          'p',
          'modal-error',
          err.name === 'AbortError' ? '读取目录超时，请重试' : err.message,
        ),
      );
    } finally {
      window.clearTimeout(timer);
      pendingRequests.delete(controller);
      loading = false;
    }
  }

  load(currentPath);
}

/* ---------------- 任务运行日志 ---------------- */

let taskLogClose = null;

/** 日志行格式与后端 `task_log.rs` 一致：`MM-DD HH:MM:SS [LEVEL] 消息`。 */
const LOG_LINE_RE = /^(\d{2}-\d{2} \d{2}:\d{2}:\d{2})\s+(?:(\[(?:ERROR|WARN|DEBUG|TRACE)\])\s+)?([\s\S]*)$/;

/**
 * 动作词 → 颜色。同一动作在所有任务里必须是同一个颜色，因此只在这里维护一份
 * 映射：生成类（写文件）绿、下载/复制类（搬伴生文件）橙、删除类（清理本地）红。
 */
const LOG_OP_RULES = [
  { className: 'log-op-delete', pattern: /^删除/ },
  { className: 'log-op-fetch', pattern: /^(下载|复制)/ },
  { className: 'log-op-make', pattern: /^(生成|更新|写入)/ },
];

/** 日志行距离底部多少像素内仍视为「贴底」，用于决定新行是否自动跟随滚动。 */
const LOG_BOTTOM_TOLERANCE = 18;

function logOpRule(message) {
  return LOG_OP_RULES.find((rule) => rule.pattern.test(message)) || null;
}

/**
 * 单行日志的富文本渲染：整行按动作类型上色（生成绿 / 下载橙 / 删除红），动作词
 * 再加粗，时间戳与级别标记降透明度。非动作行保持正文色，避免满屏彩色。
 */
function renderLogLine(line) {
  const node = element('div', 'log-line');
  const matched = LOG_LINE_RE.exec(line);
  if (!matched) {
    node.textContent = line;
    return node;
  }

  const [, time, tag, message] = matched;
  node.appendChild(element('span', 'log-time', `${time} `));
  if (tag) {
    const level = tag.slice(1, -1).toLowerCase();
    node.appendChild(element('span', `log-tag log-tag-${level}`, `${tag} `));
  }

  const rule = logOpRule(message);
  if (!rule) {
    node.appendChild(document.createTextNode(message));
    return node;
  }

  node.classList.add(rule.className);
  const next = message.charAt(2);
  const colon = next === ':' || next === '：' ? next : '';
  node.appendChild(element('span', 'log-op', `${message.slice(0, 2)}${colon}`));
  node.appendChild(document.createTextNode(message.slice(2 + colon.length)));
  return node;
}

function closeTaskLog() {
  if (!taskLogClose) return;
  const dispose = taskLogClose;
  taskLogClose = null;
  dispose();
}

/**
 * 打开单个任务的日志。弹层期间每 0.5 秒重新拉取同一个任务的日志，因此运行中
 * 新增的行会持续追加；关闭后立刻停止轮询，不留后台请求。同一时刻只保留一个
 * 在途请求，避免高频轮询时响应乱序或请求堆积。
 */
function openTaskLog(section, item) {
  const module = section.module;
  const id = String(item.id || '').trim();
  if (!module || !id) {
    toast('warn', '任务还没有 ID，请先编辑填写任务 ID 后再查看日志');
    return;
  }

  closeTaskLog();
  closeEditor();

  let disposed = false;
  // null 表示尚未渲染过，保证首次（哪怕是空日志）也画出空态提示。
  let lastLines = null;
  let running = false;
  let inFlight = false;
  const logAbort = new AbortController();

  const mask = element('div', 'modal-mask');
  const modal = element('div', 'modal modal-log');

  const head = element('div', 'modal-head');
  head.appendChild(element('span', 'modal-title', `运行日志 · ${id}`));
  const status = element('span', 'pill log-status', '加载中…');
  head.appendChild(status);
  head.appendChild(iconButton('', 'close', '关闭', () => closeTaskLog()));
  modal.appendChild(head);

  const body = element('div', 'modal-body log-body');
  modal.appendChild(body);

  const foot = element('div', 'modal-foot');
  foot.appendChild(button('btn btn-ghost btn-small', '刷新', 'refresh', () => refresh()));
  foot.appendChild(button('btn btn-primary btn-small', '关闭', null, () => closeTaskLog()));
  modal.appendChild(foot);

  mask.appendChild(modal);
  document.body.appendChild(mask);

  function renderLines(lines) {
    // 用户往上翻看历史时不强行拽回底部。
    const atBottom = body.scrollTop + body.clientHeight >= body.scrollHeight - LOG_BOTTOM_TOLERANCE;
    body.textContent = '';
    if (!lines.length) {
      body.appendChild(
        element('p', 'modal-hint', '暂无运行日志，运行一次任务后即可在这里查看。'),
      );
      return;
    }
    const list = element('div', 'log-lines');
    for (const line of lines) {
      list.appendChild(renderLogLine(line));
    }
    body.appendChild(list);
    if (atBottom) body.scrollTop = body.scrollHeight;
  }

  function updateStatus() {
    status.className = running ? 'pill pill-live log-status' : 'pill log-status';
    status.textContent = running
      ? '运行中 · 自动刷新'
      : `共 ${lastLines.length} 行 · 最多保留 1024 行`;
  }

  async function refresh() {
    if (disposed || inFlight) return;
    inFlight = true;
    try {
      const { response, data } = await requestTaskLog(module, id, logAbort.signal);
      if (disposed) return;
      if (!response.ok) {
        throw new Error(data.message || `读取日志失败（HTTP ${response.status}）`);
      }
      const lines = Array.isArray(data.lines) ? data.lines.map(String) : [];
      const changed =
        lastLines === null ||
        lines.length !== lastLines.length ||
        lines.some((line, index) => line !== lastLines[index]);
      lastLines = lines;
      // 后端 running 来自调度闸门；面板刚点「立即运行」时 pending 也算运行中。
      running = Boolean(data.running) || isRunning(`${module}:${id}`);
      if (changed) renderLines(lines);
      updateStatus();
    } catch (err) {
      if (disposed) return;
      body.textContent = '';
      body.appendChild(
        element(
          'p',
          'modal-error',
          err.name === 'AbortError' ? '读取日志超时，正在重试…' : err.message,
        ),
      );
    } finally {
      inFlight = false;
    }
  }

  const onKeydown = (event) => {
    if (event.key === 'Escape') closeTaskLog();
  };
  const onMaskClick = (event) => {
    if (event.target === mask) closeTaskLog();
  };
  window.addEventListener('keydown', onKeydown);
  mask.addEventListener('click', onMaskClick);

  const timer = window.setInterval(refresh, TASK_LOG_POLL_INTERVAL);
  taskLogClose = () => {
    disposed = true;
    logAbort.abort();
    window.clearInterval(timer);
    window.removeEventListener('keydown', onKeydown);
    mask.remove();
  };

  refresh();
}

/* ---------------- 立即运行 ---------------- */

function isRunning(key) {
  if (!key || key.endsWith(':')) return false;
  if (state.pending.has(key)) return true;
  return ((state.runtime && state.runtime.running) || []).includes(key);
}

function anyRunning() {
  if (state.pending.size) return true;
  return ((state.runtime && state.runtime.running) || []).length > 0;
}

function syncRunButtons() {
  for (const node of document.querySelectorAll('[data-run-key]')) {
    const key = node.dataset.runKey || '';
    const running = isRunning(key);
    node.classList.toggle('is-running', running);
    node.disabled = running || key.endsWith(':');
    node.title = running ? '正在运行…' : node.dataset.runTitle || '立即运行';
    node.textContent = '';
    node.appendChild(icon(running ? 'refresh' : 'play', 'icon-sm'));
    node.setAttribute('aria-label', node.title);
  }
}

function syncNextRuns() {
  for (const node of document.querySelectorAll('[data-next-run]')) {
    const [module, id] = String(node.dataset.nextRun).split(':');
    const text = nextRunText(module, id);
    node.textContent = text || '—';
    node.classList.toggle('cell-dim', !text);
  }
}

function syncRuntimeUi() {
  syncRunButtons();
  syncNextRuns();
  syncLocks();
  syncMediaWarpStatus();
}

async function runTask(section, item) {
  if (!section.module) return;
  if (!item.id) {
    toast('warn', '任务还没有 ID，请先编辑填写任务 ID 后再运行');
    return;
  }

  const key = `${section.module}:${item.id}`;
  if (isRunning(key)) {
    toast('warn', '该任务正在运行，请等待本次执行结束');
    return;
  }

  state.pending.add(key);
  syncRunButtons();

  try {
    const { response, data } = await requestRun(section.module, item.id);
    if (!response.ok) {
      state.pending.delete(key);
      syncRunButtons();
      toast('error', data.message || `触发失败（HTTP ${response.status}）`);
      return;
    }
    toast('ok', data.message || '已触发任务');
    startPolling();
  } catch (err) {
    state.pending.delete(key);
    syncRunButtons();
    toast('error', `触发失败：${err.message}`);
  }
}

async function runAll(section) {
  if (!section.module) return;
  try {
    const { response, data } = await requestRunAll(section.module);
    if (!response.ok) {
      toast('error', data.message || `触发失败（HTTP ${response.status}）`);
      return;
    }
    toast('ok', data.message || '已触发全部任务');
    const items = state.config[section.key] || [];
    for (const item of items) {
      if (item.id) state.pending.add(`${section.module}:${item.id}`);
    }
    syncRunButtons();
    startPolling();
  } catch (err) {
    toast('error', `触发失败：${err.message}`);
  }
}

function startPolling() {
  if (state.pollTimer != null) return;
  state.pollTimer = window.setInterval(pollRuntime, POLL_INTERVAL);
  state.pollKick = window.setTimeout(pollRuntime, 900);
}

function stopPolling() {
  if (state.pollTimer == null) return;
  window.clearInterval(state.pollTimer);
  window.clearTimeout(state.pollKick);
  state.pollTimer = null;
  state.pollKick = null;
}

async function pollRuntime() {
  try {
    const response = await fetch('/api/runtime', {
      headers: { Accept: 'application/json' },
      cache: 'no-store',
    });
    if (redirectIfUnauthorized(response)) return;
    if (response.ok) {
      const data = await response.json();
      state.runtime = data.runtime || {};

      // 后端已经报告该任务在运行（或已经跑完），乐观标记就可以撤掉了。
      const running = state.runtime.running || [];
      for (const key of Array.from(state.pending)) {
        if (running.includes(key)) state.pending.delete(key);
      }
      syncRuntimeUi();

      // 外部编辑或其它会话改动了配置：本地没有草稿时自动同步，保持界面与运行态一致。
      const versionChanged =
        (data.version != null && data.version !== state.version) ||
        (data.web_version != null && data.web_version !== state.webVersion);
      if (versionChanged && !state.dirty && !state.saving && !editorModal) {
        await refresh();
      }
    }
  } catch (err) {
    /* 轮询失败不打扰用户，下一轮重试 */
  }

  // 兜底：触发后 20 秒仍未观测到运行状态，撤掉乐观标记，避免按钮永久卡住。
  // 兜底计时不能被「其它任务仍在运行」卡住：那会让未被后端确认的 pending key
  // 一直滞留、轮询永不停止。anyRunning() 仍会依据 state.runtime.running 保住
  // 真正运行中的任务，所以这里可以独立计数。
  if (state.pending.size) {
    state.pendingTimer = (state.pendingTimer || 0) + 1;
    if (state.pendingTimer * POLL_INTERVAL > 20000) {
      state.pending.clear();
      state.pendingTimer = 0;
      syncRunButtons();
    }
  } else {
    state.pendingTimer = 0;
  }
  // 轮询常驻：运行中任务的锁定状态需要随时开始 / 解除，不能随任务结束而停。
}

/* ---------------- 编辑状态、锁定与保存反馈 ---------------- */

const SAVE_STATUS_TEXT = {
  saved: '全部更改已实时生效',
  dirty: '有未保存的改动',
  saving: '保存中…',
  error: '保存失败',
  invalid: '校验未通过，请修正后再保存',
  locked: '任务运行中，配置已锁定',
};

function setSaveStatus(kind, text) {
  const node = byId('save-status');
  state.saveState = kind;
  if (!node) return;
  node.className = `save-status save-status-${kind}`;
  node.textContent = text || SAVE_STATUS_TEXT[kind] || SAVE_STATUS_TEXT.saved;
}

/** 依据与上次保存快照的差异重算 dirty，并同步顶部状态。 */
function syncDirty() {
  state.dirty =
    JSON.stringify(businessPayload()) !== state.savedBusiness ||
    JSON.stringify(webPayload()) !== state.savedWeb;
  setSaveStatus(state.dirty ? 'dirty' : 'saved');
  updateChrome();
}

function markDirty() {
  state.dirty = true;
  setSaveStatus('dirty');
}

/** 只有带 module 的任务分区、且该任务正在运行时才锁定。 */
function isLocked(section, item) {
  if (!section || !section.module || !item || !item.id) return false;
  return isRunning(`${section.module}:${item.id}`);
}

/** 就地更新锁定状态，不重绘 DOM，避免打断正在输入的用户。 */
function syncLocks() {
  for (const root of document.querySelectorAll('[data-lock-key]')) {
    const key = root.dataset.lockKey || '';
    const locked = !key.endsWith(':') && isRunning(key);
    root.classList.toggle('is-locked', locked);
    for (const note of root.querySelectorAll('.lock-note')) {
      note.classList.toggle('is-hidden', !locked);
    }
    for (const node of root.querySelectorAll('.lock-sensitive')) {
      if (node.dataset.baseDisabled == null) {
        node.dataset.baseDisabled = node.disabled ? 'true' : 'false';
      }
      node.disabled = locked || node.dataset.baseDisabled === 'true';
    }
  }
}

function clearFieldErrors() {
  for (const node of document.querySelectorAll('.field.is-invalid')) {
    node.classList.remove('is-invalid');
  }
  for (const node of document.querySelectorAll('.field-error')) {
    node.remove();
  }
  for (const node of document.querySelectorAll('.item-card.is-invalid')) {
    node.classList.remove('is-invalid');
  }
}

/** 校验失败时打开出错行的编辑弹窗，否则字段根本不在 DOM 里。 */
function revealErrorFields(errors) {
  for (const error of errors) {
    const match = /^([A-Za-z0-9_]+)\[(\d+)\]/.exec(error.field || '');
    if (!match) continue;
    const section = allSections().find((item) => item.key === match[1]);
    const index = Number(match[2]);
    if (!section || !(state.config[match[1]] || [])[index]) continue;
    if (isExpanded(section, index)) return false;
    openEditor(section, index);
    return true;
  }
  return false;
}

function applyErrors(errors) {
  clearFieldErrors();
  let first = null;
  for (const error of errors) {
    const container = document.querySelector(`[data-field="${error.field}"]`);
    if (!container) continue;
    container.classList.add('is-invalid');
    const row = container.closest('tr');
    if (row) row.classList.add('is-invalid');
    container.appendChild(element('p', 'field-error', error.message));
    if (!first) first = container;
  }
  if (first) first.scrollIntoView({ behavior: 'smooth', block: 'center' });

  const details = errors.map((error) => (error.field ? `${error.field}：${error.message}` : error.message));
  showAlert('error', `配置校验未通过（${errors.length} 处问题），已定位到具体字段`, details);
}

/* ---------------- 保存（保存即实时生效） ---------------- */

function setBusy(busy) {
  if (busy) setSaveStatus('saving');
  else if (state.saveState === 'saving') syncDirty();
}

function reportSaveFailure(response, data, label) {
  const message = data.message || `${label}保存失败（HTTP ${response.status}）`;

  if (response.status === 423) {
    setSaveStatus('locked', message);
    showAlert('error', message);
    toast('error', message);
    return;
  }
  if (response.status === 409) {
    setSaveStatus('error', '配置版本冲突');
    showAlert('error', message);
    toast('error', `${label}已被其它会话或外部文件修改，请刷新页面后重试`);
    return;
  }
  if (Array.isArray(data.errors) && data.errors.length) {
    setSaveStatus('invalid', '校验未通过，请修正后再保存');
    if (revealErrorFields(data.errors)) render();
    applyErrors(data.errors);
    toast('error', data.message || `${label}校验未通过`);
    return;
  }
  setSaveStatus('error', '保存失败');
  showAlert('error', message);
  toast('error', message);
}

/**
 * 保存当前所有改动，返回是否全部成功。
 *
 * 成功后只更新本地快照与版本号，不调用 refresh()：重绘会打断正在输入的表单。
 * 运行时状态与「运行中」标记由常驻轮询负责刷新。
 */
async function save() {
  if (state.saving) return false;

  // 两份配置分别判定是否有改动：只提交真正改过的那一份，避免无谓地推进
  // 另一份配置的版本号（也就不会让其他会话白白遇到 409）。
  const businessDirty = JSON.stringify(businessPayload()) !== state.savedBusiness;
  const webDirty = JSON.stringify(webPayload()) !== state.savedWeb;
  if (!businessDirty && !webDirty) {
    syncDirty();
    toast('warn', '没有需要保存的改动');
    return true;
  }

  state.saving = true;
  setBusy(true);
  clearAlert();

  try {
    const notes = [];
    const okNotes = [];
    let businessOk = false;
    let webOk = false;

    if (businessDirty) {
      const { response, data } = await requestSave({
        base_version: state.version,
        config: businessPayload(),
      });
      if (!response.ok) {
        reportSaveFailure(response, data, '业务配置');
        return false;
      }
      businessOk = true;
      state.version = data.version;
      state.savedBusiness = JSON.stringify(businessPayload());
      notes.push(...((data.report && data.report.warnings) || []));
      okNotes.push(...((data.report && data.report.notes) || []));
    }

    if (webDirty) {
      const { response, data } = await requestSaveWeb({
        base_version: state.webVersion,
        web: webPayload(),
      });
      if (!response.ok) {
        reportSaveFailure(response, data, '面板配置');
        return false;
      }
      webOk = true;
      state.webVersion = data.version;
      state.savedWeb = JSON.stringify(webPayload());
      notes.push(...((data.report && data.report.warnings) || []));
      okNotes.push(...((data.report && data.report.notes) || []));
    }

    syncDirty();
    toast('ok', businessOk && webOk ? '两份配置已保存并实时生效' : '配置已保存并实时生效');
    // 自动重启这类「成功但值得告知」的信息用成功提示，不混入警告。
    for (const note of okNotes) toast('ok', note);
    if (notes.length) {
      for (const note of notes) toast('warn', note);
      showAlert('warn', '配置已保存并实时生效', notes);
    }
    return true;
  } catch (err) {
    const message = `保存失败：${err.message}`;
    setSaveStatus('error', '保存失败');
    showAlert('error', message);
    toast('error', message);
    return false;
  } finally {
    state.saving = false;
    setBusy(false);
  }
}

/** 弹窗内「保存」：成功才关闭，失败时保留弹窗与已填内容。 */
async function saveEditor() {
  const ok = await save();
  if (!ok) return;
  state.editorSnapshot = null;
  closeEditor();
}

/* ---------------- 启动 ---------------- */

function bindChrome() {
  byId('btn-theme').addEventListener('click', toggleTheme);
  const logoutButton = byId('btn-logout');
  logoutButton.appendChild(icon('logout'));
  logoutButton.title = '退出登录';
  logoutButton.addEventListener('click', logout);
  window.addEventListener('hashchange', () => {
    const key = activeFromHash();
    if (key === state.active) return;
    state.active = key;
    closeEditor(false);
    renderNav();
    render();
  });
  window.addEventListener('beforeunload', (event) => {
    if (!state.dirty) return;
    event.preventDefault();
    event.returnValue = '';
  });
}

async function boot() {
  loadTheme();
  bindChrome();
  try {
    applyState(await requestState());
  } catch (err) {
    showAlert('error', err.message);
  }
}

boot();
