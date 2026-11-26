//! Запись экрана рядом с дорожками встречи — по желанию, по умолчанию выключена.
//!
//! Файл делится так же, как `capture/macos.rs`: сверху ЧИСТАЯ часть (режим,
//! выбор окна встречи, размер кадра и битрейт, имя файла) — она проверяется
//! тестами на любой машине; снизу — [`ScreenCapture`], тонкая обёртка над
//! платформенным захватом (ScreenCaptureKit на macOS, Windows Graphics Capture
//! на Windows), которую тестом не взять.
//!
//! Видео — спутник записи, а не её часть: оно пишется в ту же месячную папку под
//! той же основой имени (`<основа>.screen.mp4`), но звук от него не зависит.
//! Не поднялся захват экрана — встреча всё равно пишется, просто без картинки.
//! Расшифровка видео не касается вовсе: она читает только `.wav`.

use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// Хвост имени файла видео. Основа — та же, что у `.mic.wav`/`.system.wav`.
pub const VIDEO_SUFFIX: &str = ".screen.mp4";

/// Кадров в секунду — потолок, а не обещание: экран встречи меняется редко,
/// и десяти кадров хватает и на слайды, и на лица, а файл выходит в разы
/// меньше, чем при системных 60.
pub const FPS: u32 = 10;

/// Длинная сторона кадра не больше этого. 4K-монитор в 4K на часовой встрече —
/// гигабайты ради мелкого текста, который и в 1080p читается.
pub const MAX_LONG_SIDE: u32 = 1920;

pub fn video_filename(base: &str) -> String {
    format!("{base}{VIDEO_SUFFIX}")
}

/// Хвост имени второго, третьего… видео одной записи: `<основа>.screen-2.mp4`.
/// Появляется, когда источник сменили посреди записи, а ScreenCaptureKit не
/// смог переключить идущий поток, или когда запись экрана включили заново.
pub fn video_filename_n(base: &str, n: u32) -> String {
    if n <= 1 {
        video_filename(base)
    } else {
        format!("{base}.screen-{n}.mp4")
    }
}

/// Первое свободное имя видео под основой: `.screen.mp4`, потом `.screen-2.mp4`…
/// Уже записанное видео не перетирается никогда.
pub fn free_video_path(dir: &Path, base: &str) -> PathBuf {
    (1..)
        .map(|n| dir.join(video_filename_n(base, n)))
        .find(|p| !p.exists())
        .expect("бесконечный ряд имён")
}

/// Номер видео по имени файла: `<основа>.screen.mp4` — 1,
/// `<основа>.screen-N.mp4` — N (от 2). Чужое имя — `None`.
pub fn video_number(file_name: &str, base: &str) -> Option<u32> {
    let rest = file_name.strip_prefix(base)?;
    if rest == VIDEO_SUFFIX {
        return Some(1);
    }
    let n = rest.strip_prefix(".screen-")?.strip_suffix(".mp4")?;
    if n.is_empty() || n.starts_with('0') || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse().ok().filter(|n| *n >= 2)
}

/// Все видео записи на диске — `.screen.mp4`, `.screen-2.mp4`… — по порядку
/// номеров. Единый шаблон для удаления, переименования и автоочистки: видео
/// под основой может быть несколько (экран выключали и включали посреди
/// записи, источник не сменился на лету).
pub fn video_files(dir: &Path, base: &str) -> Vec<(u32, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<(u32, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name();
            let n = video_number(name.to_str()?, base)?;
            Some((n, e.path()))
        })
        .collect();
    found.sort_by_key(|(n, _)| *n);
    found
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScreenMode {
    Off,
    /// Окна приложения встречи (Zoom, браузер с Meet, Discord…) — находятся сами.
    Meeting,
    /// Одно окно, которое выбрал человек.
    Window,
    /// Монитор целиком.
    Screen,
}

impl ScreenMode {
    /// Отсутствие поля в конфиге и любое незнакомое значение — `Off`:
    /// включать запись экрана должен человек, а не опечатка.
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("meeting") => Self::Meeting,
            Some("window") => Self::Window,
            Some("screen") => Self::Screen,
            _ => Self::Off,
        }
    }

    /// Режим по конфигу. До выбора отдельного окна `"window"` значил «окна
    /// звонилки»: такой конфиг без `window_id` читается как «окно встречи».
    pub fn from_config(mode: Option<&str>, window_id: Option<u32>) -> Self {
        match Self::parse(mode) {
            Self::Window if window_id.is_none() => Self::Meeting,
            m => m,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Meeting => "meeting",
            Self::Window => "window",
            Self::Screen => "screen",
        }
    }
}

