'use strict';

/**
 * AutoFilm 管理控制台前端。
 *
 * 数据流仍是「读取整份配置 → 本地编辑 → 整份提交」：提交时携带 base_version 做
 * 乐观并发控制，后端校验通过后写盘并热重载调度器。字段结构全部来自 /api/state
 * 返回的 schema，前端不硬编码字段，避免与 Rust 结构体漂移。
 *
 * 配置分成两份互不影响的文档：
 * - state.config / state.version：业务配置（config.yaml），保存后热重载调度器；
 * - state.web / state.webVersion：面板配置（web.yaml），保存后需重启进程生效。
 *
 * 列表型分区的交互：默认只显示摘要表格，点「编辑」才在行下方展开完整表单；
 * 「立即运行」直接调用后端 /api/run，与定时触发共用同一个执行入口。
 */

const SVG_NS = 'http://www.w3.org/2000/svg';
const THEME_KEY = 'autofilm-theme';
const POLL_INTERVAL = 3000;

const state = {
  app: null,
  schema: null,
  config: null,
  version: 0,
  web: null,
  webVersion: 0,
  runtime: {},
  active: 'overview',
  dirty: false,
  saving: false,
  savedBusiness: '',
  savedWeb: '',
  theme: 'dark',
  /** 已展开编辑器的行，键为 `分区#下标`。 */
  expanded: new Set(),
  /** 刚点过「立即运行」、等后端确认的任务键，用于即时反馈。 */
  pending: new Set(),
  pollTimer: null,
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

async function requestReload() {
  const response = await fetch('/api/reload', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-AutoFilm-Request': '1' },
    body: '{}',
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
  if (anyRunning()) startPolling();
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
  nav.appendChild(navButton('overview', '总览'));
  for (const section of allSections()) {
    nav.appendChild(navButton(section.key, section.title));
  }
}

function navButton(key, label) {
  const node = element('button', `nav-item${state.active === key ? ' is-active' : ''}`, label);
  node.type = 'button';
  node.addEventListener('click', () => setActive(key));
  return node;
}

/** 切换分区：同步到地址栏 hash，刷新或分享链接后仍停留在同一分区。 */
function setActive(key) {
  state.active = key;
  const hash = key === 'overview' ? '' : `#${encodeURIComponent(key)}`;
  if (window.location.hash !== hash) {
    window.history.replaceState(null, '', hash || window.location.pathname);
  }
  clearAlert();
  renderNav();
  render();
  window.scrollTo({ top: 0, behavior: 'smooth' });
}

/** 从地址栏 hash 还原当前分区，非法值回落到总览。 */
function activeFromHash() {
  const key = decodeURIComponent((window.location.hash || '').replace(/^#/, '')).trim();
  if (!key) return 'overview';
  if (key === 'overview') return 'overview';
  return allSections().some((section) => section.key === key) ? key : 'overview';
}

function currentSection() {
  return allSections().find((section) => section.key === state.active) || null;
}

function updateChrome() {
  const section = currentSection();
  byId('section-title').textContent = section ? section.title : '总览';
  byId('section-subtitle').textContent = section
    ? section.subtitle || ''
    : '运行状态、配置概览与校验规则';
  byId('dirty-pill').classList.toggle('is-hidden', !state.dirty);
  updateRuntimeLine();
}

function runtimeParts() {
  const parts = [];
  parts.push(`业务配置 v${state.version}`);
  parts.push(`面板配置 v${state.webVersion}`);
  if (state.runtime.scheduled_count != null) parts.push(`调度任务 ${state.runtime.scheduled_count}`);
  const running = (state.runtime.running || []).length;
  if (running) parts.push(`运行中 ${running}`);
  if (state.runtime.last_reload_at) {
    parts.push(
      `最近重载 ${state.runtime.last_reload_at}（${state.runtime.last_reload_source || '未知来源'}）`,
    );
  }
  if (state.runtime.last_error) parts.push(`最近错误：${state.runtime.last_error}`);
  return parts;
}

function updateRuntimeLine() {
  byId('runtime-line').textContent = runtimeParts().join(' · ');
}

function render() {
  if (!state.schema) return;
  const content = byId('content');
  content.textContent = '';

  const section = currentSection();
  if (!section) {
    renderOverview(content);
  } else if (section.single) {
    renderSingleSection(content, section);
  } else {
    renderListSection(content, section);
  }
  updateChrome();
}

/* ---------------- 渲染：总览 ---------------- */

function cardHead(title, subtitle, actions) {
  const head = element('header', 'card-head');
  const titles = element('div');
  titles.appendChild(element('h2', null, title));
  if (subtitle) titles.appendChild(element('p', 'card-sub', subtitle));
  head.appendChild(titles);
  if (actions) head.appendChild(actions);
  return head;
}

function statCard(label, value, hint) {
  const node = element('div', 'stat');
  node.appendChild(element('span', 'stat-label', label));
  node.appendChild(element('span', 'stat-value', value));
  if (hint) node.appendChild(element('span', 'stat-hint', hint));
  return node;
}

// 总览只保留状态卡片：配置文件位置、校验规则等说明已从页面移除。
function renderOverview(container) {
  const stats = element('div', 'grid-cards');
  const running = (state.runtime.running || []).length;
  stats.appendChild(statCard('业务配置版本', `v${state.version}`, '每次成功应用后自增'));
  stats.appendChild(statCard('面板配置版本', `v${state.webVersion}`, '保存在独立的 web.yaml'));
  stats.appendChild(
    statCard('调度任务', String(state.runtime.scheduled_count ?? 0), '当前已注册的定时任务数量'),
  );
  stats.appendChild(
    statCard('运行中', String(running), running ? '正在执行的任务' : '当前空闲'),
  );
  stats.appendChild(
    statCard(
      '最近热重载',
      state.runtime.last_reload_at || '—',
      state.runtime.last_reload_source || '尚未触发热重载',
    ),
  );
  container.appendChild(stats);
}

/* ---------------- 渲染：单一设置分区 ---------------- */

function renderSingleSection(container, section) {
  const wrapper = element('section', 'card');
  const actions = element('div', 'card-actions');
  if (section.restart_required) actions.appendChild(element('span', 'pill pill-warn', '重启生效'));
  wrapper.appendChild(cardHead(section.item_label, '', actions));

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

function isExpanded(section, index) {
  return state.expanded.has(`${section.key}#${index}`);
}

function toggleEditor(section, index) {
  const key = `${section.key}#${index}`;
  if (state.expanded.has(key)) state.expanded.delete(key);
  else state.expanded.add(key);
  render();
}

function renderListSection(container, section) {
  const items = Array.isArray(state.config[section.key]) ? state.config[section.key] : [];
  const columns = columnsOf(section);

  const wrapper = element('section', 'card');

  const actions = element('div', 'card-actions');
  if (section.module) {
    actions.appendChild(button('btn btn-primary', '运行所有', 'bolt', () => runAll(section)));
  }
  if (section.addable) {
    actions.appendChild(
      button('btn btn-primary', `添加${section.item_label}`, 'plus', () => addItem(section)),
    );
  }
  wrapper.appendChild(cardHead(`${items.length} 个${section.item_label}`, '', actions));

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
    if (isExpanded(section, index)) {
      tbody.appendChild(renderEditorRow(section, item, index, columns.length + 1));
    }
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

  columns.forEach((column, columnIndex) => {
    const cell = element('td');
    if (columnIndex === 0) cell.classList.add('cell-name');
    cell.appendChild(cellContent(section, item, column));
    row.appendChild(cell);
  });

  const actionCell = element('td', 'col-actions');
  const group = element('div', 'cell-actions');

  if (section.module) {
    const key = `${section.module}:${item.id || ''}`;
    const running = isRunning(key);
    const play = iconButton(
      'is-play',
      running ? 'refresh' : 'play',
      running ? '正在运行…' : '立即运行',
      () => runTask(section, item),
    );
    play.dataset.runKey = key;
    play.dataset.runTitle = '立即运行（按已保存的配置执行）';
    play.disabled = running || !item.id;
    if (running) play.classList.add('is-running');
    if (!item.id) play.title = '请先填写任务 ID';
    group.appendChild(play);
  }

  group.appendChild(
    iconButton('is-edit', 'pencil', isExpanded(section, index) ? '收起配置' : '编辑配置', () =>
      toggleEditor(section, index),
    ),
  );

  if (section.removable) {
    group.appendChild(
      iconButton('is-delete', 'trash', '删除', () => removeItem(section, index, item)),
    );
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

function renderEditorRow(section, item, index, columnCount) {
  const row = element('tr', 'editor-row');
  const cell = element('td', 'editor-cell');
  cell.colSpan = columnCount;

  const editor = element('div', 'editor');

  const head = element('div', 'editor-head');
  head.appendChild(
    element(
      'span',
      'editor-title',
      `编辑：${item.id ? item.id : `未命名${section.item_label}`}`,
    ),
  );
  head.appendChild(button('btn btn-ghost btn-small', '收起', 'chevron-up', () => toggleEditor(section, index)));
  editor.appendChild(head);

  const grid = element('div', 'editor-grid');
  for (const field of section.fields || []) {
    const node = renderField(section, item, index, field);
    if (node) grid.appendChild(node);
  }
  editor.appendChild(grid);

  const foot = element('div', 'editor-foot');
  if (section.removable) {
    foot.appendChild(button('btn btn-danger btn-small', '删除', 'trash', () => removeItem(section, index, item)));
  }
  if (section.module) {
    foot.appendChild(button('btn btn-ghost btn-small', '立即运行', 'play', () => runTask(section, item)));
  }
  foot.appendChild(button('btn btn-primary btn-small', '完成', 'check', () => toggleEditor(section, index)));
  editor.appendChild(foot);

  cell.appendChild(editor);
  row.appendChild(cell);
  return row;
}

function addItem(section) {
  const items = Array.isArray(state.config[section.key]) ? state.config[section.key] : [];
  state.config[section.key] = items;
  items.push(deepClone(section.template || {}));
  // 新条目直接展开，省一次「编辑」点击。
  state.expanded.add(`${section.key}#${items.length - 1}`);
  markDirty();
  render();
}

function removeItem(section, index, item) {
  const label = item && item.id ? item.id : `第 ${index + 1} 个${section.item_label}`;
  if (!window.confirm(`确认删除「${label}」？保存并应用后生效。`)) return;
  const items = state.config[section.key] || [];
  items.splice(index, 1);
  // 下标已经变化，展开状态一并清掉，避免展开到别的行。
  state.expanded.clear();
  markDirty();
  render();
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
      input.addEventListener('input', () => commit(emptyToNull(input.value)));
      row.appendChild(input);
      row.appendChild(toggle);
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
    window.removeEventListener('keydown', onKeydown);
    mask.remove();
  };

  async function load(path) {
    body.textContent = '';
    body.appendChild(element('p', 'modal-hint', '正在读取目录…'));

    try {
      const response = await fetch('/api/browse', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', 'X-AutoFilm-Request': '1' },
        body: JSON.stringify({ alist: alistId, path }),
      });
      const data = await response.json().catch(() => ({}));
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
      body.textContent = '';
      body.appendChild(element('p', 'modal-error', err.message));
    }
  }

  load(currentPath);
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
  updateRuntimeLine();
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
  window.setTimeout(pollRuntime, 900);
}

function stopPolling() {
  if (state.pollTimer == null) return;
  window.clearInterval(state.pollTimer);
  state.pollTimer = null;
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
    }
  } catch (err) {
    /* 轮询失败不打扰用户，下一轮重试 */
  }

  // 兜底：触发后 20 秒仍未观测到运行状态，撤掉乐观标记，避免按钮永久卡住。
  if (state.pending.size && !(state.runtime.running || []).length) {
    state.pendingTimer = (state.pendingTimer || 0) + 1;
    if (state.pendingTimer * POLL_INTERVAL > 20000) {
      state.pending.clear();
      state.pendingTimer = 0;
      syncRunButtons();
    }
  } else {
    state.pendingTimer = 0;
  }

  if (!anyRunning()) stopPolling();
}

/* ---------------- 编辑状态 ---------------- */

function markDirty() {
  state.dirty = true;
  updateChrome();
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

/** 校验失败时把出错行所在的编辑器展开，否则字段根本不在 DOM 里。 */
function revealErrorFields(errors) {
  let changed = false;
  for (const error of errors) {
    const match = /^([A-Za-z0-9_]+)\[(\d+)\]/.exec(error.field || '');
    if (!match) continue;
    const key = `${match[1]}#${match[2]}`;
    if (state.expanded.has(key)) continue;
    state.expanded.add(key);
    changed = true;
  }
  return changed;
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

/* ---------------- 保存与重载 ---------------- */

function setBusy(busy) {
  const save = byId('btn-save');
  const reload = byId('btn-reload');
  save.disabled = busy;
  reload.disabled = busy;
  save.textContent = busy ? '应用中…' : '保存并应用';
}

function reportSaveFailure(response, data, label) {
  if (response.status === 409) {
    showAlert('error', data.message || `${label}版本冲突，请重新加载后再保存`);
    toast('error', `${label}已被其他会话修改，请点击「重新加载」`);
    return;
  }

  if (Array.isArray(data.errors) && data.errors.length) {
    if (revealErrorFields(data.errors)) render();
    applyErrors(data.errors);
    toast('error', data.message || `${label}校验未通过`);
    return;
  }

  const message = data.message || `${label}保存失败（HTTP ${response.status}）`;
  showAlert('error', message);
  toast('error', message);
}

async function save() {
  if (state.saving) return;

  // 两份配置分别判定是否有改动：只提交真正改过的那一份，避免无谓地推进
  // 另一份配置的版本号（也就不会让其他会话白白遇到 409）。
  const businessDirty = JSON.stringify(businessPayload()) !== state.savedBusiness;
  const webDirty = JSON.stringify(webPayload()) !== state.savedWeb;
  if (!businessDirty && !webDirty) {
    toast('warn', '没有需要保存的改动');
    return;
  }

  state.saving = true;
  setBusy(true);
  clearAlert();

  let savedAny = false;

  try {
    const warnings = [];

    if (businessDirty) {
      const { response, data } = await requestSave({
        base_version: state.version,
        config: businessPayload(),
      });
      if (!response.ok) {
        reportSaveFailure(response, data, '业务配置');
        return;
      }
      savedAny = true;
      toast('ok', data.message || '业务配置已保存并生效');
      warnings.push(...((data.report && data.report.warnings) || []));
    }

    if (webDirty) {
      const { response, data } = await requestSaveWeb({
        base_version: state.webVersion,
        web: webPayload(),
      });
      if (!response.ok) {
        if (savedAny) await refresh();
        reportSaveFailure(response, data, '面板配置');
        return;
      }
      savedAny = true;
      toast('ok', data.message || '面板配置已保存');
      warnings.push(...((data.report && data.report.warnings) || []));
    }

    // 先刷新再提示：refresh 会清空顶部提示区，顺序反了警告会被立刻抹掉。
    await refresh();
    if (warnings.length) {
      for (const warning of warnings) toast('warn', warning);
      showAlert('warn', '配置已保存，但部分设置需要重启进程', warnings);
    }
  } catch (err) {
    showAlert('error', `保存失败：${err.message}`);
    toast('error', `保存失败：${err.message}`);
  } finally {
    state.saving = false;
    setBusy(false);
  }
}

async function reloadFromDisk() {
  if (state.saving) return;
  if (state.dirty && !window.confirm('当前有未保存的改动，重新加载会丢弃这些改动，继续？')) return;

  state.saving = true;
  setBusy(true);
  clearAlert();

  try {
    const { response, data } = await requestReload();
    if (response.ok) {
      toast('ok', data.message || '已重新加载配置文件');
      for (const warning of (data.report && data.report.warnings) || []) toast('warn', warning);
      await refresh();
      return;
    }

    if (Array.isArray(data.errors) && data.errors.length) {
      if (revealErrorFields(data.errors)) render();
      applyErrors(data.errors);
      toast('error', data.message || '配置文件校验未通过');
      return;
    }

    const message = data.message || `重新加载失败（HTTP ${response.status}）`;
    showAlert('error', message);
    toast('error', message);
  } catch (err) {
    showAlert('error', `重新加载失败：${err.message}`);
  } finally {
    state.saving = false;
    setBusy(false);
  }
}

/* ---------------- 启动 ---------------- */

function bindChrome() {
  byId('btn-save').addEventListener('click', save);
  byId('btn-reload').addEventListener('click', reloadFromDisk);
  byId('btn-theme').addEventListener('click', toggleTheme);
  const logoutButton = byId('btn-logout');
  logoutButton.appendChild(icon('logout'));
  logoutButton.title = '退出登录';
  logoutButton.addEventListener('click', logout);
  window.addEventListener('hashchange', () => {
    const key = activeFromHash();
    if (key === state.active) return;
    state.active = key;
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
