'use strict';

/**
 * 主题预判定（Anti-FOUC）。
 *
 * 必须在 `<head>` 里以同步脚本引入，且早于样式表：浏览器解析到这里就确定
 * `data-theme`，首帧绘制直接用正确的配色，不会出现"先画深色、再跳浅色"的
 * 白/黑闪。面板与登录页共用同一份文件（CSP 不允许内联脚本，所以独立成文件）。
 *
 * 判定顺序与 app.js / login.js 里的 loadTheme 严格一致：
 *   1. 地址栏 ?theme=light|dark 强制值（便于投屏与嵌入）
 *   2. localStorage 里用户上次的选择
 *   3. 系统 prefers-color-scheme（无匹配时默认深色）
 * 两处规则若不一致，会在多脚本执行后产生第二次切换，等于没有防闪。
 */

(function () {
  var KEY = 'autofilm-theme';

  var forced = null;
  try {
    forced = new URLSearchParams(window.location.search).get('theme');
  } catch (err) {
    forced = null;
  }

  var theme = forced === 'light' || forced === 'dark' ? forced : null;

  if (!theme) {
    var saved = null;
    try {
      saved = window.localStorage.getItem(KEY);
    } catch (err) {
      saved = null;
    }
    if (saved === 'light' || saved === 'dark') theme = saved;
  }

  if (!theme) {
    theme =
      window.matchMedia && window.matchMedia('(prefers-color-scheme: light)').matches
        ? 'light'
        : 'dark';
  }

  document.documentElement.dataset.theme = theme;
})();
