mod app;
mod capture;
mod detector;
mod ringbuf;
mod session;
mod storage;

use app::App;
use detector::{MeetingDetector, MicSession, WindowsDetector, POLL_INTERVAL};
use session::Event;
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

/// Первая mic-сессия, которая не наша собственная.
///
/// Мы сами держим микрофон всё время, пока живы Armed и Recording (его берут на
/// детекте, до вопроса), и детектор честно возвращает нас в списке — он сообщает
/// факт «процесс держит мик», а не мнение о том, звонок это или нет. Отфильтровать
/// себя — обязанность потребителя, иначе собственный захват маскирует уход
/// настоящей сессии, и `SessionGone` не приходит НИКОГДА:
///
///  * в `Armed` — вопрос висит вечно, кольцо крутится, индикатор мика горит,
///    хотя встреча давно кончилась (сломан инвариант «сессия исчезла до ответа →
///    кольцо выброшено, микрофон отпущен»);
///  * в `Recording(Auto)` — запись не останавливается по концу звонка вообще,
///    только руками (сломано правило «Recording{Auto} — стоп по SessionGone»).
///
/// Ни детектор, ни захват поодиночке этой ошибки иметь не могут — она живёт ровно
/// в шве между ними, то есть появляется впервые здесь, когда оба модуля впервые
/// оказались в одном процессе.
fn first_foreign_session(sessions: Vec<MicSession>, me: u32) -> Option<MicSession> {
    sessions.into_iter().find(|s| s.pid != me)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = PathBuf::from(r"C:\Users\Cypher\Recordings");
    let det = WindowsDetector::new()?;
    let mut app = App::new(dir);
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
                    active = first_foreign_session(sessions, me);
                    let is_active = active.is_some();

                    if is_active && !was_active {
                        app.on_event(Event::SessionAppeared, active.as_ref())?;
                        if let (true, Some(s)) = (app.state_is_armed(), active.as_ref()) {
                            println!("Похоже, встреча ({}). Записать? y/n", s.process_name);
                            asked = true;
                        }
                    } else if !is_active && was_active {
                        app.on_event(Event::SessionGone, None)?;
                        asked = false;
                    }
                    was_active = is_active;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn сессия(pid: u32, name: &str) -> MicSession {
        MicSession {
            pid,
            process_name: name.into(),
        }
    }

    /// Регресс на реальную багу, найденную при сборке целого: пока мы держим
    /// микрофон (Armed/Recording), детектор возвращает нас самих, и без фильтра
    /// уход настоящей сессии становится невидимым — `SessionGone` не приходит.
    #[test]
    fn собственная_сессия_не_считается_встречей() {
        let sessions = vec![сессия(42, "meeting-recorder.exe")];
        assert_eq!(first_foreign_session(sessions, 42), None);
    }

    /// Именно этот случай ломался: мы уже в Armed (держим мик), Zoom вышел из
    /// звонка. Остаться должно «сессий нет» → SessionGone → кольцо выброшено.
    #[test]
    fn после_ухода_zoom_остаёмся_только_мы_и_это_значит_сессий_нет() {
        let sessions = vec![сессия(42, "meeting-recorder.exe")];
        assert_eq!(first_foreign_session(sessions, 42), None);
    }

    #[test]
    fn чужая_сессия_находится_даже_если_наша_идёт_первой() {
        let sessions = vec![сессия(42, "meeting-recorder.exe"), сессия(7, "Zoom.exe")];
        assert_eq!(
            first_foreign_session(sessions, 42),
            Some(сессия(7, "Zoom.exe"))
        );
    }

    #[test]
    fn пустой_список_даёт_none() {
        assert_eq!(first_foreign_session(Vec::new(), 42), None);
    }

    #[test]
    fn единственная_чужая_сессия_возвращается_как_есть() {
        let sessions = vec![сессия(7, "Zoom.exe")];
        assert_eq!(
            first_foreign_session(sessions, 42),
            Some(сессия(7, "Zoom.exe"))
        );
    }
}
