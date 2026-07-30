//! Разделяемая истина о состоянии: единственное место, где UI может СПРОСИТЬ, а
//! не только УСЛЫШАТЬ.
//!
//! Одного `emit` для этого мало, и не по недосмотру. Аудио-поток поднимается в
//! `setup()`, то есть ДО того, как webview выполнит `listen("fatal")`. Tauri не
//! буферизует события для поздних подписчиков — значит, `emit` из ранней ошибки
//! уходит в никуда, а в релизе (`windows_subsystem = "windows"`) от неё не
//! остаётся даже `eprintln`. Итог, который это чинит: живой GUI, серый трей,
//! «Ожидание встречи» — и ни одной записи.
//!
//! Поэтому истина живёт здесь, а `emit` — только уведомление о её изменении. Кто
//! опоздал на событие, тот спросит `get_state` (см. `main.rs`) и получит ровно
//! то же самое. Гонка «подписался позже, чем случилось» закрывается не тем, что
//! мы угадали момент, а тем, что момент перестал иметь значение.
//!
//! Это же чинит перезагрузку webview посреди записи: страница поднимается с
//! настоящим состоянием, а не с захардкоженным «Ожидание встречи» из `index.html`.

use crate::audio::UiState;
use crate::tray;
use serde::Serialize;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

/// Что случилось, когда команда не доехала до аудио-потока.
///
/// Один текст на все места, потому что случай один: канал закрыт (поток вышел)
/// или мьютекс отравлен (поток паниковал, держа его). И то и другое — навсегда:
/// поднимать аудио-поток заново некому, всё `!Send` умерло вместе с ним.
pub const DEAD: &str = "аудио-поток остановился: запись не работает, перезапустите приложение";

/// Всё, что UI должен знать о нас в любой момент времени.
#[derive(Serialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct Snapshot {
    pub state: UiState,
    /// `Some` — записи не будет до перезапуска. Не «ошибка на этом тике»
    /// (`emit("error")`), а «дальше ничего не работает».
    pub fatal: Option<String>,
    /// `Some(имя)` — просили этот микрофон, не нашли, пишем в системный дефолт.
    /// Не фатально (запись идёт), но молчать нельзя.
    pub device_warning: Option<String>,
}

/// Разделяемое состояние. Живёт в `tauri::State`, пишется аудио-потоком (и теми,
/// у кого не прошёл `send`), читается командой `get_state` из webview.
#[derive(Default)]
pub struct Status(Mutex<Snapshot>);

impl Status {
    /// Отравленный мьютекс здесь не повод терять состояние: `Snapshot` — это два
    /// независимых поля, порванной середины у него не бывает. Паниковать в ответ
    /// на чужую панику значило бы менять «GUI без записи» на «GUI без GUI».
    fn lock(&self) -> std::sync::MutexGuard<'_, Snapshot> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn snapshot(&self) -> Snapshot {
        self.lock().clone()
    }

    /// Записать состояние. `true` — оно изменилось, то есть есть о чём сообщать
    /// наружу. Проверка «изменилось ли» живёт здесь, а не в вызывающем: копия
    /// «последнего отправленного» рядом с настоящим состоянием — это второй
    /// источник правды, который однажды разъедется с первым.
    pub fn set_state(&self, s: UiState) -> bool {
        let mut g = self.lock();
        if g.state == s {
            return false;
        }
        g.state = s;
        true
    }

    /// Записать фатальную ошибку. `true` — она первая; о повторной сообщать
    /// незачем (и нечем: выхода из этого состояния нет).
    pub fn set_fatal(&self, msg: &str) -> bool {
        let mut g = self.lock();
        if g.fatal.is_some() {
            return false;
        }
        g.fatal = Some(msg.to_string());
        // Раз записи больше не будет, состояние обязано это отражать, а не
        // застыть на «Идёт запись»: аудио-поток, который его двигал, уже мёртв,
        // и `sync` больше никогда не поправит эту надпись.
        g.state = UiState::Idle;
        true
    }

    /// `true` — изменилось, есть о чём сообщать. Снятие предупреждения — тоже
    /// изменение: устройство могли вернуть, и висящий баннер врал бы.
    pub fn set_device_warning(&self, w: Option<&str>) -> bool {
        let mut g = self.lock();
        let new = w.map(str::to_string);
        if g.device_warning == new {
            return false;
        }
        g.device_warning = new;
        true
    }
}