/// Приложение с открытыми окнами — строка выпадашки «какое окно писать».
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ScreenApp {
    /// Bundle id на macOS. Стабилен между запусками — по нему и матчится выбор.
    pub id: String,
    /// Имя для показа.
    pub name: String,
    /// Заголовки окон: по ним в браузере находится вкладка Meet.
    pub titles: Vec<String>,
}

/// Известные звонилки: bundle id и кусок хвоста имени записи, который ставит
/// детектор (`storage::sanitize_tail` от имени процесса: `zoom.us` → `zoom-us`).
/// Порядок — приоритет, когда подсказки от детектора нет.
const MEETING_APPS: &[(&str, &str)] = &[
    ("us.zoom.xos", "zoom"),
    ("com.microsoft.teams2", "teams"),
    ("com.microsoft.teams", "teams"),
    ("com.hnc.Discord", "discord"),
    ("com.tinyspeck.slackmacgap", "slack"),
    ("ru.yandex.telemost", "telemost"),
];

/// Браузеры: в них живут Meet и Телемост. Берутся, только если в заголовке
/// какого-то окна видна встреча — иначе «окно встречи» оказалось бы просто
/// браузером с почтой.
const BROWSERS: &[&str] = &[
    "com.google.Chrome",
    "com.apple.Safari",
    "company.thebrowser.Browser",
    "com.microsoft.edgemac",
    "org.mozilla.firefox",
    "com.brave.Browser",
    "ru.yandex.desktop.yandex-browser",
    "com.operasoftware.Opera",
];

const MEETING_TITLES: &[&str] = &["Meet", "Телемост", "Telemost", "Jitsi"];

pub fn is_meeting_app(id: &str) -> bool {
    MEETING_APPS.iter().any(|(b, _)| *b == id)
}

/// Чьи окна писать в режиме «окно».
///
/// 1. Выбранное в настройках приложение, если оно сейчас открыто.
/// 2. Звонилка, на которую указывает имя записи (детектор знает, кто взял
///    микрофон): `source` — хвост основы, например `zoom-us`.
/// 3. Первая открытая звонилка из [`MEETING_APPS`].
/// 4. Браузер, в заголовке окна которого видна встреча.
///
/// `None` — писать нечего. Подменять это «тогда весь экран» нельзя: человек
/// выбрал «окно» как раз затем, чтобы на запись не попало остальное.
pub fn pick_app(apps: &[ScreenApp], chosen: Option<&str>, source: Option<&str>) -> Option<usize> {
    let find = |id: &str| apps.iter().position(|a| a.id == id);
    if let Some(i) = chosen.and_then(find) {
        return Some(i);
    }
    if let Some(src) = source.map(str::to_lowercase) {
        let hinted = MEETING_APPS
            .iter()
            .filter(|(_, hint)| src.contains(hint))
            .find_map(|(id, _)| find(id));
        if hinted.is_some() {
            return hinted;
        }
    }
    if let Some(i) = MEETING_APPS.iter().find_map(|(id, _)| find(id)) {
        return Some(i);
    }
    apps.iter().position(|a| {
        BROWSERS.contains(&a.id.as_str())
            && a.titles.iter().any(|t| MEETING_TITLES.iter().any(|m| t.contains(m)))
    })
}

/// Окно в меню выбора источника.
#[derive(Clone, PartialEq, Debug)]
pub struct WindowInfo {
    /// `CGWindowID`. Живёт, пока живёт окно: после перезапуска приложения у
    /// того же окна другой id, поэтому выбор хранит ещё приложение и заголовок.
    pub id: u32,
    /// Bundle id приложения.
    pub app_id: String,
    pub app_name: String,
    pub title: String,
    /// Монитор, на котором центр окна.
    pub display_id: u32,
    /// Размер в точках — по нему из нескольких окон приложения берётся главное.
    pub width: f64,
    pub height: f64,
    /// `data:image/jpeg;base64,…`; `None` — снимка нет (свёрнуто, не успел).
    pub thumb: Option<String>,
    /// Иконка приложения `data:image/png;base64,…`, 64×64 пикселя (32×32 точки
    /// на Retina). `None` — у приложения нет иконки.
    pub icon: Option<String>,
}

/// Монитор в меню выбора источника.
#[derive(Clone, PartialEq, Debug)]
pub struct DisplayInfo {
    /// `CGDirectDisplayID`.
    pub id: u32,
    /// Как его зовёт система («Встроенный дисплей Retina», «L27e-30»).
    pub name: String,
    pub main: bool,
    pub thumb: Option<String>,
}

