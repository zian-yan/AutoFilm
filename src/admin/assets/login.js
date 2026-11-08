'use strict';

/**
 * AutoFilm 登录页脚本。
 *
 * 只做三件事：按面板保存的偏好初始化主题、提交凭据换会话 Cookie、把失败原因
 * 显示出来。会话 Cookie 由后端以 HttpOnly 下发，这里不接触 token。
 */

const THEME_KEY = 'autofilm-theme';

const byId = (id) => document.getElementById(id);

function applyTheme(theme) {
  document.documentElement.dataset.theme = theme === 'light' ? 'light' : 'dark';
}

function loadTheme() {
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
  if (saved === 'light' || saved === 'dark') {
    applyTheme(saved);
    return;
  }

  const light = window.matchMedia && window.matchMedia('(prefers-color-scheme: light)').matches;
  applyTheme(light ? 'light' : 'dark');
}

function showError(message) {
  const node = byId('login-error');
  node.textContent = message;
  node.classList.remove('is-hidden');
}

function clearError() {
  const node = byId('login-error');
  node.textContent = '';
  node.classList.add('is-hidden');
}

async function submit(event) {
  event.preventDefault();
  clearError();

  const username = byId('username').value.trim();
  const password = byId('password').value;
  const submitButton = byId('submit');

  if (!username || !password) {
    showError('请输入用户名和密码');
    return;
  }

  submitButton.disabled = true;
  submitButton.textContent = '登录中…';

  try {
    const response = await fetch('/api/login', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'X-AutoFilm-Request': '1' },
      body: JSON.stringify({ username, password }),
    });

    if (response.ok) {
      // 会话 Cookie 已由后端写入，直接进入控制台。
      window.location.replace('/');
      return;
    }

    const data = await response.json().catch(() => ({}));
    showError(data.message || `登录失败（HTTP ${response.status}）`);
    byId('password').value = '';
    byId('password').focus();
  } catch (err) {
    showError('无法连接面板服务，请稍后重试');
  } finally {
    submitButton.disabled = false;
    submitButton.textContent = '登录';
  }
}

function boot() {
  loadTheme();
  const form = byId('login-form');
  form.addEventListener('submit', submit);
  byId('username').focus();
}

boot();
