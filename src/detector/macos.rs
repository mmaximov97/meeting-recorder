//! Детект на macOS: два сигнала, и оба читаются без открытия устройств.
//!
//! 1. Известные процессы звонилок (`MeetingDetector::poll`, список ниже).
//! 2. «Микрофон держит кто-то, кроме нас» (`MacDetector::mic_in_use_by_others`):
//!    список аудио-процессов Core Audio и флаг `kAudioProcessPropertyIsRunningInput`
//!    у каждого. Это метаданные HAL — ни устройство, ни поток здесь не
//!    открываются, поэтому оранжевый индикатор микрофона от опроса не загорается.
//!
//! Соединяются сигналы в `audio::Detect::gate` (src-tauri): встреча — это
//! известная звонилка И чужой захват микрофона.
//!
//! # Почему не уровень системного звука (было до 2026-09-11)
//!
//! Раньше вторым сигналом был уровень системного звука. Мерить его было не из
//! чего, кроме процесс-тапа, поэтому тап жил весь процесс — а его агрегат
//! открывал вход устройства вывода. На гарнитуре с микрофоном (SteelSeries
//! Arctis Nova 5, AirPods) это значило «приложение слушает микрофон всё время»:
//! оранжевый индикатор горел, пока приложение запущено. Отключить входные
//! потоки агрегата не выходит (см. maven.de, «CoreAudio taps for dummies»).
//!
//! # Почему не `kAudioDevicePropertyDeviceIsRunningSomewhere`
//!
//! Свойство уровня устройства: оно `true`, когда у гарнитуры работает хотя бы
//! выход, то есть на любой музыке. И у Apple есть известная ошибка — на
//! Bluetooth-микрофонах оно не срабатывает. Флаг по процессам лишён обоих
//! недостатков; проверено на USB-гарнитуре и встроенном микрофоне.

use objc2_core_audio::{
    kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    kAudioProcessPropertyIsRunningInput, kAudioProcessPropertyPID, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress,
};
use std::ptr::{null, NonNull};
use sysinfo::{ProcessRefreshKind, RefreshKind, System};

/// Звонилки с собственным приложением: имя процесса — надёжный признак, ни с
/// чем не путается. Сверка точным равенством, а не подстрокой: у Discord рядом
/// живут `Discord Helper` и `Discord Helper (Renderer)`, и по подстроке один
/// открытый Discord давал бы три «сессии» с разными pid.
///
/// **Google Meet сюда добавить нельзя.** Встреча в Meet — вкладка браузера, и
/// имя процесса у неё «Google Chrome» / «Arc» / «Safari», то есть ровно то же,
/// что у почты и у YouTube. Гейт по чужому микрофону (`audio::Detect::gate`)
/// от этого не спасает: браузер держит микрофон и в голосовом поиске, и в
/// диктовке, — любая такая минута поднимала бы вопрос «записать встречу?».
/// См. тест `встреча_в_google_meet_не_ловится_по_имени_процесса`.
const KNOWN_MEETING_PROCESSES: &[&str] = &["zoom.us", "Microsoft Teams", "Slack", "Discord"];

pub fn is_known_meeting_process(name: &str) -> bool {
    KNOWN_MEETING_PROCESSES.contains(&name)
}

/// Чистая часть второго сигнала: держит ли вход хоть один процесс, кроме `me`.
///
/// `procs` — пары `(pid, IsRunningInput)`. Свой pid отбрасывается обязательно:
/// в `Armed` и в записи микрофон держим мы сами (и cpal-поток, и агрегат тапа
/// числятся за нашим процессом), и без фильтра звонок никогда не кончился бы —
/// та же ловушка, что у `first_foreign_session` в `src/app.rs`.
pub fn others_capturing_input(procs: &[(u32, bool)], me: u32) -> bool {
    procs.iter().any(|&(pid, running)| running && pid != me)
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

    /// Держит ли микрофон какой-нибудь процесс, кроме `me`.
    ///
    /// Читает только свойства объектов-процессов HAL
    /// (`kAudioHardwarePropertyProcessObjectList`, macOS 14+; ниже 14.4 мы и
    /// так не запускаемся). Ничего не открывает — индикатор микрофона от этого
    /// вызова не загорается.
    ///
    /// Процесс, пропавший между чтением списка и чтением его свойств, — не
    /// ошибка, а обычная гонка: такой объект пропускается. Ошибкой считается
    /// только отказ в самом списке.
    pub fn mic_in_use_by_others(&self, me: u32) -> Result<bool, super::DetectError> {
        let procs: Vec<(u32, bool)> = process_objects()?
            .into_iter()
            .filter_map(|p| {
                let running = read_u32(p, kAudioProcessPropertyIsRunningInput)? != 0;
                let pid = read_u32(p, kAudioProcessPropertyPID)?;
                Some((pid, running))
            })
            .collect();
        Ok(others_capturing_input(&procs, me))
    }
}

