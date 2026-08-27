// В релизе консольного окна за GUI быть не должно. Раньше это означало, что
// eprintln! из аудио-потока в релизе улетал в никуда — единственным способом
// увидеть, что происходит, была отладочная сборка с консолью. С
// tauri-plugin-log (см. main()) лог теперь пишется в файл в любой сборке;
// консоль в отладке остаётся удобством, а не единственным источником истины.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod config;
mod imbalance;
mod rename;
mod status;
mod transcribe;
mod tray;

use audio::Ctl;
use config::Config;
use imbalance::Cache;
use meeting_recorder::session::Event;
use serde::Serialize;
use status::{Snapshot, Status};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, WindowEvent};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tauri_plugin_log::{Target, TargetKind};

/// Корень записей. Тот же, что у консольного бинаря (`src/main.rs`): разъедься
/// эти два пути, GUI перестал бы показывать записи, сделанные консолью, — а
/// консоль остаётся инструментом отладки того же ядра.
///
/// Конкретная запись ложится в месячную подпапку, см. `storage::month_dir`.
#[cfg(target_os = "windows")]
fn recordings_root() -> PathBuf {
    let home = std::env::var("USERPROFILE").expect("%USERPROFILE% обязан быть установлен");
    PathBuf::from(home).join("Recordings")
}

#[cfg(target_os = "macos")]
fn recordings_root() -> PathBuf {
    let home = std::env::var("HOME").expect("$HOME обязан быть установлен");
    PathBuf::from(home).join("Recordings")
}

/// Канал в аудио-поток. `Mutex` — потому что `tauri::State` шарится между
/// потоками, а `Sender` не `Sync`.
struct Cmd(Mutex<Sender<Ctl>>);

impl Cmd {
    fn send(&self, c: Ctl) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .send(c)
            .map_err(|_| "аудио-поток не отвечает".to_string())
    }
}

/// Одна ждущая или обрабатываемая транскрипция.
#[derive(Clone, PartialEq, Debug)]
struct QueueItem {
    folder: Option<String>,
    base: String,
}

/// Что стало с записью, которую попросили отменить.
///
/// Три исхода, а не «получилось / не получилось», потому что снаружи они
/// требуют разного: снятую из хвоста очереди никто больше не тронет, и сказать
/// об этом обязана сама команда; прерванную на ходу объявляет воркер, когда
/// расшифровка действительно остановится; а неизвестную объявлять некому и
/// нечего.
#[derive(PartialEq, Debug)]
enum Cancelled {
    /// Записи нет ни в очереди, ни в работе — отменять нечего. Не ошибка:
    /// расшифровка могла закончиться ровно между показом меню и нажатием.
    Unknown,
    /// Стояла в очереди и снята, не начавшись. Внутри — кому и какую позицию
    /// сообщить заново; идущая запись сюда НЕ попадает (см. `cancel`).
    Dropped(Vec<(usize, QueueItem)>),
    /// Обрабатывалась прямо сейчас: воркеру послан сигнал остановиться.
    Stopped,
}

/// Изменяемая часть очереди — под одним локом целиком.
///
/// Разложить эти три поля по трём мьютексам значило бы завести гонку на ровном
/// месте: отмена решает, снимать запись из списка или прерывать её на ходу,
/// ровно по тому, начал ли воркер `items[0]`. Читайся `items` и `running`
/// порознь, отмена успела бы застать «ещё не начал» между `recv` воркера и
/// подъёмом флага — и вычеркнула бы из списка запись, которая уже пошла в
/// работу и всё равно дошла бы до конца.
#[derive(Default)]
struct Pending {
    items: VecDeque<QueueItem>,
    /// Поднят на всё время обработки `items[0]`, отдельно от `cancel`: послать
    /// в `oneshot` можно ровно один раз, поэтому после первой отмены `cancel`
    /// пуст — и без этого флага повторное нажатие приняло бы идущую запись за
    /// ещё не начатую и вычеркнуло бы её из `items`, а воркер потом снял бы с
    /// фронта уже чужую.
    running: bool,
    /// Куда сказать идущей расшифровке «хватит». `Some` ровно тогда, когда
    /// `running` поднят и отмены ещё не было.
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    /// Идентификаторы задач шлюза, уже отправленных для `items[0]`.
    ///
    /// Живут под тем же локом, что `running` и `cancel`, по той же причине:
    /// отмена решает «снять из очереди или прервать на ходу» и «что гасить на
    /// шлюзе» одним снимком. Читайся они порознь, отмена успела бы взять id
    /// уже следующей записи.
    jobs: Vec<String>,
}

/// Очередь транскрипций на всё приложение: `items[0]` обрабатывается прямо
/// сейчас (или вот-вот начнёт), `items[1..]` ждут своей очереди в порядке
/// постановки.
///
/// Один воркер (см. `spawn_transcribe_worker`) читает `rx` строго
/// последовательно — это и есть очередь, а не просто «не начинать вторую,
/// пока не кончится первая», как было раньше: там второй клик отвечал
/// ошибкой и требовал повторного клика вручную после первой.
///
/// `items` и канал меняются под одним и тем же локом (`enqueue`), поэтому
/// порядок в `items` всегда совпадает с порядком, в котором воркер реально
/// получит записи — иначе позиции, которые видит UI, могли бы разойтись с
/// тем, что происходит на самом деле.
///
/// **Отмена ломает равенство «канал = очередь», но не порядок.** Забрать
/// запись из середины `tokio::mpsc` нельзя, поэтому `cancel` вычёркивает её
/// только из `items` — в канале остаётся мёртвая запись. Уцелевшее свойство:
/// `items` всегда ПОДПОСЛЕДОВАТЕЛЬНОСТЬ того, что ещё лежит в канале. Значит,
/// пришедшая воркеру запись, не совпавшая с текущим фронтом, — это в точности
/// отменённая, и её надо пропустить; проверку делает `start_front`.
struct TranscribeQueue {
    pending: Mutex<Pending>,
    tx: tokio::sync::mpsc::UnboundedSender<QueueItem>,
}

impl TranscribeQueue {
    fn new() -> (Self, tokio::sync::mpsc::UnboundedReceiver<QueueItem>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self { pending: Mutex::new(Pending::default()), tx }, rx)
    }

    /// Ставит запись в очередь, если её там ещё нет — двойной клик по кнопке
    /// не создаёт вторую копию, а просто отдаёт ту же позицию, что и первый.
    /// Позиция 1-индексирована: 1 — обрабатывается прямо сейчас, 2 —
    /// следующая, и так далее.
    fn enqueue(&self, item: QueueItem) -> Result<usize, String> {
        let mut pending = self.pending.lock().map_err(|e| e.to_string())?;
        if let Some(pos) = pending.items.iter().position(|i| *i == item) {
            return Ok(pos + 1);
        }
        pending.items.push_back(item.clone());
        let position = pending.items.len();
        self.tx.send(item).map_err(|_| "воркер транскрипции недоступен".to_string())?;
        Ok(position)
    }

    /// Воркер получил запись из канала и спрашивает разрешения начать.
    ///
    /// `None` — запись отменили, пока она ждала: в `items` её больше нет, а в
    /// канале осталась мёртвая копия (см. докблок типа). Пропустить её здесь
    /// обязательно: иначе расшифровка пошла бы после отмены, а `finish_front`
    /// снял бы с фронта чужую запись.
    ///
    /// `Some(rx)` — можно работать, а по этому каналу придёт отмена.
    fn start_front(&self, item: &QueueItem) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let mut pending = self.pending.lock().expect("лок очереди транскрипции");
        if pending.items.front() != Some(item) {
            return None;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        pending.running = true;
        pending.cancel = Some(tx);
        Some(rx)
    }

    /// Запомнить отправленную задачу шлюза. Зовётся из `run_transcription`
    /// сразу после `submit`, до того как начнётся ожидание.
    fn note_job(&self, job_id: String) {
        let mut pending = self.pending.lock().expect("лок очереди транскрипции");
        pending.jobs.push(job_id);
    }

    /// Забрать накопленные id — именно забрать: отменять одну задачу дважды
    /// незачем, а вот попасть вторым вызовом по уже следующей записи можно.
    fn take_jobs(&self) -> Vec<String> {
        let mut pending = self.pending.lock().expect("лок очереди транскрипции");
        std::mem::take(&mut pending.jobs)
    }

    /// Убирает обработанную запись с фронта и отдаёт тех, кто остался — под
    /// тем же локом, что и `enqueue`, чтобы снимок для пересчёта позиций не
    /// мог оказаться устаревшим уже в момент чтения.
    fn finish_front(&self) -> Vec<QueueItem> {
        let mut pending = self.pending.lock().expect("лок очереди транскрипции");
        pending.running = false;
        pending.cancel = None;
        pending.jobs.clear();
        pending.items.pop_front();
        pending.items.iter().cloned().collect()
    }

    /// Снимает запись с очереди или останавливает её на ходу.
    ///
    /// Идущая запись из `items` НЕ вычёркивается: её снимет с фронта
    /// `finish_front`, когда воркер действительно остановится. Вычеркнуть её
    /// здесь значило бы сдвинуть фронт под работающим воркером, и тот снял бы
    /// потом следующую, ни разу не начатую.
    ///
    /// В `Dropped` едут только ждущие и только с новыми позициями: идущей
    /// записи `queued:1` слать нельзя — на экране у неё стадия («отправка»,
    /// «расшифровка»), и позиция поверх стадии выглядела бы откатом назад.
    fn cancel(&self, item: &QueueItem) -> Result<Cancelled, String> {
        let mut pending = self.pending.lock().map_err(|e| e.to_string())?;
        let Some(pos) = pending.items.iter().position(|i| i == item) else {
            return Ok(Cancelled::Unknown);
        };
        if pos == 0 && pending.running {
            // `take` — потому что послать в oneshot можно единожды; повторное
            // нажатие попадёт сюда же по флагу `running` и просто ничего не
            // сделает.
            if let Some(tx) = pending.cancel.take() {
                let _ = tx.send(());
            }
            return Ok(Cancelled::Stopped);
        }
        pending.items.remove(pos);
        let skip = usize::from(pending.running);
        let moved = pending
            .items
            .iter()
            .enumerate()
            .skip(skip)
            .map(|(i, q)| (i + 1, q.clone()))
            .collect();
        Ok(Cancelled::Dropped(moved))
    }
}

