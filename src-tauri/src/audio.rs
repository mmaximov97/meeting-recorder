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

use meeting_recorder::app::{poll_to_event, App, Levels};
use meeting_recorder::capture::DeviceChoice;
#[cfg(target_os = "macos")]
use meeting_recorder::detector::MacDetector;
#[cfg(target_os = "windows")]
use meeting_recorder::detector::WindowsDetector;
use meeting_recorder::detector::{MeetingDetector, MicSession, POLL_INTERVAL};
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

/// Через сколько проверка микрофона выключается сама.
const MONITOR_TIMEOUT: Duration = Duration::from_secs(60);

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
    /// Сменить микрофон. Вступает в силу со следующего открытия потоков —
    /// менять устройство под идущей записью значило бы порвать дорожку.
    SetMicDevice(DeviceChoice),
    /// Включить/выключить проверку микрофона. Как и `SetMicDevice`, это не
    /// событие машины: состояние записи от проверки не меняется.
    Monitor(bool),
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

    // Наверх идёт ИДЕНТИФИКАТОР эндпоинта: аудио-поток знает только его.
    // Человеческое имя подставляет тот, у кого есть конфиг, — окно (для баннера)
    // и код ниже (для тоста, который до окна не доходит).
    let warn = app.device_warning();
    if status.set_device_warning(warn.as_deref()) {
        let _ = handle.emit("device-warning", warn.clone());
        if let Some(id) = warn {
            // Тост показывается и при закрытом окне, поэтому имя ему нужно
            // здесь; конфиг читается только в момент изменения, а не каждый тик.
            let имя = crate::config::Config::load(handle)
                .mic_device_name
                .unwrap_or(id);
            let _ = handle
                .notification()
                .builder()
                .title("Пишется не тот микрофон")
                .body(format!(
                    "«{имя}» недоступен — запись идёт с системного по умолчанию."
                ))
                .show();
        }
    }

    // Тоста здесь нет намеренно: это не беда, а объяснение, и нужно оно ровно
    // тому, кто прямо сейчас смотрит в выпадашку. Всплывашка поверх экрана
    // посреди встречи была бы дороже пользы.
    if status.set_mic_deferred(app.mic_change_deferred()) {
        let _ = handle.emit("mic-deferred", app.mic_change_deferred());
    }
}

