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

/// Решение по одному опросу детектора: кто теперь активная сессия и какое
/// событие из этого следует.
///
/// Вынесено из `fn main` намеренно. Фикс само-детекта — это ДВЕ вещи: сама
/// `first_foreign_session` и то, что её результат заведён в `was_active`,
/// откуда и берутся `SessionAppeared`/`SessionGone`. Хелпер был покрыт
/// тестами, проводка — нет, а бага жила именно в проводке. Пока эта строка
/// стояла в `main`, её можно было откатить на `.next()`, и все тесты
/// остались бы зелёными: приложение снова детектило бы само себя, а
/// `SessionGone` не приходил бы никогда.
///
/// `was_active` передаётся, а не хранится: функция чистая — тот же вход даёт
/// тот же выход, и оба перехода (появление/уход) проверяются без цикла,
/// детектора и WASAPI.
fn poll_to_event(
    was_active: bool,
    sessions: Vec<MicSession>,
    me: u32,
) -> (Option<MicSession>, Option<Event>) {
    let active = first_foreign_session(sessions, me);
    let event = match (active.is_some(), was_active) {
        (true, false) => Some(Event::SessionAppeared),
        (false, true) => Some(Event::SessionGone),
        // Состояние не изменилось: повторный детект той же встречи — шум,
        // а повторное «сессий по-прежнему нет» — тем более.
        _ => None,
    };
    (active, event)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = PathBuf::from(r"C:\Users\<username>\Recordings");
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

    // ---- проводка фикса: poll_to_event ------------------------------------
    //
    // Тесты выше проверяют хелпер, эти — то, что его результат действительно
    // заведён в решение о событии. Мутация «вернуть `.next()` вместо фильтра»
    // (в любом из двух мест — в самой `first_foreign_session` или в вызове из
    // `poll_to_event`) валит именно эту группу.

    /// Гвоздь всей баги. Мы в Armed/Recording, то есть сами держим микрофон и
    /// сами же попадаем в список детектора. Zoom вышел из звонка — остались
    /// только мы. Это обязано читаться как «сессий нет» → `SessionGone`.
    ///
    /// Без фильтра своего pid `active` был бы `Some(мы)`, `is_active` осталось
    /// бы `true` при `was_active == true`, и события не случилось бы ВООБЩЕ:
    /// в Armed вопрос висел бы вечно с горящим микрофоном, в Recording(Auto)
    /// запись не остановилась бы по концу звонка никогда.
    #[test]
    fn уход_zoom_даёт_session_gone_даже_пока_мы_держим_микрофон() {
        let (active, event) = poll_to_event(true, vec![сессия(42, "meeting-recorder.exe")], 42);
        assert_eq!(active, None, "наш собственный захват — не встреча");
        assert_eq!(
            event,
            Some(Event::SessionGone),
            "уход настоящей сессии обязан быть виден, даже когда мик держим мы"
        );
    }

    /// Обратная сторона того же шва: собственный захват не имеет права
    /// выглядеть началом встречи — иначе приложение предложило бы записать
    /// само себя, а на `y` ушло бы в самоподдерживающийся детект.
    #[test]
    fn собственный_захват_не_поднимает_session_appeared() {
        let (active, event) = poll_to_event(false, vec![сессия(42, "meeting-recorder.exe")], 42);
        assert_eq!(active, None);
        assert_eq!(event, None, "мы сами себе не встреча");
    }

    /// Чужая сессия рядом с нашей — всё ещё встреча: фильтр обязан убирать
    /// ровно нас, а не всё подряд.
    #[test]
    fn zoom_рядом_с_нашей_сессией_остаётся_активной_встречей() {
        let (active, event) = poll_to_event(
            true,
            vec![сессия(42, "meeting-recorder.exe"), сессия(7, "Zoom.exe")],
            42,
        );
        assert_eq!(active, Some(сессия(7, "Zoom.exe")));
        assert_eq!(event, None, "встреча уже шла — повторного события не нужно");
    }

    #[test]
    fn появление_zoom_из_тишины_даёт_session_appeared() {
        let (active, event) = poll_to_event(false, vec![сессия(7, "Zoom.exe")], 42);
        assert_eq!(active, Some(сессия(7, "Zoom.exe")));
        assert_eq!(event, Some(Event::SessionAppeared));
    }

    #[test]
    fn исчезновение_последней_сессии_даёт_session_gone() {
        let (active, event) = poll_to_event(true, Vec::new(), 42);
        assert_eq!(active, None);
        assert_eq!(event, Some(Event::SessionGone));
    }

    /// Тишина в Idle — самый частый опрос, событий быть не должно.
    #[test]
    fn пустой_опрос_без_активной_сессии_не_даёт_события() {
        let (active, event) = poll_to_event(false, Vec::new(), 42);
        assert_eq!(active, None);
        assert_eq!(event, None);
    }
}
