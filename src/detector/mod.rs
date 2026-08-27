#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "macos")]
pub mod macos;

use std::time::Duration;

#[cfg(target_os = "windows")]
pub use self::windows::WindowsDetector;

#[cfg(target_os = "macos")]
pub use self::macos::MacDetector;

/// Активная сессия захвата микрофона: какой-то процесс держит мик.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicSession {
    pub pid: u32,
    pub process_name: String,
}

#[derive(Debug, thiserror::Error)]
pub enum DetectError {
    #[cfg(target_os = "windows")]
    #[error("ошибка COM/WASAPI: {0}")]
    Com(#[from] ::windows::core::Error),
}

/// Интервал опроса детектора — единственный источник правды для всех вызывающих.
///
/// Поллинг, а не события: перечисление каждый раз видит все сессии, включая
/// созданные до старта приложения.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Опрашивается раз в [`POLL_INTERVAL`].
pub trait MeetingDetector {
    fn poll(&self) -> Result<Vec<MicSession>, DetectError>;
}

/// Сырое имя процесса → идентификатор звонилки для интерфейса.
///
/// Одна таблица на обе системы, потому что имена процессов у них разные, а
/// звонилка одна и та же: `zoom.us` на macOS и `Zoom.exe` на Windows — это
/// Zoom, и логотип у него общий.
///
/// **Это НЕ детект.** Детект на двух системах устроен по-разному и правильно:
/// на macOS список имён и есть детект (кто держит микрофон, система без лишних
/// прав не показывает), а на Windows `WindowsDetector` видит настоящие сессии
/// захвата WASAPI. Заводить этот список в детекте Windows нельзя — перестанут
/// ловиться Zoom во вкладке браузера, Webex и всё, чего в списке нет.
///
/// **Имена файлов эта функция не трогает.** `recording_filename`
/// (`src/storage.rs:22`) как прогонял сырое имя через `sanitize_source`, так и
/// прогоняет: канонизация имён на диске переименовала бы и записи macOS, а
/// таблица в `ui/main.js` всё равно обязана понимать всё, что когда-либо было
/// записано.
pub fn canonical_source(raw: &str) -> &'static str {
    // Сначала lowercase, потом срез `.exe` — а не наоборот. Смешанный регистр
    // (`Zoom.Exe`) не совпадал ни с `.exe`, ни с `.EXE` при срезе ДО lowercase,
    // и обрезка молча не срабатывала: правило здесь обязано быть тем же самым,
    // что и в `sanitize_tail` (`src/storage.rs`) — иначе тултип и имя файла для
    // одного и того же процесса расходятся (см. докблок задачи).
    let имя = raw.trim().to_ascii_lowercase();
    let имя = имя.strip_suffix(".exe").unwrap_or(&имя);
    match имя {
        "zoom.us" | "zoom" => "zoom",
        "microsoft teams" | "ms-teams" | "teams" => "teams",
        "slack" => "slack",
        "discord" => "discord",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn имена_macos_канонизируются() {
        assert_eq!(canonical_source("zoom.us"), "zoom");
        assert_eq!(canonical_source("Microsoft Teams"), "teams");
        assert_eq!(canonical_source("Slack"), "slack");
        assert_eq!(canonical_source("Discord"), "discord");
    }

    /// Гвоздь задачи: на Windows имена другие, и до этой правки ни одно из них
    /// не совпадало ни с одной таблицей в интерфейсе — у каждого звонка был
    /// общий значок микрофона и сырое «.exe» в строке взвода.
    #[test]
    fn имена_windows_канонизируются_в_то_же_самое() {
        assert_eq!(canonical_source("Zoom.exe"), "zoom");
        assert_eq!(canonical_source("ms-teams.exe"), "teams");
        assert_eq!(canonical_source("Teams.exe"), "teams");
        assert_eq!(canonical_source("slack.exe"), "slack");
        assert_eq!(canonical_source("Discord.exe"), "discord");
    }

    /// Регистр имени процесса между системами и версиями не постоянен, а
    /// узнавание от него зависеть не должно.
    #[test]
    fn регистр_не_влияет() {
        assert_eq!(canonical_source("ZOOM.EXE"), "zoom");
        assert_eq!(canonical_source("slack"), "slack");
    }

    /// Гвоздь задачи: смешанный регистр расширения (`Zoom.Exe`) раньше не
    /// совпадал ни с `.exe`, ни с `.EXE` — срез не срабатывал, обрезанное имя
    /// не попадало ни в одну ветку `match`, и результатом был `unknown` вместо
    /// `zoom`. Правило теперь «сначала lowercase, потом срез», и регистру
    /// расширения (а не только регистру буквы `Z`) тоже полагается не влиять.
    #[test]
    fn смешанный_регистр_расширения_не_ломает_узнавание() {
        assert_eq!(canonical_source("Zoom.Exe"), "zoom");
        assert_eq!(canonical_source("Slack.eXe"), "slack");
    }

    /// Незнакомое имя — это «встреча, программу не узнали», а не пустота:
    /// интерфейс на этом идентификаторе рисует общий значок микрофона.
    #[test]
    fn незнакомое_имя_даёт_unknown() {
        assert_eq!(canonical_source("Finder"), "unknown");
        assert_eq!(canonical_source("explorer.exe"), "unknown");
        assert_eq!(canonical_source(""), "unknown");
    }

    /// Вспомогательные процессы Discord не должны выдавать себя за звонилку —
    /// то же правило, что уже зафиксировано в детекторе macOS.
    #[test]
    fn хелперы_не_звонилка() {
        assert_eq!(canonical_source("Discord Helper"), "unknown");
        assert_eq!(canonical_source("Google Chrome Helper (Renderer)"), "unknown");
    }

    /// Фиксирует решение, а не поведение: встреча в Meet — вкладка браузера, и
    /// имя процесса у неё то же, что у почты и у YouTube. Если этот тест
    /// «починят», добавив браузер, приложение начнёт спрашивать «записать
    /// встречу?» на каждом видео.
    #[test]
    fn браузер_не_звонилка() {
        for b in ["Google Chrome", "chrome.exe", "Arc", "Safari", "msedge.exe"] {
            assert_eq!(
                canonical_source(b),
                "unknown",
                "{b} — браузер, а не звонилка"
            );
        }
    }
}
