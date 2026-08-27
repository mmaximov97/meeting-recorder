//! Детект на macOS: известные процессы звонилок. Активность звука сюда не
//! входит намеренно — сигнал подмешивается в audio::run() поверх уже
//! посчитанного App::levels(), см. docs/2026-08-10-macos-port-design.md.

use sysinfo::{ProcessRefreshKind, RefreshKind, System};

/// Звонилки с собственным приложением: имя процесса — надёжный признак, ни с
/// чем не путается. Сверка точным равенством, а не подстрокой: у Discord рядом
/// живут `Discord Helper` и `Discord Helper (Renderer)`, и по подстроке один
/// открытый Discord давал бы три «сессии» с разными pid.
///
/// **Google Meet сюда добавить нельзя.** Встреча в Meet — вкладка браузера, и
/// имя процесса у неё «Google Chrome» / «Arc» / «Safari», то есть ровно то же,
/// что у почты и у YouTube. Гейт по звуку (`audio::Detect::gate`) от этого не
/// спасает: он проверяет, что звук ЕСТЬ, а не что это разговор, — значит любой
/// ролик в браузере поднимал бы вопрос «записать встречу?». См. тест
/// `встреча_в_google_meet_не_ловится_по_имени_процесса`.
const KNOWN_MEETING_PROCESSES: &[&str] = &["zoom.us", "Microsoft Teams", "Slack", "Discord"];

pub fn is_known_meeting_process(name: &str) -> bool {
    KNOWN_MEETING_PROCESSES.contains(&name)
}

/// Без полей: `sysinfo::System::refresh_processes` требует `&mut self`, а
/// `MeetingDetector::poll` даёт только `&self` — хранить `System` между
/// опросами и мутировать его через `RefCell` ради этого не стоит: пришлось
/// бы городить внутреннюю изменяемость ради экономии, которая на POLL_INTERVAL
/// в 2с не измерима. `new()` при этом не лишний: он даёт точку конструирования,
/// симметричную `WindowsDetector::new()`, и именно её зовёт `audio::run()`.
pub struct MacDetector;

impl MacDetector {
    pub fn new() -> Self {
        Self
    }
}

impl super::MeetingDetector for MacDetector {
    fn poll(&self) -> Result<Vec<super::MicSession>, super::DetectError> {
        let sys = System::new_with_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::new()));
        Ok(sys
            .processes()
            .values()
            .filter(|p| is_known_meeting_process(&p.name().to_string_lossy()))
            .map(|p| super::MicSession {
                pid: p.pid().as_u32(),
                process_name: p.name().to_string_lossy().to_string(),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_известная_звонилка() {
        assert!(is_known_meeting_process("zoom.us"));
    }

    #[test]
    fn teams_известная_звонилка() {
        assert!(is_known_meeting_process("Microsoft Teams"));
    }

    #[test]
    fn slack_известная_звонилка() {
        assert!(is_known_meeting_process("Slack"));
    }

    #[test]
    fn discord_известная_звонилка() {
        assert!(is_known_meeting_process("Discord"));
    }

    /// Вспомогательные процессы Discord не должны считаться отдельными
    /// сессиями: один открытый Discord — одна звонилка, а не три.
    #[test]
    fn хелперы_discord_не_считаются_звонилкой() {
        for h in [
            "Discord Helper",
            "Discord Helper (Renderer)",
            "Discord Helper (GPU)",
        ] {
            assert!(
                !is_known_meeting_process(h),
                "{h} — вспомогательный процесс, не звонилка"
            );
        }
    }

    /// Фиксирует решение, а не поведение кода: Google Meet по имени процесса
    /// не ловится и ловиться не будет. Вкладка Meet неотличима от вкладки с
    /// почтой — процесс у них общий. Если этот тест кто-то «починит», добавив
    /// браузер в список, приложение начнёт спрашивать «записать встречу?» на
    /// каждом видео в браузере.
    #[test]
    fn встреча_в_google_meet_не_ловится_по_имени_процесса() {
        for b in [
            "Google Chrome",
            "Google Chrome Helper (Renderer)",
            "Arc",
            "Safari",
        ] {
            assert!(!is_known_meeting_process(b), "{b} — браузер, а не звонилка");
        }
    }

    #[test]
    fn обычный_процесс_не_звонилка() {
        assert!(!is_known_meeting_process("Finder"));
        assert!(!is_known_meeting_process("Safari"));
    }

    /// Браузеры сознательно не в списке: они открыты почти всегда независимо
    /// от звонков (см. «Отказы» в дизайн-документе) — включи их, и эвристика
    /// «известный процесс» перестанет что-либо фильтровать.
    #[test]
    fn браузеры_не_считаются_известной_звонилкой() {
        for b in ["Google Chrome", "Safari", "Firefox", "Arc"] {
            assert!(!is_known_meeting_process(b), "{b} не должен считаться звонилкой");
        }
    }

    #[test]
    fn пустое_имя_не_звонилка() {
        assert!(!is_known_meeting_process(""));
    }
}
