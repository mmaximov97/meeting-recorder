//! Запись экрана в GUI: свой рабочий поток, который идёт следом за записью звука.
//!
//! Видео не знает про машину состояний. Оно следит за одним фактом —
//! `App::current_recording` («прямо сейчас пишется пара дорожек под этой
//! основой»): появилась основа — старт, пропала — стоп. Поэтому любой старт и
//! стоп записи (пульт, трей, хоткей, детект, «Зайти и записать», выход из
//! приложения) запускает и останавливает экран одинаково, без второй проводки.
//!
//! Поток отдельный, а не аудио-цикл, потому что ScreenCaptureKit отвечает
//! асинхронно и иногда секундами: ждать его на тике, который качает звук, —
//! значит рвать звук ради картинки. Здесь наоборот: картинка не поднялась —
//! звук пишется как ни в чём не бывало, а человек получает объяснение.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use meeting_recorder::screen::{self, ScreenCapture, ScreenError, Showing, Target};
use meeting_recorder::storage::split_name;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;

use crate::config::Config;
use crate::i18n;

/// Как часто рабочий поток проверяет, не оборвалась ли запись сама.
const TICK: Duration = Duration::from_millis(500);

/// Сколько выход из приложения ждёт, пока видео допишется.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(15);

/// Обрыв записи в первые секунды после смены источника считается тем, что
/// `SCRecordingOutput` смену не пережил: тогда пишем дальше в новый файл,
/// а не бросаем видео до конца встречи.
const SWITCH_GRACE: Duration = Duration::from_secs(3);

enum Cmd {
    Start { dir: PathBuf, base: String },
    Stop,
    /// Выбор источника в конфиге поменялся — применить к идущей записи.
    Switch,
    Quit(SyncSender<()>),
}

/// Событие `screen` для пульта. Форма — контракт с интерфейсом
/// (`tmp/plans/screen-picker.md`): `state` — `on | failed | off`; `kind` —
/// `meeting | window | screen` (у `off` нет); `why` — короткий код сбоя
/// ([`why`]), текст для человека подставляет интерфейс; `detail` — исходный
/// текст ошибки, для подсказки и логов.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct ScreenEvent {
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Только при `state: "on"` + `why: "blocked"`: окно этого приложения не
    /// дало себя записать, и запись вернулась к прежнему источнику (его и
    /// описывают `kind`/`app`/`title`/`display`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}

impl ScreenEvent {
    fn off() -> Self {
        Self {
            state: "off",
            kind: None,
            app: None,
            title: None,
            display: None,
            why: None,
            detail: None,
            blocked: None,
        }
    }

    fn on(s: &Showing) -> Self {
        Self {
            state: "on",
            kind: Some(s.kind.as_str()),
            app: s.app.clone(),
            title: s.title.clone(),
            display: s.display.clone(),
            ..Self::off()
        }
    }

    /// Выбранный источник не дал ни одного кадра — `app`/`title` его.
    fn blocked(s: &Showing) -> Self {
        Self {
            state: "failed",
            why: Some("blocked"),
            ..Self::on(s)
        }
    }

    /// Новое окно не дало себя записать, пишется прежний источник `prev`.
    fn reverted(prev: &Showing, blocked: &Showing) -> Self {
        Self {
            why: Some("blocked"),
            blocked: blocked.app.clone(),
            ..Self::on(prev)
        }
    }

    fn failed(target: &Target, e: &ScreenError) -> Self {
        Self {
            state: "failed",
            kind: Some(target.kind().as_str()),
            why: Some(why(e)),
            detail: Some(e.to_string()),
            ..Self::off()
        }
    }
}

/// Короткий код ошибки для интерфейса: `permission`, `unsupported`,
/// `no_meeting_window` (окно встречи не нашлось), `no_window` (выбранного
/// окна нет), `blocked` (окно не даёт себя записать), `failed` (всё остальное).
pub fn why(e: &ScreenError) -> &'static str {
    match e {
        ScreenError::Permission => "permission",
        ScreenError::NoMeetingWindow => "no_meeting_window",
        ScreenError::NoWindow => "no_window",
        ScreenError::Blocked => "blocked",
        ScreenError::Unsupported(_) => "unsupported",
        ScreenError::Failed(_) => "failed",
    }
}

/// Последнее событие `screen` — для `get_screen_status`: пульт после
/// перезагрузки окна не должен ждать следующего события, чтобы узнать,
/// что пишется.
fn last() -> &'static Mutex<Option<ScreenEvent>> {
    static LAST: OnceLock<Mutex<Option<ScreenEvent>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(None))
}

