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

use crate::status::{self, Status};
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
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum UiState {
    #[default]
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

/// Записать состояние в [`Status`] и разослать, если оно изменилось.
///
/// Только на изменении: цикл крутится 5 раз в секунду, и слать одно и то же в
/// webview и в трей — это шум на ровном месте. «Изменилось ли» спрашивается у
/// самого `Status`, а не у локальной копии: копия рядом с истиной — это второй
/// источник правды, который однажды разъедется с первым.
///
/// Запись в `Status` идёт ПЕРЕД `emit` намеренно: кто спросит `get_state` в
/// промежутке, получит новое состояние, а следом ещё и событие о нём —
/// повторение безвредно, а вот обратный порядок дал бы окно, в котором событие
/// уже ушло, а спросивший получил бы старое.
fn sync(handle: &AppHandle, app: &App, status: &Status) {
    let now = UiState::of(app.state());
    if status.set_state(now) {
        let _ = handle.emit("state", now);
        tray::set_state(handle, now);
    }
}

/// Решение по одной команде: какое событие уходит в машину и надо ли после этого
/// выходить.
///
/// Чистая функция по образцу `poll_to_event`, и ровно по той же причине: решение
/// «`Shutdown` → `ManualStop`» — это инвариант 6 («выход из трея финализирует
/// запись») в чистом виде, и жить оно должно там, где его можно прочитать
/// тестом, а не внутри `loop`, до которого тест не дотянется.
fn ctl_to_event(c: &Ctl, s: State) -> (Event, bool) {
    match c {
        Ctl::Event(e) => (*e, false),
        // В Armed ManualStart — это подтверждение (кольцо уезжает в файл), в
        // Idle — старт с нуля, в записи — стоп. Решает машина, мы только
        // выбираем событие по её состоянию.
        Ctl::Toggle => match s {
            State::Recording(_) => (Event::ManualStop, false),
            _ => (Event::ManualStart, false),
        },
        // Выход обязан пройти через машину, а не через process::exit: ManualStop
        // закроет и финализирует файл, если запись идёт. hound пишет длину данных
        // в заголовок только на finalize(); убить процесс во время записи — это
        // WAV, который существует, открывается и играет тишину.
        //
        // В Armed это тоже верно: там ManualStop = DiscardRing, то есть кольцо
        // выброшено и микрофон отпущен.
        Ctl::Shutdown => (Event::ManualStop, true),
    }
}

/// Разобрать всё, что накопилось в канале. `true` — пришёл `Shutdown`, цикл
/// обязан закончиться.
///
/// Отделено от [`run`] не ради красоты. `ctl_to_event` сама по себе инварианта 6
/// не доказывает: она может честно вернуть `ManualStop`, а вызывающий — забыть
/// его скормить и выйти сразу. Это ровно та бага «в проводке», о которой
/// предупреждает докблок `poll_to_event`: хелпер покрыт, проводка нет. Поэтому
/// `feed` здесь — параметр: тест подставляет свой и смотрит, что именно уехало в
/// машину и в каком порядке относительно выхода.
fn drain_ctl(
    app: &mut App,
    rx: &Receiver<Ctl>,
    active: Option<&MicSession>,
    mut feed: impl FnMut(&mut App, Event, Option<&MicSession>),
) -> bool {
    for c in rx.try_iter() {
        let (e, quit) = ctl_to_event(&c, app.state());
        // На выходе источник не нужен: ManualStop закрывает то, что уже пишется,
        // а не начинает новое.
        feed(app, e, if quit { None } else { active });
        if quit {
            return true;
        }
    }
    false
}

/// Крутится в СВОЁМ потоке. Detector и App конструируются здесь и отсюда не
/// уезжают — оба `!Send`.
pub fn run(handle: AppHandle, rx: Receiver<Ctl>, dir: PathBuf) {
    let status = handle.state::<Status>();
    let det = match WindowsDetector::new() {
        Ok(d) => d,
        Err(e) => {
            // Через status::fatal, а не голым emit: это происходит в setup(), то
            // есть раньше, чем webview успевает подписаться, — событие ушло бы в
            // пустоту, и в релизе от ошибки не осталось бы следа вообще.
            status::fatal(&handle, format!("детектор не поднялся: {e}"));
            return;
        }
    };
    // Временно: всегда системный дефолт, конфиг устройства появится в Task 4.
    let mut app = App::new(dir, meeting_recorder::capture::DeviceChoice::Default);
    let me = std::process::id();
    let mut was_active = false;
    let mut active: Option<MicSession> = None;
    // None — «ещё не опрашивали», первый опрос идёт сразу. Считать
    // `Instant::now() - POLL_INTERVAL` нельзя: вычитание у Instant паникует,
    // если результат не представим.
    let mut last_poll: Option<Instant> = None;

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

        let quit = drain_ctl(&mut app, &rx, active.as_ref(), |a, e, src| {
            feed(a, &handle, e, src)
        });
        if quit {
            // Финализация уже прошла внутри drain_ctl — см. её докблок.
            handle.exit(0);
            return;
        }

        if let Err(e) = app.pump_audio() {
            let _ = handle.emit("error", e.to_string());
            eprintln!("прокачка аудио: {e}");
        }
        sync(&handle, &app, &status);
        std::thread::sleep(TICK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meeting_recorder::session::Trigger;
    use std::sync::mpsc::channel;

    /// `App` в тесте — не подвиг: `App::new` микрофон НЕ открывает (инвариант 1,
    /// потоки поднимаются только на `Action::StartRingBuffer`). Ни COM, ни
    /// устройств, ни каталога на диске здесь не появляется — `dir` нужен только
    /// как значение поля, до файловой системы дело не доходит.
    fn app() -> App {
        App::new(
            PathBuf::from(r"C:\nonexistent\meeting-recorder-tests"),
            meeting_recorder::capture::DeviceChoice::Default,
        )
    }

    fn session(pid: u32) -> MicSession {
        MicSession {
            pid,
            process_name: "zoom.exe".into(),
        }
    }

    #[test]
    fn finalizing_показывается_как_idle() {
        assert_eq!(UiState::of(State::Idle), UiState::Idle);
        assert_eq!(UiState::of(State::Armed), UiState::Armed);
        assert_eq!(
            UiState::of(State::Recording(Trigger::Auto)),
            UiState::Recording
        );
        assert_eq!(
            UiState::of(State::Recording(Trigger::Manual)),
            UiState::Recording,
            "чем запущена запись — не дело UI: лампа одна и та же"
        );
        // Миг между стопом и Idle. Для UI это уже «не пишем»: отдельная лампочка
        // на 200 мс только моргала бы впустую.
        assert_eq!(UiState::of(State::Finalizing), UiState::Idle);
    }

    #[test]
    fn toggle_в_записи_останавливает_а_в_остальном_стартует() {
        for (state, want) in [
            (State::Idle, Event::ManualStart),
            // В Armed ManualStart — подтверждение, а не второй старт.
            (State::Armed, Event::ManualStart),
            (State::Finalizing, Event::ManualStart),
            (State::Recording(Trigger::Auto), Event::ManualStop),
            (State::Recording(Trigger::Manual), Event::ManualStop),
        ] {
            assert_eq!(
                ctl_to_event(&Ctl::Toggle, state),
                (want, false),
                "состояние {state:?}"
            );
        }
    }

    #[test]
    fn однозначное_событие_проходит_как_есть() {
        for e in [
            Event::UserConfirmed,
            Event::UserDeclined,
            Event::ManualStart,
            Event::ManualStop,
        ] {
            assert_eq!(
                ctl_to_event(&Ctl::Event(e), State::Armed),
                (e, false),
                "{e:?} — ответ пользователя, состояние машины его не переписывает"
            );
        }
    }

    #[test]
    fn shutdown_это_manualstop_в_любом_состоянии() {
        for s in [
            State::Idle,
            State::Armed,
            State::Recording(Trigger::Auto),
            State::Recording(Trigger::Manual),
            State::Finalizing,
        ] {
            assert_eq!(
                ctl_to_event(&Ctl::Shutdown, s),
                (Event::ManualStop, true),
                "состояние {s:?}"
            );
        }
    }

    /// Инвариант 6 целиком, а не наполовину: проверяется не «`Shutdown` умеет
    /// вернуть `ManualStop`», а «`ManualStop` доехал до машины ДО того, как цикл
    /// закончился». Сломай проводку — верни `true` не скормив событие — и упадёт
    /// именно этот тест, а не тест `ctl_to_event`.
    #[test]
    fn shutdown_кормит_машину_и_только_потом_выходит() {
        let (tx, rx) = channel();
        tx.send(Ctl::Shutdown).unwrap();

        let mut seen = Vec::new();
        let quit = drain_ctl(&mut app(), &rx, None, |_, e, _| seen.push(e));

        assert!(quit, "Shutdown обязан закончить цикл");
        assert_eq!(
            seen,
            vec![Event::ManualStop],
            "выход обязан финализировать запись, иначе WAV играет тишину"
        );
    }

    #[test]
    fn команды_до_shutdown_доходят_после_него_нет() {
        let (tx, rx) = channel();
        tx.send(Ctl::Event(Event::UserConfirmed)).unwrap();
        tx.send(Ctl::Shutdown).unwrap();
        // Уже не наше дело: цикла, который это исполнит, больше нет.
        tx.send(Ctl::Event(Event::UserDeclined)).unwrap();

        let mut seen = Vec::new();
        assert!(drain_ctl(&mut app(), &rx, None, |_, e, _| seen.push(e)));

        assert_eq!(seen, vec![Event::UserConfirmed, Event::ManualStop]);
    }

    #[test]
    fn пустой_канал_это_не_повод_выходить() {
        let (_tx, rx) = channel::<Ctl>();
        let mut seen = Vec::new();
        assert!(!drain_ctl(&mut app(), &rx, None, |_, e, _| seen.push(e)));
        assert!(seen.is_empty());
    }

    /// Источник — это «кто позвонил», он нужен старту для имени файла. На выходе
    /// его быть не должно: `ManualStop` закрывает уже открытое.
    #[test]
    fn shutdown_идёт_без_источника_обычная_команда_с_ним() {
        let (tx, rx) = channel();
        tx.send(Ctl::Toggle).unwrap();
        tx.send(Ctl::Shutdown).unwrap();

        let s = session(42);
        let mut seen: Vec<(Event, Option<u32>)> = Vec::new();
        drain_ctl(&mut app(), &rx, Some(&s), |_, e, src| {
            seen.push((e, src.map(|s| s.pid)))
        });

        assert_eq!(
            seen,
            vec![(Event::ManualStart, Some(42)), (Event::ManualStop, None)]
        );
    }
}
