// Тема оформления — общий загрузчик для обоих окон (index.html, ask.html).
// Подключается ПЕРВЫМ, до i18n.js и main.js/ask.js: конфиг читается один раз
// здесь, а не отдельным invoke("get_config") из каждого файла.
//
// Крючок на стороне CSS уже готов (ui/tokens.css):
//   data-theme="light" | "dark" на <html> — ручной выбор, отключает
//   `@media (prefers-color-scheme)`;
//   атрибут снят вовсе — «как в системе», решает тот самый медиазапрос.
(function () {
  function применить(тема) {
    if (тема === "light" || тема === "dark") {
      document.documentElement.setAttribute("data-theme", тема);
    } else {
      // "system", отсутствие значения и всё прочее — один и тот же путь:
      // атрибут снимается совсем, чтобы решал prefers-color-scheme, а не
      // повисал в невалидном значении, которое CSS-селектор не ловит.
      document.documentElement.removeAttribute("data-theme");
    }
  }

  async function загрузить() {
    try {
      // window.__TAURI__ выставлен синхронно (withGlobalTauri: true), но
      // код должен пережить и открытие файла в обычном браузере при вёрстке —
      // тогда конфига нет, и это не беда: остаёмся на системной теме.
      const конфиг = await window.__TAURI__?.core?.invoke("get_config");
      применить(конфиг?.theme ?? null);
    } catch {
      // Конфиг не прочитался — системная тема и так стоит по умолчанию.
    }
  }

  window.theme = { apply: применить, ready: загрузить() };
})();
