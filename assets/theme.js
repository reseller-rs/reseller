/* Applies the saved theme before the first paint, so the page never flashes.
   Plain script, loaded synchronously in <head> before the stylesheets.
   Choice: localStorage "theme" = "system" | "light" | "dark". Without a saved
   choice, the operator default from <meta name="color-scheme"> applies when it is
   exactly "light" or "dark", otherwise the page follows the OS.
   Result: html[data-theme-mode] holds the choice and html[data-theme] the palette
   in use ("light" or "dark"). app.js shows the menu and saves new choices. */
(function () {
  var root = document.documentElement;
  var mode = 'system';
  var saved = null;
  try { saved = localStorage.getItem('theme'); } catch (e) { saved = null; }
  if (saved === 'light' || saved === 'dark' || saved === 'system') {
    mode = saved;
  } else {
    var meta = document.querySelector('meta[name="color-scheme"]');
    var fallback = meta && meta.getAttribute('content');
    if (fallback === 'light' || fallback === 'dark') mode = fallback;
  }
  var query = window.matchMedia ? matchMedia('(prefers-color-scheme: light)') : null;
  function apply() {
    var current = root.dataset.themeMode || mode;
    var resolved = current === 'system' ? (query && query.matches ? 'light' : 'dark') : current;
    root.dataset.themeMode = current;
    root.dataset.theme = resolved;
    root.style.colorScheme = resolved;
  }
  root.dataset.themeMode = mode;
  apply();
  if (query && query.addEventListener) query.addEventListener('change', function () {
    if (root.dataset.themeMode === 'system') apply();
  });
})();