/// Всё, что можно писать прямо сейчас.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Sources {
    pub windows: Vec<WindowInfo>,
    pub displays: Vec<DisplayInfo>,
    /// Чьи окна сейчас взяло бы «окно встречи» — имя приложения.
    pub meeting_app: Option<String>,
}

/// Окно, как его отдаёт система, — до решения, показывать ли его в меню.
#[derive(Clone, PartialEq, Debug)]
pub struct RawWindow {
    pub layer: i64,
    pub width: f64,
    pub height: f64,
    pub on_screen: bool,
    /// Окно самого MeetRec.
    pub own: bool,
    pub app_id: String,
    pub title: String,
}

/// Окна системы, которые окнами для человека не являются.
const SYSTEM_APPS: &[&str] = &[
    "com.apple.dock",
    "com.apple.WindowManager",
    "com.apple.controlcenter",
    "com.apple.systemuiserver",
    "com.apple.notificationcenterui",
    "com.apple.Spotlight",
    "com.apple.loginwindow",
    "com.apple.screencaptureui",
];

/// Показывать ли окно в меню выбора. Не только видимые: окно на другом
/// рабочем столе ScreenCaptureKit пишет так же, а встречу часто держат там.
/// Невидимое и без заголовка — служебное окно приложения, а не окно.
pub fn is_listable(w: &RawWindow) -> bool {
    w.layer == 0
        && !w.own
        && w.width >= 120.0
        && w.height >= 80.0
        && !w.app_id.is_empty()
        && !SYSTEM_APPS.contains(&w.app_id.as_str())
        && (w.on_screen || !w.title.trim().is_empty())
}

/// Прямоугольник монитора `(x, y, ширина, высота)` в общих координатах всех
/// мониторов, в точках.
pub type Rect = (f64, f64, f64, f64);

/// Монитор, в прямоугольник которого попадает точка.
pub fn display_at(displays: &[(u32, Rect)], x: f64, y: f64) -> Option<u32> {
    displays
        .iter()
        .find(|(_, (dx, dy, dw, dh))| x >= *dx && x < dx + dw && y >= *dy && y < dy + dh)
        .map(|(id, _)| *id)
}

/// Какое окно писать в режиме «одно окно».
///
/// 1. То же окно по id — если оно всё ещё того же приложения (id после
///    перезапуска могут достаться чужому окну).
/// 2. Окно того же приложения с тем же заголовком.
/// 3. Самое большое окно того же приложения.
///
/// `None` — окна нет; писать вместо него «что-нибудь» нельзя.
pub fn pick_window(windows: &[WindowInfo], id: u32, app_id: Option<&str>, title: Option<&str>) -> Option<usize> {
    let same_app = |w: &WindowInfo| app_id.is_none_or(|a| w.app_id == a);
    if let Some(i) = windows.iter().position(|w| w.id == id && same_app(w)) {
        return Some(i);
    }
    let app = app_id?;
    if let Some(t) = title.filter(|t| !t.is_empty()) {
        if let Some(i) = windows.iter().position(|w| w.app_id == app && w.title == t) {
            return Some(i);
        }
    }
    windows
        .iter()
        .enumerate()
        .filter(|(_, w)| w.app_id == app)
        .max_by(|(_, a), (_, b)| (a.width * a.height).total_cmp(&(b.width * b.height)))
        .map(|(i, _)| i)
}

/// Какой монитор писать: по id → по имени (id меняются при переподключении)
/// → основной → любой.
pub fn pick_display(displays: &[DisplayInfo], id: Option<u32>, name: Option<&str>) -> Option<usize> {
    id.and_then(|id| displays.iter().position(|d| d.id == id))
        .or_else(|| name.and_then(|n| displays.iter().position(|d| d.name == n)))
        .or_else(|| displays.iter().position(|d| d.main))
        .or(if displays.is_empty() { None } else { Some(0) })
}

/// Размер кадра под запись: пропорции исходника, длинная сторона не больше
/// [`MAX_LONG_SIDE`], обе стороны чётные (H.264 с нечётными не дружит).
pub fn target_size(width: u32, height: u32) -> (u32, u32) {
    let (w, h) = (width.max(2), height.max(2));
    let long = w.max(h);
    let (w, h) = if long > MAX_LONG_SIDE {
        let k = MAX_LONG_SIDE as f64 / long as f64;
        ((w as f64 * k).round() as u32, (h as f64 * k).round() as u32)
    } else {
        (w, h)
    };
    (w & !1, h & !1)
}