/// Воркер очереди: читает канал строго по одной записи за раз, поэтому
/// параллельных транскрипций не бывает в принципе — не только по логике
/// `enqueue`, но и потому, что второй `.recv()` физически не начнётся, пока
/// первый `await` внутри цикла не вернётся.
///
/// Ошибку `run_transcription` не пробрасывает и не логирует отдельно: она уже
/// ушла тому, кто умеет её показать, через `emit_transcribe_error` внутри
/// самой функции — здесь важно только то, что очередь обязана двигаться
/// дальше независимо от того, чем кончилась предыдущая запись.
///
/// Отмена идущей записи — это `select!`, который бросает саму расшифровку
/// недоделанной. Бросить её безопасно ровно потому, что все точки ожидания у
/// неё сетевые: файлы пишутся сплошным куском в самом конце, между ними нет ни
/// одного `await`, и оборваться посередине набора `.md`/`.txt` расшифровка не
/// может. Задание на стороне шлюза при этом остаётся жить — мы всего лишь
/// перестаём ждать ответ.
fn spawn_transcribe_worker(
    app: AppHandle,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<QueueItem>,
) {
    tauri::async_runtime::spawn(async move {
        while let Some(item) = rx.recv().await {
            let Some(cancel) = app.state::<TranscribeQueue>().start_front(&item) else {
                // Отменена, пока ждала: об этом уже сказала сама команда.
                continue;
            };
            let stopped = tokio::select! {
                // `biased` — чтобы уже дошедшая до конца расшифровка считалась
                // завершённой, а не отменённой: нажатие, опоздавшее на доли
                // секунды, не должно превращать готовую расшифровку в
                // «отменено» при том, что файлы на диске уже лежат.
                biased;
                _ = run_transcription(item.folder.clone(), item.base.clone(), app.clone()) => false,
                _ = cancel => true,
            };
            if stopped {
                emit_transcribe_cancelled(&app, &item.folder, &item.base);
            }
            let remaining = app.state::<TranscribeQueue>().finish_front();
            for (i, next) in remaining.iter().enumerate() {
                emit_transcribe_progress(&app, &next.folder, &next.base, &format!("queued:{}", i + 1));
            }
        }
    });
}

/// Одна запись: пара дорожек под общим именем.
///
/// `Eq` из производных убран: появилось поле `f32`, на котором он не выводится.
/// `assert_eq!` в тестах работает и на одном `PartialEq`.
#[derive(Serialize, PartialEq, Debug)]
struct Recording {
    /// `2026-07-17_14-30_zoom` — общая основа обеих дорожек.
    name: String,
    /// Месячная папка или `None` для корня (записи до перехода на папки).
    folder: Option<String>,
    mic: bool,
    system: bool,
    /// Суммарный размер дорожек в байтах.
    size: u64,
    /// Рядом с дорожками лежит папка `<name>.transcript` с готовой расшифровкой.
    ///
    /// Только «да/нет»: разбирать содержимое папки незачем — окну нужно лишь
    /// решить, предлагать ли «Открыть расшифровку» вместо «Расшифровать».
    transcript: bool,
    /// Длительность записи в секундах — по более полной из двух дорожек.
    ///
    /// Не по `size`: там сумма обеих дорожек, и оборвавшаяся дорожка сделала бы
    /// из 25 минут «40». См. `duration_sec`.
    duration_sec: u32,
    /// Насколько mic-дорожка тише system, в дБ. Заполняется в `list_recordings`
    /// после группировки — считать это здесь значило бы тащить в чистую
    /// функцию чтение файлов.
    #[serde(skip_serializing_if = "Option::is_none")]
    imbalance_db: Option<f32>,
}

#[tauri::command]
fn send_event(name: &str, state: tauri::State<Cmd>, app: AppHandle) -> Result<(), String> {
    let ctl = match name {
        "confirm" => Ctl::Event(Event::UserConfirmed),
        "decline" => Ctl::Event(Event::UserDeclined),
        "start" => Ctl::Event(Event::ManualStart),
        "stop" => Ctl::Event(Event::ManualStop),
        // Toggle — не событие ядра: что делать, знает только машина (см. Ctl).
        "toggle" => Ctl::Toggle,
        other => return Err(format!("неизвестное событие: {other}")),
    };
    // Ошибку увидит и нажавший (она вернётся в webview), но одного этого мало:
    // кнопка в окне — не единственный вход, а причина у всех входов общая.
    // Поэтому провал send здесь — такой же фатальный случай, как в трее и на
    // хоткее, и объявляется он одинаково.
    state
        .send(ctl)
        .inspect_err(|_| status::fatal(&app, status::DEAD.to_string()))
}

/// Текущее состояние целиком — для тех, кто опоздал на события.
///
/// Без этой команды webview узнаёт состояние только из `emit`, а `emit` уходит
/// лишь на изменении и только уже подписанным. Значит, страница, загрузившаяся
/// после старта аудио-потока (то есть всегда) или перезагруженная посреди
/// записи, осталась бы с захардкоженным «Ожидание встречи» из `index.html` —
/// вплоть до следующей смены состояния. См. `status` целиком.
#[tauri::command]
fn get_state(status: tauri::State<Status>) -> Snapshot {
    status.snapshot()
}

/// Сколько байт занимает секунда записи.
///
/// Формат дорожки задан в одном месте — `storage::WavSink::create`: моно,
/// `SAMPLE_RATE`, 16 бит на отсчёт. Считаем отсюда, а не константой 32000,
/// чтобы смена частоты дискретизации в ядре не оставила здесь молча врущую
/// арифметику.
const WAV_BYTES_PER_SEC: u64 = meeting_recorder::storage::SAMPLE_RATE as u64 * 2;

/// Длина заголовка WAV, который пишет `hound` для моно 16 бит.
///
/// `hound` выбирает `PCMWAVEFORMAT` для всего, что не больше двух каналов и не
/// глубже 16 бит (`WavWriter::new_with_spec_ex`), а у него заголовок ровно 44
/// байта: RIFF (12) + `fmt ` (24) + шапка `data` (8). Величина мелкая — 44
/// байта это 1,4 мс, — но вычитается честно, чтобы недописанный файл из одного
/// заголовка давал ноль, а не единицу.
const WAV_HEADER_BYTES: u64 = 44;

/// Длительность дорожки в секундах по её размеру на диске.
///
/// Читать заголовок каждого файла было бы точнее лишь на бумаге: длину данных
/// `hound` пишет туда же, откуда мы её и берём, — из размера файла, — а обход
/// каталога и так знает размер из `metadata()`, без единого открытия файла.
///
/// Обрезка вниз намеренная: «40 мин» у записи 40:59 честнее, чем «41».
/// `saturating_sub` закрывает недописанный или чужой файл короче заголовка.
fn duration_sec(track_bytes: u64) -> u32 {
    (track_bytes.saturating_sub(WAV_HEADER_BYTES) / WAV_BYTES_PER_SEC) as u32
}

/// Склеить дорожки в записи по основе имени И папке.
///
/// Имя дорожки — `{основа}.{mic|system}.wav`, где основа это
/// `YYYY-MM-DD_HH-MM_источник` с необязательным `_N` у повторов в ту же минуту
/// (см. `app::free_name_pair`). Группируем срезанием суффикса дорожки: пара
/// склеивается обратно ровно тем же правилом, которым её разложили.
///
/// **Ключ — пара `(base, folder)`, а не одна `base`.** Прежняя версия считала
/// «дата в основе однозначно задаёт месячную папку, поэтому одна запись не
/// может лежать в двух папках сразу» — эта посылка ложна в двух достижимых
/// сценариях:
///
/// 1. Прерванная миграция: `scripts/migrate-to-month-folders.sh` переносит
///    ПОФАЙЛОВО (`for entry in *`, один `mv` на файл) и на конфликте выходит с
///    кодом 1, не откатывая уже перенесённые члены тройки. После такого
///    прогона `.mic.wav` может остаться в корне, а `.system.wav` — уже уехать
///    в `2026-07/`.
/// 2. Коллизия имён между корнем и месячной папкой: `free_name_pair`
///    (`src/app.rs`) проверяет занятость имени только внутри целевой месячной
///    папки — список файлов корня в неё не попадает. Пока миграция не
///    прогнана, новая запись в ту же минуту с тем же источником, что и старая
///    корневая запись, получит имя БЕЗ суффикса `_2` и совпадёт с ней по основе.
///
/// Ключ только по `base` в обоих случаях схлопнул бы две половинки в одну
/// «полную» запись, чья `folder` бралась бы от первого встреченного файла:
/// «Переименовать» переименовало бы только эту половину, вторая дорожка (и,
/// возможно, `.transcript`) осталась бы под старым именем молча — `rename_
/// recording` вернул бы `Ok`, ничего не сообщив о разрыве. Ключ `(base,
/// folder)` вместо этого честно показывает такую ситуацию как ДВЕ неполные
/// записи — ровно то, чем она и является на диске.
///
/// `BTreeMap` по-прежнему сортирует в первую очередь по `base` — оно первый
/// компонент кортежа, `folder` работает только тайбрейком при совпадении
/// основы. Основа начинается с `YYYY-MM-DD_HH-MM`, так что лексикографический
/// порядок и есть хронологический. Наверх список отдаётся перевёрнутым:
/// свежее сверху.
///
/// `transcripts` — ключи `(основа, папка)` найденных рядом папок
/// `<основа>.transcript`, тем же ключом, что и группировка. Одинокая папка
/// расшифровки записи НЕ создаёт: пометка ставится только той паре, у которой
/// на диске есть хотя бы одна дорожка, — иначе в списке появилась бы запись
/// без единого файла, которую нельзя ни открыть, ни удалить.
///
/// Отделено от обхода каталога намеренно: правило склейки — это единственное
/// здесь, что можно сломать незаметно (отсутствие дорожки в паре UI показывает
/// предупреждением, и ошибка в группировке выглядела бы как испорченная запись).
/// Проверять его через `read_dir` значило бы держать в тесте настоящие файлы
/// ради логики, которой файлы не нужны.
fn group_recordings(
    files: impl IntoIterator<Item = (Option<String>, String, u64)>,
    transcripts: &HashSet<(String, Option<String>)>,
) -> Vec<Recording> {
    let mut found: BTreeMap<(String, Option<String>), Recording> = BTreeMap::new();
    for (folder, file, size) in files {
        let (base, is_mic) = match (file.strip_suffix(".mic.wav"), file.strip_suffix(".system.wav"))
        {
            (Some(b), _) => (b.to_string(), true),
            (_, Some(b)) => (b.to_string(), false),
            // Не наша дорожка — чужой файл в каталоге, не наше дело.
            _ => continue,
        };
        let key = (base.clone(), folder.clone());
        let transcript = transcripts.contains(&key);
        let rec = found.entry(key).or_insert(Recording {
            name: base,
            folder,
            mic: false,
            system: false,
            size: 0,
            transcript,
            duration_sec: 0,
            imbalance_db: None,
        });
        if is_mic {
            rec.mic = true;
        } else {
            rec.system = true;
        }
        rec.size += size;
        // Именно max, а не сумма: дорожки пишутся параллельно, и запись длится
        // столько, сколько длится более полная из них.
        rec.duration_sec = rec.duration_sec.max(duration_sec(size));
    }
    found.into_values().rev().collect()
}

