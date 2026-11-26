//! Google Календарь: встречи со ссылкой Zoom, Google Meet или Teams.
//!
//! Что видит человек — `tmp/plans/calendar-join.md`. Весь модуль живёт,
//! только если сборка получила ключи Google (`keys.rs`); без них раздел
//! «Календарь» скрыт, и приложение работает как раньше.

pub mod commands;
pub mod google;
pub mod keys;
pub mod menu;
pub mod model;
pub mod oauth;
pub mod planner;
mod poller;
pub mod runtime;
pub mod token_store;

use crate::config::Config;
use chrono::{DateTime, Utc};
use planner::{Mode, OnScreen, PopupAction, PopupPayload};
use runtime::{Auth, Runtime, StatusView};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

pub struct Calendar {
    pub rt: Mutex<Runtime>,
    /// Разбудить опрос раньше 5 минут: сразу после входа.
    pub poll_now: Arc<tokio::sync::Notify>,
    /// Разбудить тикер раньше 15 секунд: смена состояния записи, новый
    /// список встреч, ответ во всплывашке.
    pub tick_tx: Mutex<Option<std::sync::mpsc::Sender<()>>>,
}

pub fn available() -> bool {
    keys::keys().is_some()
}

/// Только из мест, где `init` гарантированно уже был (команды, опрос).
/// Аудио-поток и трей зовут `try_state`.
pub fn lock(app: &AppHandle) -> MutexGuard<'_, Runtime> {
    app.state::<Calendar>()
        .inner()
        .rt
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub fn status_now(app: &AppHandle) -> StatusView {
    let rt = lock(app);
    runtime::status_view(&rt, chrono::Utc::now(), available())
}

pub fn emit_status(app: &AppHandle) {
    let view = status_now(app);
    let _ = app.emit("calendar-status", view);
}

/// Зовётся из `setup()` до старта аудио-потока. Связку ключей здесь не
/// читаем: на старте это мог бы быть системный вопрос о доступе ровно в
/// момент запуска. Нет токена — узнаем на первом опросе (→ «отключился»).
pub fn init(app: &AppHandle) {
    let cfg = Config::load(app);
    let mut rt = Runtime::default();
    if available() && cfg.calendar_connected {
        rt.auth = Auth::Connected;
        rt.email = cfg.calendar_email.clone();
    }
    app.manage(Calendar {
        rt: Mutex::new(rt),
        poll_now: Arc::new(tokio::sync::Notify::new()),
        tick_tx: Mutex::new(None),
    });
    if available() {
        listen_menu(app);
        poller::spawn(app.clone());
        spawn_ticker(app.clone());
    }
}

/// Тик планировщика. Окна времени — минуты, 15 секунд погрешности не видно.
pub const TICK: Duration = Duration::from_secs(15);

pub enum DetectorAnswer {
    Classic,
    Calendar(PopupPayload),
    Silent,
}

pub fn wake(app: &AppHandle) {
    if let Some(cal) = app.try_state::<Calendar>() {
        if let Some(tx) = cal.tick_tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = tx.send(());
        }
    }
}

/// Всплывашку спрятали снаружи (`audio::hide_ask_popup` на выходе из взвода
/// или при старте записи).
pub fn popup_gone(app: &AppHandle) {
    if let Some(cal) = app.try_state::<Calendar>() {
        cal.rt.lock().unwrap_or_else(|e| e.into_inner()).on_screen = None;
    }
}

/// Детектор взвёлся: чем заменить старый вопрос. Без ключей, без входа и
/// без встреч — всегда `Classic`, то есть всё как раньше.
pub fn on_detector_armed(app: &AppHandle) -> DetectorAnswer {
    let Some(cal) = app.try_state::<Calendar>() else {
        return DetectorAnswer::Classic;
    };
    let mut rt = cal.rt.lock().unwrap_or_else(|e| e.into_inner());
    match planner::on_armed(&rt.meetings, Utc::now(), &rt.memory) {
        planner::Armed::Classic => DetectorAnswer::Classic,
        planner::Armed::Silent => DetectorAnswer::Silent,
        planner::Armed::Calendar(id) => {
            let found = rt.meetings.iter().find(|m| m.id == id).cloned();
            match found {
                Some(m) => {
                    rt.memory.shown.insert((id.clone(), m.start));
                    rt.on_screen = Some(OnScreen { id, start: m.start, mode: Mode::Record });
                    DetectorAnswer::Calendar(planner::payload(&m, Mode::Record))
                }
                None => DetectorAnswer::Classic,
            }
        }
    }
}

