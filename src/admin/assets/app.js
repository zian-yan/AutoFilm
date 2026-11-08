'use strict';

/**
 * AutoFilm 管理控制台前端。
 *
 * 采用「读取整份配置 → 本地编辑 → 整份提交」的方式，提交时携带 base_version
 * 做乐观并发控制，后端校验通过后写盘并热重载调度器。所有字段结构由
 * /api/state 返回的 schema 描述，前端不硬编码字段，避免与 Rust 结构体漂移。
 *
 * 配置分成两份互不影响的文档：
 * - state.config / state.version：业务配置（config.yaml），保存后热重载调度器；
 * - state.web / state.webVersion：面板配置（web.yaml），保存后需重启进程生效。
 * 两份配置各自带版本号、各自提交到独立接口，因此不会互相覆盖。
 */

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

async function requestState() {
  const response = await fetch('/api/state', {
    headers: { Accept: 'application/json' },
    cache: 'no-store',
  });
  if (!response.ok) throw new Error(`加载面板数据失败（HTTP ${response.status}）`);
  return response.json();
}

async function requestSave(payload) {
  const response = await fetch('/api/config', {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json', 'X-AutoFilm-Request': '1' },
    body: JSON.stringify(payload),
  });
  const data = await response.json().catch(() => ({}));
  return { response, data };
}

async function requestSaveWeb(payload) {
  const response = await fetch('/api/web', {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json', 'X-AutoFilm-Request': '1' },
    body: JSON.stringify(payload),
  });
  const data = await response.json().catch(() => ({}));
  return { response, data };
}

async function requestReload() {
  const response = await fetch('/api/reload', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-AutoFilm-Request': '1' },
    body: '{}',
  });
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
  clearAlert();
  byId('app-version').textContent = state.app.version ? `v${state.app.version}` : '-';
  renderNav();
  render();
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
  const button = element('button', `nav-item${state.active === key ? ' is-active' : ''}`, label);
  button.type = 'button';
  button.addEventListener('click', () => {
    state.active = key;
    clearAlert();
    renderNav();
    render();
  });
  return button;
}

function currentSection() {
  return allSections().find((section) => section.key === state.active) || null;
}

function updateChrome() {
  const section = currentSection();
  byId('section-title').textContent = section ? section.title : '总览';
  byId('section-subtitle').textContent = section ? section.subtitle || '' : '运行状态、配置概览与校验规则';
  byId('dirty-pill').classList.toggle('is-hidden', !state.dirty);

  const parts = [];
  parts.push(`业务配置 v${state.version}`);
  parts.push(`面板配置 v${state.webVersion}`);
  if (state.runtime.scheduled_count != null) parts.push(`调度任务 ${state.runtime.scheduled_count}`);
  if (state.runtime.last_reload_at) {
    parts.push(`最近重载 ${state.runtime.last_reload_at}（${state.runtime.last_reload_source || '未知来源'}）`);
  }
  if (state.runtime.last_error) parts.push(`最近错误：${state.runtime.last_error}`);
  byId('runtime-line').textContent = parts.join(' · ');
}

function render() {
  if (!state.schema) return;
  const content = byId('content');
  content.textContent = '';

  const section = currentSection();
  if (!section) {
    renderOverview(content);
  } else {
    renderSection(content, section);
  }
  updateChrome();
}

/* ---------------- 渲染：总览 ---------------- */

function card(title, subtitle) {
  const wrapper = element('section', 'card');
  const head = element('div', 'card-head');
  const titles = element('div');
  titles.appendChild(element('h2', null, title));
  if (subtitle) titles.appendChild(element('p', 'card-sub', subtitle));
  head.appendChild(titles);
  wrapper.appendChild(head);
  const body = element('div', 'card-body');
  wrapper.appendChild(body);
  return { wrapper, head, body };
}

function statCard(label, value, hint) {
  const cardNode = element('div', 'stat');
  cardNode.appendChild(element('span', 'stat-label', label));
  cardNode.appendChild(element('span', 'stat-value', value));
  if (hint) cardNode.appendChild(element('span', 'stat-hint', hint));
  return cardNode;
}

function infoRow(label, value) {
  const row = element('div', 'field');
  row.appendChild(element('span', 'field-label', label));
  row.appendChild(element('span', null, value == null || value === '' ? '—' : String(value)));
  return row;
}