pub fn status() -> Option<ScreenEvent> {
    last().lock().ok().and_then(|g| g.clone())
}

/// Канал в рабочий поток для команд Tauri: они живут не в аудио-потоке, где
/// лежит [`Follower`].
static SWITCH: OnceLock<Mutex<Sender<Cmd>>> = OnceLock::new();

/// Выбор источника сохранён — применить его к идущей записи (если идёт).
pub fn apply_source() {
    if let Some(tx) = SWITCH.get().and_then(|m| m.lock().ok()) {
        let _ = tx.send(Cmd::Switch);
    }
}

/// Живёт в аудио-потоке рядом с `App`.
pub struct Follower {
    tx: Sender<Cmd>,
    /// Основа, под которую видео уже запрошено. Сравнивается с
    /// `current_recording` на каждом тике.
    following: Option<(String, String)>,
}

impl Follower {
    pub fn spawn(handle: AppHandle, root: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        let _ = SWITCH.set(Mutex::new(tx.clone()));
        std::thread::Builder::new()
            .name("screen".into())
            .spawn(move || worker(handle, root, rx))
            .expect("поток записи экрана не поднялся");
        Self { tx, following: None }
    }

    /// Звать на каждом тике с `App::current_recording()`.
    pub fn follow(&mut self, now: Option<(String, String)>) {
        if now == self.following {
            return;
        }
        if self.following.is_some() {
            let _ = self.tx.send(Cmd::Stop);
        }
        if let Some((folder, base)) = &now {
            let _ = self.tx.send(Cmd::Start {
                dir: PathBuf::from(folder),
                base: base.clone(),
            });
        }
        self.following = now;
    }

    /// Остановить и дождаться, пока видео допишется. Для выхода из приложения:
    /// `handle.exit` сразу после него убил бы недописанный mp4.
    pub fn shutdown(&mut self) {
        let (ack, done) = mpsc::sync_channel(1);
        if self.tx.send(Cmd::Quit(ack)).is_ok() {
            let _ = done.recv_timeout(SHUTDOWN_WAIT);
        }
        self.following = None;
    }
}

fn emit(handle: &AppHandle, s: ScreenEvent) {
    let _ = handle.emit("screen", &s);
    if let Ok(mut g) = last().lock() {
        *g = Some(s);
    }
}

/// Тост — потому что во время встречи окно MeetRec обычно закрыто, а
/// «экран не пишется» узнать надо сейчас, а не после встречи.
fn toast(handle: &AppHandle, s: &ScreenEvent) {
    let lang = i18n::active_lang(handle);
    let body = match s.why {
        Some("permission") => i18n::t("screen.errPermission", &lang),
        Some("no_meeting_window") => i18n::t("screen.errNoWindow", &lang),
        Some("blocked") => {
            let app = s.blocked.as_deref().or(s.app.as_deref()).unwrap_or("");
            i18n::t_params("screen.errBlocked", &lang, &[("app", app)])
        }
        // Строку `screen.errWindowGone` добавляет интерфейс; пока её нет в
        // словаре, `t` вернул бы сам ключ — тогда текст про окно встречи.
        Some("no_window") => match i18n::t("screen.errWindowGone", &lang) {
            s if s == "screen.errWindowGone" => i18n::t("screen.errNoWindow", &lang),
            s => s,
        },
        _ => s.detail.clone().unwrap_or_default(),
    };
    let _ = handle
        .notification()
        .builder()
        .title(i18n::t("screen.errTitle", &lang))
        .body(body)
        .show();
}

fn fail(handle: &AppHandle, target: &Target, e: &ScreenError) {
    log::warn!("запись экрана не идёт: {e}");
    let s = ScreenEvent::failed(target, e);
    toast(handle, &s);
    emit(handle, s);
}

/// Цель по свежему конфигу: выбор могли поменять секунду назад.
fn target_now(handle: &AppHandle, base: &str) -> Option<Target> {
    let source = split_name(base).map(|(_, tail)| tail);
    Config::load(handle).screen_target(source)
}