enum Apply {
    Nothing,
    Show(PopupPayload),
    Hide,
}

pub fn spawn_ticker(app: AppHandle) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    *app.state::<Calendar>().tick_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    std::thread::spawn(move || loop {
        match rx.recv_timeout(TICK) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => tick(&app, Utc::now()),
            Err(RecvTimeoutError::Disconnected) => return,
        }
    });
}

/// Один тик: посчитать план под замком, применить — без замка (показ окна
/// уходит на главный поток, держать там наш Mutex незачем).
fn tick(app: &AppHandle, now: DateTime<Utc>) {
    let Some(cal) = app.try_state::<Calendar>() else { return };
    let machine = app.state::<crate::status::Status>().snapshot().state;
    let lang = crate::i18n::active_lang(app);
    let (apply, lines) = {
        let mut rt = cal.rt.lock().unwrap_or_else(|e| e.into_inner());
        let plan = planner::plan(&rt.meetings, now, &rt.memory, machine, rt.on_screen.as_ref());
        let apply = match plan.popup {
            PopupAction::Show(id) => {
                let found = rt.meetings.iter().find(|m| m.id == id).cloned();
                match found {
                    Some(m) => {
                        rt.memory.shown.insert((id.clone(), m.start));
                        rt.on_screen = Some(OnScreen { id, start: m.start, mode: Mode::Join });
                        Apply::Show(planner::payload(&m, Mode::Join))
                    }
                    None => Apply::Nothing,
                }
            }
            PopupAction::Hide => {
                rt.on_screen = None;
                Apply::Hide
            }
            PopupAction::Stay => Apply::Nothing,
        };
        let tr = |k: &str, p: &[(&str, &str)]| crate::i18n::t_params(k, &lang, p);
        let lines = menu::lines(&plan.menu, rt.auth == Auth::Revoked, machine == crate::audio::UiState::Recording, &tr, &chrono::Local);
        let changed = rt.last_menu.as_ref() != Some(&lines);
        if changed {
            rt.last_menu = Some(lines.clone());
        }
        (apply, changed.then_some(lines))
    };
    match apply {
        Apply::Show(p) => crate::audio::show_calendar_popup(app, &p),
        Apply::Hide => {
            // Окно закрывает Rust, а не JS: событие нужно только для анимации.
            let _ = app.emit_to("ask", "calendar-hide", ());
            crate::audio::hide_ask_popup(app);
        }
        Apply::Nothing => {}
    }
    if let Some(lines) = lines {
        crate::tray::set_calendar_lines(app, lines);
    }
}

/// Язык сменился — тексты пунктов встреч надо собрать заново.
pub fn invalidate_menu(app: &AppHandle) {
    if let Some(cal) = app.try_state::<Calendar>() {
        cal.rt.lock().unwrap_or_else(|e| e.into_inner()).last_menu = None;
    }
    wake(app);
}

/// Нажатия на пункты календаря. Обработчик приложения, а не трея: Tauri
/// отдаёт событие меню всем подписчикам, и обработчик трея (`tray.rs`)
/// незнакомые id молча пропускает.
fn listen_menu(app: &AppHandle) {
    app.on_menu_event(|app, event| {
        let id = event.id().as_ref();
        if let Some(meeting) = id.strip_prefix("cal-join:") {
            if let Err(e) = commands::join(app, meeting, true) {
                log::warn!("календарь: {e}");
            }
        } else if id == "cal-reauth" {
            let h = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = commands::connect(h).await {
                    log::warn!("календарь: вход из меню не удался: {e}");
                }
            });
        }
    });
}