/// Похоже ли имя папки на месячную (`2026-07`).
fn is_month_folder(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 7
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..].iter().all(u8::is_ascii_digit)
}

/// Что нашлось в каталоге записей за один обход.
///
/// Дорожки и папки расшифровок собираются вместе, потому что берутся из одного
/// и того же `read_dir`: второй проход по тому же дереву стоил бы столько же,
/// сколько первый, и мог бы застать каталог уже изменившимся.
#[derive(Default, PartialEq, Debug)]
struct Found {
    /// `(папка, имя файла, размер)` — всё, что лежит файлами.
    files: Vec<(Option<String>, String, u64)>,
    /// `(основа, папка)` записей, у которых рядом есть `<основа>.transcript`.
    transcripts: HashSet<(String, Option<String>)>,
}

/// Файлы корня плюс файлы месячных подпапок. Глубина ровно два уровня:
/// предсказуемо и не засасывает чужое дерево, если рядом окажется постороннее.
///
/// Каталоги не пропускаются целиком, как раньше: `<основа>.transcript` — это
/// папка (внутри `.md` и `.txt`, см. `run_transcription`), и другого признака
/// готовой расшифровки на диске нет. Внутрь мы не заходим — имени папки
/// достаточно, чтобы ответить «расшифровка есть».
fn collect_files(root: &Path) -> Result<Found, String> {
    fn read(dir: &Path, folder: Option<&str>, out: &mut Found) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            match e.file_type() {
                Ok(t) if t.is_file() => out.files.push((
                    folder.map(str::to_string),
                    name,
                    e.metadata().map(|m| m.len()).unwrap_or(0),
                )),
                Ok(t) if t.is_dir() => {
                    if let Some(base) = name.strip_suffix(".transcript") {
                        out.transcripts
                            .insert((base.to_string(), folder.map(str::to_string)));
                    }
                }
                _ => {}
            }
        }
    }

    let mut out = Found::default();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        // Каталога нет — записей просто ещё не было. Это не ошибка.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Found::default()),
        Err(e) => return Err(format!("не удалось прочитать {}: {e}", root.display())),
    };
    let mut months: Vec<PathBuf> = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        match e.file_type() {
            Ok(t) if t.is_dir() && is_month_folder(&name) => months.push(e.path()),
            // Расшифровка записи, которая осталась в корне и не переехала в
            // месячную папку, лежит тоже в корне — рядом со своими дорожками.
            Ok(t) if t.is_dir() => {
                if let Some(base) = name.strip_suffix(".transcript") {
                    out.transcripts.insert((base.to_string(), None));
                }
            }
            Ok(t) if t.is_file() => out.files.push((
                None,
                name,
                e.metadata().map(|m| m.len()).unwrap_or(0),
            )),
            _ => {}
        }
    }
    for m in months {
        let folder = m.file_name().map(|n| n.to_string_lossy().into_owned());
        read(&m, folder.as_deref(), &mut out);
    }
    Ok(out)
}

/// Список записей для окна: обход каталога, склейка дорожек в пары и разметка
/// дисбаланса громкости.
///
/// Три шага одной команды, а не три отдельных, и это не одно и то же с точки
/// зрения того, где что тестируется. Обход (`collect_files`) и склейка
/// (`group_recordings`) разделены сознательно — см. их докблоки: это
/// единственная логика здесь, которую можно сломать незаметно, и она чистая,
/// без файлового ввода-вывода, значит проверяется без диска. Разметка
/// дисбаланса, наоборот, СОБРАНА прямо тут, а не вынесена рядом: она читает
/// содержимое файлов через `Cache::rms`, и утаскивать чтение файлов в чистую
/// функцию значило бы отнять у неё главное свойство — тестируемость без диска.
///
/// Пометка считается только для полных пар (`r.mic && r.system`): одинокая
/// дорожка уже помечена как неполная в UI, второе предупреждение поверх неё
/// ничего не добавит, а чтение файла стоит времени зря.
#[tauri::command]
fn list_recordings(cache: tauri::State<Cache>) -> Result<Vec<Recording>, String> {
    let root = recordings_root();
    let found = collect_files(&root)?;
    let mut list = group_recordings(found.files, &found.transcripts);
    for r in &mut list {
        // Пометка имеет смысл только для полной пары: одинокая дорожка уже
        // помечена как неполная, и второе предупреждение о ней ничего не добавит.
        if !(r.mic && r.system) {
            continue;
        }
        let dir = match &r.folder {
            Some(f) => root.join(f),
            None => root.clone(),
        };
        let mic = cache.rms(&dir.join(format!("{}.mic.wav", r.name)));
        let sys = cache.rms(&dir.join(format!("{}.system.wav", r.name)));
        if let (Some(m), Some(s)) = (mic, sys) {
            r.imbalance_db = imbalance::imbalance(m, s);
        }
    }
    Ok(list)
}

/// Показать каталог в проводнике/Finder.
///
/// Через `explorer.exe` напрямую, без `tauri-plugin-opener`: плагин ради одной
/// строчки тянул бы за собой ещё и права в capabilities.
///
/// Код возврата не проверяется намеренно: `explorer.exe` возвращает 1 даже когда
/// окно успешно открылось. Проверять здесь нечего — либо папка открылась, либо
/// пользователь это увидит сам.
///
/// Общая для обеих команд открытия, чтобы способ открытия и это объяснение
/// жили в одном месте: разъехавшись, они разъедутся молча.
fn reveal(dir: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut cmd = std::process::Command::new("explorer.exe");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    cmd.arg(dir);
    cmd.spawn().map_err(|e| format!("не удалось открыть Finder/проводник: {e}"))?;
    Ok(())
}

/// Открыть каталог записей целиком.
#[tauri::command]
fn open_folder() -> Result<(), String> {
    let dir = recordings_root();
    // Иначе explorer откроет «Документы» вместо пустого несуществующего пути.
    std::fs::create_dir_all(&dir).map_err(|e| format!("не удалось создать {}: {e}", dir.display()))?;
    reveal(&dir)
}

/// Имя, пришедшее из окна, должно быть ровно одним шагом пути.
///
/// Разбором на компоненты, а не поиском `..` подстрокой: подстрока пропустила
/// бы `x/../../y`, а разбор — нет. Ровно один `Component::Normal` означает
/// сразу всё нужное: имя не абсолютное, не корень, не диск (`C:` на Windows),
/// не `.` и не `..`, и разделителей внутри нет.
///
/// Сравнение с исходной строкой нужно потому, что `Path` нормализует на лету:
/// у `"файл/"` компонент один и он `Normal`, но само имя уже с хвостом, и
/// пропускать такое незачем.
fn one_segment(name: &str) -> Result<(), String> {
    let mut parts = Path::new(name).components();
    match (parts.next(), parts.next()) {
        (Some(std::path::Component::Normal(n)), None) if n == std::ffi::OsStr::new(name) => Ok(()),
        _ => Err(format!("«{name}» — не имя внутри каталога записей")),
    }
}

/// Куда ведут пункты «Показать файлы» и «Открыть расшифровку».
///
/// Путь собирается ЗДЕСЬ, из корня записей, и ни один его кусок не приходит
/// готовым: из окна прилетают только имя месячной папки и основа имени записи,
/// а основу человек мог поменять сам — и через «Переименовать», и руками в
/// Finder, где на неё нет вообще никаких правил.
///
/// Папка проверяется не «на плохие символы», а на то, что она месячная
/// (`2026-07`): другого места для записей нет — `collect_files` заходит ровно в
/// такие подпапки и больше никуда, — а семь цифр с дефисом не могут вывести за
/// пределы корня в принципе. Это строже любого чёрного списка и короче.
///
/// Отделено от команды, чтобы проверяться без диска: сборка пути — это ровно
/// то, что здесь можно сломать незаметно, и файлы ей не нужны.
fn recording_dir(
    root: &Path,
    folder: Option<&str>,
    base: &str,
    transcript: bool,
) -> Result<PathBuf, String> {
    // Основа проверяется всегда, а не только когда из неё строят подпапку:
    // правило «всё, что пришло из окна, проверено» держится в голове, а
    // «проверено в одной ветке из двух» — нет.
    one_segment(base)?;
    let mut dir = root.to_path_buf();
    if let Some(f) = folder {
        if !is_month_folder(f) {
            return Err(format!("«{f}» — не месячная папка записей"));
        }
        dir.push(f);
    }
    if transcript {
        dir.push(format!("{base}.transcript"));
    }
    Ok(dir)
}