/// Начать видео под основой. Файл — первый свободный: `.screen.mp4`, а если
/// он уже есть (экран выключали и включали посреди записи) — `.screen-2.mp4`…
fn start(handle: &AppHandle, dir: &Path, base: &str) -> Option<ScreenCapture> {
    let Some(target) = target_now(handle, base) else {
        emit(handle, ScreenEvent::off());
        return None;
    };
    let path = screen::free_video_path(dir, base);
    match ScreenCapture::start(&target, &path) {
        Ok(cap) => {
            emit(handle, ScreenEvent::on(cap.showing()));
            Some(cap)
        }
        Err(e) => {
            fail(handle, &target, &e);
            None
        }
    }
}

/// Остановить и дописать файл, ничего не сообщая пульту.
fn finish(handle: &AppHandle, cap: ScreenCapture) {
    match cap.stop() {
        Ok(p) => log::info!("видео записано: {}", p.display()),
        Err(e) => {
            log::error!("видео не дописалось: {e}");
            let _ = handle.emit("error", e.to_string());
        }
    }
}

/// Источник не дал ни одного кадра — такое видео не откроется: остановить
/// без ошибки и убрать файл, если ScreenCaptureKit успел его создать.
fn discard(cap: ScreenCapture) {
    let path = cap.path().to_path_buf();
    if let Err(e) = cap.stop() {
        log::info!("пустое видео остановлено: {e}");
    }
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
}

/// Сторож первого кадра (см. `ScreenCapture::blocked`).
fn check_blocked(handle: &AppHandle, active: &mut Option<ScreenCapture>) {
    let Some(after_switch) = active.as_mut().and_then(|c| c.blocked()) else { return };
    let Some(cap) = active.as_mut() else { return };
    let blocked = cap.showing().clone();
    log::warn!("источник не дал ни одного кадра за 3 с: {blocked:?}");
    if after_switch {
        if let Ok(prev) = cap.revert().cloned() {
            let s = ScreenEvent::reverted(&prev, &blocked);
            toast(handle, &s);
            emit(handle, s);
            return;
        }
    }
    if let Some(cap) = active.take() {
        if cap.has_frames() {
            finish(handle, cap);
        } else {
            discard(cap);
        }
    }
    let s = ScreenEvent::blocked(&blocked);
    toast(handle, &s);
    emit(handle, s);
}

fn stop(handle: &AppHandle, cap: ScreenCapture) {
    finish(handle, cap);
    emit(handle, ScreenEvent::off());
}

/// Сменить источник у идущей записи.
///
/// - Экран выключили — видео дописывается и закрывается.
/// - Записи экрана не было (была выключена или не поднялась) — начинается.
/// - Новая цель не нашлась — прежнее видео закрывается: писать дальше не то,
///   что выбрал человек, хуже, чем честное «не пишется».
/// - ScreenCaptureKit не смог переключить поток — то видео закрывается и
///   начинается новый файл `<основа>.screen-2.mp4`.
fn switch(handle: &AppHandle, active: &mut Option<ScreenCapture>, dir: &Path, base: &str) -> bool {
    let target = target_now(handle, base);
    match (target, active.as_mut()) {
        (None, Some(_)) => {
            if let Some(cap) = active.take() {
                stop(handle, cap);
            }
            false
        }
        (None, None) => {
            emit(handle, ScreenEvent::off());
            false
        }
        (Some(_), None) => {
            *active = start(handle, dir, base);
            false
        }
        (Some(target), Some(cap)) => match cap.switch(&target) {
            Ok(showing) => {
                let s = ScreenEvent::on(showing);
                emit(handle, s);
                true
            }
            Err(ScreenError::Failed(why)) => {
                log::warn!("источник на лету не сменился ({why}) — новый файл");
                if let Some(cap) = active.take() {
                    finish(handle, cap);
                }
                *active = start(handle, dir, base);
                false
            }
            Err(e) => {
                if let Some(cap) = active.take() {
                    finish(handle, cap);
                }
                fail(handle, &target, &e);
                false
            }
        },
    }
}