/// Сообщить о фатальной ошибке всеми каналами разом.
///
/// Каналов три, потому что ни один по отдельности не доходит:
/// - `emit` — только для тех, кто уже подписан (а на старте не подписан никто);
/// - трей — единственная часть GUI, которая на экране всегда, но её надо
///   заметить;
/// - тост — единственный канал, который сам придёт к пользователю, но Windows
///   глушит его под Focus Assist.
///
/// Четвёртый канал — `Snapshot` + `get_state` — не здесь, а в самом факте
/// записи в `Status`: он работает для тех, кто ещё даже не загрузился.
pub fn fatal(app: &AppHandle, msg: String) {
    if !app.state::<Status>().set_fatal(&msg) {
        return;
    }
    eprintln!("фатально: {msg}");
    let _ = app.emit("fatal", msg.clone());
    tray::set_fatal(app, &msg);
    let _ = app
        .notification()
        .builder()
        .title("Запись не работает")
        .body(&msg)
        .show();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn снимок_по_умолчанию_это_idle_без_ошибки() {
        let s = Status::default();
        assert_eq!(
            s.snapshot(),
            Snapshot {
                state: UiState::Idle,
                fatal: None,
                device_warning: None,
            }
        );
    }

    #[test]
    fn set_state_сообщает_только_об_изменении() {
        let s = Status::default();
        assert!(s.set_state(UiState::Recording), "первое изменение — новость");
        assert!(!s.set_state(UiState::Recording), "то же самое — не новость");
        assert!(s.set_state(UiState::Idle));
        assert_eq!(s.snapshot().state, UiState::Idle);
    }

    /// Смысл всей находки 1: тот, кто спросит ПОСЛЕ ошибки, обязан её увидеть —
    /// независимо от того, слушал ли он `emit` в момент, когда она случилась.
    #[test]
    fn fatal_виден_в_снимке_тому_кто_опоздал_на_событие() {
        let s = Status::default();
        assert!(s.set_fatal("детектор не поднялся"));
        assert_eq!(s.snapshot().fatal.as_deref(), Some("детектор не поднялся"));
    }

    #[test]
    fn fatal_гасит_состояние_в_idle() {
        let s = Status::default();
        s.set_state(UiState::Recording);
        s.set_fatal("аудио-поток умер");
        assert_eq!(
            s.snapshot().state,
            UiState::Idle,
            "мёртвый поток не может «продолжать запись»"
        );
    }

    #[test]
    fn повторный_fatal_не_перетирает_первый() {
        let s = Status::default();
        assert!(s.set_fatal("первая причина"));
        assert!(!s.set_fatal("вторая причина"), "о повторной сообщать нечего");
        assert_eq!(
            s.snapshot().fatal.as_deref(),
            Some("первая причина"),
            "интересна причина, а не последствия"
        );
    }

    #[test]
    fn предупреждение_об_устройстве_сообщается_только_об_изменении() {
        let s = Status::default();
        assert!(s.set_device_warning(Some("Headset (Boss Bose)")));
        assert!(!s.set_device_warning(Some("Headset (Boss Bose)")));
        assert!(s.set_device_warning(None), "снятие — тоже изменение");
        assert_eq!(s.snapshot().device_warning, None);
    }

    /// Тот, кто открыл окно посреди записи, обязан увидеть предупреждение —
    /// ровно та же причина, по которой в снимке живёт fatal.
    #[test]
    fn предупреждение_видно_в_снимке() {
        let s = Status::default();
        s.set_device_warning(Some("Yeti"));
        assert_eq!(s.snapshot().device_warning.as_deref(), Some("Yeti"));
    }
}
