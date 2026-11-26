// Подставной Tauri для стенда состояний.
//
// Стенд открывает НАСТОЯЩИЕ ui/index.html и ui/main.js — здесь подменён только
// тот слой, который в приложении отвечает Rust: команды `invoke` и события
// `listen`. Поэтому на стенде видно ровно то, что увидит человек в окне, а не
// его нарисованное подобие: разъедется вёрстка — разъедется и здесь.
//
// Чего стенд НЕ проверяет: саму машину записи, права macOS, поведение трея и
// то, что бэкенд действительно шлёт эти события в этом порядке.
//
// Сцена выбирается параметром `?s=`; список — в serve.py.
(function () {
  const п = new URLSearchParams(location.search);
  const ВСПЛЫВАШКА = location.pathname.endsWith("ask.html");
  const S = п.get("s") || "idle";
  // Язык сцены задаётся `?lang=`, а не системой: у README две версии, и для
  // английской нужны английские кадры. Без параметра — русский, как раньше.
  const ЯЗЫК = п.get("lang") || "ru";
  // Тема — `?theme=light|dark`; без параметра «как в системе» (в headless —
  // светлая). Обе темы проверяются перед показом.
  const ТЕМА = п.get("theme") || "system";

  /// Самое длинное имя устройства, какое встречалось живьём. Стоит здесь,
  /// потому что канон требует проверять раскладку именно на нём.
  const ДЛИННЫЙ_МИК =
    "Microphone Array (Intel(R) Smart Sound Technology (Intel(R) SST))";
  const ДЛИННОЕ_ИМЯ =
    "Созвон с командой про редизайн интерфейса и планы на следующий квартал — часть вторая";

  const слушатели = new Map();
  const emit = (имя, payload) =>
    (слушатели.get(имя) || []).forEach((cb) => cb({ payload }));
  const пауза = (мс, v) => new Promise((r) => setTimeout(() => r(v), мс));

  // ── Подставной список записей ───────────────────────────────────────────
  const сутки = 86400000;
  const t = (сдвиг, час, мин) => {
    const d = new Date(Date.now() - сдвиг * сутки);
    d.setHours(час, мин, 0, 0);
    return d;
  };
  const имя_файла = (d, хвост) => {
    const p = (n) => String(n).padStart(2, "0");
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}_${p(d.getHours())}-${p(d.getMinutes())}_${хвост}`;
  };
  const запись = (d, хвост, доп = {}) => ({
    name: имя_файла(d, хвост),
    folder: имя_файла(d, "x").slice(0, 7),
    mic: true,
    system: true,
    size: 48_500_000,
    transcript: false,
    duration_sec: 2412,
    ...доп,
  });

  const СПИСОК = [
    запись(t(0, 10, 0), "zoom-us"),
    // Переименованная запись — название встречи вместо имени звонилки.
    запись(t(0, 14, 30), "Синк команды MeetRec", { duration_sec: 1830, size: 31_200_000 }),
    запись(t(1, 11, 15), "Интервью с пользователем Ольгой", { duration_sec: 640, size: 12_800_000, mic_muted: true }),
    запись(t(1, 16, 5), "discord", { duration_sec: 3720, size: 74_000_000 }),
    // Без системной дорожки — у неё карточка «Собеседников не слышно».
    запись(t(12, 9, 0), "unknown", { duration_sec: 55, size: 1_100_000, system: false }),
    // Тихий микрофон — карточка «Ваш голос почти не записался».
    запись(t(12, 18, 40), "manual", { duration_sec: 900, size: 18_000_000, quiet_mic: true }),
  ];
  if (S === "long" || S === "rename") {
    СПИСОК[0].name = имя_файла(t(0, 10, 0), "x").slice(0, 17) + ДЛИННОЕ_ИМЯ;
  }
  if (S === "done" || S === "menudone" || S === "menu-copy-path" || S.startsWith("tr-")) {
    СПИСОК[0].transcript = true;
  }
  // Вторая запись — расшифровка есть, но дорожка владельца не вышла: пункт
  // «Расшифровать заново» появляется только у неё, а не у первой записи
  // (полная расшифровка, см. `menu-copy-path` выше) — так стенд проверяет
  // оба состояния сразу.
  if (S === "menu-partial") {
    СПИСОК[1].transcript = true;
    СПИСОК[1].transcript_partial = true;
  }

  // ── Расшифровка для панели чтения (transcript_path/read_transcript) ─────
  // ~40 реплик — достаточно длинная запись, чтобы увидеть и прокрутку
  // .tr-body, и вертикальный ритм между отдельными репликами (панель их не
  // группирует — каждая реплика своя, даже подряд у одного говорящего), и
  // перенос заголовка в две строки (см. ДЛИННОЕ_ИМЯ выше).
  const РЕПЛИКИ = [
    "Слышно нормально, погнали.",
    "Да, у меня то же самое, звук чистый.",
    "Тогда идём по пунктам сверху вниз, как договаривались.",
    "Первое — экран разрешений. Там до сих пор путают запись экрана со звуком.",
    "Да, я видел жалобы в фидбэке. Человек пугается системного окна.",
    "Значит, перед системным диалогом нужен наш экран с объяснением.",
    "Согласен. Текстом и одной кнопкой «Продолжить», без картинки диалога.",
    "Картинку я бы тоже не рисовала — она устареет при следующем macOS.",
    "Второе — всплывашка «Записать встречу?». Половина принимает её за чужую.",
    "Может, показать её один раз в онбординге на тестовой встрече?",
    "Мне нравится. Прямо живую, не картинку: нажал — увидел, как идёт запись.",
    "Заодно проверим микрофон — до кнопки «Проверить» в настройках почти никто не доходит.",
    "Третье — люди не находят файлы. Один спрашивал, можно ли расшифровку в чат.",
    "Вот это я и хотела обсудить: чтение расшифровки прямо в приложении.",
    "И кнопку «Скопировать путь» — чтобы можно было отдать агенту.",
    "Путь именно, не текст?",
    "И то и другое. Текст — для чата, путь — для агента, который сам читает файлы.",
    "Логично. В меню строки это будет отдельным пунктом?",
    "Да, вторым, сразу после «Открыть расшифровку».",
    "Хорошо, тогда по срокам — я успеваю к следующему релизу.",
    "Анимации потом, главное — чтобы экран разрешений был понятным.",
    "Ещё бы позвать тех же людей, когда будет готово, посмотреть, стало ли лучше.",
    "Да, я им напишу на следующей неделе.",
    "Ладно, вроде всё обсудили. Скинешь потом заметки?",
    "Скину. Спасибо, до связи!",
  ];
  function расшифровка_демо() {
    const блоки = [];
    let секунда = 4;
    for (let i = 0; i < 40; i++) {
      const владелец = i % 7 === 3 || i % 7 === 4;
      const метка = владелец ? "Владелец" : "Собеседники";
      const текст = РЕПЛИКИ[i % РЕПЛИКИ.length];
      const мин = Math.floor(секунда / 60);
      const сек = секунда % 60;
      const ts = `${String(мин).padStart(2, "0")}:${String(сек).padStart(2, "0")}`;
      блоки.push(`**[${ts}]** _${метка}_ ${текст}`);
      секунда += 14 + (i % 5) * 6;
    }
    return блоки.join("\n\n");
  }
  const РАСШИФРОВКА_ДЕМО = расшифровка_демо();

  // ── Календарь: зона «Дальше» ────────────────────────────────────────
  // Встречи считаются от «сейчас», чтобы отсчёт на стенде был живым:
  // первая через 4 минуты (есть «Зайти и записать»), вторая через полтора
  // часа, третья вечером.
  const мин = 60000;
  const сейчас = Date.now();
  const встреча = (id, title, через_мин, длится_мин, source) => ({
    id,
    title,
    start_ms: сейчас + через_мин * мин,
    end_ms: сейчас + (через_мин + длится_мин) * мин,
    source,
    has_link: true,
  });
  const ВСТРЕЧИ =
    S === "calempty"
      ? []
      : S === "scr-menu-one"
        ? [встреча("a", "Дизайн-ревью онбординга", 4, 45, "meet")]
      : S === "calnow"
        ? [
            встреча("now", "Дизайн-ревью онбординга", -12, 45, "meet"),
            встреча("b", "Созвон с Мишей по релизу 0.3", 94, 30, "zoom"),
          ]
        : [
            встреча("a", "Дизайн-ревью онбординга", 4, 45, "meet"),
            встреча("b", "Созвон с Мишей по релизу 0.3", 94, 30, "zoom"),
            встреча("c", "Еженедельная встреча команды дизайна и разработки по мобильному приложению", 214, 60, "teams"),
          ];
  // Состояние календаря по сцене: и для зоны «Дальше», и для карточки в
  // настройках (сцены `set-cal*`).
  const АВТОРИЗАЦИЯ = {
    nocal: "disconnected",
    "set-nocal": "disconnected",
    "set-calwait": "waiting",
    "set-calrefused": "refused",
    calrevoked: "revoked",
    "set-calrevoked": "revoked",
  };
  const авторизация = АВТОРИЗАЦИЯ[S] ?? "connected";
  const календарь = {
    available: S !== "nocalbuild" && S !== "set-nocalbuild",
    auth: авторизация,
    email: авторизация === "connected" || авторизация === "revoked" ? "name@example.com" : null,
    next: авторизация === "connected" && ВСТРЕЧИ.length ? { title: ВСТРЕЧИ[0].title, start_ms: ВСТРЕЧИ[0].start_ms } : null,
    today: авторизация === "connected" ? ВСТРЕЧИ : [],
    offline: S === "caloffline" || S === "set-caloffline",
    loaded_at_ms: сейчас - 3 * мин,
  };

  // ── Запись экрана: окна, мониторы, выбор ─────────────────────────────
  const ОКНА = [
    { id: 4101, appId: "us.zoom.xos", appName: "zoom.us", title: "Zoom Meeting", displayId: 1, вид: "zoom", w: 16, h: 10 },
    { id: 4102, appId: "com.google.Chrome", appName: "Google Chrome", title: "Meet — Синк по MeetRec", displayId: 2, вид: "meet", w: 16, h: 10 },
    { id: 4103, appId: "ru.keepcoder.Telegram", appName: "Telegram", title: "Telegram", displayId: 1, вид: "tg", w: 10, h: 12 },
    { id: 4104, appId: "com.apple.finder", appName: "Finder", title: "Загрузки", displayId: 1, вид: "finder", w: 16, h: 11 },
    { id: 4105, appId: "com.apple.Keynote", appName: "Keynote", title: "Итоги квартала.key", displayId: 2, вид: "keynote", w: 16, h: 10 },
    { id: 4106, appId: "com.figma.Desktop", appName: "Figma", title: "MeetRec — экраны", displayId: 2, вид: "figma", w: 16, h: 10 },
    // Свёрнутое окно: снимка нет — в меню плейсхолдер со значком приложения.
    { id: 4107, appId: "com.google.Chrome", appName: "Google Chrome", title: "Входящие — Gmail", displayId: 1, вид: "meet", w: 16, h: 10, свёрнуто: true },
  ];
  const МОНИТОРЫ = [
    { id: 1, name: "Встроенный дисплей", main: true },
    { id: 2, name: "L27e-30", main: false },
  ];
  const НЕТ_ДОСТУПА = S === "scr-noperm" || S === "set-screen-noperm";
  const экран_сохранено = (() => {
    const з = { hideSelf: true, displayId: null, displayName: null, windowId: null, appId: null, appName: null, windowTitle: null };
    const окно = (о) => ({ windowId: о.id, appId: о.appId, appName: о.appName, windowTitle: о.title });
    if (["scr-on", "set-screen-window", "set-screen-menu", "recscreen", "recscreen-failed", "recmicmenu"].includes(S))
      return { ...з, mode: "window", ...окно(ОКНА[0]) };
    if (S === "recscrmenu") return { ...з, mode: "window", ...окно(ОКНА[1]) };
    if (["scr-menu-win", "scr-menu-one", "scr-noperm"].includes(S)) return { ...з, mode: "meeting" };
    if (["scr-menu-disp", "set-screen", "recscreen-disp", "set-screen-win", "set-screen-noperm"].includes(S))
      return { ...з, mode: "screen", displayId: 2, displayName: "L27e-30" };
    return { ...з, mode: "off" };
  })();
  const экран_настройки = () => ({
    ...экран_сохранено,
    support: {
      window: !["set-screen-win", "set-screen-old"].includes(S),
      screen: S !== "set-screen-old",
      reason: S === "set-screen-old" ? "нужна macOS 15 или новее (у вас 14.6)" : null,
    },
    permission: !НЕТ_ДОСТУПА,
  });
  const экран_статус = () =>
    S === "recscreen" || S === "recmicmenu" ? { state: "on", kind: "window", app: "zoom.us", title: "Zoom Meeting" }
    : S === "recscrmenu" ? { state: "on", kind: "window", app: "Google Chrome", title: ОКНА[1].title }
    : S === "recscreen-disp" ? { state: "on", kind: "screen", display: "L27e-30" }
    : S === "recscreen-failed"
      ? { state: "failed", kind: "window", app: "zoom.us", title: "Zoom Meeting", why: "no_window", detail: "SCShareableContent: window 4101 not found" }
    : null;

  // Миниатюры рисуются холстом — небольшие JPEG шириной 240, как у бэкенда.
  const холст = (w, h, рисовать, тип = "image/jpeg") => {
    const c = document.createElement("canvas");
    c.width = w;
    c.height = h;
    const x = c.getContext("2d");
    рисовать(x, w, h);
    return c.toDataURL(тип, 0.8);
  };
  const прям = (x, цвет, l, t, w, h, r = 2) => {
    x.fillStyle = цвет;
    x.beginPath();
    x.roundRect(l, t, w, h, r);
    x.fill();
  };
  const снимок_окна = (о) =>
    холст(240, Math.round((240 * о.h) / о.w), (x, w, h) => {
      if (о.вид === "zoom") {
        прям(x, "#1f1f23", 0, 0, w, h, 0);
        const g = 6, cw = (w - g * 3) / 2, ch = (h - 28 - g * 3) / 2;
        for (let i = 0; i < 4; i++)
          прям(x, ["#3a3a42", "#45454f", "#3a3a42", "#4b4b55"][i], g + (i % 2) * (cw + g), g + Math.floor(i / 2) * (ch + g), cw, ch, 4);
        x.strokeStyle = "#34c759"; x.lineWidth = 2; x.strokeRect(g + 1, g + 1, cw - 2, ch - 2);
        прям(x, "#2a2a30", 0, h - 22, w, 22, 0);
        прям(x, "#e5342a", w / 2 - 14, h - 17, 28, 12, 6);
      } else if (о.вид === "meet") {
        прям(x, "#ffffff", 0, 0, w, h, 0);
        прям(x, "#e8eaed", 0, 0, w, 20, 0);
        прям(x, "#ffffff", 40, 5, w - 80, 10, 5);
        прям(x, "#202124", 16, 30, w - 32, h - 44, 3);
        const cw = (w - 32 - 12) / 2, ch = (h - 44 - 12) / 2;
        for (let i = 0; i < 4; i++) прям(x, "#3c4043", 20 + (i % 2) * (cw + 4), 34 + Math.floor(i / 2) * (ch + 4), cw, ch, 2);
      } else if (о.вид === "tg") {
        прям(x, "#ffffff", 0, 0, w, h, 0);
        прям(x, "#f1f3f6", 0, 0, w * 0.34, h, 0);
        for (let i = 0; i < 6; i++) прям(x, "#dfe3e8", 8, 12 + i * 34, w * 0.34 - 16, 24, 4);
        for (let i = 0; i < 5; i++) прям(x, i % 2 ? "#d6f0c7" : "#e6f2fb", i % 2 ? w * 0.55 : w * 0.38, 20 + i * 44, w * 0.38, 26, 6);
      } else if (о.вид === "finder") {
        прям(x, "#ffffff", 0, 0, w, h, 0);
        прям(x, "#eceff3", 0, 0, w * 0.26, h, 0);
        for (let i = 0; i < 9; i++) прям(x, "#8fb8f0", w * 0.32 + (i % 3) * 52, 22 + Math.floor(i / 3) * 46, 40, 32, 3);
      } else if (о.вид === "keynote") {
        прям(x, "#d9dce3", 0, 0, w, h, 0);
        прям(x, "#ffffff", 14, 16, w * 0.68, h - 32, 1);
        прям(x, "#1d1d1f", 30, 38, w * 0.4, 12, 1);
        прям(x, "#c7c7cc", 30, 60, w * 0.28, 7, 1);
        прям(x, "#ececf0", w - 52, 16, 38, h - 32, 1);
      } else {
        прям(x, "#2c2c2c", 0, 0, w, h, 0);
        прям(x, "#383838", 0, 0, 44, h, 0);
        прям(x, "#ffffff", 60, 24, 70, 100, 4);
        прям(x, "#a259ff", 140, 24, 70, 46, 4);
        прям(x, "#0acf83", 140, 78, 70, 46, 4);
      }
    });
  // Значок приложения 32×32, как отдаёт бэкенд. У Finder и Keynote его
  // нет (null) — в меню там нейтральный значок окна.
  const ЦВЕТ_ЗНАЧКА = { "us.zoom.xos": "#0b5cff", "com.google.Chrome": "#1a73e8", "ru.keepcoder.Telegram": "#2aabee", "com.figma.Desktop": "#a259ff" };
  const значки = {};
  const значок = (о) => {
    const цвет = ЦВЕТ_ЗНАЧКА[о.appId];
    if (!цвет) return null;
    return (значки[о.appId] ??= холст(32, 32, (x) => {
      прям(x, цвет, 0, 0, 32, 32, 8);
      x.fillStyle = "#fff";
      x.font = "bold 18px -apple-system, system-ui, sans-serif";
      x.textAlign = "center";
      x.textBaseline = "middle";
      x.fillText(о.appName[0].toUpperCase(), 16, 17);
    }, "image/png"));
  };
  const снимок_монитора = (д) =>
    холст(240, 150, (x, w, h) => {
      прям(x, д.main ? "#6b7a8f" : "#445063", 0, 0, w, h, 0);
      прям(x, "rgba(255,255,255,.35)", 0, 0, w, 8, 0);
      if (д.main) {
        прям(x, "#1e1e1e", 18, 22, 132, 92, 4);
        прям(x, "#ffffff", 156, 40, 70, 76, 4);
      } else {
        прям(x, "#ffffff", 24, 20, 168, 108, 4);
        прям(x, "#e8eaed", 24, 20, 168, 14, 4);
      }
    });

  // ── Снимок состояния, который отдаёт get_state ──────────────────────────
  // Сцены пульта, которые начинают запись кнопкой при открытом окне — тогда
  // виден таймер (снимок «идёт запись» таймера не даёт, см. main.js).
  const ПУЛЬТ = ["recquiet", "recmuted", "recstopping", "recdevwarn", "recdeferred", "recnosys", "recscreen", "recscreen-disp", "recscreen-failed", "recmicmenu", "recscrmenu"];
  const снимок = {
    state:
      S === "recording" || S === "updaterec" ? "recording" : S === "armed" ? "armed" : "idle",
    fatal: S === "fatal" ? "детектор не поднялся: no such device" : null,
    device_warning: S === "devwarn" || S === "recdevwarn" ? "{0.0.1.00000000}.{9f1c…}" : null,
    mic_deferred: S === "deferred" || S === "recdeferred",
    no_system_audio: S === "nosys" || S === "recnosys" ? "process tap: разрешение не выдано (-3812)" : null,
  };

  // ── Обновления (update.rs) ──────────────────────────────────────────────
  // Баннер есть во всех сценах `update*`, кроме ручных проверок в настройках:
  // там важен ответ на кнопку, а не то, что уже нашла фоновая проверка.
  // `updatewin` — Windows: встроенной установки нет, кнопка ведёт на страницу.
  const РЕЛИЗ = { version: "0.3.0", url: "https://github.com/mmaximov97/meeting-recorder/releases/tag/v0.3.0" };
  const БАННЕР = ["update", "updating", "updateerror", "updaterec", "updatewin"].includes(S);
  const ОТВЕТ_ПРОВЕРКИ = {
    updatecheck: { current: "0.2.0", latest: РЕЛИЗ },
    uptodate: { current: "0.2.0", latest: null },
  };
  // Скачивание идёт по-настоящему медленно только там, где на него смотрят:
  // в `updating` оно замирает на 42 %, чтобы кадр был одинаковым.
  const установить = () => {
    if (S === "updateerror")
      return пауза(700).then(() =>
        Promise.reject({ key: "update.errReadOnly", detail: "/Volumes/MeetRec" }),
      );
    const шаги = S === "updating" ? [0, 17, 42] : [0, 23, 58, 91, 100];
    шаги.forEach((p, i) =>
      setTimeout(() => emit("update-progress", { stage: "downloading", percent: p }), 150 * i),
    );
    if (S === "updating") return пауза(1e9);
    const конец = 150 * шаги.length;
    setTimeout(() => emit("update-progress", { stage: "installing", percent: null }), конец);
    setTimeout(() => emit("update-progress", { stage: "restarting", percent: null }), конец + 600);
    return пауза(конец + 700, null);
  };

  let метроном = null;
  const метр = () => {
    const ш = (b) => Math.max(0, Math.min(1, b + (Math.random() - 0.5) * 0.12));
    // 0,28 линейного пика — это -11 дБFS, то есть обычная речь на здоровом
    // уровне записи. На метре она даёт ~80%: см. канон 2.9, почему подсказка
    // говорит «до конца зелёного», а не «до середины».
    // «Собеседников не слышно» — системная дорожка молчит.
    const тихо = S === "recquiet" || S === "recnosys";
    emit("levels", { mic: ш(0.28), system: тихо ? 0 : ш(0.19), monitoring: true });
  };

  const сохранено = {};
  const команды = {
    get_state: () => {
      // В приложении `ask` уходит раньше смены состояния — повторяем порядок,
      // иначе строка взвода не узнает, кого услышали.
      if (S === "armed") emit("ask", "zoom.us");
      return пауза(30, снимок);
    },
    // Сцена «первая загрузка» — это список, который не приехал никогда.
    list_recordings: () =>
      пауза(S === "loading" ? 1e9 : 260, S === "empty" ? [] : СПИСОК),
    list_mic_devices: () =>
      пауза(120, [
        { id: "id-1", name: ДЛИННЫЙ_МИК },
        { id: "id-2", name: "MacBook Pro Microphone" },
        { id: "id-3", name: "AirPods Pro" },
      ]),
    get_config: () =>
      пауза(20, {

        mic_device_id:
          S === "devwarn" ? "{0.0.1.00000000}.{9f1c…}" : S === "longmic" ? "id-1" : "id-2",
        mic_device_name:
          S === "devwarn" || S === "longmic" ? ДЛИННЫЙ_МИК : "MacBook Pro Microphone",
        stt_gateway_url: "http://localhost:8080",
        stt_api_key: "секрет",
        // Поля редизайна. Без них `theme.js` и `i18n.js` не находят, что
        // применить, и молча уходят в системные значения — в headless-браузере
        // это светлая тема и английский, то есть стенд показывает не то, что
        // увидит человек с русской системой и тёмной темой.
        theme: сохранено.theme ?? ТЕМА,
        language: ЯЗЫК,
        transcribe_mode: сохранено.transcribe_mode ?? (S.startsWith("set-local") ? "local" : "server"),
        transcribe_server: сохранено.transcribe_server ?? (S === "set-whisper" ? "whisper_cpp" : "gateway"),
        audio_retention_days: null,
      }),
    // Запись экрана — контракт из tmp/plans/screen-picker.md.
    // Сцены `scr-*` — кнопка и меню на главном, `recscreen*` — пульт,
    // `set-screen*` — строка в настройках. `*-noperm` — нет доступа macOS,
    // `set-screen-win` — Windows (без режима «окно»), `set-screen-old` —
    // macOS 14.
    get_screen_settings: () => пауза(10, экран_настройки()),
    set_screen_source: (а) => {
      Object.assign(экран_сохранено, {
        mode: а.mode,
        windowId: а.windowId ?? null,
        appId: а.appId ?? null,
        appName: а.appName ?? null,
        windowTitle: а.windowTitle ?? null,
        displayId: а.displayId ?? null,
        displayName: а.displayName ?? null,
      });
      // Во время записи выбор применяется на лету — бэкенд говорит событием.
      if (снимок.state === "recording")
        setTimeout(
          () =>
            emit(
              "screen",
              а.mode === "off"
                ? { state: "off" }
                : { state: "on", kind: а.mode, app: а.mode === "meeting" ? "zoom.us" : а.appName, title: а.windowTitle, display: а.displayName },
            ),
          200,
        );
      return пауза(30, null);
    },
    set_screen_hide_self: ({ hide }) => пауза(20, void (экран_сохранено.hideSelf = hide)),
    // Миниатюры снимаются ~400 мс — чтобы на стенде были видны плейсхолдеры.
    list_screen_sources: ({ thumbs } = {}) =>
      НЕТ_ДОСТУПА ? пауза(40).then(() => Promise.reject("permission")) : пауза(thumbs ? 400 : 60, {
        windows: ОКНА.map((о) => ({ ...о, icon: значок(о), thumb: thumbs && !о.свёрнуто ? снимок_окна(о) : null })),
        displays: МОНИТОРЫ.map((д) => ({ ...д, thumb: thumbs ? снимок_монитора(д) : null })),
        meetingApp: "zoom.us",
      }),
    get_screen_status: () => пауза(20, экран_статус()),
    request_screen_permission: () => пауза(40, !НЕТ_ДОСТУПА),
    open_screen_privacy: () => пауза(0, null),
    // Команды, добавленные редизайном. Шим отклоняет неизвестную команду, а
    // отказ в неудачном месте обрывает инициализацию — стенд тогда показывает
    // скелетон вместо списка. Поэтому покрываем весь вызываемый набор, даже
    // там, где настоящий бэкенд ничего не возвращает.
    delete_recording: () => пауза(20, null),
    calendar_status: () => пауза(40, календарь),
    // «Зайти и записать»: в приложении открывается ссылка и начинается запись.
    calendar_join: () =>
      пауза(200).then(() => {
        снимок.state = "recording";
        emit("state", "recording");
        метроном = setInterval(метр, 100);
      }),
    calendar_connect: () => {
      календарь.auth = "waiting";
      emit("calendar-status", { ...календарь });
      return пауза(1500).then(() => {
        Object.assign(календарь, { auth: "connected", email: "name@example.com", today: ВСТРЕЧИ });
        emit("calendar-status", { ...календарь });
      });
    },
    calendar_cancel_connect: () => пауза(0, null),
    calendar_disconnect: () =>
      пауза(80).then(() => {
        Object.assign(календарь, { auth: "disconnected", email: null, today: [] });
        emit("calendar-status", { ...календарь });
      }),
    open_url: () => пауза(0, null),
    // Помним выбор до перезагрузки сцены: окно перечитывает конфиг по фокусу.
    set_theme: ({ theme }) => пауза(0, void (сохранено.theme = theme)),
    set_language: () => пауза(0, null),
    set_audio_retention: () => пауза(0, null),
    set_transcribe_mode: ({ mode }) => пауза(0, void (сохранено.transcribe_mode = mode)),
    set_transcribe_server: ({ kind }) => пауза(0, void (сохранено.transcribe_server = kind)),
    // Локальная расшифровка: на стенде модель «на месте», чтобы был виден
    // рабочий вид экрана, а не вечное «скачайте модель».
    // Сцены `set-local-*` показывают остальные четыре состояния модели.
    local_model_status: () =>
      пауза(
        20,
        S === "set-local-missing"
          ? { state: "missing", size: 1_600_000_000 }
          : S === "set-local-downloading"
            ? { state: "downloading", done: 540_000_000, total: 1_600_000_000 }
            : S === "set-local-nospace"
              ? { state: "no_space", need: 1_600_000_000, free: 900_000_000 }
              : S === "set-local-failed"
                ? { state: "failed" }
                : { state: "ready", size: 1_600_000_000 },
      ),
    local_model_download: () => пауза(0, null),
    local_model_remove: () => пауза(0, null),

    set_monitor: ({ on }) => {
      clearInterval(метроном);
      if (on) метроном = setInterval(метр, 100);
      else emit("levels", { mic: 0, system: 0, monitoring: false });
      return пауза(80);
    },
    // Как и настоящий бэкенд, отвечает событием, а не результатом команды.
    set_mic_muted: ({ muted }) => {
      if (снимок.state === "recording") emit("mic-muted", (снимок.mic_muted = muted));
      return пауза(40);
    },
    send_event: ({ name }) => {
      // «Останавливаю…»: команда остановки ушла и ещё не вернулась.
      if (name === "toggle" && S === "recstopping" && снимок.state === "recording") return пауза(1e9);
      if (name === "toggle") {
        снимок.state = снимок.state === "recording" ? "idle" : "recording";
        emit("state", снимок.state);
        if (снимок.state === "recording") метроном = setInterval(метр, 100);
        else clearInterval(метроном);
      }
      // Во взводе «да» говорит кнопка записи (toggle), а не отдельная кнопка:
      // confirm остаётся ответом всплывашки.
      if (name === "confirm") emit("state", (снимок.state = "recording"));
      if (name === "decline") emit("state", (снимок.state = "idle"));
      return пауза(60);
    },
    transcribe_recording: ({ folder, base }) => {
      const к = { folder: folder ?? null, base };
      пауза(300).then(() => emit("transcribe-progress", { ...к, stage: "uploading" }));
      пауза(1600).then(() => emit("transcribe-progress", { ...к, stage: "polling" }));
      return пауза(60);
    },
    cancel_transcription: ({ folder, base }) =>
      пауза(80).then(() => emit("transcribe-cancelled", { folder: folder ?? null, base })),
    rename_recording: () => пауза(120),
    // Во время записи бэкенд переключает микрофон на лету и говорит об этом
    // событием `mic-switched`.
    set_mic_device: ({ name }) =>
      пауза(80).then(() => {
        if (снимок.state === "recording") setTimeout(() => emit("mic-switched", { name: name ?? null }), 150);
      }),
    set_transcribe_config: () => пауза(80),
    open_recording_folder: () => пауза(40),
    open_folder: () => пауза(40),
    open_privacy_settings: () => пауза(40),
    open_repository: () => пауза(40),

    // ── Панель расшифровки (redizайн этого стенда) ──────────────────────
    transcript_path: ({ folder, base }) =>
      пауза(20, `/Users/demo/Recordings/${folder ? folder + "/" : ""}${base}.transcript/${base}.md`),
    read_transcript: ({ base } = {}) => {
      if (S === "tr-loading") return пауза(1e9); // никогда не отвечает — вечный скелетон
      if (S === "tr-error") return Promise.reject("файл не найден");
      if (S === "tr-empty") return пауза(300, "");
      // Ровно то, что `run_transcription` дописывает в начало `.md` при
      // частичном отказе (main.rs) — стенд проверяет, что панель рисует эту
      // строку как обычный текстовый блок, а не ломает разбор остального.
      if (base === СПИСОК[1].name) {
        return пауза(
          300,
          `_Дорожка владельца не транскрибирована: таймаут запроса_\n\n${РАСШИФРОВКА_ДЕМО}`,
        );
      }
      return пауза(300, РАСШИФРОВКА_ДЕМО);
    },

    update_status: () => пауза(20, БАННЕР ? РЕЛИЗ : null),
    update_skip: () => пауза(20, null),
    update_in_app: () => пауза(0, S !== "updatewin"),
    update_check: () =>
      S === "checkerror"
        ? пауза(500).then(() =>
            Promise.reject({ key: "update.errCheck", detail: "GitHub ответил 403 Forbidden" }),
          )
        : пауза(500, ОТВЕТ_ПРОВЕРКИ[S] ?? { current: "0.2.0", latest: РЕЛИЗ }),
    update_install: установить,
  };

  window.__TAURI__ = {
    core: {
      invoke: (имя, арг = {}) =>
        команды[имя]
          ? Promise.resolve(команды[имя](арг))
          : Promise.reject(`на стенде нет команды ${имя}`),
    },
    event: {
      listen: (имя, cb) => {
        if (!слушатели.has(имя)) слушатели.set(имя, []);
        слушатели.get(имя).push(cb);
        // Всплывашка ничего не спрашивает у бэкенда — она только ждёт `ask`.
        // Главное окно получает то же событие из `get_state`, чтобы порядок
        // «сначала кто, потом состояние» совпадал с настоящим.
        // `?s=unmute` — тот же стенд, но вопрос «вы говорите?» из записи.
        if (имя === "ask" && ВСПЛЫВАШКА && S !== "unmute") setTimeout(() => emit("ask", "zoom.us"), 60);
        if (имя === "ask-unmute" && ВСПЛЫВАШКА && S === "unmute") setTimeout(() => emit("ask-unmute", null), 60);
        return Promise.resolve(() => {});
      },
    },
    // Нужен всплывашке (ui/ask.js): она прячет собственное окно.
    // `hide` нужен всплывашке (ui/ask.js), `setTitle` — главному окну: оно
    // переименовывает себя при смене языка (main.js:1549 и :2186). Обе
    // возвращают промис, потому что вызывающий вешает на них `.catch`.
    window: {
      getCurrentWindow: () => ({
        hide: () => Promise.resolve(),
        setTitle: () => Promise.resolve(),
      }),
    },
    // `main.js` разбирает `app` НА ВЕРХНЕМ УРОВНЕ, седьмой строкой. Без этой
    // ветки там TypeError до всего остального: модуль не выполняется вовсе,
    // и стенд показывает скелетон вместо списка, светлую тему вместо тёмной и
    // ключи вместо фраз. Симптомов три, причина одна — поэтому и лечится здесь,
    // а не подпорками в каждом из трёх мест.
    app: { getVersion: () => Promise.resolve("0.2.0") },
  };

  // Светофор macOS рисует система, а в браузере его нет. Стенд дорисовывает
  // три круга на их настоящем месте — только чтобы было видно, что шапка
  // оставила под них место. В приложении этого узла нет.
  if (!ВСПЛЫВАШКА) {
    addEventListener("DOMContentLoaded", () => {
      const св = document.createElement("div");
      св.setAttribute("aria-hidden", "true");
      св.style.cssText =
        "position:fixed;left:16px;top:16px;display:flex;gap:8px;z-index:99;pointer-events:none";
      for (const цвет of ["#ff5f57", "#febc2e", "#28c840"]) {
        const к = document.createElement("i");
        к.style.cssText = `width:12px;height:12px;border-radius:50%;background:${цвет};box-shadow:inset 0 0 0 .5px rgba(0,0,0,.12)`;
        св.append(к);
      }
      document.body.append(св);
    });
  }

  // ── Сцены, которые складываются из событий после старта ─────────────────
  const меню_строки = (и, потом) =>
    setTimeout(() => {
      document.querySelectorAll("#list .dots button")[и]?.click();
      потом && setTimeout(потом, 160);
    }, 450);

  addEventListener("load", () =>
    setTimeout(() => {
      if (S === "recording" || S === "updaterec") метроном = setInterval(метр, 100);
      if (S === "recstart" || ПУЛЬТ.includes(S)) $("rec-btn")?.click();
      if (S === "recstopping") setTimeout(() => $("stop-btn")?.click(), 600);
      // Пульт: событие `screen` приходит вскоре после старта записи.
      if (экран_статус()) setTimeout(() => emit("screen", экран_статус()), 300);
      // Меню у кнопки экрана: открываем кликом мыши (`detail: 1`), как
      // человек, — клавиатурный клик увёл бы фокус в меню.
      const мышью = (id, через) =>
        setTimeout(() => $(id)?.dispatchEvent(new MouseEvent("click", { bubbles: true, detail: 1 })), через);
      if (["scr-menu-win", "scr-menu-one", "scr-menu-disp", "scr-noperm"].includes(S)) мышью("scr-btn", 200);
      if (S === "recmicmenu") мышью("mic-chip", 500);
      if (S === "recscrmenu") мышью("scr-chip", 500);
      if (S === "set-screen-menu") мышью("scr-what", 500);
      // Пульт с выключенным микрофоном: выключаем вскоре после старта, как человек.
      if (S === "recmuted") setTimeout(() => emit("mic-muted", (снимок.mic_muted = true)), 400);

      const первая = { folder: СПИСОК[0].folder, base: СПИСОК[0].name };
      const вторая = { folder: СПИСОК[1].folder, base: СПИСОК[1].name };
      const третья = { folder: СПИСОК[2].folder, base: СПИСОК[2].name };

      if (S === "transcribing") emit("transcribe-progress", { ...первая, stage: "polling" });
      if (S === "queued") {
        emit("transcribe-progress", { ...первая, stage: "polling" });
        emit("transcribe-progress", { ...вторая, stage: "queued:2" });
        emit("transcribe-progress", { ...третья, stage: "queued:3" });
      }
      if (S === "rowerror")
        emit("transcribe-error", { ...первая, message: "шлюз не ответил за 120 с" });
      if (S === "menubusy" || S === "cancel" || S === "cancelled")
        emit("transcribe-progress", { ...вторая, stage: "polling" });

      if (S === "menu") меню_строки(0);
      if (S === "menudone" || S === "menu-copy-path") меню_строки(0);
      if (S === "menubusy" || S === "cancel" || S === "menu-partial") меню_строки(1);

      // Панель расшифровки: кликаем по строке-«витрине» первой записи — тем
      // же жестом, что и живой человек, а не прямым вызовом функции панели,
      // чтобы стенд проверял ровно тот путь, которым она открывается.
      if (S.startsWith("tr-")) {
        // Поиск: вводим запрос тем же путём, что человек, — событием input.
        const искать = (q) =>
          setTimeout(() => {
            const поле = $("tr-q");
            if (!поле) return;
            поле.value = q;
            поле.dispatchEvent(new Event("input"));
          }, 600);
        if (S === "tr-search") искать("экран");
        if (S === "tr-nohit") искать("бюджет");
        if (S === "tr-menu") setTimeout(() => $("tr-more")?.click(), 700);
        setTimeout(() => {
          document.querySelector("#list .rec .hit")?.click();
          if (S === "tr-again") {
            setTimeout(() => emit("transcribe-progress", { ...первая, stage: "polling" }), 500);
          }
        }, 500);
      }
      if (S === "rename")
        меню_строки(0, () =>
          [...document.querySelectorAll(".menu button")]
            .find((b) => b.textContent.includes("Переименовать"))
            ?.click(),
        );
      if (S === "cancelled")
        меню_строки(1, () =>
          [...document.querySelectorAll(".menu button")]
            .find((b) => b.textContent.includes("Отменить"))
            ?.click(),
        );

      if (S === "updating" || S === "updateerror") setTimeout(() => $("update-open")?.click(), 200);

      const ПРОВЕРКИ = ["updatecheck", "uptodate", "checkerror"];
      if (["settings", "deferred", "miccheck", "longmic", "support", "about"].includes(S) || S.startsWith("set-") || ПРОВЕРКИ.includes(S))
        $("to-settings")?.click();
      if (ПРОВЕРКИ.includes(S)) setTimeout(() => $("upd-check")?.click(), 220);
      if (S === "miccheck") setTimeout(() => $("check")?.click(), 220);
      if (S === "support") setTimeout(() => $("to-support")?.click(), 220);
      if (S === "about") setTimeout(() => $("to-about")?.click(), 220);
      if (S === "set-calconfirm")
        setTimeout(() => [...document.querySelectorAll("#cal-body .acts button")].pop()?.click(), 300);
    }, 60),
  );

  function $(id) {
    return document.getElementById(id);
  }
})();