/// Открыть папку конкретной записи: месячную с дорожками или её расшифровку.
///
/// Несуществующую папку не создаём, в отличие от `open_folder`: пустой корень
/// значит «записей ещё не было», а пустая `<имя>.transcript` — враньё, будто
/// расшифровка есть. Честнее сказать, что открывать нечего.
#[tauri::command]
fn open_recording_folder(
    folder: Option<String>,
    base: String,
    transcript: bool,
) -> Result<(), String> {
    let dir = recording_dir(&recordings_root(), folder.as_deref(), &base, transcript)?;
    if !dir.is_dir() {
        return Err(format!("папки {} нет", dir.display()));
    }
    reveal(&dir)
}

/// Открыть раздел настроек, где выдают разрешение на захват системного звука.
///
/// Тем же способом, что `open_folder`, и по той же причине: одна строка вместо
/// плагина с правами в capabilities.
///
/// Раздел — «Запись экрана и звука» (`Privacy_ScreenCapture`): Process Tap
/// живёт именно там, хотя usage description у него свой
/// (`NSAudioCaptureUsageDescription`). Отдельного якоря под захват звука в
/// схеме `x-apple.systempreferences` нет.
///
/// Кнопка нужна не для красоты: путь до этого переключателя человек по памяти
/// не наберёт, а предупреждение, которое говорит «разрешите в настройках» и не
/// показывает где, перекладывает поиск на того, кто и так уже споткнулся.
#[cfg(target_os = "macos")]
#[tauri::command]
fn open_privacy_settings() -> Result<(), String> {
    std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
        .spawn()
        .map_err(|e| format!("не удалось открыть Системные настройки: {e}"))?;
    Ok(())
}

/// На Windows этой кнопки нет — как нет и разрешения, которое она открывает:
/// WASAPI loopback его не требует. Команда существует только затем, чтобы
/// `invoke` из общего `main.js` не падал в ненайденную команду.
#[cfg(not(target_os = "macos"))]
#[tauri::command]
fn open_privacy_settings() -> Result<(), String> {
    Ok(())
}

/// Открыть страницу репозитория в браузере по умолчанию.
///
/// Тем же способом, что `open_folder`: `explorer.exe`/`open` открывают не
/// только пути, но и произвольный URL — заводить `tauri-plugin-opener` ради
/// одной ссылки в подвале окна незачем. Ссылка нужна не для красоты: человек,
/// которому переслали голый `.exe`/`.dmg` без сопроводительного текста, иначе
/// не узнает, откуда взять новую версию или куда написать про баг.
const REPOSITORY_URL: &str = "https://github.com/mmaximov97/meeting-recorder";

#[tauri::command]
fn open_repository() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut cmd = std::process::Command::new("explorer.exe");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    cmd.arg(REPOSITORY_URL);
    cmd.spawn().map_err(|e| format!("не удалось открыть браузер: {e}"))?;
    Ok(())
}

/// Доступные микрофоны для выпадашки: идентификатор и что показать.
///
/// `InputDevice` уже `Serialize`? Нет — он в ядре, где serde не подключён.
/// Поэтому здесь своя DTO: тащить serde в ядро ради одной структуры значило бы
/// расширить его зависимости под нужду GUI.
#[derive(serde::Serialize)]
struct MicDevice {
    id: String,
    name: String,
}

#[tauri::command]
fn list_mic_devices() -> Result<Vec<MicDevice>, String> {
    Ok(meeting_recorder::capture::list_input_devices()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|d| MicDevice {
            id: d.id,
            name: d.name,
        })
        .collect())
}

#[tauri::command]
fn get_config(app: AppHandle) -> Config {
    Config::load(&app)
}

/// Переименовать запись. `folder` — месячная папка или `None` для корня.
#[tauri::command]
fn rename_recording(
    folder: Option<String>,
    base: String,
    new_tail: String,
) -> Result<String, String> {
    let dir = match folder {
        Some(f) => recordings_root().join(f),
        None => recordings_root(),
    };
    rename::rename_recording(&dir, &base, &new_tail)
}

/// `id: None` — вернуться на системный дефолт.
///
/// Имя приходит вместе с идентификатором и сохраняется рядом: когда устройства
/// не окажется в системе, показать пользователю будет нечего, кроме него.
#[tauri::command]
fn set_mic_device(
    id: Option<String>,
    name: Option<String>,
    state: tauri::State<Cmd>,
    app: AppHandle,
) -> Result<(), String> {
    let mut cfg = Config::load(&app);
    cfg.mic_device_id = id;
    cfg.mic_device_name = name;
    cfg.save(&app)?;
    state
        .send(Ctl::SetMicDevice(cfg.choice()))
        .inspect_err(|_| status::fatal(&app, status::DEAD.to_string()))
}

#[tauri::command]
fn set_transcribe_config(
    gateway_url: Option<String>,
    api_key: Option<String>,
    app: AppHandle,
) -> Result<(), String> {
    let mut cfg = Config::load(&app);
    cfg.stt_gateway_url = gateway_url;
    cfg.stt_api_key = api_key;
    cfg.save(&app)
}

/// Включить/выключить проверку микрофона.
#[tauri::command]
fn set_monitor(on: bool, state: tauri::State<Cmd>, app: AppHandle) -> Result<(), String> {
    state
        .send(Ctl::Monitor(on))
        .inspect_err(|_| status::fatal(&app, status::DEAD.to_string()))
}

fn emit_transcribe_progress(app: &AppHandle, folder: &Option<String>, base: &str, stage: &str) {
    let _ = app.emit(
        "transcribe-progress",
        serde_json::json!({ "folder": folder, "base": base, "stage": stage }),
    );
}

fn emit_transcribe_done(app: &AppHandle, folder: &Option<String>, base: &str) {
    let _ = app.emit("transcribe-done", serde_json::json!({ "folder": folder, "base": base }));
}

fn emit_transcribe_error(app: &AppHandle, folder: &Option<String>, base: &str, message: &str) {
    let _ = app.emit(
        "transcribe-error",
        serde_json::json!({ "folder": folder, "base": base, "message": message }),
    );
}

/// Отмена — отдельное событие, а не `transcribe-error`.
///
/// Ошибка и отмена выглядят на экране по-разному и должны выглядеть
/// по-разному: ошибку человек не просил, её показывают красным и предлагают
/// повторить, а отмену он только что нажал сам — извиняться за неё не за что.
fn emit_transcribe_cancelled(app: &AppHandle, folder: &Option<String>, base: &str) {
    let _ = app.emit("transcribe-cancelled", serde_json::json!({ "folder": folder, "base": base }));
}

/// Какая из двух дорожек сейчас на шлюзе.
///
/// Отдельным событием, а не полем в `stage`: строка стадии уже перегружена
/// форматом `queued:N`, и второй раз этого делать не стоит — разбор в
/// `ui/main.js` пришлось бы усложнять ради того, что к стадии отношения не имеет.
fn emit_transcribe_track(app: &AppHandle, folder: &Option<String>, base: &str, track: &str) {
    let _ = app.emit(
        "transcribe-track",
        serde_json::json!({ "folder": folder, "base": base, "track": track }),
    );
}

/// Ставит запись в очередь и возвращается сразу — саму транскрипцию проводит
/// `spawn_transcribe_worker`. Позиция > 1 значит «уже что-то обрабатывается
/// или ждёт впереди» — шлём её в UI тем же событием `transcribe-progress`,
/// которым `run_transcription` шлёт стадии, чтобы фронтенду не нужен был
/// отдельный тип состояния под «в очереди» и «обрабатывается».
#[tauri::command]
fn transcribe_recording(
    folder: Option<String>,
    base: String,
    app: AppHandle,
    queue: tauri::State<'_, TranscribeQueue>,
) -> Result<(), String> {
    let position = queue.enqueue(QueueItem { folder: folder.clone(), base: base.clone() })?;
    if position > 1 {
        emit_transcribe_progress(&app, &folder, &base, &format!("queued:{position}"));
    }
    Ok(())
}

/// Снять запись с расшифровки: и стоящую в очереди, и идущую прямо сейчас.
///
/// Отсутствие записи в очереди — не ошибка: между тем, как человек открыл
/// меню, и тем, как нажал «Отменить», расшифровка могла спокойно закончиться.
/// Вернуть здесь `Err` значило бы показать красное сообщение о том, что всё в
/// порядке.
#[tauri::command]
fn cancel_transcription(
    folder: Option<String>,
    base: String,
    app: AppHandle,
    queue: tauri::State<'_, TranscribeQueue>,
) -> Result<(), String> {
    let item = QueueItem { folder: folder.clone(), base: base.clone() };
    match queue.cancel(&item)? {
        // Об идущей объявит воркер, когда она действительно остановится:
        // скажи мы это отсюда, «отменено» появилось бы на экране раньше, чем
        // расшифровка перестала писать файлы.
        Cancelled::Stopped => {}
        Cancelled::Unknown => {}
        Cancelled::Dropped(moved) => {
            emit_transcribe_cancelled(&app, &folder, &base);
            for (position, next) in moved {
                emit_transcribe_progress(&app, &next.folder, &next.base, &format!("queued:{position}"));
            }
        }
    }
    Ok(())
}