/// Решение по одной команде: какое событие уходит в машину и надо ли после этого
/// выходить. `None` — команда не событие машины вовсе.
///
/// Чистая функция по образцу `poll_to_event`, и ровно по той же причине: решение
/// «`Shutdown` → `ManualStop`» — это инвариант 6 («выход из трея финализирует
/// запись») в чистом виде, и жить оно должно там, где его можно прочитать
/// тестом, а не внутри `loop`, до которого тест не дотянется.
///
/// `SetMicDevice`/`Monitor` дают `None`, а не `unreachable!()`. `drain_ctl`
/// перехватывает оба варианта раньше и сюда их не пропускает, поэтому на
/// практике эта ветка не исполняется, но паника здесь была бы неверной ценой
/// за это предположение: она убивает единственный поток, который умеет писать
/// на диск, причём тихо — `status::fatal` от паники в этом потоке не
/// срабатывает, а «поток мёртв» всплывает только на следующей команде из GUI.
/// `None` несёт тот же факт «сюда дойти не должны», но не обрушивает поток,
/// если предположение всё же окажется неверным (например, после будущей
/// правки, убравшей перехват в `drain_ctl`).
fn ctl_to_event(c: &Ctl, s: State) -> Option<(Event, bool)> {
    match c {
        Ctl::Event(e) => Some((*e, false)),
        // В Armed ManualStart — это подтверждение (кольцо уезжает в файл), в
        // Idle — старт с нуля, в записи — стоп. Решает машина, мы только
        // выбираем событие по её состоянию.
        Ctl::Toggle => Some(match s {
            State::Recording(_) => (Event::ManualStop, false),
            _ => (Event::ManualStart, false),
        }),
        // Выход обязан пройти через машину, а не через process::exit: ManualStop
        // закроет и финализирует файл, если запись идёт. hound пишет длину данных
        // в заголовок только на finalize(); убить процесс во время записи — это
        // WAV, который существует, открывается и играет тишину.
        //
        // В Armed это тоже верно: там ManualStop = DiscardRing, то есть кольцо
        // выброшено и микрофон отпущен.
        Ctl::Shutdown => Some((Event::ManualStop, true)),
        // Не событие машины (см. `Ctl::SetMicDevice`): `drain_ctl` перехватывает
        // этот вариант ДО вызова `ctl_to_event` и сюда его не пропускает.
        Ctl::SetMicDevice(_) => None,
        // Тот же случай, что у `SetMicDevice`: `drain_ctl` перехватывает
        // `Monitor` раньше, чем дело доходит сюда.
        Ctl::Monitor(_) => None,
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
///
/// Отказ проверки обязан дойти до окна, а не только в stderr: в релизе консоли
/// нет, и молчаливый отказ выглядел бы как «нажал Проверить, полоска стоит» —
/// то есть неотличимо от «микрофон не слышит», ровно того, что эта кнопка и
/// должна различать. Но сам `drain_ctl` не знает про `AppHandle` (и не должен:
/// на нём держатся тесты проводки без Tauri), поэтому канал ошибки приходит
/// параметром.
fn drain_ctl(
    app: &mut App,
    rx: &Receiver<Ctl>,
    active: Option<&MicSession>,
    mut feed: impl FnMut(&mut App, Event, Option<&MicSession>),
    mut on_error: impl FnMut(String),
) -> bool {
    for c in rx.try_iter() {
        // Не события машины: состояние записи от них не меняется.
        if let Ctl::SetMicDevice(choice) = c {
            app.set_mic_device(choice);
            continue;
        }
        if let Ctl::Monitor(on) = c {
            if let Err(e) = app.set_monitor(on) {
                on_error(format!("проверка микрофона: {e}"));
            }
            continue;
        }
        // `None` здесь означает «команда не должна была сюда дойти» (см.
        // докблок `ctl_to_event`) — а не «ничего не делать» через панику.
        // `SetMicDevice`/`Monitor` перехвачены веткой выше, так что на
        // практике сюда попадают только команды с `Some`.
        let Some((e, quit)) = ctl_to_event(&c, app.state()) else {
            continue;
        };
        // На выходе источник не нужен: ManualStop закрывает то, что уже пишется,
        // а не начинает новое.
        feed(app, e, if quit { None } else { active });
        if quit {
            return true;
        }
    }
    false
}

/// Решение «слать ли `levels` в этом тике».
///
/// Без простоя было бы 5 событий в секунду в пустоту, поэтому в тишине (не
/// проверяем и не пишем) событие не шлётся. Но `was_monitoring` — отдельный
/// параметр, а не то же самое, что `is_monitoring`, и вот почему: за один тик
/// проверка может успеть выключиться — командой `Ctl::Monitor(false)` в
/// `drain_ctl`, автовыключением по таймауту или концом записи (`Action::
/// CloseFile` в ядре гасит флаг проверки как побочный эффект), — и к моменту,
/// когда вызывающий спрашивает это условие, `is_monitoring()` уже вернёт
/// `false`, а `state()` уже может быть `Idle`. Без `was_monitoring` это
/// финальное «проверка кончилась» терялось бы навсегда: окно не получило бы
/// событие с `monitoring: false`, кнопка и полоски остались бы висеть в
/// состоянии «идёт проверка» до следующей настоящей записи.
///
/// `was_monitoring` — это `is_monitoring()`, снятый ДО обработки команд,
/// таймера и событий машины за этот тик (см. вызов в [`run`]), то есть «было
/// ли что послушать, когда тик начинался».
fn should_emit_levels(was_monitoring: bool, is_monitoring: bool, state: State) -> bool {
    was_monitoring || is_monitoring || state != State::Idle
}

/// Порог подобран по аналогии с `peak()` в этом же файле — 0..1, где 1.0 —
/// полная шкала. 0.01 отсекает цифровой шум тишины, но ловит любую реальную
/// речь/музыку, которая на пике даёт на порядки больше.
#[cfg(target_os = "macos")]
const MEETING_AUDIO_THRESHOLD: f32 = 0.01;

/// На macOS «известный процесс» сам по себе ничего не значит — Zoom/Teams/Slack
/// держатся открытыми и без звонка. Настоящий звонок отличается тем, что
/// система при этом ещё и издаёт звук: сочетание процесса и уровня — тот же
/// сигнал, что на Windows даёт WASAPI-сессия в состоянии Active, только
/// собранный из двух источников вместо одного.
#[cfg(target_os = "macos")]
fn mac_should_arm(process_detected: bool, system_level: f32) -> bool {
    process_detected && system_level > MEETING_AUDIO_THRESHOLD
}

/// Сколько тихих опросов подряд гейт терпит, прежде чем признать звонок
/// законченным. 30 × `POLL_INTERVAL` (2 с) = минута тишины.
///
/// Гистерезис здесь несимметричен намеренно, и вот почему. Гейт стоит на СПИСКЕ
/// сессий (см. `Detect::gate`), то есть тишина для него неотличима от «процесс
/// закрыт»: провались уровень ниже порога на один опрос — и `poll_to_event`
/// выдал бы `SessionGone`, то есть в `Armed` выбросил бы кольцо, а в
/// `Recording(Auto)` остановил бы идущую запись. Пауза в разговоре длиннее
/// одного-двух опросов — норма, а не редкость: `level_sys` затухает в 0.7 за
/// тик 200 мс, так что три секунды, пока никто не говорит, уже дают уровень
/// ниже порога. Запись, которая обрывается, потому что собеседник замолчал, —
/// беда хуже той, ради которой гейт вообще существует.
///
/// Сама «тишина» на спаде считается по ОБЕИМ дорожкам, а не только по
/// системной, — см. `Detect::gate`.
///
/// Цена терпения — обратная: настоящий конец звонка на macOS виден только по
/// звуку (Zoom/Teams/Slack остаются открытыми), поэтому автозапись переживёт
/// конец встречи на эту же минуту и допишет минуту тишины в хвост файла. Хвост
/// тишины стоит места на диске, оборванная запись — самой встречи; выбор
/// сделан в пользу первого. Ручной стоп (кнопка/хоткей/трей) работает всё это
/// время как обычно и ждать минуту не заставляет.
///
/// Минуту, а не «пока процесс жив»: без верхней границы автозапись шла бы до
/// выхода из Zoom, а его держат открытым весь рабочий день.
#[cfg(target_os = "macos")]
const MAC_SILENT_POLLS_BEFORE_DROP: u32 = 30;

/// Вся память детекта между опросами: «была ли сессия на прошлом опросе» и —
/// только на macOS — счётчик тихих опросов подряд.
///
/// Существует отдельной структурой не ради порядка, а потому, что здесь жила
/// ошибка, которую невозможно было увидеть тестом. `was_active` раньше был
/// локальной переменной цикла [`run`], и защёлкивался он ДО того, как
/// применялся гейт по звуку:
///
/// ```text
/// let (found, event) = poll_to_event(was_active, sessions, me);
/// was_active = active.is_some();        // защёлкнуто по НЕгейтованному опросу
/// if let Some(e) = event { if gate_ok { feed(...) } }   // гейт — на событии
/// ```
///
/// `poll_to_event` выдаёт `SessionAppeared` только на фронте «сессий не было →
/// сессия есть». Гейт на событии этот фронт не откладывал, а уничтожал: Zoom,
/// открытый в 09:00 без звонка, поднимал `was_active` в `true` навсегда, а
/// единственное `SessionAppeared` выбрасывалось за отсутствием звука. В 09:30
/// человек заходил в настоящий звонок — и `poll_to_event(true, [zoom], me)`
/// молчал до самого закрытия Zoom. Авто-детект, то есть главная функция
/// приложения на этой платформе, не срабатывал никогда.
///
/// Дизайн-документ говорит ровно обратное («оба условия обязаны совпасть,
/// чтобы `MeetingDetector::poll()` вернул сессию») — гейт принадлежит ВХОДУ
/// `poll_to_event`, а не его выходу. Отсюда и форма этого типа: `was_active`
/// не переменная цикла, а поле, которое обновляется в том же месте, где
/// применяется гейт, — [`Detect::step`]. Разъехаться им теперь негде, а тест
/// может прогнать хоть сто опросов подряд без Tauri, детектора и Core Audio.
/// Это та же болезнь «хелпер покрыт, проводка нет», о которой предупреждают
/// докблоки `poll_to_event` и `drain_ctl`.
#[derive(Default)]
struct Detect {
    was_active: bool,
    #[cfg(target_os = "macos")]
    silent_polls: u32,
}

impl Detect {
    /// Решение по одному опросу детектора: кто теперь активная сессия и какое
    /// событие из этого следует. Вся межопросная память — внутри.
    fn step(
        &mut self,
        sessions: Vec<MicSession>,
        levels: Levels,
        me: u32,
    ) -> (Option<MicSession>, Option<Event>) {
        // Гейт — ДО `poll_to_event`, иначе он не откладывает решение, а
        // отменяет его (см. докблок типа).
        let sessions = self.gate(sessions, levels);
        let (active, event) = poll_to_event(self.was_active, sessions, me);
        self.was_active = active.is_some();
        (active, event)
    }

    /// На Windows гейта нет: WASAPI-сессия в состоянии Active — это уже полный
    /// сигнал «микрофон держат», второго источника ему не нужно.
    #[cfg(not(target_os = "macos"))]
    fn gate(&mut self, sessions: Vec<MicSession>, _levels: Levels) -> Vec<MicSession> {
        sessions
    }

    /// На macOS «известный процесс» сам по себе ничего не значит: Zoom/Teams/
    /// Slack держатся открытыми и без звонка. Сессия доезжает до
    /// `poll_to_event` только вместе со звуком — но НЕ пропадает от первой же
    /// паузы в разговоре (см. [`MAC_SILENT_POLLS_BEFORE_DROP`]).
    ///
    /// Пустой список сюда доходит и проходит насквозь: любая ветка ниже вернёт
    /// его же (пустой), то есть `SessionGone` на закрытие звонилки приходит
    /// сразу и терпению тишины не подлежит. Отдельной ветки на этот случай нет
    /// намеренно — она бы только обнуляла счётчик, а обнулять его там нечего:
    /// `was_active` поднимается в `true` ИСКЛЮЧИТЕЛЬНО через ветку «звук
    /// есть», которая счётчик и так обнуляет, поэтому просроченное значение не
    /// может дожить до следующего взвода.
    #[cfg(target_os = "macos")]
    fn gate(&mut self, sessions: Vec<MicSession>, levels: Levels) -> Vec<MicSession> {
        // ВОСХОДЯЩИЙ фронт — только по системной дорожке. Микрофон сюда
        // подмешивать нельзя: в `Idle` он закрыт и `level_mic` там ноль, а
        // будь он открыт («Проверить»), собственный кашель поднимал бы вопрос
        // «записать?» на голом факте открытого Zoom — ровно то, ради чего гейт
        // и написан.
        if mac_should_arm(true, levels.system) {
            self.silent_polls = 0;
            return sessions;
        }
        // Тишина. На восходящем фронте это отказ: «Zoom открыт» — не повод
        // предлагать запись. Решение при этом не теряется, а откладывается —
        // `was_active` останется `false`, и первый же звучащий опрос даст
        // честный `SessionAppeared`, хоть через полчаса.
        if !self.was_active {
            return Vec::new();
        }
        // НИСХОДЯЩИЙ фронт — по обеим дорожкам сразу. Тишина в системной
        // дорожке не означает «встреча кончилась», если говорим мы сами:
        // доклад на минуту, когда все на той стороне молчат или замьючены, —
        // обычное дело, а не экзотика. Без этой строки такой доклад закрывал
        // бы файл посреди встречи, а ответная реплика собеседника прилетала бы
        // новым «записать?» — одна встреча в двух файлах с дырой посередине.
        //
        // Асимметрия с восходящим фронтом безопасна ровно потому, что
        // микрофон открыт только в `Armed`/`Recording` (инвариант 1): досюда
        // доходят лишь те, у кого запись уже идёт, а в `Idle` `level_mic`
        // ноль и ничего не меняет.
        if mac_should_arm(true, levels.system.max(levels.mic)) {
            self.silent_polls = 0;
            return sessions;
        }
        // Звонок уже идёт, и молчат обе стороны: терпим до минуты, потом
        // считаем его законченным.
        self.silent_polls += 1;
        if self.silent_polls >= MAC_SILENT_POLLS_BEFORE_DROP {
            Vec::new()
        } else {
            sessions
        }
    }
}

/// Поднять тап — или сообщить, почему не вышло.
///
/// `MR_FORCE_NO_SYSTEM_AUDIO=1` заставляет его «не подняться», не трогая при
/// этом ни одного разрешения в системе. Тот же приём, что `MR_DEBUG_TIMING` и
/// `MR_DEBUG_POLL`: живого отказа не поймать иначе как отказавшись руками, а
/// путь, который включается один раз на первом запуске, обязан быть проверяем
/// не один раз.
///
/// Заведено не для красоты. `tccutil reset` оказался ненадёжным способом
/// вернуться к «разрешение ещё не спрашивали»: сброс и `ScreenCapture`, и
/// `AudioCapture` для нашего bundle id отрабатывает с «Successfully reset», а
/// тап после этого поднимается как ни в чём не бывало — то есть настоящая
/// запись разрешения живёт где-то ещё (ad-hoc подпись, cdhash, чужой client
/// id), и добраться до неё снаружи не удалось. Без этой переменной весь режим
/// одного микрофона — ветка, которую нельзя запустить ни в тесте, ни руками.
///
/// Опасности для релиза нет ровно в той же мере, что у соседних `MR_DEBUG_*`:
/// переменная включает деградацию, а не отключает проверку, — включивший её
/// получит громкое предупреждение на весь экран, а не тихо испорченную запись.
#[cfg(target_os = "macos")]
fn start_system_tap() -> Result<meeting_recorder::capture::SystemTap, String> {
    if std::env::var_os("MR_FORCE_NO_SYSTEM_AUDIO").is_some() {
        return Err("захват выключен вручную (MR_FORCE_NO_SYSTEM_AUDIO)".to_string());
    }
    meeting_recorder::capture::SystemTap::start().map_err(|e| e.to_string())
}

/// Крутится в СВОЁМ потоке. Detector и App конструируются здесь и отсюда не
/// уезжают — оба `!Send`.
pub fn run(handle: AppHandle, rx: Receiver<Ctl>, root: PathBuf, mic: DeviceChoice) {
    let status = handle.state::<Status>();
    // Решение живёт в ядре (`capture::macos`), а не здесь: требование к версии —
    // свойство Process Tap API, и отказывать по нему обязаны оба бинаря
    // одинаково и одними словами. Консоль зовёт ту же функцию.
    #[cfg(target_os = "macos")]
    if let Some(why) = meeting_recorder::capture::macos::unsupported_reason() {
        status::fatal(&handle, why);
        return;
    }
    #[cfg(target_os = "windows")]
    let det = WindowsDetector::new();
    #[cfg(target_os = "macos")]
    let det: Result<MacDetector, meeting_recorder::detector::DetectError> = Ok(MacDetector::new());
    let det = match det {
        Ok(d) => d,
        Err(e) => {
            // Через status::fatal, а не голым emit: это происходит в setup(), то
            // есть раньше, чем webview успевает подписаться, — событие ушло бы в
            // пустоту, и в релизе от ошибки не осталось бы следа вообще.
            status::fatal(&handle, format!("детектор не поднялся: {e}"));
            return;
        }
    };
    #[cfg(target_os = "windows")]
    let mut app = App::new(root, mic);
    // Тап поднимается ЗДЕСЬ, до `App`, и живёт до выхода из процесса: системная
    // дорожка на macOS — ресурс уровня процесса, а не записи (см. докблок
    // `capture::SystemTap`). `App::new` для этого не годится — она принимает
    // один `DeviceChoice`, которым такой захват не описывается.
    //
    // Отказ здесь НЕ фатален, хотя раньше был. `.expect(...)` означал панику в
    // единственном потоке, который умеет писать на диск, — причём тихую:
    // `status::fatal` не звался, `get_state` продолжал отдавать здоровое
    // состояние, окно скрыто (`"visible": false`), приложение —
    // `ActivationPolicy::Accessory`, паника уходит в unified log, а не в
    // терминал, которого у релиза нет. Человек, отказавший в разрешении на
    // захват системного звука (диалог липкий, снимается только `tccutil`),
    // получал внешне живое приложение, которое ничего не пишет, и узнавал об
    // этом на следующем нажатии хоткея.
    //
    // Сменивший панику `status::fatal` был честен, но всё ещё строг не по делу:
    // микрофон-то захватывается, и приложение, которое отказывается писать хоть
    // что-то, полезнее не становится. Дизайн-документ обещает ровно обратное —
    // деградацию с видимым предупреждением, — и теперь так и сделано.
    //
    // Цена режима не в одной дорожке, а в автодетекте: `mac_should_arm`
    // требует системного звука, которого без тапа не будет никогда, то есть
    // взвода не случится ни разу. Поэтому предупреждение говорит именно это
    // (см. `status::NO_SYSTEM_AUDIO`), а не «нет второй дорожки».
    #[cfg(target_os = "macos")]
    let mut app = {
        let audio: Box<dyn meeting_recorder::app::AudioIo> = match start_system_tap() {
            Ok(tap) => Box::new(meeting_recorder::app::MacAudio::new(
                mic,
                std::rc::Rc::new(std::cell::RefCell::new(tap)),
            )),
            Err(why) => {
                status::no_system_audio(&handle, why);
                Box::new(meeting_recorder::app::MacAudio::new_mic_only(mic))
            }
        };
        App::new_with_audio(root, audio)
    };
    let me = std::process::id();
    // Память детекта между опросами живёт здесь целиком — см. докблок `Detect`.
    let mut detect = Detect::default();
    let mut active: Option<MicSession> = None;
    // None — «ещё не опрашивали», первый опрос идёт сразу. Считать
    // `Instant::now() - POLL_INTERVAL` нельзя: вычитание у Instant паникует,
    // если результат не представим.
    let mut last_poll: Option<Instant> = None;
    // Проверка, забытая включённой, держала бы микрофон бесконечно. Таймер
    // считает здесь, а не в webview: окно можно закрыть, и выключать проверку
    // стало бы некому.
    let mut monitor_since: Option<Instant> = None;

    loop {
        // Снимок ДО обработки этого тика — единственный способ увидеть
        // переход «было включено → стало выключено» уже после того, как он
        // случился. См. докблок `should_emit_levels`.
        let was_monitoring = app.is_monitoring();

        if last_poll.is_none_or(|t| t.elapsed() >= POLL_INTERVAL) {
            last_poll = Some(Instant::now());
            match det.poll() {
                Ok(sessions) => {
                    // `Detect::step` фильтрует НАШ pid (через `poll_to_event`):
                    // без этого приложение детектит само себя (мы держим мик в
                    // Armed и в записи), SessionGone не приходит никогда,
                    // запись не останавливается по концу звонка. Он же на
                    // macOS применяет гейт по звуку — до `poll_to_event`, а не
                    // после, иначе гейт не откладывает детект, а отменяет его
                    // (см. докблок `Detect`).
                    let (found, event) = detect.step(sessions, app.levels(), me);
                    active = found;

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

        let quit = drain_ctl(
            &mut app,
            &rx,
            active.as_ref(),
            |a, e, src| feed(a, &handle, e, src),
            |msg| {
                let _ = handle.emit("error", msg.clone());
                eprintln!("{msg}");
            },
        );
        if quit {
            // Финализация уже прошла внутри drain_ctl — см. её докблок.
            handle.exit(0);
            return;
        }

        // Взводим/снимаем таймер по факту состояния, а не по команде: так копия
        // «включена ли проверка» не может разъехаться с истиной в App.
        match (app.is_monitoring(), monitor_since) {
            (true, None) => monitor_since = Some(Instant::now()),
            (false, Some(_)) => monitor_since = None,
            (true, Some(t)) if t.elapsed() >= MONITOR_TIMEOUT => {
                if let Err(e) = app.set_monitor(false) {
                    eprintln!("автовыключение проверки: {e}");
                }
                monitor_since = None;
            }
            _ => {}
        }

        if let Err(e) = app.pump_audio() {
            let _ = handle.emit("error", e.to_string());
            eprintln!("прокачка аудио: {e}");
        }
        sync(&handle, &app, &status);

        // Рассылка уровней — только когда есть что показывать (в простое это
        // было бы 5 событий в секунду в пустоту), но обязательно включая тик,
        // на котором проверка ТОЛЬКО ЧТО выключилась — иначе окно застревает
        // в «идёт проверка» навсегда. См. докблок `should_emit_levels`.
        if should_emit_levels(was_monitoring, app.is_monitoring(), app.state()) {
            let l = app.levels();
            let _ = handle.emit(
                "levels",
                serde_json::json!({
                    "mic": l.mic,
                    "system": l.system,
                    "monitoring": app.is_monitoring(),
                }),
            );
        }

        std::thread::sleep(TICK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meeting_recorder::session::Trigger;
    use std::sync::mpsc::channel;

    /// Захват-пустышка: ни микрофона, ни системной дорожки, ни COM.
    ///
    /// Нужен из-за `Ctl::Monitor`. `drain_ctl` перехватывает эту команду и
    /// зовёт настоящий `App::set_monitor`, а тот — в отличие от
    /// `set_mic_device`, который только запоминает выбор, — реально дёргает
    /// `AudioIo::open()`/`close()`. С боевым захватом это значило, что два
    /// теста этого файла на долю секунды по-настоящему брали микрофон (а на
    /// Windows ещё и WASAPI loopback) той машины, где идёт `cargo test`:
    /// результат зависел от того, свободно ли устройство. Теперь не зависит.
    ///
    /// Раньше подставить фейк было нечем: `App::with_backends` приватна, а
    /// `App::new` жёстко строила `CpalAudio`. `App::new_with_audio` (Task 4)
    /// именно эту дверь и открывает — трейт `AudioIo` публичный, реализовать
    /// его из другого крейта можно.
    ///
    /// ВНИМАНИЕ на будущее: `set_mic_device` здесь НЕ переопределён, то есть
    /// работает пустая реализация по умолчанию из трейта. Тест, который решит
    /// проверять проводку выбора устройства через этот фейк, пройдёт
    /// впустую — он не сможет отличить «выбор доехал» от «выбор потерян».
    /// Такому тесту нужен фейк с полем под последний выбор, как `ФейкAudio`
    /// в `src/app.rs`.
    #[derive(Default)]
    struct ФейкЗахват {
        открыт: bool,
    }

    impl meeting_recorder::app::AudioIo for ФейкЗахват {
        fn open(&mut self) -> Result<(), Box<dyn std::error::Error>> {
            self.открыт = true;
            Ok(())
        }

        fn close(&mut self) {
            self.открыт = false;
        }

        fn is_open(&self) -> bool {
            self.открыт
        }

        fn drain(&mut self) -> (Vec<i16>, Vec<i16>) {
            (Vec::new(), Vec::new())
        }
    }

    /// Захват, который всегда отказывает на `open()`, — «устройство занято
    /// другим приложением». Ровно тот же приём, что у `МикЗанят` в
    /// `src/app.rs`, и нужен он здесь ради ветки, которую иначе не пройти:
    /// отказ проверки микрофона обязан доехать до окна (см. докблок
    /// `drain_ctl`).
    struct ЗахватЗанят;

    impl meeting_recorder::app::AudioIo for ЗахватЗанят {
        fn open(&mut self) -> Result<(), Box<dyn std::error::Error>> {
            Err(Box::new(std::io::Error::other(
                "микрофон занят другим приложением",
            )))
        }

        fn close(&mut self) {}

        fn is_open(&self) -> bool {
            false
        }

        fn drain(&mut self) -> (Vec<i16>, Vec<i16>) {
            (Vec::new(), Vec::new())
        }
    }

    /// Уникальный временный каталог, удаляется в Drop.
    ///
    /// Та же конвенция, что в `src/app.rs`, `rename.rs` и `storage.rs`:
    /// pid + наносекунды + счётчик. Фиксированное имя в `$TMPDIR` не годится
    /// по двум причинам сразу — оно течёт (никто не убирает) и оно общее, то
    /// есть два параллельных теста (а `cargo test` многопоточен по умолчанию)
    /// подрались бы за один каталог.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path =
                std::env::temp_dir().join(format!("meeting-recorder-gui-{tag}-{pid}-{nanos}-{n}"));
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `App` в тесте — не подвиг: захват здесь подставной, а `App` сам по себе
    /// микрофон не открывает (инвариант 1, потоки поднимаются только на
    /// `Action::StartRingBuffer`).
    ///
    /// Корень — уникальный временный каталог. Ни один тест этого файла до
    /// файловой системы не доходит (все они подставляют свой `feed` и в машину
    /// ничего не заводят, а `SinkFactory` в `App::new_with_audio` жёстко
    /// `WavSinks`), поэтому каталог даже не создаётся; но если однажды дойдёт —
    /// запись уедет в `$TMPDIR` и будет убрана, а не осядет в рабочем каталоге
    /// и тем более не в настоящих записях пользователя.
    ///
    /// `App` держится вместе со своим временным корнем, а не отдельно от него:
    /// `ScratchDir` удаляет каталог в Drop, и брось мы его сразу — корень
    /// исчез бы из-под ещё живого `App`. `DerefMut` нужен, чтобы вызывающие
    /// писали привычное `&mut app()`, а не разбирали пару.
    struct Стенд {
        app: App,
        _dir: ScratchDir,
    }

    impl std::ops::Deref for Стенд {
        type Target = App;
        fn deref(&self) -> &App {
            &self.app
        }
    }

    impl std::ops::DerefMut for Стенд {
        fn deref_mut(&mut self) -> &mut App {
            &mut self.app
        }
    }

    fn app_с_захватом(audio: Box<dyn meeting_recorder::app::AudioIo>) -> Стенд {
        let dir = ScratchDir::new("audio");
        Стенд {
            app: App::new_with_audio(dir.0.clone(), audio),
            _dir: dir,
        }
    }

    fn app() -> Стенд {
        app_с_захватом(Box::new(ФейкЗахват::default()))
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
                Some((want, false)),
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
                Some((e, false)),
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
                Some((Event::ManualStop, true)),
                "состояние {s:?}"
            );
        }
    }

    /// Дешёвая находка ревью: раньше эти две ветки паниковали через
    /// `unreachable!()`. `drain_ctl` и сейчас не пропускает их сюда, но сам
    /// `ctl_to_event` обязан молча вернуть `None`, а не убить аудио-поток,
    /// если это предположение однажды перестанет быть верным.
    #[test]
    fn set_mic_device_и_monitor_дают_none() {
        assert_eq!(
            ctl_to_event(
                &Ctl::SetMicDevice(meeting_recorder::capture::DeviceChoice::Default),
                State::Idle
            ),
            None
        );
        assert_eq!(ctl_to_event(&Ctl::Monitor(true), State::Idle), None);
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
        let quit = drain_ctl(&mut app(), &rx, None, |_, e, _| seen.push(e), |_| {});

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
        assert!(drain_ctl(
            &mut app(),
            &rx,
            None,
            |_, e, _| seen.push(e),
            |_| {}
        ));

        assert_eq!(seen, vec![Event::UserConfirmed, Event::ManualStop]);
    }

    #[test]
    fn пустой_канал_это_не_повод_выходить() {
        let (_tx, rx) = channel::<Ctl>();
        let mut seen = Vec::new();
        assert!(!drain_ctl(
            &mut app(),
            &rx,
            None,
            |_, e, _| seen.push(e),
            |_| {}
        ));
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
        drain_ctl(
            &mut app(),
            &rx,
            Some(&s),
            |_, e, src| seen.push((e, src.map(|s| s.pid))),
            |_| {},
        );

        assert_eq!(
            seen,
            vec![(Event::ManualStart, Some(42)), (Event::ManualStop, None)]
        );
    }

    /// Проверка микрофона — не событие машины: состояние записи от неё не
    /// меняется, и в `feed` ничего уходить не должно.
    #[test]
    fn монитор_не_кормит_машину_событиями() {
        let (tx, rx) = channel();
        tx.send(Ctl::Monitor(true)).unwrap();
        tx.send(Ctl::Monitor(false)).unwrap();

        let mut seen = Vec::new();
        let quit = drain_ctl(&mut app(), &rx, None, |_, e, _| seen.push(e), |_| {});

        assert!(!quit);
        assert!(seen.is_empty(), "уехало в машину: {seen:?}");
    }

    /// Обратная сторона того же перехвата: `Ctl::Monitor` не кормит машину, но
    /// ОТКАЗ проверки обязан доехать до окна.
    ///
    /// Это ровно тот инвариант, который декларирует докблок `drain_ctl`
    /// («отказ проверки обязан дойти до окна, а не только в stderr»), и до сих
    /// пор его не проверял никто: с боевым захватом отказ `open()` зависел от
    /// того, занят ли микрофон на машине тестировщика, поэтому все прогоны
    /// глотали ошибку через `on_error: |_| {}`. С подставным захватом,
    /// который отказывает всегда, ветка стала детерминированной.
    ///
    /// Проверяется и то, что причина доехала ЦЕЛИКОМ: и наш префикс, и текст
    /// исходной ошибки. Без второй половины сообщение «проверка микрофона: »
    /// не сказало бы человеку ничего.
    #[test]
    fn отказ_проверки_микрофона_доезжает_до_окна() {
        let (tx, rx) = channel();
        tx.send(Ctl::Monitor(true)).unwrap();

        let mut seen = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        let quit = drain_ctl(
            &mut app_с_захватом(Box::new(ЗахватЗанят)),
            &rx,
            None,
            |_, e, _| seen.push(e),
            |msg| errors.push(msg),
        );

        assert!(!quit);
        assert!(seen.is_empty(), "уехало в машину: {seen:?}");
        assert_eq!(errors.len(), 1, "ошибки: {errors:?}");
        assert!(
            errors[0].contains("проверка микрофона"),
            "человек должен понять, ЧТО не получилось: {}",
            errors[0]
        );
        assert!(
            errors[0].contains("занят другим приложением"),
            "причина обязана доехать целиком, а не только наш префикс: {}",
            errors[0]
        );
    }

    /// Симметрично `монитор_не_кормит_машину_событиями`: смена микрофона тоже
    /// не событие машины — состояние записи от выбора устройства не меняется.
    /// Закрепляет инвариант, ради которого `ctl_to_event` отдаёт `None` вместо
    /// паники на `SetMicDevice`/`Monitor` (см. её докблок).
    #[test]
    fn set_mic_device_не_кормит_машину_событиями() {
        let (tx, rx) = channel();
        tx.send(Ctl::SetMicDevice(
            meeting_recorder::capture::DeviceChoice::Default,
        ))
        .unwrap();
        tx.send(Ctl::SetMicDevice(
            meeting_recorder::capture::DeviceChoice::Id("{0.0.1.00000000}.{guid}".into()),
        ))
        .unwrap();

        let mut seen = Vec::new();
        let quit = drain_ctl(&mut app(), &rx, None, |_, e, _| seen.push(e), |_| {});

        assert!(!quit);
        assert!(seen.is_empty(), "уехало в машину: {seen:?}");
    }

    #[test]
    fn shutdown_после_монитора_всё_равно_финализирует() {
        let (tx, rx) = channel();
        tx.send(Ctl::Monitor(true)).unwrap();
        tx.send(Ctl::Shutdown).unwrap();

        let mut seen = Vec::new();
        assert!(drain_ctl(
            &mut app(),
            &rx,
            None,
            |_, e, _| seen.push(e),
            |_| {}
        ));
        assert_eq!(seen, vec![Event::ManualStop]);
    }

    // ---- should_emit_levels -------------------------------------------------
    //
    // Регресс-тесты на находку ревью: финальное «проверка выключилась» обязано
    // долететь до окна, даже если к моменту проверки условия `is_monitoring()`
    // уже вернул `false`, а `state()` уже `Idle` — ровно то, что бывает после
    // `Ctl::Monitor(false)`, автовыключения по таймауту или конца записи.

    /// Гвоздь находки: без `was_monitoring` это условие читало бы уже погасший
    /// флаг и не отправило бы финальное событие вообще — кнопка и полоски в
    /// окне замерли бы в «идёт проверка» навсегда.
    #[test]
    fn финальное_выключение_проверки_всё_равно_шлёт_событие() {
        assert!(
            should_emit_levels(true, false, State::Idle),
            "переход «было включено → стало выключено» обязан долететь до окна"
        );
    }

    /// Обратная сторона: тишина без проверки и без записи не имеет права
    /// слать события — иначе 5 событий в секунду в пустоту, как и говорит
    /// докблок функции.
    #[test]
    fn простой_без_проверки_и_без_записи_не_шлёт_событие() {
        assert!(!should_emit_levels(false, false, State::Idle));
    }

    /// Проверка идёт (включалась ли она только что в этом тике или уже шла) —
    /// событие шлётся в обоих случаях.
    #[test]
    fn идущая_проверка_шлёт_событие_независимо_от_того_когда_она_включилась() {
        assert!(
            should_emit_levels(false, true, State::Idle),
            "включилась только что в этом тике"
        );
        assert!(
            should_emit_levels(true, true, State::Idle),
            "шла уже и до этого тика"
        );
    }

    /// Запись сама по себе — повод слать уровни, даже если проверку никто не
    /// нажимал: `state() != Idle` в OR ровно за это и отвечает.
    #[test]
    fn идущая_запись_шлёт_событие_даже_без_проверки() {
        assert!(should_emit_levels(false, false, State::Armed));
        assert!(should_emit_levels(
            false,
            false,
            State::Recording(Trigger::Manual)
        ));
    }

    // ---- mac_should_arm -------------------------------------------------
    //
    // На macOS «известный процесс» (Zoom/Teams/Slack) сам по себе не значит
    // «идёт звонок» — эти приложения держатся открытыми и без звонка.
    // Настоящий звонок отличается ещё и звуком: гейт требует ОБА сигнала.

    #[cfg(target_os = "macos")]
    #[test]
    fn известный_процесс_и_звук_дают_детект() {
        assert!(mac_should_arm(true, 0.05));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn известный_процесс_без_звука_не_детектится() {
        assert!(!mac_should_arm(true, 0.0));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn звук_без_известного_процесса_не_детектится() {
        assert!(!mac_should_arm(false, 0.5));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn порог_не_ловит_шум_на_грани_тишины() {
        assert!(!mac_should_arm(true, 0.001));
    }

    // ---- Detect: проводка детекта ------------------------------------------
    //
    // `mac_should_arm` покрыт выше как чистая функция — и ровно этого было
    // недостаточно. Бага C1 жила не в пороге, а в том, КУДА он подставлен:
    // гейт стоял на событии `SessionAppeared`, а `was_active` защёлкивался
    // строкой раньше по НЕгейтованному опросу, из-за чего фронт «сессии нет →
    // сессия есть» сгорал впустую и второй раз не наступал никогда. Это тот
    // самый разрыв «хелпер покрыт, проводка нет», о котором предупреждает
    // докблок `drain_ctl`. Тесты ниже гоняют именно проводку — по нескольку
    // опросов подряд, через `Detect::step`, где память состояния и гейт живут
    // в одном месте.
    //
    // Наш собственный pid — не 0 и не 1; `me` берём заведомо чужим, чтобы
    // фильтр своего pid не мешал (у него свой тест ниже).
    const ЧУЖОЙ_ME: u32 = 999_999;

    /// Пустой опрос — «известных процессов не видно».
    fn нет_сессий() -> Vec<MicSession> {
        Vec::new()
    }

    /// Говорит далёкая сторона — сигнал, по которому детект взводится.
    /// Микрофон при этом молчит: в `Idle` он вообще закрыт.
    const ЗВОНОК_СЛЫШЕН: Levels = Levels {
        system: 0.5,
        mic: 0.0,
    };
    /// Молчат обе стороны.
    ///
    /// Здесь и ниже — только для macOS: на Windows гейта нет вовсе, и
    /// непокрытая `cfg`'ом константа стала бы там мёртвым кодом.
    #[cfg(target_os = "macos")]
    const ТИШИНА: Levels = Levels {
        system: 0.0,
        mic: 0.0,
    };
    /// Говорим мы, далёкая сторона молчит: доклад, или все на той стороне
    /// замьючены. Микрофон открыт — значит запись уже идёт (инвариант 1).
    #[cfg(target_os = "macos")]
    const ГОВОРИМ_МЫ: Levels = Levels {
        system: 0.0,
        mic: 0.5,
    };

    /// База на обеих платформах: фронты считаются по опросам, а не по одному
    /// вызову, и повтор того же состояния событий не порождает.
    #[test]
    fn фронты_считаются_между_опросами_а_повтор_молчит() {
        let mut d = Detect::default();

        let (active, event) = d.step(нет_сессий(), ЗВОНОК_СЛЫШЕН, ЧУЖОЙ_ME);
        assert!(active.is_none());
        assert_eq!(
            event, None,
            "сессий не было и нет — событию взяться неоткуда"
        );

        let (active, event) = d.step(vec![session(7)], ЗВОНОК_СЛЫШЕН, ЧУЖОЙ_ME);
        assert_eq!(active.map(|s| s.pid), Some(7));
        assert_eq!(event, Some(Event::SessionAppeared));

        let (active, event) = d.step(vec![session(7)], ЗВОНОК_СЛЫШЕН, ЧУЖОЙ_ME);
        assert_eq!(active.map(|s| s.pid), Some(7));
        assert_eq!(
            event, None,
            "та же сессия на втором опросе — не новое событие"
        );

        let (active, event) = d.step(нет_сессий(), ЗВОНОК_СЛЫШЕН, ЧУЖОЙ_ME);
        assert!(active.is_none());
        assert_eq!(event, Some(Event::SessionGone));
    }

    /// Фильтр своего pid обязан работать и через `step`: иначе приложение
    /// детектит само себя, и `SessionGone` не приходит никогда.
    #[test]
    fn свой_pid_не_считается_сессией_и_через_step() {
        let мы = std::process::id();
        let mut d = Detect::default();
        let (active, event) = d.step(vec![session(мы)], ЗВОНОК_СЛЫШЕН, мы);
        assert!(active.is_none());
        assert_eq!(event, None);
    }

    /// **Регресс C1.** Zoom открыт в 09:00 без звонка, звонок начинается в
    /// 09:30. Гейт обязан ОТЛОЖИТЬ детект, а не отменить его.
    ///
    /// Против прежней проводки этот тест красный: там первый же тихий опрос
    /// защёлкивал `was_active = true`, единственное `SessionAppeared`
    /// выбрасывалось гейтом, и на звучащем опросе `poll_to_event(true, …)`
    /// возвращал `None` — до самого закрытия Zoom.
    #[cfg(target_os = "macos")]
    #[test]
    fn звонок_через_полчаса_после_запуска_zoom_всё_равно_детектится() {
        let mut d = Detect::default();

        // 09:00–09:30: процесс есть, звука нет. 900 опросов по 2 с — полчаса.
        for опрос in 0..900 {
            let (active, event) = d.step(vec![session(7)], ТИШИНА, ЧУЖОЙ_ME);
            assert!(
                active.is_none(),
                "опрос {опрос}: открытый Zoom без звука — ещё не звонок"
            );
            assert_eq!(event, None, "опрос {опрос}: предлагать запись нечего");
        }

        // 09:30: человек зашёл в звонок — в системе появился звук.
        let (active, event) = d.step(vec![session(7)], ЗВОНОК_СЛЫШЕН, ЧУЖОЙ_ME);
        assert_eq!(active.map(|s| s.pid), Some(7));
        assert_eq!(
            event,
            Some(Event::SessionAppeared),
            "звук пришёл позже процесса — детект обязан сработать именно сейчас, \
             а не быть съеденным первым тихим опросом"
        );
    }

    /// Обратная сторона того же гейта: взвод происходит на ЗВУЧАЩЕМ опросе, а
    /// не на первом тихом. Минимальная форма — два опроса.
    #[cfg(target_os = "macos")]
    #[test]
    fn взвод_приходится_на_звучащий_опрос_а_не_на_первый_тихий() {
        let mut d = Detect::default();
        assert_eq!(d.step(vec![session(7)], ТИШИНА, ЧУЖОЙ_ME).1, None);
        assert_eq!(
            d.step(vec![session(7)], ЗВОНОК_СЛЫШЕН, ЧУЖОЙ_ME).1,
            Some(Event::SessionAppeared)
        );
    }

    /// Гистерезис. Пауза в разговоре — не конец встречи: `SessionGone` на
    /// первом же тихом опросе выбросил бы кольцо в `Armed` и остановил идущую
    /// авто-запись.
    #[cfg(target_os = "macos")]
    #[test]
    fn пауза_в_разговоре_не_обрывает_идущий_звонок() {
        let mut d = Detect::default();
        assert_eq!(
            d.step(vec![session(7)], ЗВОНОК_СЛЫШЕН, ЧУЖОЙ_ME).1,
            Some(Event::SessionAppeared)
        );

        // Терпим на один опрос меньше порога — событий быть не должно вовсе.
        for опрос in 0..(MAC_SILENT_POLLS_BEFORE_DROP - 1) {
            let (active, event) = d.step(vec![session(7)], ТИШИНА, ЧУЖОЙ_ME);
            assert_eq!(active.map(|s| s.pid), Some(7), "тихий опрос {опрос}");
            assert_eq!(
                event, None,
                "тихий опрос {опрос}: собеседник просто молчит, встреча идёт"
            );
        }

        // Заговорили снова — счётчик тишины обнуляется, и следующая пауза
        // отсчитывается заново, а не добирает старую.
        assert_eq!(d.step(vec![session(7)], ЗВОНОК_СЛЫШЕН, ЧУЖОЙ_ME).1, None);
        for опрос in 0..(MAC_SILENT_POLLS_BEFORE_DROP - 1) {
            assert_eq!(
                d.step(vec![session(7)], ТИШИНА, ЧУЖОЙ_ME).1,
                None,
                "тишина после возобновления разговора, опрос {опрос}"
            );
        }
    }

    /// Верхняя граница терпения: минута полной тишины — это уже конец звонка.
    /// Без неё авто-запись шла бы до выхода из Zoom, то есть весь рабочий день.
    #[cfg(target_os = "macos")]
    #[test]
    fn минута_тишины_завершает_звонок() {
        let mut d = Detect::default();
        d.step(vec![session(7)], ЗВОНОК_СЛЫШЕН, ЧУЖОЙ_ME);

        // Собираем НОМЕР опроса вместе с событием: важно не только «звонок
        // однажды кончился», но и что это случилось ровно на пороге. Без
        // номера тест остался бы зелёным и в отсутствие гистерезиса вообще —
        // одно `SessionGone` там приходит на первом же тихом опросе.
        let mut события = Vec::new();
        for опрос in 1..=MAC_SILENT_POLLS_BEFORE_DROP {
            if let (_, Some(e)) = d.step(vec![session(7)], ТИШИНА, ЧУЖОЙ_ME) {
                события.push((опрос, e));
            }
        }
        assert_eq!(
            события,
            vec![(MAC_SILENT_POLLS_BEFORE_DROP, Event::SessionGone)],
            "ровно одно событие и ровно на {MAC_SILENT_POLLS_BEFORE_DROP}-м тихом опросе"
        );

        // После конца звонка приложение снова готово к следующему: звук в том
        // же процессе даёт честный новый `SessionAppeared`.
        assert_eq!(
            d.step(vec![session(7)], ЗВОНОК_СЛЫШЕН, ЧУЖОЙ_ME).1,
            Some(Event::SessionAppeared)
        );
    }

    /// **Регресс: доклад не имеет права закрыть файл.** Нисходящий фронт
    /// считает тишину по ОБЕИМ дорожкам. Пока говорим мы, встреча идёт — даже
    /// если на той стороне не звучит ничего часами.
    ///
    /// Против прежней формы (спад только по `level_sys`) этот тест красный: там
    /// минута собственного доклада давала `SessionGone`,
    /// `(Recording(Auto), SessionGone) => CloseFile` закрывал файл посреди
    /// встречи, а ответная реплика собеседника прилетала новым «записать?» —
    /// одна встреча в двух файлах с дырой посередине.
    #[cfg(target_os = "macos")]
    #[test]
    fn собственная_речь_держит_звонок_пока_собеседники_молчат() {
        let mut d = Detect::default();
        assert_eq!(
            d.step(vec![session(7)], ЗВОНОК_СЛЫШЕН, ЧУЖОЙ_ME).1,
            Some(Event::SessionAppeared)
        );

        // Втрое дольше порога терпения: три минуты доклада подряд.
        for опрос in 0..(MAC_SILENT_POLLS_BEFORE_DROP * 3) {
            let (active, event) = d.step(vec![session(7)], ГОВОРИМ_МЫ, ЧУЖОЙ_ME);
            assert_eq!(active.map(|s| s.pid), Some(7), "опрос {опрос}");
            assert_eq!(
                event, None,
                "опрос {опрос}: говорит пользователь — встреча идёт, файл закрывать нельзя"
            );
        }

        // И наоборот: как только замолчали ОБЕ стороны, терпение отсчитывается
        // с нуля и звонок всё-таки кончается — доклад не отменил гистерезис,
        // а только не дал ему сработать раньше времени.
        let mut события = Vec::new();
        for опрос in 1..=MAC_SILENT_POLLS_BEFORE_DROP {
            if let (_, Some(e)) = d.step(vec![session(7)], ТИШИНА, ЧУЖОЙ_ME) {
                события.push((опрос, e));
            }
        }
        assert_eq!(
            события,
            vec![(MAC_SILENT_POLLS_BEFORE_DROP, Event::SessionGone)]
        );
    }

    /// Обратная сторона той же асимметрии: на ВОСХОДЯЩЕМ фронте микрофон
    /// не считается. Иначе открытый Zoom плюс собственный кашель в режиме
    /// «Проверить» поднимал бы вопрос «записать?» — ровно то, ради чего гейт
    /// и написан. В `Idle` микрофон вообще закрыт и `level_mic` там ноль, но
    /// инвариант закрепляется явно: подмешать микрофон в `mac_should_arm` выше
    /// — однострочная и очень соблазнительная правка.
    #[cfg(target_os = "macos")]
    #[test]
    fn микрофон_без_системного_звука_не_взводит_детект() {
        let mut d = Detect::default();
        for опрос in 0..(MAC_SILENT_POLLS_BEFORE_DROP * 3) {
            let (active, event) = d.step(vec![session(7)], ГОВОРИМ_МЫ, ЧУЖОЙ_ME);
            assert!(active.is_none(), "опрос {опрос}");
            assert_eq!(
                event, None,
                "опрос {опрос}: свой звук — не признак звонка, взводит только системный"
            );
        }
    }
}
