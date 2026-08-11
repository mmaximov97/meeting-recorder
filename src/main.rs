//! Консольный отладочный бинарь ядра.
//!
//! GUI живёт в `src-tauri/`, но этот бинарь остаётся и не является legacy: это
//! единственный способ прогнать детект и запись без webview. Когда что-то не
//! так со звуком, проверять надо здесь — Tauri добавляет свои слои, а ядро тут
//! ровно то же самое.

use meeting_recorder::app::{poll_to_event, App};
use meeting_recorder::detector::{MeetingDetector, MicSession, POLL_INTERVAL};
#[cfg(target_os = "windows")]
use meeting_recorder::detector::WindowsDetector;
#[cfg(target_os = "macos")]
use meeting_recorder::detector::MacDetector;
use meeting_recorder::session::Event;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::{thread, time::Duration, time::Instant};

/// Шаг цикла. В 10 раз чаще детекта — но только ради отзывчивости ввода и
/// прокачки аудио из каналов захвата, а не ради самого детекта.
const TICK: Duration = Duration::from_millis(200);

/// Ввод с консоли в отдельном потоке, чтобы не блокировать аудио-цикл.
fn spawn_stdin() -> Receiver<String> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines().map_while(Result::ok) {
            if tx.send(line.trim().to_lowercase()).is_err() {
                break;
            }
        }
    });
    rx
}

#[cfg(target_os = "windows")]
fn recordings_root() -> PathBuf {
    PathBuf::from(r"C:\Users\<username>\Recordings")
}

#[cfg(target_os = "macos")]
fn recordings_root() -> PathBuf {
    let home = std::env::var("HOME").expect("$HOME обязан быть установлен");
    PathBuf::from(home).join("Recordings")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = recordings_root();
    #[cfg(target_os = "windows")]
    let det = WindowsDetector::new()?;
    #[cfg(target_os = "macos")]
    let det = MacDetector::new();
    // Консоль — отладочный инструмент ядра, конфига у неё нет: всегда системный
    // дефолт. Выбор устройства живёт в GUI, где его есть где хранить.
    #[cfg(target_os = "windows")]
    let mut app = App::new(root, meeting_recorder::capture::DeviceChoice::Default);
    // На macOS захват собирается снаружи — тем же способом, что в
    // `src-tauri/src/audio.rs::run()`: системная дорожка это ресурс уровня
    // процесса, и одним `DeviceChoice`, который принимает `App::new`, она не
    // описывается.
    //
    // Отсюда следствие, которого у консоли раньше не было: она поднимает
    // процесс-тап, а значит при первом запуске покажет системный диалог
    // разрешения на захват звука и будет держать приватное агрегированное
    // устройство всё время работы. Это осознанно: бинарь существует ровно
    // затем, чтобы прогнать детект и запись без webview, а консоль, не умеющая
    // писать системную дорожку, эту работу не выполняет.
    #[cfg(target_os = "macos")]
    let mut app = {
        let system_tap = std::rc::Rc::new(std::cell::RefCell::new(
            meeting_recorder::capture::SystemTap::start()?,
        ));
        App::new_with_audio(
            root,
            Box::new(meeting_recorder::app::MacAudio::new(
                meeting_recorder::capture::DeviceChoice::Default,
                system_tap,
            )),
        )
    };
    let input = spawn_stdin();
    let me = std::process::id();
    // Живой звонок отлаживать нечем, кроме глаз: MR_DEBUG_POLL=1 печатает,
    // что именно детектор увидел на каждом опросе.
    let debug_poll = std::env::var_os("MR_DEBUG_POLL").is_some();

    println!("Готов. Команды: s — старт вручную, x — стоп, y/n — ответ на предложение, q — выход.");
    let mut was_active = false;
    let mut asked = false;
    let mut active: Option<MicSession> = None;
    // None — «ещё не опрашивали», первый опрос идёт сразу. Считать
    // `Instant::now() - POLL_INTERVAL` нельзя: вычитание у Instant паникует,
    // если результат не представим.
    let mut last_poll: Option<Instant> = None;

    loop {
        // Детектор опрашивается раз в POLL_INTERVAL (2 с) — это глобальное
        // ограничение, а не деталь этого цикла.
        if last_poll.is_none_or(|t| t.elapsed() >= POLL_INTERVAL) {
            last_poll = Some(Instant::now());
            match det.poll() {
                Ok(sessions) => {
                    if debug_poll {
                        eprintln!("[poll] мы pid={me} → {sessions:?}");
                    }
                    let (found, event) = poll_to_event(was_active, sessions, me);
                    active = found;
                    was_active = active.is_some();

                    match event {
                        Some(Event::SessionAppeared) => {
                            app.on_event(Event::SessionAppeared, active.as_ref())?;
                            if let (true, Some(s)) = (app.state_is_armed(), active.as_ref()) {
                                println!("Похоже, встреча ({}). Записать? y/n", s.process_name);
                                asked = true;
                            }
                        }
                        Some(Event::SessionGone) => {
                            app.on_event(Event::SessionGone, None)?;
                            asked = false;
                        }
                        // Других событий poll_to_event не порождает.
                        Some(_) | None => {}
                    }
                }
                // Ошибку опроса нельзя трактовать как «сессия исчезла»: сбой
                // COM/WASAPI на один тик оборвал бы идущую авто-запись и тут же
                // переспросил «записать?» посреди встречи. Молча держим прошлое
                // состояние — ручной стоп у пользователя никто не отнимал.
                Err(e) => eprintln!("детектор: {e} (состояние сохранено)"),
            }
        }

        let mut quit = false;
        for cmd in input.try_iter() {
            // Любая из команд снимает висящий вопрос: иначе `s` в Armed
            // оставил бы asked=true, и следующее `n` прилетело бы уже в
            // Recording, где UserDeclined проглатывается — человек сказал
            // «не записывать», а запись продолжила бы литься на диск.
            let e = match cmd.as_str() {
                "y" if asked => Some(Event::UserConfirmed),
                "n" if asked => Some(Event::UserDeclined),
                "s" => Some(Event::ManualStart),
                "x" => Some(Event::ManualStop),
                // Выход через машину, а не через process::exit: ManualStop
                // закроет и финализирует файл, если запись идёт (в Idle это
                // Action::None). Убить процесс на Ctrl+C во время записи —
                // это WAV с нулевой длиной в заголовке, то есть тишина.
                "q" => Some(Event::ManualStop),
                _ => None,
            };
            if cmd == "q" {
                quit = true;
            }
            if let Some(e) = e {
                asked = false;
                app.on_event(e, active.as_ref())?;
            }
        }

        app.pump_audio()?;
        std::io::stdout().flush().ok();
        if quit {
            return Ok(());
        }
        thread::sleep(TICK);
    }
}
