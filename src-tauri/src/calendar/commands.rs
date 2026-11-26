//! Команды календаря для окон и для меню иконки.

use super::keys::{keys, Keys};
use super::oauth::{self, Callback, Wait};
use super::runtime::{commit_login, settle_login, Auth, LoginKind, Runtime, StatusView};
use super::{emit_status, google, lock, status_now, token_store, Calendar};
use crate::audio::{Ctl, UiState};
use super::planner::Mode;
use crate::config::Config;
use meeting_recorder::session::Event;
use chrono::{TimeDelta, Utc};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

/// Сколько ждать ответа из браузера. Закрытая вкладка ответа не присылает
/// вовсе, и кнопка «Жду ответа из браузера…» не должна висеть вечно.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[tauri::command]
pub fn calendar_status(app: AppHandle) -> StatusView {
    // Открыли настройки — человек, скорее всего, только что поставил встречу и
    // ждёт её увидеть. Обычный опрос раз в 5 минут для этого слишком редкий,
    // поэтому будим его, если список старше 30 секунд. Новый статус придёт
    // событием `calendar-status`, как после любого опроса.
    let stale = {
        let rt = lock(&app);
        rt.auth == Auth::Connected && is_stale(rt.loaded_at, Utc::now())
    };
    if stale {
        app.state::<Calendar>().poll_now.notify_one();
    }
    status_now(&app)
}

/// Список встреч старше 30 секунд (или ещё не загружался) — пора обновить.
fn is_stale(loaded_at: Option<chrono::DateTime<Utc>>, now: chrono::DateTime<Utc>) -> bool {
    loaded_at.is_none_or(|t| now.signed_duration_since(t) > TimeDelta::seconds(30))
}

#[cfg(test)]
mod stale_tests {
    use super::is_stale;
    use chrono::{TimeDelta, Utc};

    #[test]
    fn не_загружался_значит_обновить() {
        assert!(is_stale(None, Utc::now()));
    }

    #[test]
    fn свежий_список_не_трогаем() {
        let now = Utc::now();
        assert!(!is_stale(Some(now - TimeDelta::seconds(10)), now));
    }

    #[test]
    fn старше_30_секунд_обновить() {
        let now = Utc::now();
        assert!(is_stale(Some(now - TimeDelta::seconds(31)), now));
    }
}

#[tauri::command]
pub async fn calendar_connect(app: AppHandle) -> Result<(), String> {
    connect(app).await
}

#[tauri::command]
pub fn calendar_cancel_connect(app: AppHandle) {
    if let Some(c) = &lock(&app).cancel {
        c.store(true, Ordering::SeqCst);
    }
}

#[tauri::command]
pub async fn calendar_disconnect(app: AppHandle) -> Result<(), String> {
    // Идёт вход — не даём ему подключить календарь после «Отключить».
    if let Some(c) = &lock(&app).cancel {
        c.store(true, Ordering::SeqCst);
    }
    let refresh = tauri::async_runtime::spawn_blocking(|| {
        let r = token_store::load().ok().flatten();
        token_store::delete();
        r
    })
    .await
    .map_err(|e| e.to_string())?;
    {
        let mut rt = lock(&app);
        let session = rt.session + 1;
        *rt = Runtime { session, ..Runtime::default() };
    }
    emit_status(&app);
    let _ = app.emit_to("ask", "calendar-hide", ());
    super::wake(&app);
    let mut cfg = Config::load(&app);
    cfg.calendar_connected = false;
    cfg.calendar_email = None;
    let saved = cfg.save(&app);
    if let Some(r) = refresh {
        google::revoke(&google::client(), &r).await;
    }
    saved
}

enum Login {
    Connected { email: Option<String>, refresh: String, access: String, expires_in: i64 },
    NoAccess,
    Abandoned,
    Failed(String),
}

impl Login {
    fn kind(&self) -> LoginKind {
        match self {
            Login::Connected { .. } => LoginKind::Connected,
            Login::NoAccess => LoginKind::NoAccess,
            Login::Abandoned => LoginKind::Abandoned,
            Login::Failed(_) => LoginKind::Failed,
        }
    }
}

