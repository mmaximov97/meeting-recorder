//! Аудио-поток: здесь живёт всё, что `!Send`.
//!
//! `WindowsDetector` держит `IMMDeviceEnumerator` (COM привязан к апартаменту
//! потока), `App` внутри себя — `cpal::Stream`. Оба `!Send`, поэтому
//! конструируются ВНУТРИ [`run`] и отсюда не уезжают. Передать их сюда извне
//! нельзя ничем: ни `Arc<Mutex<_>>`, ни `unsafe impl Send` (последнее не
//! решение, а UB — COM-вызов с чужого апартамента).
//!
//! Отсюда же следует форма связи с UI: только сообщения. [`Ctl`] приходит по
//! каналу (`Sender` — `Send`), состояние уходит обратно через `AppHandle`
//! (`Send + Sync`). Ничего общего с UI-потоком, кроме этих двух вещей, у
//! аудио-цикла нет.

use meeting_recorder::app::{poll_to_event, App};
use meeting_recorder::detector::{MeetingDetector, MicSession, WindowsDetector, POLL_INTERVAL};
use meeting_recorder::session::{Event, State};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::tray;

/// Шаг цикла. В 10 раз чаще детекта — ради отзывчивости на команды из UI и
/// прокачки аудио из каналов захвата, а не ради самого детекта: детектор
/// опрашивается раз в `POLL_INTERVAL` (2 с), и это его глобальное свойство.
const TICK: Duration = Duration::from_millis(200);

/// Команда аудио-потоку.
///
/// Шире, чем `Event` из ядра, намеренно: `Toggle` и `Shutdown` — это вопросы к
/// текущему состоянию машины, а не события в ней. Ответить на них может только
/// тот, кто состояние знает, то есть сам аудио-цикл; в `enum Event` их места
/// нет, и добавлять его туда — значит менять готовое ядро ради удобства GUI.
pub enum Ctl {
    /// Однозначное событие: ответ на вопрос, старт, стоп.
    Event(Event),
    /// «Переключи»: хоткей и пункт трея не знают, что сейчас происходит, и знать
    /// не должны. Решение принимается здесь, по настоящему состоянию машины —
    /// иначе пришлось бы держать копию состояния в UI и ловить рассинхрон.
    Toggle,
    /// Выход. Обязан пройти через машину: см. [`run`].
    Shutdown,
}

/// Состояние в терминах UI: три ответа на вопрос «горит ли индикатор и пишем ли
/// мы». `Trigger` наружу не выносится — пользователю всё равно, чем запущена
/// запись, а `Finalizing` живёт доли секунды.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum UiState {
    Idle,
    Armed,
    Recording,
}

impl UiState {
    fn of(s: State) -> Self {
        match s {
            State::Idle => UiState::Idle,
            State::Armed => UiState::Armed,
            State::Recording(_) => UiState::Recording,
            // Finalizing — миг между стопом и Idle. Для UI это уже «не пишем»:
            // отдельная лампочка на 200 мс только моргала бы впустую.
            State::Finalizing => UiState::Idle,
        }
    }
}

/// Событие в машину + рассказать UI об ошибке.
///
/// Ошибка НЕ выходит наверх по `?` и не роняет цикл. В консольном бинаре ранний
/// выход ронял процесс — громко и заметно. Здесь ронять нечего: GUI переживёт
/// ошибку и будет выглядеть живым, ничего не записывая. Поэтому причина уходит
/// в UI событием, а цикл продолжает крутиться — ядро на ошибке уже свело машину
/// с миром (`App::on_event` сбрасывает в Idle), так что следующая встреча
/// начнётся с чистого листа.
fn feed(app: &mut App, handle: &AppHandle, e: Event, source: Option<&MicSession>) {
    if let Err(err) = app.on_event(e, source) {
        let _ = handle.emit("error", err.to_string());
        eprintln!("ошибка обработки {e:?}: {err}");
    }
}

