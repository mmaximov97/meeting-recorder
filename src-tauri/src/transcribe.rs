//! HTTP-клиент шлюза транскрипции: submit одной дорожки, поллинг задачи,
//! слияние двух дорожек по времени. Контракт API — тот же, что уже проверен
//! скиллом `ailab-transcribe` (`POST /v1/audio/transcriptions/async` +
//! `GET /v1/jobs/:id`), здесь не изобретается заново.

use crate::local;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

/// Где расшифровывать — действующее значение `Config::transcribe_mode`
/// (`config.rs`). Разбор строки в это значение живёт здесь, а не в
/// `config.rs`, тем же способом, каким разбор языка живёт в
/// `i18n::effective_lang`, а не в `config.rs` (см. докблок поля `language`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Server,
    Local,
}

/// `Some("local")` — на этом компьютере. Всё остальное, включая `None`
/// (поля ещё не было в конфиге, когда он появился на диске) — сервер, как
/// было всегда.
pub fn effective_mode(cfg_mode: Option<&str>) -> Mode {
    match cfg_mode {
        Some("local") => Mode::Local,
        _ => Mode::Server,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    Owner,
    Others,
}

impl Label {
    fn title(self) -> &'static str {
        match self {
            Label::Owner => "Владелец",
            Label::Others => "Собеседники",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub start: f64,
    pub label: Label,
    pub text: String,
    /// Метка спикера от шлюза (`SPEAKER_00`, ...), если диаризация уместилась
    /// на GPU. `None` — либо диаризация не запускалась/не уместилась, либо
    /// шлюз её не прислал.
    pub speaker: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TrackResult {
    pub segments: Vec<Segment>,
    pub text: String,
}

#[derive(Debug, thiserror::Error)]
pub enum TranscribeError {
    #[error("сеть: {0}")]
    Network(#[from] reqwest::Error),
    #[error("файл не читается: {0}")]
    Io(#[from] std::io::Error),
    #[error("не удалось разобрать ответ шлюза: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("шлюз отклонил запрос ({0}) — проверьте ключ")]
    SubmitRejected(String),
    #[error("задача {0} завершилась ошибкой: {1}")]
    JobFailed(String, String),
    #[error("задача {0} не завершилась за отведённое время")]
    Timeout(String),
    #[error("связь со шлюзом потеряна, задача {0} осталась на нём")]
    PollLost(String),
    #[error("задача {0} отменена")]
    JobCancelled(String),
    /// Отдельно от `SubmitRejected`: то сообщение («проверьте ключ») пишется
    /// про отправку новой задачи и на `DELETE` не подходит — гашение чужой
    /// задачи на шлюзе не имеет отношения к вводу ключа.
    #[error("шлюз отклонил отмену задачи ({0})")]
    CancelRejected(String),
    /// Локальный режим: дорожку обслуживает честная заглушка из `local.rs`.
    /// `{0}` — её `Display`, буквально ключ словаря (`local.notWired`), а не
    /// готовый текст: интерфейс переводит его сам, тем же способом, что и
    /// `status::DEAD` (см. докблок `local::LocalError`).
    #[error("{0}")]
    Local(#[from] local::LocalError),
}

/// Шаг опроса задачи. Прежний, десять секунд: на минутных масштабах работы
/// шлюза чаще спрашивать нечего.
pub const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Таймаут ОДНОГО GET-запроса опроса статуса — не клиента целиком.
///
/// `reqwest::Client` тут общий с `submit`, который льёт до 115 МБ дорожки и
/// таймаутиться по времени НЕ должен, — поэтому граница стоит на самом
/// `RequestBuilder` (`.timeout(...)`), а не на клиенте: у reqwest таймаутов по
/// умолчанию нет вовсе, ни у клиента, ни у запроса. Без этой границы протухшее
/// соединение из пула (сон ноутбука, отвалившийся VPN, зависший шлюз) висит на
/// `.send()` бесконечно: `send()` не возвращается — счётчик подряд идущих
/// неудач не растёт, `POLL_DEADLINE` перечитывается только на верхушке
/// `while`, и один такой зависон молча съедает часы.
///
/// 20 секунд — опрос статуса это одна строка JSON, ответ в здоровой сети
/// занимает миллисекунды; величина не впритык, а с запасом на короткий затор,
/// но достаточно маленькая, чтобы механизм «шесть неудач подряд»
/// (`MAX_CONSECUTIVE_POLL_FAILURES`) срабатывал за разумные минуты, а не за час.
pub const POLL_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Абсолютный предел ожидания одной дорожки, по настенным часам.
///
/// Три часа, а не два: у шлюза лимит 2 часа НА ЗАПРОС
/// (`ai-lab/src/clients/whisper-client.ts:23`), а дорожка с диаризацией — это
/// два прохода по файлу плюс возможная загрузка модели. Принцип: клиент не
/// сдаётся раньше сервера. Если задача действительно зависла, её похоронит
/// таймаут шлюза, и мы увидим `failed` с внятной причиной вместо своего
/// немого «не завершилась за отведённое время».
pub const POLL_DEADLINE: Duration = Duration::from_secs(3 * 60 * 60);

/// Сколько неудачных опросов ПОДРЯД терпим, прежде чем признать поражение.
///
/// Шесть — это примерно минута. Счётчик обнуляется на любом удачном опросе, и
/// это важно: суммарный лимит за трёхчасовую задачу выбрала бы и здоровая
/// сеть, а нам нужно отличить «сеть моргнула» от «сеть пропала».
pub const MAX_CONSECUTIVE_POLL_FAILURES: u32 = 6;

/// Что делать с неудачным опросом.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollFailure {
    /// Ждём шаг и спрашиваем снова.
    Transient,
    /// Повторять бессмысленно — причина не рассосётся.
    Terminal(&'static str),
}

/// Транспортные ошибки (соединение не встало, оборвалось) сюда не попадают:
/// они преходящие по определению и считаются `Transient` на месте вызова.
pub fn classify_poll_status(status: u16) -> PollFailure {
    match status {
        404 => PollFailure::Terminal("шлюз не помнит такую задачу"),
        401 | 403 => PollFailure::Terminal("шлюз отклонил ключ"),
        _ => PollFailure::Transient,
    }
}

#[derive(Deserialize)]
struct SubmitResponse {
    id: String,
}

#[derive(Deserialize)]
struct JobResponse {
    status: String,
    #[serde(default)]
    result: Option<JobResult>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize, Default)]
struct JobResult {
    #[serde(default)]
    text: String,
    #[serde(default)]
    segments: Vec<RawSegment>,
}

#[derive(Deserialize)]
struct RawSegment {
    start: f64,
    text: String,
    #[serde(default)]
    speaker: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobOutcome {
    Pending,
    Succeeded(TrackResult),
    /// Отменена — на шлюзе или нами. Отдельно от `Failed`: отмену человек
    /// сделал сам, и красное «не удалось» на своё же действие читается как сбой.
    Cancelled,
    Failed(String),
}

pub fn parse_job_response(body: &str, label: Label) -> Result<JobOutcome, TranscribeError> {
    let resp: JobResponse = serde_json::from_str(body)?;
    match resp.status.as_str() {
        "succeeded" => {
            let result = resp.result.unwrap_or_default();
            let segments = result
                .segments
                .into_iter()
                .map(|s| Segment { start: s.start, label, text: s.text, speaker: s.speaker })
                .collect();
            Ok(JobOutcome::Succeeded(TrackResult { segments, text: result.text }))
        }
        "cancelled" => Ok(JobOutcome::Cancelled),
        "failed" | "error" => {
            Ok(JobOutcome::Failed(resp.error.unwrap_or_else(|| "неизвестная ошибка".to_string())))
        }
        _ => Ok(JobOutcome::Pending),
    }
}

/// Отправить дорожку и вернуть id задачи.
///
/// Отдельно от ожидания именно ради отмены: id нужен снаружи сразу, а не
/// через час, когда функция вернётся.
pub async fn submit(
    client: &reqwest::Client,
    gateway: &str,
    key: &str,
    wav_path: &Path,
) -> Result<String, TranscribeError> {
    let file_name = wav_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("audio.wav")
        .to_string();
    // `Part::file` открывает файл ПЕРВЫМ и берёт длину с уже открытого
    // дескриптора (`file.metadata().await` внутри reqwest), а не отдельным
    // `stat` по пути. Так Content-Length не может разойтись с тем, что
    // реально уйдёт в поток, даже если файл на диске подменят между вызовами
    // — окна для гонки stat/open просто нет.
    let part = reqwest::multipart::Part::file(wav_path)
        .await?
        .file_name(file_name)
        .mime_str("audio/wav")?;
    let form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("language", "ru")
        .text("model", "Systran/faster-whisper-large-v3")
        .text("diarize", "true");

    let submit_url = format!("{gateway}/v1/audio/transcriptions/async");
    let resp = client.post(&submit_url).bearer_auth(key).multipart(form).send().await?;
    if !resp.status().is_success() {
        return Err(TranscribeError::SubmitRejected(resp.status().to_string()));
    }
    let submit: SubmitResponse = resp.json().await?;
    Ok(submit.id)
}

/// Итог одной попытки опроса — без сети. Ровно то, что умеет разобрать
/// `decide_poll`: разделение с `poll_once` и есть ответ на «как сделать
/// `poll_until_done` тестируемой без сети» — `poll_once` умеет только ходить в
/// сеть и превращать результат в `PollAttempt`, а решение, что считать
/// неудачей, когда сдаваться и когда сбрасывать счётчик, целиком живёт в
/// `decide_poll`, куда тест подсовывает заранее заготовленную серию значений
/// без единого запроса по сети.
#[derive(Debug, Clone, PartialEq)]
enum PollAttempt {
    /// Соединение не встало, оборвалось, истёк таймаут запроса (см.
    /// `POLL_REQUEST_TIMEOUT`) или тело ответа не удалось дочитать — всё
    /// преходящее по определению.
    NetworkError,
    /// HTTP-статус ответа, ещё не 2xx.
    HttpStatus(u16),
    /// Успешный HTTP-ответ, тело уже разобрано в исход задачи.
    Job(JobOutcome),
}

/// Что решил цикл после одной попытки опроса.
enum PollDecision {
    /// Опрос продолжается со следующим значением счётчика неудач подряд.
    Continue { failures: u32 },
    /// Цикл закончен — с любым исходом, включая ошибку.
    Done(Result<TrackResult, TranscribeError>),
}

/// Чистое решение одного шага опроса: без сети, без времени, без `await`.
///
/// Раньше это было зашито прямо внутри `while` в `poll_until_done`, и снаружи
/// не проверялось ничем, кроме двух тестов, сравнивавших константу с
/// константой (см. докблок задачи в дизайн-документе) — они были зелёными,
/// когда реальный цикл ещё жил на жёстко зашитых 360 итерациях по 10 секунд,
/// никак с `POLL_DEADLINE` не связанных. Вынесенная сюда функция — то, что
/// реально решает судьбу опроса, и тестам есть что проверить.
fn decide_poll(attempt: PollAttempt, failures: u32, job_id: &str) -> PollDecision {
    match attempt {
        PollAttempt::NetworkError => count_failure(failures, job_id),
        PollAttempt::HttpStatus(status) => match classify_poll_status(status) {
            PollFailure::Terminal(причина) => PollDecision::Done(Err(
                TranscribeError::JobFailed(job_id.to_string(), причина.to_string()),
            )),
            PollFailure::Transient => count_failure(failures, job_id),
        },
        // Дошли до ответа, который шлюз сумел составить, — значит связь есть,
        // и счётчик подряд идущих неудач обнуляется независимо от того, что
        // внутри: `Pending` тоже сбрасывает его, а не только терминальные исходы.
        PollAttempt::Job(JobOutcome::Pending) => PollDecision::Continue { failures: 0 },
        PollAttempt::Job(JobOutcome::Succeeded(result)) => PollDecision::Done(Ok(result)),
        PollAttempt::Job(JobOutcome::Cancelled) => {
            PollDecision::Done(Err(TranscribeError::JobCancelled(job_id.to_string())))
        }
        PollAttempt::Job(JobOutcome::Failed(msg)) => {
            PollDecision::Done(Err(TranscribeError::JobFailed(job_id.to_string(), msg)))
        }
    }
}

/// Общий хвост для обеих преходящих неудач (`NetworkError` и `Transient`
/// HTTP-статус) — счётчик им обоим безразличен к тому, чем именно опрос не
/// удался, лишь бы неудачи шли подряд.
fn count_failure(failures: u32, job_id: &str) -> PollDecision {
    let failures = failures + 1;
    if failures >= MAX_CONSECUTIVE_POLL_FAILURES {
        PollDecision::Done(Err(TranscribeError::PollLost(job_id.to_string())))
    } else {
        PollDecision::Continue { failures }
    }
}

/// Один сетевой поход за статусом задачи — без решений о том, что дальше.
/// Решение принимает чистая `decide_poll`; здесь только ввод-вывод.
async fn poll_once(
    client: &reqwest::Client,
    job_url: &str,
    key: &str,
    label: Label,
) -> Result<PollAttempt, TranscribeError> {
    let resp = match client
        .get(job_url)
        .bearer_auth(key)
        .timeout(POLL_REQUEST_TIMEOUT)
        .send()
        .await
    {
        Ok(resp) => resp,
        // Соединение не встало, оборвалось или истёк таймаут строкой выше —
        // всё это преходящее по определению.
        Err(_) => return Ok(PollAttempt::NetworkError),
    };

    if !resp.status().is_success() {
        return Ok(PollAttempt::HttpStatus(resp.status().as_u16()));
    }

    let body = match resp.text().await {
        Ok(body) => body,
        Err(_) => return Ok(PollAttempt::NetworkError),
    };

    // `?` тут осознанно НЕ преходящее: битый JSON от шлюза не лечится
    // повтором, поэтому разбор уходит наружу как есть, минуя счётчик неудач.
    Ok(PollAttempt::Job(parse_job_response(&body, label)?))
}

/// Каркас цикла опроса — без единого реального сетевого вызова: сам поход в
/// сеть спрятан за `attempt`, а время (`interval`, `deadline`) параметризовано
/// специально ради тестов, которым нечего делать с настоящими секундами и
/// часами. `poll_until_done` — единственный настоящий вызывающий, с реальными
/// `POLL_INTERVAL`/`POLL_DEADLINE` и `poll_once` внутри `attempt`.
async fn poll_loop<F, Fut>(
    job_id: &str,
    interval: Duration,
    deadline: Duration,
    mut attempt: F,
) -> Result<TrackResult, TranscribeError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<PollAttempt, TranscribeError>>,
{
    let начало = std::time::Instant::now();
    let mut подряд_неудач: u32 = 0;

    while начало.elapsed() < deadline {
        tokio::time::sleep(interval).await;

        let попытка = attempt().await?;
        match decide_poll(попытка, подряд_неудач, job_id) {
            PollDecision::Continue { failures } => {
                подряд_неудач = failures;
                continue;
            }
            PollDecision::Done(result) => return result,
        }
    }

    Err(TranscribeError::Timeout(job_id.to_string()))
}

/// Ждать задачу, пока шлюз отвечает нетерминальным статусом.
///
/// Предел — по настенным часам (`POLL_DEADLINE`), а не по числу итераций:
/// повторы после сетевых неудач не имеют права укорачивать бюджет ожидания.
/// Сама логика решения живёт в `poll_loop`/`decide_poll` и тестируется без
/// сети (см. их докблоки) — здесь только сборка с настоящим клиентом.
pub async fn poll_until_done(
    client: &reqwest::Client,
    gateway: &str,
    key: &str,
    job_id: &str,
    label: Label,
) -> Result<TrackResult, TranscribeError> {
    let job_url = format!("{gateway}/v1/jobs/{job_id}");
    let result = poll_loop(job_id, POLL_INTERVAL, POLL_DEADLINE, || {
        poll_once(client, &job_url, key, label)
    })
    .await;

    if let Err(TranscribeError::PollLost(_)) = &result {
        // Опрос сдался, но задача НА ШЛЮЗЕ никуда не делась: шлюз не в курсе,
        // что клиент перестал спрашивать, и продолжает её держать. concurrency:
        // 1 у шлюза означает, что до тех пор занят GPU, и следующая наша
        // задача в очереди не сдвинется. Гасим тем же DELETE, что и ручная
        // отмена (`cancel_job`, задача 10) — id под рукой, это тот же job_id.
        if let Err(e) = cancel_job(client, gateway, key, job_id).await {
            log::warn!("не удалось погасить потерянную задачу {job_id} на шлюзе: {e}");
        }
    }

    result
}

/// Одна дорожка целиком, с разводкой по режиму — единственное место в
/// приложении, которое решает «локально или на сервере» на уровне вызова.
///
/// `Server` — ровно то, что раньше было зашито прямо в `main.rs::дорожка_целиком`:
/// `submit`, затем `on_job(job_id)` (кладёт задачу в очередь отмены и шлёт
/// прогресс «Опрашиваю…»), затем `poll_until_done`. Ни порядок вызовов, ни
/// сами функции здесь не поменялись — они переехали на одну функцию выше, но
/// делают буквально то же самое.
///
/// `Local` игнорирует `client`/`url`/`key`/`on_job` целиком (замыкание не
/// вызывается вовсе — заводить задачу в очереди отмены здесь нечего) и идёт
/// прямиком в честную заглушку `local::transcribe_local`.
pub async fn transcribe_track(
    mode: Mode,
    client: &reqwest::Client,
    url: &str,
    key: &str,
    path: &Path,
    label: Label,
    on_job: impl FnOnce(String),
) -> Result<TrackResult, TranscribeError> {
    match mode {
        Mode::Local => local::transcribe_local(path, label).await.map_err(TranscribeError::from),
        Mode::Server => {
            let job_id = submit(client, url, key, path).await?;
            on_job(job_id.clone());
            poll_until_done(client, url, key, &job_id, label).await
        }
    }
}

/// Погасить задачу на шлюзе.
///
/// 404 и 409 — не ошибка: задача могла закончиться сама между нажатием и этим
/// вызовом, и «не нашли, что отменять» здесь означает ровно то, чего человек
/// и хотел.
///
/// Таймаут на `DELETE` (тот же `POLL_REQUEST_TIMEOUT`, что и на `GET` в
/// `poll_once`) обязателен, а не «на всякий случай»: единственный вызывающий
/// внутри пакета — `poll_until_done` при `PollLost`, то есть ровно момент,
/// когда шесть опросов подряд уже провалились и сеть признана мёртвой (сон
/// ноутбука, упавший VPN, протухший сокет в пуле). Без границы `.send()` на
/// уже нерабочей сети не возвращается никогда, `poll_until_done` не
/// возвращается вслед за ним, воркер очереди стоит, и строка навсегда
/// остаётся в «Расшифровываю…». Тот же путь используется ручной отменой
/// (`cancel_transcription` в `main.rs`) — без таймаута там повисшая задача
/// просто утекала бы отдельной таской.
pub async fn cancel_job(
    client: &reqwest::Client,
    gateway: &str,
    key: &str,
    job_id: &str,
) -> Result<(), TranscribeError> {
    let url = format!("{gateway}/v1/jobs/{job_id}");
    let resp = client
        .delete(&url)
        .bearer_auth(key)
        .timeout(POLL_REQUEST_TIMEOUT)
        .send()
        .await?;
    match resp.status().as_u16() {
        200 | 202 | 404 | 409 => Ok(()),
        code => Err(TranscribeError::CancelRejected(code.to_string())),
    }
}

fn fmt_ts(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    format!("{:02}:{:02}", total / 60, total % 60)
}

fn chronological_segments<'a>(mic: &'a TrackResult, system: &'a TrackResult) -> Vec<&'a Segment> {
    let mut all: Vec<&Segment> = mic.segments.iter().chain(system.segments.iter()).collect();
    all.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));
    all
}

/// Дорожка без сегментов (диаризация не уместилась на GPU) идёт отдельным
/// блоком в конце, а не пытается влезть в хронологию, которой для неё не
/// существует — см. докблок задачи в дизайн-документе.
fn tail_block(label: Label, track: &TrackResult) -> Option<String> {
    if !track.segments.is_empty() || track.text.is_empty() {
        return None;
    }
    Some(format!(
        "_{} (без привязки ко времени — диаризация не уместилась)_\n\n{}",
        label.title(),
        track.text
    ))
}

/// Присваивает различимым меткам `speaker` внутри дорожки порядковый номер по
/// первому появлению в сегментах.
///
/// Порядок появления, а не алфавит и не что-то ещё: это единственный критерий,
/// который не требует ничего сверх самих сегментов, и он же читается
/// естественно — кто заговорил первым, тот и «собеседник 1».
fn speaker_numbers(track: &TrackResult) -> HashMap<&str, usize> {
    let mut numbers = HashMap::new();
    for s in &track.segments {
        if let Some(sp) = s.speaker.as_deref() {
            let next = numbers.len() + 1;
            numbers.entry(sp).or_insert(next);
        }
    }
    numbers
}

/// Метка сегмента для markdown.
///
/// Номер добавляется, только если на дорожке различимо БОЛЬШЕ одного
/// голоса — `numbers.len() > 1`, а не просто «спикер известен». Один голос на
/// дорожке уже однозначно назван меткой дорожки (`Владелец`/`Собеседники`), и
/// пририсовывать к нему номер значило бы утверждать различие, которого нет:
/// шлюз мог вернуть один и тот же `SPEAKER_00` на все сегменты, и это не
/// повод писать «Собеседники (1)» так, будто есть кто-то ещё.
fn segment_label(s: &Segment, numbers: &HashMap<&str, usize>) -> String {
    if numbers.len() > 1 {
        if let Some(n) = s.speaker.as_deref().and_then(|sp| numbers.get(sp)) {
            return format!("{} ({n})", s.label.title());
        }
    }
    s.label.title().to_string()
}

pub fn merge_markdown(mic: &TrackResult, system: &TrackResult) -> String {
    let mic_speakers = speaker_numbers(mic);
    let sys_speakers = speaker_numbers(system);
    let mut blocks: Vec<String> = chronological_segments(mic, system)
        .into_iter()
        .map(|s| {
            let numbers = match s.label {
                Label::Owner => &mic_speakers,
                Label::Others => &sys_speakers,
            };
            format!("**[{}]** _{}_ {}", fmt_ts(s.start), segment_label(s, numbers), s.text)
        })
        .collect();
    blocks.extend(tail_block(Label::Owner, mic));
    blocks.extend(tail_block(Label::Others, system));
    blocks.join("\n\n")
}

pub fn merge_plain(mic: &TrackResult, system: &TrackResult) -> String {
    let mut lines: Vec<String> = chronological_segments(mic, system)
        .into_iter()
        .map(|s| s.text.clone())
        .collect();
    if mic.segments.is_empty() && !mic.text.is_empty() {
        lines.push(mic.text.clone());
    }
    if system.segments.is_empty() && !system.text.is_empty() {
        lines.push(system.text.clone());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ответ_succeeded_с_сегментами_даёт_result_с_нужной_меткой() {
        let body = r#"{"status":"succeeded","result":{"text":"привет","segments":[{"start":1.5,"speaker":"SPEAKER_00","text":"привет"}]}}"#;
        let outcome = parse_job_response(body, Label::Owner).unwrap();
        match outcome {
            JobOutcome::Succeeded(r) => {
                assert_eq!(r.segments.len(), 1);
                assert_eq!(r.segments[0].start, 1.5);
                assert_eq!(r.segments[0].label, Label::Owner);
                assert_eq!(r.segments[0].text, "привет");
            }
            _ => panic!("ожидали Succeeded"),
        }
    }

    /// Регресс на то, что раньше было осознанным упрощением: `speaker` от
    /// шлюза отбрасывался, и все «Собеседники» в системной дорожке сливались
    /// в одну метку, даже если шлюз честно различал голоса. Теперь метка
    /// обязана дойти до `Segment` — без неё `segment_label` нечем нумеровать.
    #[test]
    fn speaker_id_от_шлюза_попадает_в_segment() {
        let body = r#"{"status":"succeeded","result":{"text":"текст","segments":[{"start":0.0,"speaker":"SPEAKER_03","text":"текст"}]}}"#;
        let outcome = parse_job_response(body, Label::Others).unwrap();
        match outcome {
            JobOutcome::Succeeded(r) => {
                assert_eq!(r.segments[0].label, Label::Others);
                assert_eq!(r.segments[0].speaker.as_deref(), Some("SPEAKER_03"));
            }
            _ => panic!("ожидали Succeeded"),
        }
    }

    #[test]
    fn сегмент_без_speaker_в_ответе_даёт_none() {
        let body = r#"{"status":"succeeded","result":{"text":"текст","segments":[{"start":0.0,"text":"текст"}]}}"#;
        let outcome = parse_job_response(body, Label::Owner).unwrap();
        match outcome {
            JobOutcome::Succeeded(r) => assert_eq!(r.segments[0].speaker, None),
            _ => panic!("ожидали Succeeded"),
        }
    }

    #[test]
    fn ответ_succeeded_без_диаризации_даёт_пустые_segments_и_сплошной_текст() {
        let body = r#"{"status":"succeeded","result":{"text":"весь текст","diarization_skipped":true}}"#;
        let outcome = parse_job_response(body, Label::Others).unwrap();
        match outcome {
            JobOutcome::Succeeded(r) => {
                assert!(r.segments.is_empty());
                assert_eq!(r.text, "весь текст");
            }
            _ => panic!("ожидали Succeeded"),
        }
    }

    #[test]
    fn ответ_failed_даёт_ошибку_с_сообщением() {
        let body = r#"{"status":"failed","error":"CUDA out of memory"}"#;
        let outcome = parse_job_response(body, Label::Owner).unwrap();
        match outcome {
            JobOutcome::Failed(msg) => assert_eq!(msg, "CUDA out of memory"),
            _ => panic!("ожидали Failed"),
        }
    }

    #[test]
    fn ответ_error_считается_отказом() {
        let outcome = parse_job_response(r#"{"status":"error"}"#, Label::Owner).unwrap();
        assert!(matches!(outcome, JobOutcome::Failed(_)));
    }

    /// Отмена — не отказ. Отдельный исход нужен, чтобы гонка «опрос увидел
    /// отмену раньше локального сигнала» не показала красную ошибку на то,
    /// что человек сделал сам.
    #[test]
    fn ответ_cancelled_это_отмена_а_не_отказ() {
        let outcome = parse_job_response(r#"{"status":"cancelled"}"#, Label::Owner).unwrap();
        assert!(matches!(outcome, JobOutcome::Cancelled));
    }

    #[test]
    fn ответ_queued_и_processing_дают_pending() {
        for status in ["queued", "processing"] {
            let body = format!(r#"{{"status":"{status}"}}"#);
            let outcome = parse_job_response(&body, Label::Owner).unwrap();
            assert!(matches!(outcome, JobOutcome::Pending), "status={status}");
        }
    }

    #[test]
    fn битый_json_даёт_ошибку_разбора_а_не_панику() {
        assert!(parse_job_response("не json", Label::Owner).is_err());
    }

    fn seg(start: f64, label: Label, text: &str) -> Segment {
        Segment { start, label, text: text.to_string(), speaker: None }
    }

    fn seg_sp(start: f64, label: Label, speaker: &str, text: &str) -> Segment {
        Segment { start, label, text: text.to_string(), speaker: Some(speaker.to_string()) }
    }

    #[test]
    fn сегменты_двух_дорожек_сортируются_по_времени_вперемешку() {
        let mic = TrackResult { segments: vec![seg(5.0, Label::Owner, "второе")], text: String::new() };
        let system = TrackResult { segments: vec![seg(1.0, Label::Others, "первое")], text: String::new() };
        let md = merge_markdown(&mic, &system);
        assert!(md.find("первое").unwrap() < md.find("второе").unwrap());
    }

    #[test]
    fn метки_говорящего_это_владелец_и_собеседники_а_не_speaker_id() {
        let mic = TrackResult { segments: vec![seg(0.0, Label::Owner, "я")], text: String::new() };
        let system = TrackResult { segments: vec![seg(1.0, Label::Others, "они")], text: String::new() };
        let md = merge_markdown(&mic, &system);
        assert!(md.contains("Владелец"));
        assert!(md.contains("Собеседники"));
        assert!(!md.contains("SPEAKER"));
    }

    /// Один и тот же `speaker` на всех сегментах дорожки — это по-прежнему
    /// один голос, а не два. `numbers.len() > 1` не даёт `HashMap` слить
    /// повторные вставки одного ключа в двойку.
    #[test]
    fn один_и_тот_же_speaker_на_всех_сегментах_не_получает_номер() {
        let system = TrackResult {
            segments: vec![
                seg_sp(0.0, Label::Others, "SPEAKER_00", "первое"),
                seg_sp(1.0, Label::Others, "SPEAKER_00", "второе"),
            ],
            text: String::new(),
        };
        let md = merge_markdown(&TrackResult::default(), &system);
        assert!(
            md.contains("_Собеседники_"),
            "один различимый голос не должен нумероваться: {md}"
        );
    }

    /// Гвоздь задачи: диаризация реально различает голоса на шлюзе — это
    /// обязано стать видно в транскрипте, а не потеряться за общей меткой
    /// дорожки.
    #[test]
    fn несколько_speaker_на_дорожке_получают_номер_по_порядку_появления() {
        let system = TrackResult {
            segments: vec![
                seg_sp(0.0, Label::Others, "SPEAKER_02", "первым заговорил"),
                seg_sp(1.0, Label::Others, "SPEAKER_00", "вторым"),
                seg_sp(2.0, Label::Others, "SPEAKER_02", "снова первый"),
            ],
            text: String::new(),
        };
        let md = merge_markdown(&TrackResult::default(), &system);
        assert!(
            md.contains("_Собеседники (1)_ первым заговорил"),
            "первый по появлению SPEAKER_02 обязан стать (1): {md}"
        );
        assert!(
            md.contains("_Собеседники (2)_ вторым"),
            "второй по появлению SPEAKER_00 обязан стать (2): {md}"
        );
        assert!(
            md.contains("_Собеседники (1)_ снова первый"),
            "повторное появление того же SPEAKER_02 обязано получить тот же номер: {md}"
        );
    }

    /// Нумерация считается по дорожке отдельно: два голоса на system не
    /// имеют права навесить номер на единственный голос mic.
    #[test]
    fn нумерация_дорожек_независима() {
        let mic = TrackResult {
            segments: vec![seg_sp(0.0, Label::Owner, "SPEAKER_00", "я один")],
            text: String::new(),
        };
        let system = TrackResult {
            segments: vec![
                seg_sp(1.0, Label::Others, "SPEAKER_01", "первый"),
                seg_sp(2.0, Label::Others, "SPEAKER_02", "второй"),
            ],
            text: String::new(),
        };
        let md = merge_markdown(&mic, &system);
        assert!(
            md.contains("_Владелец_ я один"),
            "единственный голос на mic не должен получить номер: {md}"
        );
        assert!(md.contains("_Собеседники (1)_ первый"));
        assert!(md.contains("_Собеседники (2)_ второй"));
    }

    #[test]
    fn дорожка_без_сегментов_идёт_хвостовым_блоком_без_хронологии() {
        let mic = TrackResult { segments: vec![], text: "сплошной текст владельца".to_string() };
        let system = TrackResult {
            segments: vec![seg(1.0, Label::Others, "у них есть таймкоды")],
            text: String::new(),
        };
        let md = merge_markdown(&mic, &system);
        assert!(md.contains("сплошной текст владельца"));
        assert!(md.contains("без привязки ко времени"));
    }

    #[test]
    fn обе_дорожки_без_сегментов_дают_только_текстовые_блоки() {
        let mic = TrackResult { segments: vec![], text: "мик-текст".to_string() };
        let system = TrackResult { segments: vec![], text: "системный текст".to_string() };
        let md = merge_markdown(&mic, &system);
        assert!(md.contains("мик-текст"));
        assert!(md.contains("системный текст"));
    }

    #[test]
    fn пустые_дорожки_не_добавляют_пустых_блоков() {
        let md = merge_markdown(&TrackResult::default(), &TrackResult::default());
        assert!(!md.contains("без привязки ко времени"));
    }

    #[test]
    fn plain_текст_без_меток_говорящего_и_без_таймкодов() {
        let mic = TrackResult { segments: vec![seg(1.0, Label::Owner, "привет")], text: String::new() };
        let txt = merge_plain(&mic, &TrackResult::default());
        assert_eq!(txt, "привет");
    }

    #[test]
    fn plain_текст_тоже_хронологический() {
        let mic = TrackResult { segments: vec![seg(5.0, Label::Owner, "второе")], text: String::new() };
        let system = TrackResult { segments: vec![seg(1.0, Label::Others, "первое")], text: String::new() };
        let txt = merge_plain(&mic, &system);
        assert!(txt.find("первое").unwrap() < txt.find("второе").unwrap());
    }

    #[test]
    fn таймкод_форматируется_как_мм_сс() {
        assert_eq!(fmt_ts(65.0), "01:05");
        assert_eq!(fmt_ts(0.0), "00:00");
        assert_eq!(fmt_ts(3661.0), "61:01");
    }

    #[test]
    fn пятисотки_шлюза_считаются_преходящими() {
        for status in [500, 502, 503, 504] {
            assert_eq!(
                classify_poll_status(status),
                PollFailure::Transient,
                "status={status}"
            );
        }
    }

    /// Задача исчезла — повторять нечего: её не воскресит ни одна попытка.
    #[test]
    fn четыреста_четыре_терминально() {
        assert!(matches!(classify_poll_status(404), PollFailure::Terminal(_)));
    }

    /// Ключ или проект не те. Повтор через десять секунд ответит тем же самым,
    /// и так триста раз подряд — молча, потому что ошибка не всплывает.
    #[test]
    fn отказ_по_ключу_терминален() {
        for status in [401, 403] {
            assert!(
                matches!(classify_poll_status(status), PollFailure::Terminal(_)),
                "status={status}"
            );
        }
    }

    /// Гвоздь задачи: наш предел обязан быть заведомо больше, чем предел
    /// шлюза на один запрос (2 часа, whisper-client.ts:23). Иначе причину
    /// отказа называем мы — немым «не завершилась за отведённое время»
    /// вместо внятного сообщения от шлюза.
    ///
    /// Единственный оставшийся тест, который сравнивает константу с
    /// константой, — и он честно стережёт реальный внешний инвариант (предел
    /// шлюза), а не переписывает своими же словами то же число, что уже
    /// стоит в `POLL_DEADLINE`. Второй такой тест (`предела_в_шестьдесят_
    /// минут_больше_нет`) отсюда убран: он не мог упасть ни при какой
    /// реализации цикла — что и доказал живьём (см. докблок `decide_poll`).
    /// Его роль — «цикл действительно уважает `POLL_DEADLINE`, а не свою
    /// жёстко зашитую границу» — теперь честно проверяет
    /// `poll_loop_уважает_переданный_deadline_а_не_зашитое_число_попыток`
    /// ниже: она реально гоняет цикл и смотрит, что он делает, а не читает
    /// константу.
    #[test]
    fn наш_предел_больше_предела_шлюза() {
        const ПРЕДЕЛ_ШЛЮЗА: Duration = Duration::from_secs(2 * 60 * 60);
        assert!(
            POLL_DEADLINE > ПРЕДЕЛ_ШЛЮЗА,
            "клиент не имеет права сдаваться раньше сервера"
        );
    }

    // ---- decide_poll: чистая логика цикла опроса, без сети ------------------

    fn track(text: &str) -> TrackResult {
        TrackResult { segments: vec![], text: text.to_string() }
    }

    /// Прогоняет последовательность попыток через `decide_poll`, вручную
    /// продевая счётчик неудач между вызовами — ровно так, как это делает
    /// `poll_loop`. Возвращает решение по ПОСЛЕДНЕЙ попытке в серии; тест сам
    /// решает, что с ним делать.
    fn прогнать(попытки: impl IntoIterator<Item = PollAttempt>, job_id: &str) -> PollDecision {
        let mut failures = 0u32;
        let mut попытки = попытки.into_iter().peekable();
        loop {
            let attempt = попытки.next().expect("серия попыток не должна быть пустой");
            let decision = decide_poll(attempt, failures, job_id);
            if попытки.peek().is_none() {
                return decision;
            }
            match decision {
                PollDecision::Continue { failures: f } => failures = f,
                PollDecision::Done(_) => panic!(
                    "серия оборвалась раньше конца: цикл закончился внутри, а не на последней попытке"
                ),
            }
        }
    }

    /// Гвоздь задачи: ровно шесть сетевых неудач подряд — и ни одной раньше —
    /// обязаны дать `PollLost`.
    #[test]
    fn шесть_неудач_подряд_дают_polllost() {
        let попытки = std::iter::repeat(PollAttempt::NetworkError).take(6);
        match прогнать(попытки, "job-1") {
            PollDecision::Done(Err(TranscribeError::PollLost(id))) => assert_eq!(id, "job-1"),
            other => panic!("ожидали PollLost на шестой неудаче подряд, получили другое решение (вариант: {})",
                match other { PollDecision::Continue { .. } => "Continue", PollDecision::Done(_) => "Done(не PollLost)" }),
        }
    }

    /// Симметричный сторож: пять неудач подряд — это ещё не повод сдаваться.
    #[test]
    fn пять_неудач_подряд_не_дают_polllost() {
        let попытки = std::iter::repeat(PollAttempt::NetworkError).take(5);
        match прогнать(попытки, "job-1") {
            PollDecision::Continue { failures } => assert_eq!(failures, 5),
            PollDecision::Done(_) => panic!("пять неудач подряд не должны прекращать опрос"),
        }
    }

    /// Пять неудач и шестым — успех: `PollLost` не наступает, потому что
    /// успешный опрос обнуляет счётчик ДО того, как он дошёл до предела.
    #[test]
    fn пять_неудач_и_успех_не_дают_polllost() {
        let попытки = std::iter::repeat(PollAttempt::NetworkError)
            .take(5)
            .chain(std::iter::once(PollAttempt::Job(JobOutcome::Pending)));
        match прогнать(попытки, "job-1") {
            PollDecision::Continue { failures } => {
                assert_eq!(failures, 0, "успешный опрос обязан обнулить счётчик неудач")
            }
            PollDecision::Done(_) => panic!("успешный опрос после пяти неудач не должен обрывать цикл"),
        }
    }

    /// Счётчику всё равно, чем именно опрос не удался — сетевым обрывом или
    /// преходящим HTTP-статусом (500): подряд идущие неудачи разных видов
    /// суммируются в одну и ту же серию.
    #[test]
    fn сетевая_и_http_неудачи_считаются_в_одну_серию() {
        let попытки = vec![
            PollAttempt::NetworkError,
            PollAttempt::HttpStatus(503),
            PollAttempt::NetworkError,
            PollAttempt::HttpStatus(502),
            PollAttempt::NetworkError,
            PollAttempt::HttpStatus(500),
        ];
        match прогнать(попытки, "job-1") {
            PollDecision::Done(Err(TranscribeError::PollLost(id))) => assert_eq!(id, "job-1"),
            _ => panic!("шесть разнородных преходящих неудач подряд обязаны дать PollLost"),
        }
    }

    /// Гвоздь задачи: терминальный статус обрывает цикл СРАЗУ, не дожидаясь
    /// шести неудач подряд, — и это не `PollLost`, а `JobFailed` с внятной
    /// причиной от шлюза.
    #[test]
    fn терминальный_статус_404_прекращает_цикл_сразу() {
        match прогнать([PollAttempt::HttpStatus(404)], "job-1") {
            PollDecision::Done(Err(TranscribeError::JobFailed(id, msg))) => {
                assert_eq!(id, "job-1");
                assert!(msg.contains("не помнит"), "сообщение шлюза должно дойти до ошибки: {msg}");
            }
            other => panic!(
                "404 обязан оборвать цикл первой же попыткой, а не ждать шести неудач: {}",
                match other { PollDecision::Continue { .. } => "получили Continue", _ => "получили не тот Done" }
            ),
        }
    }

    /// То же самое для отказа по ключу — другой терминальный статус, тот же
    /// принцип «не ждать шести».
    #[test]
    fn терминальный_статус_401_прекращает_цикл_сразу() {
        match прогнать([PollAttempt::HttpStatus(401)], "job-1") {
            PollDecision::Done(Err(TranscribeError::JobFailed(..))) => {}
            _ => panic!("401 обязан оборвать цикл немедленно"),
        }
    }

    /// Успешный опрос обнуляет счётчик даже после нескольких неудач подряд, и
    /// цикл при этом продолжается (`Pending`), а не завершается.
    #[test]
    fn успешный_опрос_обнуляет_счётчик_неудач() {
        let попытки = vec![
            PollAttempt::NetworkError,
            PollAttempt::NetworkError,
            PollAttempt::NetworkError,
            PollAttempt::Job(JobOutcome::Pending),
        ];
        match прогнать(попытки, "job-1") {
            PollDecision::Continue { failures } => assert_eq!(failures, 0),
            PollDecision::Done(_) => panic!("Pending не должен завершать цикл"),
        }
    }

    /// `Succeeded` и `Cancelled` — разные исходы на выходе: первый превращает
    /// результат в `Ok`, второй остаётся ошибкой, но отдельной от `Failed`.
    #[test]
    fn succeeded_и_cancelled_различаются_на_выходе() {
        match прогнать([PollAttempt::Job(JobOutcome::Succeeded(track("текст")))], "job-1") {
            PollDecision::Done(Ok(result)) => assert_eq!(result.text, "текст"),
            _ => panic!("Succeeded обязан дать Ok с результатом"),
        }
        match прогнать([PollAttempt::Job(JobOutcome::Cancelled)], "job-1") {
            PollDecision::Done(Err(TranscribeError::JobCancelled(id))) => assert_eq!(id, "job-1"),
            _ => panic!("Cancelled обязан дать свой собственный вариант ошибки, не Failed"),
        }
    }

    /// `Failed` — третий, отдельный от `Cancelled` исход: шлюз сам отказал
    /// задаче, человек тут ни при чём.
    #[test]
    fn failed_даёт_jobfailed_с_сообщением_шлюза() {
        match прогнать([PollAttempt::Job(JobOutcome::Failed("CUDA out of memory".to_string()))], "job-1") {
            PollDecision::Done(Err(TranscribeError::JobFailed(id, msg))) => {
                assert_eq!(id, "job-1");
                assert_eq!(msg, "CUDA out of memory");
            }
            _ => panic!("Failed обязан дать JobFailed с тем же сообщением"),
        }
    }

    // ---- poll_loop: сборка вокруг decide_poll, без сети, без настоящего времени ---

    /// Обёртка над очередью канonных попыток: реализует `FnMut() -> Fut`,
    /// которого просит `poll_loop`, без единого сетевого вызова.
    fn канон(
        попытки: Vec<PollAttempt>,
    ) -> impl FnMut() -> std::future::Ready<Result<PollAttempt, TranscribeError>> {
        let mut it = попытки.into_iter();
        move || {
            std::future::ready(Ok(it
                .next()
                .expect("poll_loop запросил попытку сверх заготовленной серии")))
        }
    }

    /// Собранный `poll_loop` (не только чистая `decide_poll`) обязан реально
    /// довести шесть сетевых неудач подряд до `PollLost` — это ловит ошибки
    /// именно в сборке (например, забытое присваивание счётчика между
    /// итерациями `while`), а не в самой логике решения.
    #[tokio::test]
    async fn poll_loop_шесть_неудач_подряд_дают_polllost() {
        let попытки = канон(vec![PollAttempt::NetworkError; 6]);
        let result = poll_loop("job-1", Duration::ZERO, Duration::from_secs(60), попытки).await;
        assert!(
            matches!(&result, Err(TranscribeError::PollLost(id)) if id == "job-1"),
            "получили {result:?}"
        );
    }

    /// Гвоздь регресса на удалённый жёстко зашитый потолок (было 360 попыток
    /// по 10 секунд = 60 минут, никак не связанных с `POLL_DEADLINE`): цикл
    /// обязан закончиться по НАСТОЯЩЕМУ переданному `deadline`, а не раньше и
    /// не позже, даже если опрашиваемый вечно висит в `Pending`.
    ///
    /// Одного `elapsed() >= deadline` тут недостаточно — это одностороннее
    /// условие: цикл с зашитым `for _ in 0..360 { ... }` вместо `while
    /// начало.elapsed() < deadline` тоже успевает натикать больше 30 мс (360
    /// попыток по 5 мс = 1.8 с) и тест бы прошёл, ничего не заметив. Поэтому
    /// вторая, верхняя граница считает сами вызовы `attempt`: при
    /// `interval=5мс`/`deadline=30мс` их должно быть около 6, и уж точно не
    /// 360. Время не годится для верхней границы (машина под нагрузкой может
    /// притормозить `sleep`), а счётчик попыток детерминирован независимо от
    /// планировщика.
    #[tokio::test]
    async fn poll_loop_уважает_переданный_deadline_а_не_зашитое_число_попыток() {
        let попыток = std::cell::Cell::new(0u32);
        let deadline = Duration::from_millis(30);
        let interval = Duration::from_millis(5);
        let всегда_pending = || {
            попыток.set(попыток.get() + 1);
            std::future::ready(Ok(PollAttempt::Job(JobOutcome::Pending)))
        };
        let начало = std::time::Instant::now();

        let result = poll_loop("job-1", interval, deadline, всегда_pending).await;

        assert!(matches!(result, Err(TranscribeError::Timeout(id)) if id == "job-1"));
        assert!(
            начало.elapsed() >= deadline,
            "цикл обязан отработать хотя бы весь переданный deadline, а не выйти раньше"
        );

        // ~6 ожидаемых попыток (30мс / 5мс) плюс запас на дрожание таймера —
        // но никак не 360, которые дал бы зашитый потолок.
        let потолок_попыток = (deadline.as_millis() / interval.as_millis()) as u32 + 5;
        assert!(
            попыток.get() <= потолок_попыток,
            "попыток опроса {}, ожидали не больше {} — похоже, цикл крутится \
             на зашитом числе итераций, а не на переданном deadline",
            попыток.get(),
            потолок_попыток
        );
    }

    /// Тот же цикл целиком доводит успех до `Ok` после нескольких `Pending` —
    /// без этого тест выше проверял бы только провал, а не рабочий путь.
    #[tokio::test]
    async fn poll_loop_доводит_succeeded_до_ok_через_несколько_pending() {
        let попытки = канон(vec![
            PollAttempt::Job(JobOutcome::Pending),
            PollAttempt::Job(JobOutcome::Pending),
            PollAttempt::Job(JobOutcome::Succeeded(track("готово"))),
        ]);
        let result = poll_loop("job-1", Duration::ZERO, Duration::from_secs(60), попытки).await;
        match result {
            Ok(r) => assert_eq!(r.text, "готово"),
            Err(e) => panic!("ожидали Ok(\"готово\"), получили ошибку: {e}"),
        }
    }

    #[test]
    fn отсутствие_поля_и_незнакомое_значение_дают_сервер() {
        assert_eq!(effective_mode(None), Mode::Server);
        assert_eq!(effective_mode(Some("что-то незнакомое")), Mode::Server);
        assert_eq!(effective_mode(Some("server")), Mode::Server);
    }

    #[test]
    fn local_разбирается_явно() {
        assert_eq!(effective_mode(Some("local")), Mode::Local);
    }

    /// Гвоздь задачи «развилка выбирает правильную ветку»: в режиме `Local`
    /// `transcribe_track` не трогает сеть вовсе (замыкание `on_job` не
    /// вызывается — если бы дошло до `submit`, оно бы сработало) и отдаёт
    /// ровно ключ заглушки, а не какую-то сетевую ошибку от фиктивных
    /// `url`/`key`.
    #[tokio::test]
    async fn развилка_в_local_идёт_в_заглушку_а_не_в_сеть() {
        let client = reqwest::Client::new();
        let err = transcribe_track(
            Mode::Local,
            &client,
            "http://127.0.0.1:1", // заведомо нерабочий адрес — если бы развилка
            // промахнулась мимо Local, тест упал бы с сетевой ошибкой, а не с
            // "local.notWired"
            "key",
            Path::new("/dev/null"),
            Label::Owner,
            |_| panic!("в Local-режиме job на шлюзе не заводится"),
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "local.notWired");
    }
}