/// Вход целиком: браузер → редирект → обмен кода → связка ключей → конфиг.
/// Его же зовёт пункт меню иконки «Войти заново…».
pub async fn connect(app: AppHandle) -> Result<(), String> {
    let keys = keys().ok_or_else(|| "календарь выключен в этой сборке".to_string())?;
    let cancel = Arc::new(AtomicBool::new(false));
    let (before, session) = {
        let mut rt = lock(&app);
        // Второй клик или пункт меню во время входа — второй вход не начинаем.
        if rt.auth == Auth::Waiting {
            return Ok(());
        }
        // Флаг свой на каждый вход: «Отмена» старого ожидания не сбрасывается новым.
        rt.cancel = Some(cancel.clone());
        let b = rt.auth;
        rt.auth = Auth::Waiting;
        (b, rt.session)
    };
    emit_status(&app);

    let login = run_login(&app, keys, cancel).await;
    let mut kind = login.kind();
    let result = match login {
        Login::Connected { email, refresh, access, expires_in } => {
            finish_login(&app, session, email, refresh, access, expires_in).await
        }
        Login::Failed(e) => {
            log::warn!("календарь: вход не удался: {e}");
            Err(e)
        }
        Login::NoAccess | Login::Abandoned => Ok(()),
    };
    if result.is_err() {
        kind = LoginKind::Failed;
    }
    {
        let mut rt = lock(&app);
        if settle_login(&mut rt, before, kind, session) {
            rt.cancel = None;
        }
    }
    emit_status(&app);
    if kind == LoginKind::Connected {
        app.state::<Calendar>().poll_now.notify_one();
    }
    result
}

async fn run_login(app: &AppHandle, keys: Keys, cancel: Arc<AtomicBool>) -> Login {
    let (listener, redirect) = match oauth::bind_loopback() {
        Ok(v) => v,
        Err(e) => return Login::Failed(format!("порт для входа: {e}")),
    };
    let pkce = oauth::Pkce::new();
    let state = oauth::new_state();
    let url = oauth::auth_url(keys.client_id, &redirect, &pkce.challenge, &state);
    if let Err(e) = crate::open_url(url) {
        return Login::Failed(e);
    }
    let lang = crate::i18n::active_lang(app);
    let ok = crate::i18n::t("calendar.browserDone", &lang);
    let fail = crate::i18n::t("calendar.browserFailed", &lang);
    let waited = tauri::async_runtime::spawn_blocking(move || {
        oauth::wait_for_callback(listener, &state, &cancel, LOGIN_TIMEOUT, &ok, &fail)
    })
    .await;
    let code = match waited {
        Ok(Wait::Got(Callback::Code(c))) => c,
        Ok(Wait::Got(Callback::Denied)) => return Login::NoAccess,
        Ok(Wait::Got(Callback::Failed(e))) => return Login::Failed(e),
        // Браузер молчит 5 минут: вкладку закрыли или Google показал свою
        // страницу ошибки без редиректа. Для человека это тот же «не дал доступ».
        Ok(Wait::TimedOut) => return Login::NoAccess,
        Ok(Wait::Cancelled) => return Login::Abandoned,
        Err(e) => return Login::Failed(e.to_string()),
    };
    match google::exchange_code(&google::client(), keys, &code, &pkce.verifier, &redirect).await {
        // Галочку календаря на экране согласия могли снять: токен без доступа
        // не сохраняем, это «Google не дал доступ».
        Ok(t) if !t.has_calendar_scope() => Login::NoAccess,
        Ok(t) => match t.refresh_token {
            Some(refresh) => Login::Connected { email: t.email, refresh, access: t.access_token, expires_in: t.expires_in },
            None => Login::Failed("Google не выдал refresh token".into()),
        },
        Err(e) => Login::Failed(format!("обмен кода: {e:?}")),
    }
}

async fn finish_login(
    app: &AppHandle,
    session: u64,
    email: Option<String>,
    refresh: String,
    access: String,
    expires_in: i64,
) -> Result<(), String> {
    // «Отключить» пришло уже после обмена кода — ничего не записываем.
    if lock(app).session != session {
        return Ok(());
    }
    tauri::async_runtime::spawn_blocking(move || token_store::save(&refresh))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("связка ключей: {e}"))?;
    let mut cfg = Config::load(app);
    cfg.calendar_connected = true;
    cfg.calendar_email = email.clone();
    cfg.save(app)?;
    let until = Utc::now() + TimeDelta::seconds(expires_in);
    let kept = commit_login(&mut lock(app), session, email, (access, until));
    if !kept {
        // «Отключить» пришло во время записи: откатываем связку и конфиг.
        let _ = tauri::async_runtime::spawn_blocking(token_store::delete).await;
        let mut cfg = Config::load(app);
        cfg.calendar_connected = false;
        cfg.calendar_email = None;
        cfg.save(app)?;
    }
    Ok(())
}

/// «Зайти и записать» (`record: true`) и «Подключиться без записи»
/// (`record: false`). Пункт меню иконки «Подключиться и записать» зовёт
/// [`join`] с `record: true`.
#[tauri::command]
pub fn calendar_join(id: String, record: bool, app: AppHandle) -> Result<(), String> {
    join(&app, &id, record)
}