/// Показать вопрос «записать?»: тост + окно.
///
/// Тост и окно вместе, а не по отдельности: кнопок «Записать»/«Нет» в тосте не
/// будет. `tauri-plugin-notification` 2.3 на десктопе кладёт в `notify-rust`
/// только title/body/icon/sound — `actions` он не прокидывает вовсе (сам
/// notify-rust кнопки на Windows умеет, плагин их не отдаёт). Поэтому тост здесь
/// — это «обрати внимание», а отвечают в окне, где кнопки настоящие.
///
/// Вдобавок тост — ненадёжный канал: Windows глушит его в полноэкранном режиме
/// (Focus Assist), а это ровно ситуация «идёт звонок». Окно переживает и это.
fn ask(handle: &AppHandle, source: &str) {
    let _ = handle.emit("ask", source);

    let _ = handle
        .notification()
        .builder()
        .title("Похоже, встреча")
        .body(format!("{source} — записать? Ответьте в окне приложения."))
        .show();

    // Вопрос без окна — это вопрос в пустоту: ответить негде.
    if let Some(w) = handle.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// Разослать состояние, если оно изменилось.
///
/// Только на изменении: цикл крутится 5 раз в секунду, и слать одно и то же в
/// webview и в трей — это шум на ровном месте.
fn sync(handle: &AppHandle, app: &App, last: &mut UiState) {
    let now = UiState::of(app.state());
    if now != *last {
        *last = now;
        let _ = handle.emit("state", now);
        tray::set_state(handle, now);
    }
}

/// Крутится в СВОЁМ потоке. Detector и App конструируются здесь и отсюда не
/// уезжают — оба `!Send`.
pub fn run(handle: AppHandle, rx: Receiver<Ctl>, dir: PathBuf) {
    let det = match WindowsDetector::new() {
        Ok(d) => d,
        Err(e) => {
            let _ = handle.emit("fatal", format!("детектор не поднялся: {e}"));
            eprintln!("детектор не поднялся: {e}");
            return;
        }
    };
    let mut app = App::new(dir);
    let me = std::process::id();
    let mut was_active = false;
    let mut active: Option<MicSession> = None;
    // None — «ещё не опрашивали», первый опрос идёт сразу. Считать
    // `Instant::now() - POLL_INTERVAL` нельзя: вычитание у Instant паникует,
    // если результат не представим.
    let mut last_poll: Option<Instant> = None;
    let mut last_ui = UiState::Idle;

    loop {
        if last_poll.is_none_or(|t| t.elapsed() >= POLL_INTERVAL) {
            last_poll = Some(Instant::now());
            match det.poll() {
                Ok(sessions) => {
                    // poll_to_event фильтрует НАШ pid: без этого приложение
                    // детектит само себя (мы держим мик в Armed и в записи),
                    // SessionGone не приходит никогда, запись не
                    // останавливается по концу звонка.
                    let (found, event) = poll_to_event(was_active, sessions, me);
                    active = found;
                    was_active = active.is_some();

                    if let Some(e) = event {
                        feed(&mut app, &handle, e, active.as_ref());
                        if let (true, Some(s)) = (app.state_is_armed(), active.as_ref()) {
                            ask(&handle, &s.process_name);
                        }
                    }
                }
                // Ошибку опроса нельзя трактовать как «сессия исчезла»: сбой
                // COM/WASAPI на один тик оборвал бы идущую авто-запись и тут же
                // переспросил «записать?» посреди встречи. Молча держим прошлое
                // состояние — ручной стоп у пользователя никто не отнимал.
                Err(e) => eprintln!("детектор: {e} (состояние сохранено)"),
            }
        }

        for c in rx.try_iter() {
            match c {
                Ctl::Event(e) => feed(&mut app, &handle, e, active.as_ref()),
                Ctl::Toggle => {
                    // В Armed ManualStart — это подтверждение (кольцо уезжает в
                    // файл), в Idle — старт с нуля, в записи — стоп. Решает
                    // машина, мы только выбираем событие по её состоянию.
                    let e = match app.state() {
                        State::Recording(_) => Event::ManualStop,
                        _ => Event::ManualStart,
                    };
                    feed(&mut app, &handle, e, active.as_ref());
                }
                Ctl::Shutdown => {
                    // Выход обязан пройти через машину, а не через process::exit:
                    // ManualStop закроет и финализирует файл, если запись идёт.
                    // hound пишет длину данных в заголовок только на finalize();
                    // убить процесс во время записи — это WAV, который
                    // существует, открывается и играет тишину.
                    //
                    // В Armed это тоже верно: там ManualStop = DiscardRing, то
                    // есть кольцо выброшено и микрофон отпущен.
                    feed(&mut app, &handle, Event::ManualStop, None);
                    handle.exit(0);
                    return;
                }
            }
        }

        if let Err(e) = app.pump_audio() {
            let _ = handle.emit("error", e.to_string());
            eprintln!("прокачка аудио: {e}");
        }
        sync(&handle, &app, &mut last_ui);
        std::thread::sleep(TICK);
    }
}