/// Сколько ждать первый кадр после старта или смены источника. ScreenCaptureKit
/// шлёт кадр сразу после старта и после `updateContentFilter`, дальше — только
/// при изменениях; 3 секунды тишины — источник кадров не даёт вовсе.
pub const FIRST_FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameWatch {
    /// Кадра ещё нет, но и время не вышло.
    Wait,
    /// Кадр пришёл — сторож больше не нужен.
    Alive,
    /// Время вышло, кадров нет: источник не даёт себя записать.
    Blocked,
}

/// Сторож первого кадра: `elapsed` — с момента старта или смены источника,
/// `frames` — сколько кадров с картинкой пришло с того момента.
pub fn frame_watch(elapsed: std::time::Duration, frames: u64) -> FrameWatch {
    if frames > 0 {
        FrameWatch::Alive
    } else if elapsed >= FIRST_FRAME_TIMEOUT {
        FrameWatch::Blocked
    } else {
        FrameWatch::Wait
    }
}

/// Ошибка, которой оборвалась запись, — в наш вид. «failure to process first
/// sample buffer» ScreenCaptureKit отвечает на окно, которое не дало ни
/// одного кадра: это запрет захвата, а не сбой.
pub fn classify_failure(text: String) -> ScreenError {
    if text.to_lowercase().contains("first sample buffer") {
        ScreenError::Blocked
    } else {
        ScreenError::Failed(text)
    }
}

/// Ширина миниатюры в меню выбора, пиксели.
pub const THUMB_WIDTH: u32 = 240;

/// Размер миниатюры: [`THUMB_WIDTH`] шириной, пропорции источника, высота не
/// больше двух ширин (высокое узкое окно не раздувает снимок).
pub fn thumb_size(width: f64, height: f64) -> (u32, u32) {
    let k = height.max(1.0) / width.max(1.0);
    let h = (THUMB_WIDTH as f64 * k).round().clamp(2.0, (THUMB_WIDTH * 2) as f64);
    (THUMB_WIDTH, h as u32)
}

/// Битрейт H.264 под размер кадра: 0,05 бита на пиксель при [`FPS`], в
/// пределах 0,5–2,5 Мбит/с. 1080p выходит ~1 Мбит/с — около 450 МБ за час,
/// вдвое больше одной WAV-дорожки, а не в десятки раз.
pub fn bitrate(width: u32, height: u32) -> u32 {
    let bits = width as f64 * height as f64 * FPS as f64 * 0.05;
    (bits as u32).clamp(500_000, 2_500_000)
}

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum ScreenError {
    #[error("запись экрана здесь недоступна: {0}")]
    Unsupported(String),
    #[error("нет разрешения на запись экрана — включите MeetRec в «Системные настройки → Конфиденциальность и безопасность → Запись экрана и системного звука» и перезапустите приложение")]
    Permission,
    #[error("окно встречи не найдено — экран не записывается")]
    NoMeetingWindow,
    #[error("выбранное окно не найдено — экран не записывается")]
    NoWindow,
    /// Окно запрещает себя записывать (`NSWindow.sharingType = none`):
    /// ScreenCaptureKit не отдаёт ни одного кадра.
    #[error("окно не даёт себя записать")]
    Blocked,
    #[error("запись экрана: {0}")]
    Failed(String),
}

/// Что умеет эта система. `reason` — почему запись экрана недоступна целиком.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Support {
    pub window: bool,
    pub screen: bool,
    pub reason: Option<String>,
}

/// Что именно писать.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Target {
    /// Все окна звонилки ([`pick_app`]) на том экране, где её окно.
    /// `source` — хвост основы имени записи, подсказка детектора.
    Meeting { source: Option<String> },
    /// Одно окно ([`pick_window`]).
    Window {
        id: u32,
        app_id: Option<String>,
        title: Option<String>,
    },
    /// Монитор целиком ([`pick_display`]). `hide_self` — убрать с записи окна
    /// самого MeetRec.
    Display {
        id: Option<u32>,
        name: Option<String>,
        hide_self: bool,
    },
}

/// Какой источник на деле пишется — для пульта.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Meeting,
    Window,
    Screen,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Meeting => "meeting",
            Self::Window => "window",
            Self::Screen => "screen",
        }
    }
}

impl Target {
    pub fn kind(&self) -> Kind {
        match self {
            Self::Meeting { .. } => Kind::Meeting,
            Self::Window { .. } => Kind::Window,
            Self::Display { .. } => Kind::Screen,
        }
    }
}

/// Что нашлось под цель: имя приложения, заголовок окна, имя монитора.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Showing {
    pub kind: Kind,
    pub app: Option<String>,
    pub title: Option<String>,
    pub display: Option<String>,
}