/// Значения берутся владением, а не ссылками: расшифровка живёт в `select!`
/// вместе с каналом отмены и не может одалживать ничего у цикла воркера.
async fn run_transcription(folder: Option<String>, base: String, app: AppHandle) -> Result<(), String> {
    let (folder, base, app) = (&folder, base.as_str(), &app);
    let cfg = Config::load(app);
    let (url, key) = match (cfg.stt_gateway_url, cfg.stt_api_key) {
        (Some(u), Some(k)) if !u.trim().is_empty() && !k.trim().is_empty() => (
            u.trim().trim_end_matches('/').to_string(),
            k.trim().to_string(),
        ),
        _ => {
            let msg = "настройте URL и ключ шлюза";
            emit_transcribe_error(app, folder, base, msg);
            return Err(msg.to_string());
        }
    };

    let dir = match folder {
        Some(f) => recordings_root().join(f),
        None => recordings_root(),
    };
    let mic_path = dir.join(format!("{base}.mic.wav"));
    let sys_path = dir.join(format!("{base}.system.wav"));

    emit_transcribe_progress(app, folder, base, "uploading");
    let client = match reqwest::Client::builder().build() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("не удалось создать HTTP-клиент: {e}");
            emit_transcribe_error(app, folder, base, &msg);
            return Err(msg);
        }
    };
    emit_transcribe_progress(app, folder, base, "polling");
    // Дорожки ПО ОЧЕРЕДИ, а не через join!.
    //
    // Ускорения параллельность не давала никогда: воркер шлюза работает с
    // concurrency: 1 и всё равно выстраивает задачи друг за другом. Зато
    // клиентские часы у обеих тикали одновременно, и вторая дорожка тратила
    // свой бюджет ожидания, стоя в чужой очереди, — ровно поэтому часовые
    // встречи не доезжали. См. docs/2026-08-27-...-design.md, раздел 2.
    let queue = app.state::<TranscribeQueue>();

    emit_transcribe_track(app, folder, base, "mic");
    let mic_res = дорожка_целиком(&client, &url, &key, &mic_path, transcribe::Label::Owner, &*queue).await;
    emit_transcribe_track(app, folder, base, "system");
    let sys_res = дорожка_целиком(&client, &url, &key, &sys_path, transcribe::Label::Others, &*queue).await;

    let (mic, mic_err) = match mic_res {
        Ok(r) => (Some(r), None),
        Err(e) => (None, Some(e.to_string())),
    };
    let (sys, sys_err) = match sys_res {
        Ok(r) => (Some(r), None),
        Err(e) => (None, Some(e.to_string())),
    };

    if mic.is_none() && sys.is_none() {
        let msg = format!(
            "обе дорожки не удались — мик: {}; система: {}",
            mic_err.unwrap_or_else(|| "?".to_string()),
            sys_err.unwrap_or_else(|| "?".to_string())
        );
        emit_transcribe_error(app, folder, base, &msg);
        return Err(msg);
    }

    emit_transcribe_progress(app, folder, base, "merging");
    let mic = mic.unwrap_or_default();
    let sys = sys.unwrap_or_default();
    let mut md = transcribe::merge_markdown(&mic, &sys);
    // Частичный отказ — не теряем то, что получилось, но явно помечаем,
    // какая дорожка не удалась (см. Global Constraints и дизайн).
    if let Some(e) = &mic_err {
        md = format!("_Дорожка владельца не транскрибирована: {e}_\n\n{md}");
    }
    if let Some(e) = &sys_err {
        md = format!("_Дорожка собеседников не транскрибирована: {e}_\n\n{md}");
    }
    let mut txt = transcribe::merge_plain(&mic, &sys);
    if let Some(e) = &mic_err {
        txt = format!("[Дорожка владельца не транскрибирована: {e}]\n\n{txt}");
    }
    if let Some(e) = &sys_err {
        txt = format!("[Дорожка собеседников не транскрибирована: {e}]\n\n{txt}");
    }

    let out_dir = dir.join(format!("{base}.transcript"));
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        let msg = e.to_string();
        emit_transcribe_error(app, folder, base, &msg);
        return Err(msg);
    }
    if let Err(e) = std::fs::write(out_dir.join(format!("{base}.md")), &md) {
        let msg = e.to_string();
        emit_transcribe_error(app, folder, base, &msg);
        return Err(msg);
    }
    if let Err(e) = std::fs::write(out_dir.join(format!("{base}.txt")), &txt) {
        let msg = e.to_string();
        emit_transcribe_error(app, folder, base, &msg);
        return Err(msg);
    }

    emit_transcribe_done(app, folder, base);
    Ok(())
}

/// Одна дорожка целиком: отправить, запомнить id для отмены, дождаться.
///
/// id кладётся в очередь ДО ожидания — иначе отмена, нажатая в первую же
/// минуту, не нашла бы что гасить на шлюзе.
async fn дорожка_целиком(
    client: &reqwest::Client,
    url: &str,
    key: &str,
    path: &Path,
    label: transcribe::Label,
    queue: &TranscribeQueue,
) -> Result<transcribe::TrackResult, transcribe::TranscribeError> {
    let job_id = transcribe::submit(client, url, key, path).await?;
    queue.note_job(job_id.clone());
    transcribe::poll_until_done(client, url, key, &job_id, label).await
}