function renderOverview(container) {
  const stats = element('div', 'grid-cards');
  stats.appendChild(statCard('业务配置版本', `v${state.version}`, '每次成功应用后自增'));
  stats.appendChild(statCard('面板配置版本', `v${state.webVersion}`, '保存在独立的 web.yaml'));
  stats.appendChild(statCard('调度任务', String(state.runtime.scheduled_count ?? 0), '当前已注册的定时任务数量'));
  stats.appendChild(statCard('累计热重载', String(state.runtime.reload_count ?? 0), '本次进程生命周期内'));
  stats.appendChild(
    statCard('最近热重载', state.runtime.last_reload_at || '—', state.runtime.last_reload_source || '尚未触发热重载'),
  );
  container.appendChild(stats);

  const info = card('运行信息', '面板与进程的当前状态');
  const infoGrid = element('div', 'item-body');
  infoGrid.appendChild(infoRow('业务配置文件', state.app.config_path));
  infoGrid.appendChild(infoRow('面板配置文件', state.app.web_config_path));
  infoGrid.appendChild(infoRow('应用时区', state.app.timezone));
  infoGrid.appendChild(infoRow('面板监听', state.app.listen));
  infoGrid.appendChild(infoRow('访问认证', state.app.auth_enabled ? 'HTTP Basic 已启用' : '未启用（仅限本机访问）'));
  infoGrid.appendChild(infoRow('配置文件监听', state.app.watch_file ? '已启用（2 秒轮询）' : '未启用'));
  infoGrid.appendChild(infoRow('启动时间', state.app.started_at));
  if (state.runtime.last_error) infoGrid.appendChild(infoRow('最近错误', state.runtime.last_error));
  info.body.appendChild(infoGrid);
  container.appendChild(info.wrapper);

  const links = card('配置分区', '点击进入对应的配置分组');
  const chips = element('div', 'section-links');
  for (const section of allSections()) {
    const value = sectionData(section);
    const count = section.single ? 1 : Array.isArray(value) ? value.length : 0;
    const chip = element('button', 'chip');
    chip.type = 'button';
    chip.appendChild(element('span', null, section.title));
    chip.appendChild(element('strong', null, section.single ? '设置' : `${count} 项`));
    chip.addEventListener('click', () => {
      state.active = section.key;
      renderNav();
      render();
    });
    chips.appendChild(chip);
  }
  links.body.appendChild(chips);
  container.appendChild(links.wrapper);

  const rules = card('校验与热重载规则', '保存前会完整校验，任何一处不通过都不会写入磁盘');
  const list = element('ul', 'rule-list');
  const items = [
    '标识 ID 必填且在各分区内唯一；任务引用的 AList 客户端 / 媒体服务器必须存在。',
    'Cron 表达式需为 6 段（秒 分 时 日 月 周）或 7 段；留空表示只手工触发，不参与调度。',
    '数值范围：并发数 1-1024、下载并发 1-256、主题色强度 0-1、超时与阈值必须大于 0。',
    '路径形态：AList 扫描目录与挂载路径必须以 / 开头；媒体库海报任务需至少配置一个媒体库。',
    '面板安全：监听非回环地址时必须设置密码；设置密码时必须填写用户名。',
    '配置分离：业务配置（config.yaml）与面板配置（web.yaml）各自独立保存、各自持有版本号，任何一份为空都能正常启动。',
    '保存流程：校验通过 → 原子写盘（原文件备份为 .bak）→ 重建客户端与全部 cron 任务 → 停用旧调度器；面板配置只写 web.yaml，不触碰调度器。',
    '生效范围：新增 / 修改 / 删除的任务立即按新配置生效；正在执行的任务不会被打断，其运行上下文仍是启动时的配置快照。',
    '不热更新：面板监听地址与认证信息保存后需重启进程生效，保存成功时会给出提示。',
    '外部改动：直接编辑 config.yaml 或 web.yaml 会在 2 秒内自动重载（只读取、不回写，注释与排版保持原样）；解析或校验失败时保留当前运行配置并在上方显示最近错误。',
  ];
  for (const item of items) list.appendChild(element('li', null, item));
  rules.body.appendChild(list);
  container.appendChild(rules.wrapper);
}

/* ---------------- 渲染：分区与条目 ---------------- */