pub fn support() -> Support {
    #[cfg(target_os = "macos")]
    return macos::support();
    #[cfg(target_os = "windows")]
    return windows::support();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    Support {
        window: false,
        screen: false,
        reason: Some("эта система не поддерживается".into()),
    }
}

/// Есть ли уже разрешение. На Windows его не спрашивают — всегда `true`.
pub fn has_permission() -> bool {
    #[cfg(target_os = "macos")]
    return macos::has_permission();
    #[cfg(not(target_os = "macos"))]
    true
}

/// Попросить разрешение. На macOS первый вызов показывает системный вопрос и
/// сразу возвращает `false`; выданное разрешение начинает действовать после
/// перезапуска приложения.
pub fn request_permission() -> bool {
    #[cfg(target_os = "macos")]
    return macos::request_permission();
    #[cfg(not(target_os = "macos"))]
    true
}

/// Приложения с окнами на экране — для выпадашки режима «окно».
pub fn list_apps() -> Result<Vec<ScreenApp>, ScreenError> {
    #[cfg(target_os = "macos")]
    return macos::list_apps();
    #[cfg(not(target_os = "macos"))]
    Ok(Vec::new())
}

/// Окна и мониторы для меню выбора. `thumbs` — снять миниатюры (до ~1 с;
/// не успевшие — `None`).
pub fn list_sources(thumbs: bool) -> Result<Sources, ScreenError> {
    #[cfg(target_os = "macos")]
    return macos::list_sources(thumbs);
    #[cfg(target_os = "windows")]
    return windows::list_sources(thumbs);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = thumbs;
        Ok(Sources::default())
    }
}

/// Идущая запись экрана. Создаётся, переключается и останавливается на одном
/// потоке: объекты ScreenCaptureKit между потоками не передаются.
pub struct ScreenCapture {
    #[cfg(target_os = "macos")]
    inner: macos::Active,
    #[cfg(target_os = "windows")]
    inner: windows::Active,
    path: PathBuf,
    showing: Showing,
    /// Что писалось до последней смены источника — для [`ScreenCapture::revert`].
    prev_showing: Option<Showing>,
}

impl ScreenCapture {
    pub fn start(target: &Target, path: &Path) -> Result<Self, ScreenError> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            #[cfg(target_os = "macos")]
            let (inner, showing) = macos::Active::start(target, path)?;
            #[cfg(target_os = "windows")]
            let (inner, showing) = windows::Active::start(target, path)?;
            Ok(Self {
                inner,
                path: path.to_path_buf(),
                showing,
                prev_showing: None,
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = (target, path);
            Err(ScreenError::Unsupported("эта система не поддерживается".into()))
        }
    }