fn main() {
    let (tx, rx) = channel::<Ctl>();
    let tray_tx = tx.clone();
    let hotkey_tx = tx.clone();
    let (transcribe_queue, transcribe_rx) = TranscribeQueue::new();

    tauri::Builder::default()
        // Первым — до .manage(Status::default()), у которого свой докблок
        // «аудио-поток пишет сюда с первой же строки»: если сбой случится
        // раньше, чем плагин поднимется, он снова уйдёт в никуда, ровно как
        // раньше уходил eprintln! из GUI без консоли.
        .plugin(
            tauri_plugin_log::Builder::new()
                .targets([
                    Target::new(TargetKind::LogDir { file_name: None }),
                    Target::new(TargetKind::Stdout),
                ])
                .level(log::LevelFilter::Info)
                .build(),
        )
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(Cmd(Mutex::new(tx)))
        // Заводится до setup(): аудио-поток пишет сюда с первой же строки, а
        // фатальная ошибка там случается раньше, чем webview успеет подписаться.
        .manage(Status::default())
        .manage(Cache::default())
        .manage(transcribe_queue)
        .invoke_handler(tauri::generate_handler![
            send_event,
            get_state,
            list_recordings,
            open_folder,
            open_privacy_settings,
            open_repository,
            list_mic_devices,
            get_config,
            set_mic_device,
            set_transcribe_config,
            set_monitor,
            rename_recording,
            transcribe_recording,
            cancel_transcription,
            open_recording_folder
        ])
        .setup(move |app| {
            // Приложение строки меню, а не Dock: окно стартует скрытым, крестик
            // его прячет, а не выходит, — иконка в Dock, за которой нет окна и
            // по клику на которую ничего не происходит (обработчика Reopen у
            // нас нет), только вводила бы в заблуждение.
            //
            // Парная половина решения — `LSUIElement` в `src-tauri/Info.plist`.
            // Нужны обе, и вот почему ни одной по отдельности не хватает:
            // `LSUIElement` убирает Dock на момент запуска, но tao на
            // `applicationDidFinishLaunching` безусловно зовёт
            // `setActivationPolicy` своим значением, а его дефолт — `Regular`
            // (tao 0.35.3, `app_state.rs`: `launched` → `apply_activation_policy`),
            // и иконка вернулась бы. Эта строка задаёт tao нужное значение ДО
            // старта цикла событий, но сама по себе успела бы дать Dock'у
            // мигнуть.
            //
            // На показ окна из `status::fatal` это не влияет: `set_focus()` в
            // tao — это `makeKeyAndOrderFront` + `activateIgnoringOtherApps`,
            // то есть явная активация, которую accessory-приложению как раз и
            // положено делать самому.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = app.handle().clone();
            tray::build(&handle, tray_tx)?;

            // Ctrl+Shift+R — toggle. Что именно делать, решает аудио-поток по
            // состоянию машины: хоткей обязан работать и с закрытым окном, а
            // спрашивать состояние у webview, которого может не быть на экране,
            // — способ однажды не остановить запись.
            let shortcut = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyR);
            app.global_shortcut().on_shortcut(shortcut, move |app, _sc, event| {
                // Только на нажатие: без этого один хоткей даёт две команды
                // (нажатие + отпускание), то есть старт и мгновенный стоп.
                if event.state() != ShortcutState::Pressed {
                    return;
                }
                // Хоткей — самый молчаливый из входов: нажатие вслепую, без
                // окна и без меню. Проглотить здесь ошибку значит оставить
                // пользователя уверенным, что запись идёт.
                if hotkey_tx.send(Ctl::Toggle).is_err() {
                    status::fatal(app, status::DEAD.to_string());
                }
            })?;

            // Аудио-поток. Всё !Send рождается ВНУТРИ него.
            let mic = Config::load(&handle).choice();
            std::thread::spawn(move || audio::run(handle, rx, recordings_root(), mic));

            spawn_transcribe_worker(app.handle().clone(), transcribe_rx);
            Ok(())
        })
        .on_window_event(|window, event| {
            // Крестик прячет окно, а не выходит: это трей-приложение, детект
            // обязан продолжать работать. Выход — только через меню трея, где он
            // проходит через финализацию записи.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("не удалось запустить приложение");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Каталог записей обязан выводиться из домашнего каталога ТЕКУЩЕГО
    /// пользователя, а не быть прибитым к чьему-то конкретному профилю.
    /// Раньше под Windows здесь стоял литерал `C:\Users\<username>\Recordings`,
    /// и на чужой машине приложение писало в чужой домашний каталог.
    #[cfg(target_os = "windows")]
    #[test]
    fn каталог_записей_выводится_из_профиля_пользователя() {
        let profile = std::env::var("USERPROFILE").expect("%USERPROFILE%");
        assert_eq!(recordings_root(), PathBuf::from(profile).join("Recordings"));
    }

    /// Симметричный сторож для macOS: ветки двух систем должны оставаться
    /// одинаковыми по смыслу, и если кто-то починит одну, вторая не должна
    /// тихо разъехаться.
    #[cfg(target_os = "macos")]
    #[test]
    fn каталог_записей_выводится_из_домашнего_каталога() {
        let home = std::env::var("HOME").expect("$HOME");
        assert_eq!(recordings_root(), PathBuf::from(home).join("Recordings"));
    }

    /// Запись без расшифровки и нулевой длительности: размеры в этих тестах
    /// исчисляются десятками байт, то есть меньше секунды звука.
    fn rec(name: &str, folder: Option<&str>, mic: bool, system: bool, size: u64) -> Recording {
        Recording {
            name: name.to_string(),
            folder: folder.map(str::to_string),
            mic,
            system,
            size,
            transcript: false,
            duration_sec: 0,
            imbalance_db: None,
        }
    }

    fn group(files: &[(Option<&str>, &str, u64)]) -> Vec<Recording> {
        group_with(files, &[])
    }

    /// То же, что `group`, но с найденными рядом папками `<основа>.transcript`
    /// — ключом `(основа, папка)`, каким их отдаёт `collect_files`.
    fn group_with(
        files: &[(Option<&str>, &str, u64)],
        transcripts: &[(&str, Option<&str>)],
    ) -> Vec<Recording> {
        let transcripts: HashSet<(String, Option<String>)> = transcripts
            .iter()
            .map(|(b, f)| (b.to_string(), f.map(str::to_string)))
            .collect();
        group_recordings(
            files
                .iter()
                .map(|(f, n, s)| (f.map(str::to_string), n.to_string(), *s)),
            &transcripts,
        )
    }

    /// Дорожка длиной ровно `sec` секунд: заголовок плюс отсчёты.
    fn wav_bytes(sec: u64) -> u64 {
        WAV_HEADER_BYTES + sec * WAV_BYTES_PER_SEC
    }

    #[test]
    fn пара_дорожек_склеивается_в_одну_запись() {
        assert_eq!(
            group(&[
                (None, "2026-07-17_14-45_zoom.mic.wav", 100),
                (None, "2026-07-17_14-45_zoom.system.wav", 20),
            ]),
            vec![rec("2026-07-17_14-45_zoom", None, true, true, 120)],
            "размер записи — сумма дорожек, имя — общая основа"
        );
    }

    /// Отсутствие дорожки — не косметика: пара mic+system и есть запись, и UI
    /// показывает неполную пару предупреждением.
    #[test]
    fn одинокая_дорожка_видна_как_неполная() {
        assert_eq!(
            group(&[(None, "2026-07-17_14-45_zoom.mic.wav", 100)]),
            vec![rec("2026-07-17_14-45_zoom", None, true, false, 100)]
        );
        assert_eq!(
            group(&[(None, "2026-07-17_14-45_zoom.system.wav", 100)]),
            vec![rec("2026-07-17_14-45_zoom", None, false, true, 100)]
        );
    }

    #[test]
    fn чужие_файлы_в_каталоге_не_наше_дело() {
        assert_eq!(
            group(&[
                (None, "заметки.txt", 10),
                (None, "2026-07-17_14-45_zoom.wav", 10),
                (None, "mic.wav", 10),
                (None, ".mic.wav.bak", 10),
                (None, "2026-07-17_14-45_zoom.mic.wav", 100),
            ]),
            vec![rec("2026-07-17_14-45_zoom", None, true, false, 100)]
        );
    }

    /// Основа начинается с `YYYY-MM-DD_HH-MM`, поэтому лексикографический
    /// порядок BTreeMap и есть хронологический, а `.rev()` даёт «свежее сверху».
    #[test]
    fn свежее_сверху_независимо_от_порядка_обхода() {
        let list = group(&[
            (None, "2026-07-17_09-00_meet.mic.wav", 1),
            (None, "2026-07-18_10-00_zoom.mic.wav", 1),
            (None, "2026-07-16_23-59_teams.mic.wav", 1),
        ]);
        let names: Vec<&str> = list.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "2026-07-18_10-00_zoom",
                "2026-07-17_09-00_meet",
                "2026-07-16_23-59_teams"
            ]
        );
    }

    /// `_N` у повтора в ту же минуту — часть основы (см. `app::with_seq`),
    /// значит это отдельная запись, а не вторая дорожка первой.
    #[test]
    fn повтор_в_ту_же_минуту_это_отдельная_запись() {
        let list = group(&[
            (None, "2026-07-17_14-45_zoom.mic.wav", 1),
            (None, "2026-07-17_14-45_zoom.system.wav", 1),
            (None, "2026-07-17_14-45_zoom_2.mic.wav", 5),
            (None, "2026-07-17_14-45_zoom_2.system.wav", 5),
        ]);
        assert_eq!(
            list,
            vec![
                rec("2026-07-17_14-45_zoom_2", None, true, true, 10),
                rec("2026-07-17_14-45_zoom", None, true, true, 2),
            ]
        );
    }

    #[test]
    fn пустой_каталог_это_пустой_список_а_не_ошибка() {
        assert_eq!(group(&[]), vec![]);
    }

    #[test]
    fn записи_из_подпапки_и_из_корня_живут_в_одном_списке() {
        let list = group(&[
            (Some("2026-07"), "2026-07-30_13-03_chrome.mic.wav", 10),
            (Some("2026-07"), "2026-07-30_13-03_chrome.system.wav", 10),
            (None, "2026-06-01_10-00_zoom.mic.wav", 5),
        ]);
        assert_eq!(
            list,
            vec![
                rec("2026-07-30_13-03_chrome", Some("2026-07"), true, true, 20),
                rec("2026-06-01_10-00_zoom", None, true, false, 5),
            ],
            "порядок хронологический независимо от папки"
        );
    }

    #[test]
    fn папка_записи_запоминается() {
        let list = group(&[(Some("2026-07"), "2026-07-30_13-03_chrome.mic.wav", 1)]);
        assert_eq!(list[0].folder.as_deref(), Some("2026-07"));
    }

    /// БЛОКЕР ревью: одна и та же основа в двух разных папках — сценарий
    /// прерванной миграции (`.mic.wav` не успел переехать, `.system.wav` уже в
    /// `2026-07/`) или коллизии имён между корнем и месячной папкой
    /// (`free_name_pair` не видит файлы корня). Раньше ключом группировки была
    /// только основа, и обе половинки схлопывались в одну «полную» запись, чья
    /// `folder` бралась от первого встреченного файла, — «Переименовать»
    /// переименовало бы только эту половину, вторая дорожка молча осталась бы
    /// под старым именем. Ключ `(base, folder)` обязан показать это честно:
    /// ДВЕ неполные записи, каждая под своей папкой, а не одна целая.
    #[test]
    fn одна_основа_в_двух_папках_даёт_две_неполные_записи_а_не_одну_целую() {
        let list = group(&[
            (Some("2026-07"), "2026-07-30_13-03_chrome.mic.wav", 10),
            (None, "2026-07-30_13-03_chrome.system.wav", 20),
        ]);
        assert_eq!(
            list,
            vec![
                rec("2026-07-30_13-03_chrome", Some("2026-07"), true, false, 10),
                rec("2026-07-30_13-03_chrome", None, false, true, 20),
            ],
            "половинки одной основы из разных папок не имеют права слиться в одну запись"
        );
    }

    #[test]
    fn месячной_папкой_считается_только_yyyy_mm() {
        assert!(is_month_folder("2026-07"));
        assert!(!is_month_folder("2026-7"));
        assert!(!is_month_folder("2026-07-30"));
        assert!(!is_month_folder("архив"));
        assert!(!is_month_folder(""));
    }

    // ---- расшифровка ---------------------------------------------------------

    /// Гвоздь задачи: раньше обход каталога пропускал всё, что не файл, а
    /// расшифровка лежит именно папкой — окно предлагало расшифровать заново
    /// уже расшифрованную запись, и так каждый раз.
    #[test]
    fn папка_расшифровки_рядом_помечает_запись() {
        let files = [
            (None, "2026-07-17_14-45_zoom.mic.wav", 100),
            (None, "2026-07-17_14-45_zoom.system.wav", 20),
        ];
        assert!(
            !group_with(&files, &[])[0].transcript,
            "без папки рядом расшифровки нет"
        );
        assert!(
            group_with(&files, &[("2026-07-17_14-45_zoom", None)])[0].transcript,
            "папка 2026-07-17_14-45_zoom.transcript рядом с дорожками и есть признак расшифровки"
        );
    }

    /// Ключ пометки — тот же `(основа, папка)`, что и у группировки. Одна
    /// основа может лежать в двух папках сразу (см.
    /// `одна_основа_в_двух_папках_даёт_две_неполные_записи_а_не_одну_целую`), и
    /// расшифровка корневой половины не имеет отношения к половине в `2026-07`.
    #[test]
    fn расшифровка_из_другой_папки_не_приписывается_записи() {
        let list = group_with(
            &[
                (Some("2026-07"), "2026-07-30_13-03_chrome.mic.wav", 10),
                (None, "2026-07-30_13-03_chrome.system.wav", 20),
            ],
            &[("2026-07-30_13-03_chrome", None)],
        );
        assert_eq!(
            list.iter().map(|r| r.transcript).collect::<Vec<_>>(),
            vec![false, true],
            "помечена обязана быть корневая запись, а не тёзка из месячной папки"
        );
    }

    /// Папка расшифровки, у которой дорожки удалили руками, — не запись:
    /// открывать и переименовывать в ней нечего, а в списке она выглядела бы
    /// целой строкой без единого файла.
    #[test]
    fn одинокая_папка_расшифровки_не_создаёт_запись() {
        assert_eq!(group_with(&[], &[("2026-07-17_14-45_zoom", None)]), vec![]);
    }

    /// Единственный тест здесь, которому нужен настоящий диск: остальное про
    /// расшифровку — чистая логика, а вот «обход видит папку, а не только
    /// файлы» проверяется только обходом. Раньше `collect_files` отбрасывал всё,
    /// что не файл, и никакая правка группировки этого бы не исправила.
    #[test]
    fn обход_каталога_находит_папки_расшифровок_и_в_корне_и_в_месяце() {
        let root = std::env::temp_dir().join(format!("mr-collect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("2026-06-01_10-00_zoom.transcript")).unwrap();
        std::fs::create_dir_all(root.join("2026-07/2026-07-30_13-03_chrome.transcript")).unwrap();
        std::fs::create_dir_all(root.join("архив")).unwrap();
        std::fs::write(root.join("2026-06-01_10-00_zoom.mic.wav"), b"x").unwrap();

        let found = collect_files(&root).unwrap();
        let mut got: Vec<_> = found.transcripts.iter().cloned().collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("2026-06-01_10-00_zoom".to_string(), None),
                (
                    "2026-07-30_13-03_chrome".to_string(),
                    Some("2026-07".to_string())
                ),
            ],
            "папка месяца и посторонний каталог расшифровками не считаются"
        );
        assert_eq!(
            found.files,
            vec![(None, "2026-06-01_10-00_zoom.mic.wav".to_string(), 1)],
            "дорожки собираются как и раньше, внутрь .transcript обход не заходит"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- длительность --------------------------------------------------------

    /// Караул при правке формата записи: `duration_sec` считается по размеру
    /// файла, и смена частоты дискретизации или разрядности тихо сделает из неё
    /// вранье в разы.
    #[test]
    fn секунда_дорожки_весит_32000_байт() {
        assert_eq!(
            WAV_BYTES_PER_SEC, 32_000,
            "\n\
             Формат дорожки изменился: моно 16 бит при 16000 Гц — это 32000 байт\n\
             в секунду (src/storage.rs, WavSink::create).\n\
             \n\
             Длительность записи считается по размеру файла, а не по заголовку,\n\
             поэтому новый формат обязан приехать сюда вместе с правкой ядра —\n\
             иначе окно покажет «40 мин» там, где записано 20.\n"
        );
    }

    /// Гвоздь задачи: `size` — сумма дорожек, и делить её пополам нельзя.
    /// Ровно тот случай, который окно и так помечает предупреждением: system
    /// оборвалась на 25-й минуте, mic писался все 40. Оценка по сумме дала бы
    /// 32 минуты — не длительность ни одной из дорожек.
    #[test]
    fn длительность_считается_по_более_полной_дорожке_а_не_по_сумме() {
        let list = group(&[
            (None, "2026-07-17_14-45_zoom.mic.wav", wav_bytes(2400)),
            (None, "2026-07-17_14-45_zoom.system.wav", wav_bytes(1500)),
        ]);
        assert_eq!(list[0].duration_sec, 2400, "40 минут, а не 32 и не 65");
        assert_eq!(
            list[0].size,
            wav_bytes(2400) + wav_bytes(1500),
            "size остаётся суммой — на нём держится показ занятого места"
        );
    }

    #[test]
    fn порядок_обхода_дорожек_на_длительность_не_влияет() {
        let list = group(&[
            (None, "2026-07-17_14-45_zoom.system.wav", wav_bytes(1500)),
            (None, "2026-07-17_14-45_zoom.mic.wav", wav_bytes(2400)),
        ]);
        assert_eq!(list[0].duration_sec, 2400);
    }

    #[test]
    fn у_одинокой_дорожки_длительность_её_собственная() {
        let list = group(&[(None, "2026-07-17_14-45_zoom.mic.wav", wav_bytes(600))]);
        assert_eq!(list[0].duration_sec, 600);
    }

    /// Показать «10 мин» у записи 9:59 — соврать в большую сторону. Обрезаем вниз.
    #[test]
    fn неполная_секунда_обрезается_вниз() {
        assert_eq!(duration_sec(wav_bytes(599) + WAV_BYTES_PER_SEC - 1), 599);
    }

    /// Файл, у которого есть заголовок и нет данных, остаётся после падения
    /// посреди записи. Нулевая длительность честнее единицы.
    #[test]
    fn файл_без_звука_или_короче_заголовка_даёт_ноль() {
        assert_eq!(duration_sec(wav_bytes(0)), 0, "один заголовок — ноль секунд");
        assert_eq!(duration_sec(10), 0, "обрезанный файл не уходит в минус");
        assert_eq!(duration_sec(0), 0);
    }

    fn qi(base: &str) -> QueueItem {
        QueueItem { folder: None, base: base.to_string() }
    }

    #[test]
    fn первая_запись_в_очереди_получает_позицию_1() {
        let (q, mut rx) = TranscribeQueue::new();
        assert_eq!(q.enqueue(qi("a")), Ok(1));
        assert_eq!(rx.try_recv(), Ok(qi("a")), "воркер обязан получить её немедленно");
    }

    /// Гвоздь задачи: вторая запись не отвергается ошибкой, как было раньше
    /// («уже идёт транскрипция другой записи»), а встаёт следующей.
    #[test]
    fn вторая_запись_пока_первая_обрабатывается_встаёт_второй_а_не_отвергается() {
        let (q, mut rx) = TranscribeQueue::new();
        assert_eq!(q.enqueue(qi("a")), Ok(1));
        assert_eq!(q.enqueue(qi("b")), Ok(2));
        assert_eq!(rx.try_recv(), Ok(qi("a")));
        assert_eq!(rx.try_recv(), Ok(qi("b")), "обе записи обязаны дойти до воркера по порядку");
    }

    /// Двойной клик по кнопке — реальный сценарий, а не гипотетический: между
    /// кликом и первым событием `transcribe-progress` кнопка ещё активна.
    /// Без дедупликации в очередь ушли бы два одинаковых задания.
    #[test]
    fn повторный_enqueue_той_же_записи_не_дублирует_и_отдаёт_ту_же_позицию() {
        let (q, mut rx) = TranscribeQueue::new();
        assert_eq!(q.enqueue(qi("a")), Ok(1));
        assert_eq!(q.enqueue(qi("b")), Ok(2));
        assert_eq!(
            q.enqueue(qi("a")),
            Ok(1),
            "повторная постановка уже стоящей в очереди записи не создаёт вторую копию"
        );
        assert_eq!(rx.try_recv(), Ok(qi("a")));
        assert_eq!(rx.try_recv(), Ok(qi("b")));
        assert!(
            rx.try_recv().is_err(),
            "третьего сообщения в канале быть не должно — дубликат не отправлялся"
        );
    }

    #[test]
    fn finish_front_убирает_обработанную_запись_и_отдаёт_остальных_по_порядку() {
        let (q, _rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();
        q.enqueue(qi("b")).unwrap();
        q.enqueue(qi("c")).unwrap();
        assert_eq!(q.finish_front(), vec![qi("b"), qi("c")]);
    }

    #[test]
    fn finish_front_на_последней_записи_отдаёт_пустую_очередь() {
        let (q, _rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();
        assert_eq!(q.finish_front(), Vec::<QueueItem>::new());
    }


    // ---- отмена расшифровки -------------------------------------------------

    /// Между тем, как открылось меню «⋯», и тем, как нажали «Отменить»,
    /// расшифровка успевает закончиться. Это обычный ход событий, а не сбой.
    #[test]
    fn отмена_записи_которой_в_очереди_нет_ничего_не_меняет() {
        let (q, _rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();
        assert_eq!(q.cancel(&qi("b")), Ok(Cancelled::Unknown));
        assert_eq!(q.finish_front(), Vec::<QueueItem>::new(), "очередь не тронута");
    }

    /// Позиции пересчитываются той же арифметикой, что и после `finish_front`:
    /// снялась вторая — третья становится второй.
    #[test]
    fn снятая_из_середины_запись_освобождает_позицию_следующим() {
        let (q, _rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();
        q.enqueue(qi("b")).unwrap();
        q.enqueue(qi("c")).unwrap();
        q.start_front(&qi("a")).expect("первая пошла в работу");

        assert_eq!(q.cancel(&qi("b")), Ok(Cancelled::Dropped(vec![(2, qi("c"))])));
        assert_eq!(q.finish_front(), vec![qi("c")]);
    }

    /// Идущей записи новая позиция не сообщается: у неё на экране стадия
    /// («отправка», «расшифровка»), и `queued:1` поверх стадии читался бы как
    /// откат назад.
    #[test]
    fn идущей_записи_позиция_не_пересылается() {
        let (q, _rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();
        q.enqueue(qi("b")).unwrap();
        q.start_front(&qi("a")).expect("первая пошла в работу");

        assert_eq!(q.cancel(&qi("b")), Ok(Cancelled::Dropped(vec![])));
    }

    #[test]
    fn идущая_запись_не_вычёркивается_из_очереди_а_получает_сигнал_остановиться() {
        let (q, _rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();
        q.enqueue(qi("b")).unwrap();
        let mut отмена = q.start_front(&qi("a")).expect("первая пошла в работу");

        assert_eq!(q.cancel(&qi("a")), Ok(Cancelled::Stopped));
        assert!(отмена.try_recv().is_ok(), "сигнал обязан дойти до расшифровки");
        assert_eq!(
            q.finish_front(),
            vec![qi("b")],
            "с фронта снимается именно отменённая запись, а не следующая"
        );
    }

    /// Послать в `oneshot` можно единожды, поэтому после первой отмены канал
    /// пуст. Без отдельного флага «уже в работе» второе нажатие приняло бы
    /// идущую запись за ещё не начатую, вычеркнуло бы её из очереди — и
    /// `finish_front` снял бы с фронта следующую, ни разу не начатую.
    #[test]
    fn повторная_отмена_идущей_записи_не_съедает_следующую() {
        let (q, _rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();
        q.enqueue(qi("b")).unwrap();
        q.start_front(&qi("a")).expect("первая пошла в работу");

        assert_eq!(q.cancel(&qi("a")), Ok(Cancelled::Stopped));
        assert_eq!(q.cancel(&qi("a")), Ok(Cancelled::Stopped), "повтор — не ошибка");
        assert_eq!(q.finish_front(), vec![qi("b")]);
    }

    /// Окно между `recv` воркера и началом работы: запись уже первая в
    /// очереди, но ещё не пошла. Отменить её здесь — значит просто вычеркнуть,
    /// а не слать сигнал в никуда.
    #[test]
    fn ещё_не_начатый_фронт_отменяется_вычёркиванием() {
        let (q, _rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();

        assert_eq!(q.cancel(&qi("a")), Ok(Cancelled::Dropped(vec![])));
        assert!(
            q.start_front(&qi("a")).is_none(),
            "воркер не имеет права начать отменённую запись"
        );
    }

    /// Забрать запись из середины `tokio::mpsc` нельзя, поэтому в канале после
    /// отмены остаётся мёртвая копия. Ловит её `start_front`, сверяясь с
    /// фронтом очереди.
    #[test]
    fn мёртвая_копия_из_канала_воркеру_работать_не_даёт() {
        let (q, mut rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();
        q.enqueue(qi("b")).unwrap();
        q.start_front(&qi("a")).expect("первая пошла в работу");
        q.cancel(&qi("b")).unwrap();
        q.finish_front();

        assert_eq!(rx.try_recv(), Ok(qi("a")));
        assert_eq!(rx.try_recv(), Ok(qi("b")), "канал про отмену не знает");
        assert!(q.start_front(&qi("b")).is_none(), "но работать по ней нельзя");
    }

    /// Отменили не глядя, спохватились, поставили заново — запись обязана
    /// пойти в работу, несмотря на мёртвую копию, которая всё ещё лежит в
    /// канале впереди новой.
    #[test]
    fn снятую_запись_можно_поставить_заново() {
        let (q, mut rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();
        q.enqueue(qi("b")).unwrap();
        q.start_front(&qi("a")).expect("первая пошла в работу");
        q.cancel(&qi("b")).unwrap();

        assert_eq!(q.enqueue(qi("b")), Ok(2), "встала заново, за идущей");
        q.finish_front();
        assert_eq!(rx.try_recv(), Ok(qi("a")));
        assert_eq!(rx.try_recv(), Ok(qi("b")), "мёртвая копия");
        assert!(q.start_front(&qi("b")).is_some(), "живая постановка — можно работать");
        assert_eq!(rx.try_recv(), Ok(qi("b")), "а это уже сама постановка");
    }

    /// Чтобы отменить задачу на шлюзе, надо знать её id. Он появляется только
    /// после отправки, поэтому очередь обязана уметь его принять на ходу.
    #[test]
    fn идущая_запись_запоминает_id_задач_шлюза() {
        let (queue, _rx) = TranscribeQueue::new();
        let item = qi("встреча");
        queue.enqueue(item.clone()).expect("постановка");
        queue.start_front(&item).expect("старт");

        queue.note_job("job_mic".to_string());
        queue.note_job("job_sys".to_string());

        assert_eq!(queue.take_jobs(), vec!["job_mic".to_string(), "job_sys".to_string()]);
    }

    /// `take_jobs` именно ЗАБИРАЕТ: второй вызов не имеет права отдать те же
    /// id снова, иначе повторная отмена била бы по чужой, уже новой задаче.
    #[test]
    fn забранные_id_второй_раз_не_отдаются() {
        let (queue, _rx) = TranscribeQueue::new();
        let item = qi("встреча");
        queue.enqueue(item.clone()).expect("постановка");
        queue.start_front(&item).expect("старт");
        queue.note_job("job_mic".to_string());

        assert_eq!(queue.take_jobs(), vec!["job_mic".to_string()]);
        assert!(queue.take_jobs().is_empty(), "id одноразовые");
    }

    /// Следующая запись начинает с чистого листа: id предыдущей к ней
    /// отношения не имеют.
    #[test]
    fn финиш_фронта_забывает_id_задач() {
        let (queue, _rx) = TranscribeQueue::new();
        let item = qi("первая");
        queue.enqueue(item.clone()).expect("постановка");
        queue.start_front(&item).expect("старт");
        queue.note_job("job_old".to_string());

        queue.finish_front();

        assert!(queue.take_jobs().is_empty(), "id не переезжают на следующую запись");
    }

    // ---- путь к папке записи ------------------------------------------------

    fn путь(folder: Option<&str>, base: &str, transcript: bool) -> Result<PathBuf, String> {
        recording_dir(Path::new("/записи"), folder, base, transcript)
    }

    #[test]
    fn запись_из_корня_показывается_самим_корнем() {
        assert_eq!(путь(None, "2026-07-17_14-30_zoom", false), Ok(PathBuf::from("/записи")));
    }

    #[test]
    fn запись_из_месячной_папки_показывается_этой_папкой() {
        assert_eq!(
            путь(Some("2026-07"), "2026-07-17_14-30_zoom", false),
            Ok(PathBuf::from("/записи/2026-07"))
        );
    }

    #[test]
    fn расшифровка_лежит_подпапкой_рядом_с_дорожками() {
        assert_eq!(
            путь(Some("2026-07"), "2026-07-17_14-30_zoom", true),
            Ok(PathBuf::from("/записи/2026-07/2026-07-17_14-30_zoom.transcript"))
        );
    }

    #[test]
    fn расшифровка_записи_из_корня_лежит_в_корне() {
        assert_eq!(
            путь(None, "2026-07-17_14-30_zoom", true),
            Ok(PathBuf::from("/записи/2026-07-17_14-30_zoom.transcript"))
        );
    }

    /// Из окна приходит имя папки, а не путь. Всё, что не `YYYY-MM`, — не наша
    /// подпапка: `collect_files` в другие и не заходит.
    #[test]
    fn папкой_может_быть_только_месячная() {
        for чужое in ["..", ".", "/", "2026-7", "2026-07/..", "../2026-07", "чужое"] {
            assert!(
                путь(Some(чужое), "2026-07-17_14-30_zoom", false).is_err(),
                "«{чужое}» не месячная папка и открываться не должна"
            );
        }
    }

    /// Основу человек меняет сам — и «Переименовать», и руками в Finder, где
    /// правил нет вообще. Уйти по ней вверх из каталога записей нельзя.
    #[test]
    fn основа_имени_не_выводит_за_каталог_записей() {
        for чужое in ["..", ".", "", "../секреты", "a/../../b", "/etc/passwd", "запись/"] {
            assert!(
                путь(Some("2026-07"), чужое, true).is_err(),
                "«{чужое}» не имя записи и открываться не должно"
            );
        }
    }

    /// Проверка основы не зависит от того, в подпапку идём или нет: правило
    /// «всё, что пришло из окна, проверено» не должно держаться на ветке.
    #[test]
    fn чужая_основа_отвергается_и_без_расшифровки() {
        assert!(путь(Some("2026-07"), "../секреты", false).is_err());
    }

    /// Кириллица, точки и пробелы внутри имени — обычное дело после
    /// переименования; отвергать их незачем.
    #[test]
    fn обычное_переименованное_имя_проходит() {
        assert_eq!(
            путь(Some("2026-07"), "2026-07-17_14-30_созвон с артёмом v1.2", true),
            Ok(PathBuf::from(
                "/записи/2026-07/2026-07-17_14-30_созвон с артёмом v1.2.transcript"
            ))
        );
    }

    // ---- tauri.conf.json ----------------------------------------------------

    /// Караул при значении, которое выглядит опечаткой и ею не является.
    ///
    /// `bundle.macOS.minimumSystemVersion` стоит `11.0`, хотя приложению нужна
    /// macOS 14.4, — иначе отказ на старой системе не дойдёт до человека:
    /// запуск перехватит Finder и откажет своими словами. Рассуждение записано
    /// в нескольких местах (докблок `MIN_MACOS` в `src/capture/macos.rs`,
    /// design-документ, план, README, `scripts/check-tap-lazy-bind.sh`), и ни
    /// одно из них не лежит внутри `src-tauri/` — то есть там, куда смотрит
    /// человек, решивший «привести в соответствие». JSON комментариев не держит;
    /// этот тест — единственный комментарий, который правка не сможет не
    /// заметить.
    ///
    /// Файл берётся `include_str!`, а не чтением с диска: так тест не зависит
    /// ни от рабочего каталога, ни от платформы, а расхождение всплывает уже
    /// при компиляции, если файл вообще исчезнет. `cfg` на нём нет намеренно —
    /// ключ правят чаще всего как раз не с macOS.
    #[test]
    fn минимальная_версия_macos_в_бандле_осталась_11_0() {
        const CONF: &str = include_str!("../tauri.conf.json");
        let conf: serde_json::Value =
            serde_json::from_str(CONF).expect("src-tauri/tauri.conf.json — не валидный JSON");

        assert_eq!(
            conf["bundle"]["macOS"]["minimumSystemVersion"].as_str(),
            Some("11.0"),
            "\n\
             bundle.macOS.minimumSystemVersion обязан остаться \"11.0\".\n\
             \n\
             Расхождение с настоящим требованием (macOS 14.4) выглядит \
             недосмотром, но им не является.\n\
             \n\
             ЗАЧЕМ. Этот ключ — LSMinimumSystemVersion в Info.plist, то есть гейт \
             Finder'а.\n\
             При 11.0 приложение на старой системе запускается, доходит до main, \
             зовёт\n\
             unsupported_reason() и объясняет человеку, что нужна 14.4 и почему. \
             При 14.4\n\
             запуск перехватит сама macOS и откажет своими словами — пользователь \
             не узнает,\n\
             чего именно не хватает, а мы не узнаем, что он вообще пытался.\n\
             \n\
             ЧЕГО ЭТОТ КЛЮЧ БОЛЬШЕ НЕ ДЕЛАЕТ. До 2026-08-17 он же держал \
             живучесть процесса:\n\
             в Tauri 2 он задаёт и MACOSX_DEPLOYMENT_TARGET, а ld при 11.x \
             связывал символы\n\
             тапа лениво. Опора оказалась зависящей от версии линкера — на \
             ld-1053.12\n\
             связывание жадное уже при 11.0. Теперь живучесть держит слабая \
             линковка\n\
             (-Wl,-weak_framework,CoreAudio в обоих build.rs), и её стерегут \
             отдельные тесты:\n\
             корневой_крейт_линкует_coreaudio_слабо (src/lib.rs) и \
             gui_крейт_линкует_coreaudio_слабо.\n\
             \n\
             Замеры и рассуждение целиком — докблок MIN_MACOS в \
             src/capture/macos.rs.\n\
             Проверка на собранном бандле: npm run check-tap-lazy-bind\n"
        );
    }

    // ---- build.rs -----------------------------------------------------------

    /// То же, что `корневой_крейт_линкует_coreaudio_слабо` в ядре, но для этого
    /// крейта: `cargo:rustc-link-arg` между крейтами не наследуется, линк у
    /// GUI-бинаря свой, и флаг ему нужен свой.
    ///
    /// Два почти одинаковых теста вместо одного общего — потому что забыть флаг
    /// можно в каждом файле по отдельности, и падать должен тот тест, который
    /// назовёт нужный файл.
    #[test]
    fn gui_крейт_линкует_coreaudio_слабо() {
        const BUILD_RS: &str = include_str!("../build.rs");
        assert!(
            BUILD_RS.contains("-Wl,-weak_framework,CoreAudio"),
            "\n\
             В src-tauri/build.rs пропал флаг слабой линковки CoreAudio:\n\
             \x20   println!(\"cargo:rustc-link-arg=-Wl,-weak_framework,CoreAudio\");\n\
             \n\
             Без него на macOS старее 14.4 dyld убивает GUI с \"Symbol not found:\n\
             _AudioHardwareCreateProcessTap\" ДО main: ни окна, ни тоста, ни \
             объяснения.\n\
             \n\
             Докблок MIN_MACOS в src/capture/macos.rs, проверка на бандле:\n\
             \x20   npm run check-tap-lazy-bind\n"
        );
    }
}