function renderSection(container, section) {
  if (section.single) {
    container.appendChild(renderItem(section, sectionData(section), -1));
    return;
  }

  const items = Array.isArray(state.config[section.key]) ? state.config[section.key] : [];
  const head = element('div', 'card-head');
  const titles = element('div');
  titles.appendChild(element('h2', null, `${items.length} 个${section.item_label}`));
  if (section.subtitle) titles.appendChild(element('p', 'card-sub', section.subtitle));
  head.appendChild(titles);

  if (section.addable) {
    const actions = element('div', 'card-head-actions');
    const add = element('button', 'btn btn-primary btn-small', `新增${section.item_label}`);
    add.type = 'button';
    add.addEventListener('click', () => {
      const next = deepClone(section.template || {});
      state.config[section.key] = items.concat([next]);
      markDirty();
      render();
    });
    actions.appendChild(add);
    head.appendChild(actions);
  }

  const wrapper = element('section', 'card');
  wrapper.appendChild(head);

  const body = element('div', 'card-body');
  if (!items.length) {
    body.appendChild(element('p', 'empty', `还没有${section.item_label}，点击右上角「新增${section.item_label}」开始配置`));
  } else {
    const stack = element('div', 'content');
    items.forEach((item, index) => {
      stack.appendChild(renderItem(section, item, index));
    });
    body.appendChild(stack);
  }
  wrapper.appendChild(body);
  container.appendChild(wrapper);
}

function renderItem(section, item, index) {
  const wrapper = element('div', 'item-card');
  const head = element('div', 'item-head');

  const title = element('span', 'item-title');
  if (section.single) {
    title.textContent = section.item_label;
  } else {
    const id = item && item.id ? String(item.id) : '';
    title.textContent = id || `未命名${section.item_label}`;
    head.appendChild(element('span', 'item-tag', `#${index + 1}`));
  }
  head.appendChild(title);
  head.appendChild(element('span', 'spacer'));

  if (section.restart_required) {
    head.appendChild(element('span', 'pill pill-muted', '重启生效'));
  }

  if (section.removable && !section.single) {
    const remove = element('button', 'btn btn-danger btn-small', '删除');
    remove.type = 'button';
    remove.addEventListener('click', () => {
      const label = item && item.id ? item.id : `第 ${index + 1} 个${section.item_label}`;
      if (!window.confirm(`确认删除「${label}」？保存并应用后生效。`)) return;
      const items = state.config[section.key] || [];
      items.splice(index, 1);
      markDirty();
      render();
    });
    head.appendChild(remove);
  }

  wrapper.appendChild(head);

  const body = element('div', 'item-body');
  for (const field of section.fields || []) {
    const node = renderField(section, item, index, field);
    if (node) body.appendChild(node);
  }
  wrapper.appendChild(body);
  return wrapper;
}

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
    const remove = element('button', 'btn btn-danger btn-small', '移除');
    remove.type = 'button';
    remove.addEventListener('click', () => {
      item[field.key].splice(rowIndex, 1);
      markDirty();
      render();
    });
    actionCell.appendChild(remove);
    tr.appendChild(actionCell);
    tbody.appendChild(tr);
  });
  table.appendChild(tbody);
  wrapper.appendChild(table);

  const actions = element('div', 'lib-actions');
  const add = element('button', 'btn btn-ghost btn-small', '新增媒体库');
  add.type = 'button';
  add.addEventListener('click', () => {
    item[field.key].push({ name: '', title: '', subtitle: '', sort: 'date_last_content_added' });
    markDirty();
    render();
  });
  actions.appendChild(add);
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

function applyErrors(errors) {
  clearFieldErrors();
  let first = null;
  for (const error of errors) {
    const container = document.querySelector(`[data-field="${error.field}"]`);
    if (!container) continue;
    container.classList.add('is-invalid');
    const card = container.closest('.item-card');
    if (card) card.classList.add('is-invalid');
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
    toast('error', `${label}已被其他会话修改，请点击「重新加载文件」`);
    return;
  }

  if (Array.isArray(data.errors) && data.errors.length) {
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
  window.addEventListener('beforeunload', (event) => {
    if (!state.dirty) return;
    event.preventDefault();
    event.returnValue = '';
  });
}

async function boot() {
  bindChrome();
  try {
    applyState(await requestState());
  } catch (err) {
    showAlert('error', err.message);
  }
}

boot();