fn addr(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn check(status: i32, what: &'static str) -> Result<(), super::DetectError> {
    if status == 0 {
        Ok(())
    } else {
        Err(super::DetectError::CoreAudio { what, status })
    }
}

/// Все объекты-процессы, которые HAL сейчас знает.
fn process_objects() -> Result<Vec<AudioObjectID>, super::DetectError> {
    const WHAT: &str = "kAudioHardwarePropertyProcessObjectList";
    let system = kAudioObjectSystemObject as AudioObjectID;
    let mut a = addr(kAudioHardwarePropertyProcessObjectList);
    let mut size: u32 = 0;
    check(
        unsafe { AudioObjectGetPropertyDataSize(system, NonNull::from(&mut a), 0, null(), NonNull::from(&mut size)) },
        WHAT,
    )?;
    // Запас на процессы, появившиеся между двумя вызовами: HAL заполнит сколько
    // влезет и честно вернёт в `size`, сколько записал.
    let n = size as usize / std::mem::size_of::<AudioObjectID>() + 8;
    let mut ids: Vec<AudioObjectID> = vec![0; n];
    let mut size = (n * std::mem::size_of::<AudioObjectID>()) as u32;
    check(
        unsafe {
            AudioObjectGetPropertyData(
                system,
                NonNull::from(&mut a),
                0,
                null(),
                NonNull::from(&mut size),
                NonNull::from(&mut ids[0]).cast(),
            )
        },
        WHAT,
    )?;
    ids.truncate(size as usize / std::mem::size_of::<AudioObjectID>());
    Ok(ids)
}

/// 32-битное свойство объекта. `None` — объект не ответил (обычно: процесс
/// уже завершился).
fn read_u32(obj: AudioObjectID, selector: u32) -> Option<u32> {
    let mut a = addr(selector);
    let mut v: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            obj,
            NonNull::from(&mut a),
            0,
            null(),
            NonNull::from(&mut size),
            NonNull::from(&mut v).cast(),
        )
    };
    (status == 0).then_some(v)
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
    /// каждой диктовке в браузере.
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

    // ---- второй сигнал: чужой захват микрофона -----------------------------

    const МЫ: u32 = 4242;

    #[test]
    fn никто_не_держит_вход_значит_микрофон_свободен() {
        assert!(!others_capturing_input(&[], МЫ));
        assert!(!others_capturing_input(&[(1, false), (2, false)], МЫ));
    }

    #[test]
    fn чужой_захват_входа_это_сигнал() {
        assert!(others_capturing_input(&[(1, false), (777, true)], МЫ));
    }

    /// Гвоздь: в `Armed` и в записи вход держим мы сами (микрофон и агрегат
    /// тапа). Засчитай себя — и звонок не кончился бы никогда.
    #[test]
    fn собственный_захват_не_считается() {
        assert!(!others_capturing_input(&[(МЫ, true)], МЫ));
        assert!(!others_capturing_input(&[(МЫ, true), (1, false)], МЫ));
    }

    #[test]
    fn чужой_захват_виден_и_рядом_с_нашим() {
        assert!(others_capturing_input(&[(МЫ, true), (777, true)], МЫ));
    }

    /// Живой вызов: только метаданные HAL, ни одного устройства не открывает,
    /// поэтому безопасен на любой машине с `cargo test`. Результат зависит от
    /// того, что сейчас запущено, — проверяется лишь то, что список читается.
    #[test]
    fn список_аудио_процессов_читается() {
        MacDetector::new()
            .mic_in_use_by_others(std::process::id())
            .expect("kAudioHardwarePropertyProcessObjectList обязан читаться на macOS 14.4+");
    }
}