fn worker(handle: AppHandle, root: PathBuf, rx: Receiver<Cmd>) {
    let mut active: Option<ScreenCapture> = None;
    // Что сейчас пишется звуком: папка и основа. Нужна для смены источника
    // и для включения экрана посреди записи.
    let mut current: Option<(PathBuf, String)> = None;
    let mut switched_at: Option<Instant> = None;
    loop {
        match rx.recv_timeout(TICK) {
            Ok(Cmd::Start { dir, base }) => {
                if let Some(cap) = active.take() {
                    stop(&handle, cap);
                }
                let dir = root.join(dir);
                active = start(&handle, &dir, &base);
                current = Some((dir, base));
                switched_at = None;
            }
            Ok(Cmd::Stop) => {
                if let Some(cap) = active.take() {
                    stop(&handle, cap);
                }
                current = None;
            }
            Ok(Cmd::Switch) => {
                if let Some((dir, base)) = &current {
                    if switch(&handle, &mut active, dir, base) {
                        switched_at = Some(Instant::now());
                    }
                }
            }
            Ok(Cmd::Quit(ack)) => {
                if let Some(cap) = active.take() {
                    stop(&handle, cap);
                }
                let _ = ack.send(());
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                check_blocked(&handle, &mut active);
                let broke = active.as_mut().and_then(|c| c.failure());
                if let Some(why) = broke {
                    let showing = active.as_ref().map(|c| c.showing().clone());
                    // Дописываем то, что успело записаться; пустое — убираем.
                    if let Some(cap) = active.take() {
                        if cap.has_frames() {
                            let _ = cap.stop();
                        } else {
                            discard(cap);
                        }
                    }
                    let err = screen::classify_failure(why);
                    if let (ScreenError::Blocked, Some(showing)) = (&err, &showing) {
                        switched_at = None;
                        let s = ScreenEvent::blocked(showing);
                        toast(&handle, &s);
                        emit(&handle, s);
                        continue;
                    }
                    let fresh = switched_at.take().is_some_and(|t| t.elapsed() < SWITCH_GRACE);
                    match &current {
                        Some((dir, base)) if fresh => {
                            log::warn!("видео оборвалось сразу после смены источника ({err}) — новый файл");
                            active = start(&handle, dir, base);
                        }
                        _ => {
                            let target = current
                                .as_ref()
                                .and_then(|(_, base)| target_now(&handle, base))
                                .unwrap_or(Target::Meeting { source: None });
                            fail(&handle, &target, &err);
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(cap) = active.take() {
                    stop(&handle, cap);
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meeting_recorder::screen::Kind;

    /// Фронтенд разбирает событие по полю `state` — форма здесь контракт.
    #[test]
    fn событие_пульта_по_контракту() {
        let on = ScreenEvent::on(&Showing {
            kind: Kind::Window,
            app: Some("Zoom".into()),
            title: Some("Zoom Meeting".into()),
            display: None,
        });
        assert_eq!(
            serde_json::to_value(on).unwrap(),
            serde_json::json!({ "state": "on", "kind": "window", "app": "Zoom", "title": "Zoom Meeting" })
        );
        let screen = ScreenEvent::on(&Showing {
            kind: Kind::Screen,
            app: None,
            title: None,
            display: Some("L27e-30".into()),
        });
        assert_eq!(
            serde_json::to_value(screen).unwrap(),
            serde_json::json!({ "state": "on", "kind": "screen", "display": "L27e-30" })
        );
        let f = serde_json::to_value(ScreenEvent::failed(
            &Target::Window { id: 1, app_id: None, title: None },
            &ScreenError::NoWindow,
        ))
        .unwrap();
        assert_eq!(f["state"], "failed");
        assert_eq!(f["kind"], "window");
        assert_eq!(f["why"], "no_window");
        assert_eq!(serde_json::to_value(ScreenEvent::off()).unwrap(), serde_json::json!({ "state": "off" }));
    }

    #[test]
    fn запрет_записи_окна_по_контракту() {
        let zoom = Showing { kind: Kind::Window, app: Some("Zoom".into()), title: None, display: None };
        let aikon = Showing {
            kind: Kind::Window,
            app: Some("Aikon".into()),
            title: Some("Aikon Settings".into()),
            display: None,
        };
        assert_eq!(
            serde_json::to_value(ScreenEvent::blocked(&aikon)).unwrap(),
            serde_json::json!({ "state": "failed", "kind": "window", "app": "Aikon", "title": "Aikon Settings", "why": "blocked" })
        );
        assert_eq!(
            serde_json::to_value(ScreenEvent::reverted(&zoom, &aikon)).unwrap(),
            serde_json::json!({ "state": "on", "kind": "window", "app": "Zoom", "why": "blocked", "blocked": "Aikon" })
        );
        assert_eq!(why(&ScreenError::Blocked), "blocked");
    }

    #[test]
    fn коды_сбоя_короткие() {
        assert_eq!(why(&ScreenError::Permission), "permission");
        assert_eq!(why(&ScreenError::NoMeetingWindow), "no_meeting_window");
        assert_eq!(why(&ScreenError::Unsupported("x".into())), "unsupported");
        assert_eq!(why(&ScreenError::Failed("x".into())), "failed");
    }
}