/// Ссылка открывается системой (Zoom — в Zoom, Meet и Teams — в браузере),
/// запись стартует сразу. Не открылась ссылка — запись всё равно идёт,
/// встречу можно открыть руками (описание, «Состояния»).
///
/// Запись стартует командой `Ctl::JoinMeeting`: для машины это `ManualStart`
/// (идущую запись не гасит, в отличие от `Toggle`), а аудио-поток
/// останавливает её по концу звонка, как запись из «Записать?»
/// (`audio::MeetingWatch`). Звонок в браузере детектор не видит — тогда
/// запись останавливается руками.
///
/// «Без записи» тоже отказ: иначе через минуту звонилка займёт микрофон, и
/// по той же встрече всплывёт «Записать».
pub fn join(app: &AppHandle, id: &str, record: bool) -> Result<(), String> {
    let recording = app.state::<crate::status::Status>().snapshot().state == UiState::Recording;
    let (url, key, act) = {
        let mut rt = lock(app);
        let (url, start) = rt
            .meetings
            .iter()
            .find(|m| m.id == id)
            .map(|m| (m.url.clone(), m.start))
            .ok_or("встреча пропала из календаря")?;
        let key = (id.to_string(), start);
        let record_mode = rt
            .on_screen
            .as_ref()
            .is_some_and(|s| s.id == id && s.start == start && s.mode == Mode::Record);
        let act = decide(rt.opened.contains(&key), recording, record_mode, record);
        // Ничего не делаем - и метки не трогаем.
        if act.open_url || act.start_recording {
            rt.memory.shown.insert(key.clone());
            if !record {
                rt.memory.refused.insert(key.clone());
            }
            if rt.on_screen.as_ref().is_some_and(|s| s.id == id && s.start == start) {
                rt.on_screen = None;
            }
        }
        (url, key, act)
    };
    if act.open_url {
        match crate::open_url(url) {
            // «Открыта» только после успеха: при ошибке повторный клик пробует снова.
            Ok(()) => {
                lock(app).opened.insert(key);
            }
            Err(e) => log::warn!("календарь: ссылка встречи не открылась: {e}"),
        }
    }
    if act.start_recording {
        app.state::<crate::Cmd>()
            .send(Ctl::JoinMeeting)
            .inspect_err(|_| crate::status::fatal(app, crate::status::DEAD.to_string()))?;
    }
    super::wake(app);
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub struct JoinAct {
    pub open_url: bool,
    pub start_recording: bool,
}

/// Что делать по «Зайти». `opened` - ссылку этой встречи уже открывали.
/// Ссылка открывается один раз и не из режима «Записать» (звонилка уже в
/// звонке); запись стартует, если просили и она ещё не идёт. Во взводе
/// машина сама превращает `ManualStart` в запись (`Armed + ManualStart`).
pub fn decide(opened: bool, recording: bool, record_mode: bool, record: bool) -> JoinAct {
    JoinAct { open_url: !record_mode && !opened, start_recording: record && !recording }
}

/// Плашку смахнули. `mode: "record"` — машина во взводе, отказ уходит и в
/// неё (то же, что смахнуть старое «Записать встречу?»).
#[tauri::command]
pub fn calendar_dismiss(id: String, mode: String, app: AppHandle) -> Result<(), String> {
    {
        let mut rt = lock(&app);
        // Начало берём с экрана (каким оно было при показе), иначе из списка.
        let start = rt
            .on_screen
            .as_ref()
            .filter(|s| s.id == id)
            .map(|s| s.start)
            .or_else(|| rt.meetings.iter().find(|m| m.id == id).map(|m| m.start));
        if let Some(start) = start {
            rt.memory.refused.insert((id.clone(), start));
        }
        if rt.on_screen.as_ref().is_some_and(|s| s.id == id) {
            rt.on_screen = None;
        }
    }
    if mode == "record" {
        app.state::<crate::Cmd>()
            .send(Ctl::Event(Event::UserDeclined))
            .inspect_err(|_| crate::status::fatal(&app, crate::status::DEAD.to_string()))?;
    }
    super::wake(&app);
    Ok(())
}

#[cfg(test)]
mod join_tests {
    use super::*;

    fn act(open_url: bool, start_recording: bool) -> JoinAct {
        JoinAct { open_url, start_recording }
    }

    #[test]
    fn обычный_вход_открывает_ссылку_и_пишет() {
        assert_eq!(decide(false, false, false, true), act(true, true));
        assert_eq!(decide(false, false, false, false), act(true, false));
    }

    #[test]
    fn двойной_клик_второй_раз_ничего() {
        assert_eq!(decide(true, true, false, true), act(false, false));
    }

    #[test]
    fn без_записи_потом_с_записью_ссылку_не_открываем_только_запись() {
        assert_eq!(decide(true, false, false, true), act(false, true));
    }

    #[test]
    fn ошибка_открытия_ссылки_повтор_пробует_снова() {
        // opened не поставлен, запись уже идёт
        assert_eq!(decide(false, true, false, true), act(true, false));
    }

    #[test]
    fn из_режима_записать_только_запись_без_ссылки() {
        assert_eq!(decide(false, false, true, true), act(false, true));
        assert_eq!(decide(false, true, true, true), act(false, false));
    }
}