    /// Что пишется сейчас.
    pub fn showing(&self) -> &Showing {
        &self.showing
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Сменить источник, не останавливая запись и не начиная новый файл.
    ///
    /// Ошибки две разные. Цель не нашлась ([`ScreenError::NoWindow`],
    /// [`ScreenError::NoMeetingWindow`]) — поток не тронут, пишется прежнее.
    /// [`ScreenError::Failed`] — система не дала переключить на лету; тогда
    /// вызывающий останавливает эту запись и начинает новый файл.
    pub fn switch(&mut self, target: &Target) -> Result<&Showing, ScreenError> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let showing = self.inner.switch(target)?;
            self.prev_showing = Some(std::mem::replace(&mut self.showing, showing));
            Ok(&self.showing)
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = target;
            Err(ScreenError::Unsupported("эта система не поддерживается".into()))
        }
    }

    /// Сторож первого кадра, звать на каждом тике. `Some(false)` — после
    /// старта за [`FIRST_FRAME_TIMEOUT`] не пришло ни одного кадра (такое
    /// видео не откроется, его надо бросить); `Some(true)` — то же после смены
    /// источника (можно вернуть прежний, [`ScreenCapture::revert`]).
    /// Отвечает один раз на каждый старт и смену.
    pub fn blocked(&mut self) -> Option<bool> {
        #[cfg(target_os = "macos")]
        return self.inner.blocked();
        #[cfg(not(target_os = "macos"))]
        None
    }

    /// Вернуть источник, который писался до последней смены.
    pub fn revert(&mut self) -> Result<&Showing, ScreenError> {
        #[cfg(target_os = "macos")]
        {
            let prev = self.prev_showing.take().ok_or(ScreenError::Failed("прежнего источника нет".into()))?;
            self.inner.revert()?;
            self.showing = prev;
            Ok(&self.showing)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = self.prev_showing.take();
            Err(ScreenError::Unsupported("эта система не поддерживается".into()))
        }
    }

    /// Пришёл ли с начала записи хоть один кадр. Нет — файл пустой и не
    /// откроется, хранить его незачем.
    pub fn has_frames(&self) -> bool {
        #[cfg(target_os = "macos")]
        return self.inner.has_frames();
        #[cfg(not(target_os = "macos"))]
        true
    }

    /// `Some(причина)` — запись оборвалась сама (монитор отключили, система
    /// отозвала доступ). Спрашивается на каждом тике рабочего потока.
    pub fn failure(&mut self) -> Option<String> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        return self.inner.failure();
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        None
    }

    /// Остановить и дописать файл. Блокирует, пока контейнер не закрыт:
    /// недописанный mp4 без индекса не открывается ни одним плеером.
    pub fn stop(self) -> Result<PathBuf, ScreenError> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        self.inner.stop()?;
        Ok(self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, titles: &[&str]) -> ScreenApp {
        ScreenApp {
            id: id.into(),
            name: id.into(),
            titles: titles.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn режим_по_умолчанию_выключен() {
        assert_eq!(ScreenMode::parse(None), ScreenMode::Off);
        assert_eq!(ScreenMode::parse(Some("off")), ScreenMode::Off);
        assert_eq!(ScreenMode::parse(Some("мусор")), ScreenMode::Off);
        assert_eq!(ScreenMode::parse(Some("meeting")), ScreenMode::Meeting);
        assert_eq!(ScreenMode::parse(Some("window")), ScreenMode::Window);
        assert_eq!(ScreenMode::parse(Some("screen")), ScreenMode::Screen);
    }

    #[test]
    fn старое_окно_без_id_это_окно_встречи() {
        assert_eq!(ScreenMode::from_config(Some("window"), None), ScreenMode::Meeting);
        assert_eq!(ScreenMode::from_config(Some("window"), Some(42)), ScreenMode::Window);
        assert_eq!(ScreenMode::from_config(Some("screen"), None), ScreenMode::Screen);
        assert_eq!(ScreenMode::from_config(None, Some(42)), ScreenMode::Off);
    }

    #[test]
    fn второе_видео_не_перетирает_первое() {
        assert_eq!(video_filename_n("b", 1), "b.screen.mp4");
        assert_eq!(video_filename_n("b", 2), "b.screen-2.mp4");
        let dir = std::env::temp_dir().join(format!("meetrec-screen-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(free_video_path(&dir, "b"), dir.join("b.screen.mp4"));
        std::fs::write(dir.join("b.screen.mp4"), b"").unwrap();
        assert_eq!(free_video_path(&dir, "b"), dir.join("b.screen-2.mp4"));
        std::fs::write(dir.join("b.screen-2.mp4"), b"").unwrap();
        assert_eq!(free_video_path(&dir, "b"), dir.join("b.screen-3.mp4"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn номер_видео_только_у_своих_файлов() {
        assert_eq!(video_number("b.screen.mp4", "b"), Some(1));
        assert_eq!(video_number("b.screen-2.mp4", "b"), Some(2));
        assert_eq!(video_number("b.screen-12.mp4", "b"), Some(12));
        assert_eq!(video_number("b.screen-1.mp4", "b"), None, "первое видео без номера");
        assert_eq!(video_number("b.screen-02.mp4", "b"), None);
        assert_eq!(video_number("b.screen-x.mp4", "b"), None);
        assert_eq!(video_number("b.screen-.mp4", "b"), None);
        assert_eq!(video_number("b.mic.wav", "b"), None);
        assert_eq!(video_number("b_2.screen.mp4", "b"), None, "соседняя запись с похожим именем — чужая");
        assert_eq!(video_number("b.screen-2.mp4.part", "b"), None);
    }

    #[test]
    fn все_видео_записи_по_порядку() {
        let dir = std::env::temp_dir().join(format!("meetrec-video-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["b.screen-3.mp4", "b.screen.mp4", "b.screen-2.mp4", "b.mic.wav", "b_2.screen.mp4", "c.screen.mp4"] {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        let found: Vec<_> = video_files(&dir, "b").into_iter().map(|(n, p)| (n, p.file_name().unwrap().to_owned())).collect();
        assert_eq!(
            found,
            vec![(1, "b.screen.mp4".into()), (2, "b.screen-2.mp4".into()), (3, "b.screen-3.mp4".into())]
        );
        assert!(video_files(&dir.join("нет такой"), "b").is_empty(), "нет папки — нет видео, не паника");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn win(id: u32, app: &str, title: &str, w: f64, h: f64) -> WindowInfo {
        WindowInfo {
            id,
            app_id: app.into(),
            app_name: app.into(),
            title: title.into(),
            display_id: 1,
            width: w,
            height: h,
            thumb: None,
            icon: None,
        }
    }

    #[test]
    fn окно_находится_по_id() {
        let ws = [win(1, "us.zoom.xos", "Zoom", 800.0, 600.0), win(7, "com.apple.Keynote", "Слайды", 900.0, 700.0)];
        assert_eq!(pick_window(&ws, 7, Some("com.apple.Keynote"), Some("Слайды")), Some(1));
        assert_eq!(pick_window(&ws, 7, None, None), Some(1), "без приложения — просто по id");
    }

    #[test]
    fn чужое_окно_с_тем_же_id_не_берётся() {
        // После перезапуска id 7 достался Finder, а Keynote теперь под 9.
        let ws = [win(7, "com.apple.finder", "Загрузки", 800.0, 600.0), win(9, "com.apple.Keynote", "Слайды", 900.0, 700.0)];
        assert_eq!(pick_window(&ws, 7, Some("com.apple.Keynote"), Some("Слайды")), Some(1));
    }

    #[test]
    fn окно_ищется_по_заголовку_потом_по_приложению() {
        let ws = [
            win(10, "com.google.Chrome", "Почта", 1400.0, 900.0),
            win(11, "com.google.Chrome", "Meet – abc", 800.0, 600.0),
            win(12, "com.google.Chrome", "Новости", 600.0, 400.0),
        ];
        assert_eq!(pick_window(&ws, 99, Some("com.google.Chrome"), Some("Meet – abc")), Some(1));
        assert_eq!(
            pick_window(&ws, 99, Some("com.google.Chrome"), Some("Закрытая вкладка")),
            Some(0),
            "заголовка нет — самое большое окно приложения"
        );
        assert_eq!(pick_window(&ws, 99, Some("us.zoom.xos"), Some("Zoom")), None, "приложения нет — окна нет");
        assert_eq!(pick_window(&ws, 99, None, Some("Почта")), None, "без приложения заголовку не верим");
    }

    fn disp(id: u32, name: &str, main: bool) -> DisplayInfo {
        DisplayInfo { id, name: name.into(), main, thumb: None }
    }

    #[test]
    fn монитор_по_id_по_имени_потом_основной() {
        let ds = [disp(1, "Встроенный дисплей", true), disp(5, "L27e-30", false)];
        assert_eq!(pick_display(&ds, Some(5), Some("L27e-30")), Some(1));
        assert_eq!(pick_display(&ds, Some(9), Some("L27e-30")), Some(1), "id сменился — по имени");
        assert_eq!(pick_display(&ds, Some(9), Some("Отключённый")), Some(0), "нет такого — основной");
        assert_eq!(pick_display(&ds, None, None), Some(0));
        assert_eq!(pick_display(&[disp(5, "L27e-30", false)], Some(1), None), Some(0), "нет основного — любой");
        assert_eq!(pick_display(&[], Some(1), None), None);
    }

    fn raw(app: &str, title: &str) -> RawWindow {
        RawWindow {
            layer: 0,
            width: 800.0,
            height: 600.0,
            on_screen: true,
            own: false,
            app_id: app.into(),
            title: title.into(),
        }
    }

    #[test]
    fn в_меню_только_настоящие_окна() {
        assert!(is_listable(&raw("us.zoom.xos", "Zoom")));
        assert!(!is_listable(&RawWindow { own: true, ..raw("dev.meetrec", "MeetRec") }), "свои окна не показываем");
        assert!(!is_listable(&raw("com.apple.dock", "Dock")));
        assert!(!is_listable(&RawWindow { layer: 25, ..raw("com.apple.finder", "") }), "системный слой");
        assert!(!is_listable(&RawWindow { width: 100.0, ..raw("us.zoom.xos", "Zoom") }), "крошка");
        assert!(!is_listable(&RawWindow { height: 60.0, ..raw("us.zoom.xos", "Zoom") }), "крошка");
        assert!(
            is_listable(&RawWindow { on_screen: false, ..raw("us.zoom.xos", "Zoom Meeting") }),
            "окно на другом столе тоже окно"
        );
        assert!(
            !is_listable(&RawWindow { on_screen: false, ..raw("com.google.Chrome", "") }),
            "невидимое без заголовка — служебное"
        );
    }

    #[test]
    fn сторож_ждёт_первый_кадр_три_секунды() {
        use std::time::Duration;
        assert_eq!(frame_watch(Duration::from_millis(100), 0), FrameWatch::Wait);
        assert_eq!(frame_watch(Duration::from_millis(2999), 0), FrameWatch::Wait);
        assert_eq!(frame_watch(Duration::from_secs(3), 0), FrameWatch::Blocked);
        assert_eq!(frame_watch(Duration::from_secs(60), 0), FrameWatch::Blocked);
        assert_eq!(frame_watch(Duration::from_millis(50), 1), FrameWatch::Alive);
        assert_eq!(frame_watch(Duration::from_secs(10), 3), FrameWatch::Alive, "кадр был — не запрет, даже если поздно спросили");
    }

    #[test]
    fn нет_первого_кадра_это_запрет_а_не_сбой() {
        assert_eq!(
            classify_failure("Failed due to failure to process first sample buffer".into()),
            ScreenError::Blocked
        );
        assert_eq!(classify_failure("Диск полон".into()), ScreenError::Failed("Диск полон".into()));
    }

    #[test]
    fn миниатюра_240_в_пропорциях_источника() {
        assert_eq!(thumb_size(1600.0, 1000.0), (240, 150));
        assert_eq!(thumb_size(1920.0, 1080.0), (240, 135));
        assert_eq!(thumb_size(200.0, 2000.0), (240, 480), "высокое окно упирается в потолок");
        assert_eq!(thumb_size(0.0, 0.0), (240, 240), "нулевой размер не делит на ноль");
    }

    #[test]
    fn окно_относится_к_монитору_по_центру() {
        let ds = [(1, (0.0, 0.0, 1512.0, 982.0)), (5, (1512.0, -200.0, 2560.0, 1440.0))];
        assert_eq!(display_at(&ds, 700.0, 400.0), Some(1));
        assert_eq!(display_at(&ds, 2000.0, -100.0), Some(5));
        assert_eq!(display_at(&ds, -50.0, 0.0), None);
    }

    #[test]
    fn видео_лежит_под_той_же_основой() {
        assert_eq!(
            video_filename("2026-10-24_14-30_zoom-us"),
            "2026-10-24_14-30_zoom-us.screen.mp4"
        );
    }

    #[test]
    fn выбранное_приложение_важнее_подсказки() {
        let apps = [app("us.zoom.xos", &[]), app("com.apple.Keynote", &["Слайды"])];
        assert_eq!(pick_app(&apps, Some("com.apple.Keynote"), Some("zoom-us")), Some(1));
    }

    #[test]
    fn выбранное_но_закрытое_уступает_звонилке() {
        let apps = [app("com.apple.finder", &[]), app("com.hnc.Discord", &[])];
        assert_eq!(pick_app(&apps, Some("us.zoom.xos"), None), Some(1));
    }

    #[test]
    fn подсказка_детектора_выбирает_из_нескольких_звонилок() {
        let apps = [app("us.zoom.xos", &[]), app("com.hnc.Discord", &[])];
        assert_eq!(pick_app(&apps, None, Some("discord")), Some(1));
        assert_eq!(pick_app(&apps, None, Some("zoom-us")), Some(0));
        assert_eq!(pick_app(&apps, None, Some("manual")), Some(0), "без подсказки — по приоритету");
    }

    #[test]
    fn браузер_берётся_только_со_встречей_в_заголовке() {
        let без_встречи = [app("com.google.Chrome", &["Входящие — Gmail"])];
        assert_eq!(pick_app(&без_встречи, None, None), None);
        let со_встречей = [
            app("com.apple.Safari", &["Новости"]),
            app("com.google.Chrome", &["Входящие", "Meet – abc-defg-hij"]),
        ];
        assert_eq!(pick_app(&со_встречей, None, Some("google-chrome")), Some(1));
    }

    #[test]
    fn нет_встречи_нет_окна() {
        let apps = [app("com.apple.finder", &["Загрузки"])];
        assert_eq!(pick_app(&apps, None, Some("manual")), None);
    }

    #[test]
    fn кадр_ужимается_до_1080p_с_пропорциями() {
        assert_eq!(target_size(3840, 2160), (1920, 1080));
        assert_eq!(target_size(3024, 1964), (1920, 1246));
        assert_eq!(target_size(1280, 801), (1280, 800), "чётные стороны");
        assert_eq!(target_size(1920, 1080), (1920, 1080));
    }

    #[test]
    fn битрейт_держит_час_в_пределах_гигабайта() {
        let b = bitrate(1920, 1080);
        assert!((900_000..=1_100_000).contains(&b), "{b}");
        assert_eq!(bitrate(320, 200), 500_000);
        assert_eq!(bitrate(7680, 4320), 2_500_000);
        // 2,5 Мбит/с × 3600 с / 8 ≈ 1,1 ГБ — худший случай за час.
        assert!(bitrate(7680, 4320) as u64 * 3600 / 8 < 1_200_000_000);
    }
}
