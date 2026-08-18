//! Детект на macOS: известные процессы звонилок. Активность звука сюда не
//! входит намеренно — сигнал подмешивается в audio::run() поверх уже
//! посчитанного App::levels(), см. docs/2026-08-10-macos-port-design.md.

use sysinfo::{ProcessRefreshKind, RefreshKind, System};

const KNOWN_MEETING_PROCESSES: &[&str] = &["zoom.us", "Microsoft Teams", "Slack"];

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
