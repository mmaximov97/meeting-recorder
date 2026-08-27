//! HTTP-клиент шлюза транскрипции: submit одной дорожки, поллинг задачи,
//! слияние двух дорожек по времени. Контракт API — тот же, что уже проверен
//! скиллом `ailab-transcribe` (`POST /v1/audio/transcriptions/async` +
//! `GET /v1/jobs/:id`), здесь не изобретается заново.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

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
}

/// Шаг опроса задачи. Прежний, десять секунд: на минутных масштабах работы
/// шлюза чаще спрашивать нечего.
pub const POLL_INTERVAL: Duration = Duration::from_secs(10);

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

/// Размер дорожки. Отдельной функцией, потому что его проверяет тест: тело
/// `submit` без живого шлюза не проверить.
async fn длина_файла(path: &Path) -> Result<u64, TranscribeError> {
    Ok(tokio::fs::metadata(path).await?.len())
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
    let длина = длина_файла(wav_path).await?;
    let file_name = wav_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("audio.wav")
        .to_string();
    let файл = tokio::fs::File::open(wav_path).await?;
    let поток = tokio_util::io::ReaderStream::new(файл);
    let part = reqwest::multipart::Part::stream_with_length(reqwest::Body::wrap_stream(поток), длина)
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

/// Ждать задачу, пока шлюз отвечает нетерминальным статусом.
///
/// Предел — по настенным часам (`POLL_DEADLINE`), а не по числу итераций:
/// повторы после сетевых неудач не имеют права укорачивать бюджет ожидания.
pub async fn poll_until_done(
    client: &reqwest::Client,
    gateway: &str,
    key: &str,
    job_id: &str,
    label: Label,
) -> Result<TrackResult, TranscribeError> {
    let job_url = format!("{gateway}/v1/jobs/{job_id}");
    let начало = std::time::Instant::now();
    let mut подряд_неудач: u32 = 0;

    while начало.elapsed() < POLL_DEADLINE {
        tokio::time::sleep(POLL_INTERVAL).await;

        let resp = match client.get(&job_url).bearer_auth(key).send().await {
            Ok(resp) => resp,
            // Соединение не встало или оборвалось — это всегда преходящее.
            Err(_) => {
                подряд_неудач += 1;
                if подряд_неудач >= MAX_CONSECUTIVE_POLL_FAILURES {
                    return Err(TranscribeError::PollLost(job_id.to_string()));
                }
                continue;
            }
        };

        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            match classify_poll_status(status) {
                PollFailure::Terminal(причина) => {
                    return Err(TranscribeError::JobFailed(job_id.to_string(), причина.to_string()));
                }
                PollFailure::Transient => {
                    подряд_неудач += 1;
                    if подряд_неудач >= MAX_CONSECUTIVE_POLL_FAILURES {
                        return Err(TranscribeError::PollLost(job_id.to_string()));
                    }
                    continue;
                }
            }
        }

        let body = match resp.text().await {
            Ok(body) => body,
            Err(_) => {
                подряд_неудач += 1;
                if подряд_неудач >= MAX_CONSECUTIVE_POLL_FAILURES {
                    return Err(TranscribeError::PollLost(job_id.to_string()));
                }
                continue;
            }
        };

        // Дошли до ответа, который шлюз сумел составить, — значит связь есть.
        подряд_неудач = 0;
        match parse_job_response(&body, label)? {
            JobOutcome::Pending => continue,
            JobOutcome::Succeeded(result) => return Ok(result),
            JobOutcome::Cancelled => {
                return Err(TranscribeError::JobCancelled(job_id.to_string()))
            }
            JobOutcome::Failed(msg) => {
                return Err(TranscribeError::JobFailed(job_id.to_string(), msg))
            }
        }
    }

    Err(TranscribeError::Timeout(job_id.to_string()))
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

    /// Стрим без явной длины уехал бы chunked-передачей. Принимающая сторона —
    /// multipart fastify (`ai-lab/src/routes/audio-routes.ts:113`), и известный
    /// заранее размер ей полезнее. Тест держит длину видимой, а не проверяет
    /// сам факт стрима: проверить его без сети нечем.
    #[tokio::test]
    async fn длина_дорожки_читается_без_чтения_файла_целиком() {
        let dir = std::env::temp_dir().join("mr-submit-len");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("тест.mic.wav");
        std::fs::write(&path, vec![0u8; 5000]).unwrap();

        assert_eq!(длина_файла(&path).await.unwrap(), 5000);

        std::fs::remove_file(&path).ok();
    }

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
    #[test]
    fn наш_предел_больше_предела_шлюза() {
        const ПРЕДЕЛ_ШЛЮЗА: Duration = Duration::from_secs(2 * 60 * 60);
        assert!(
            POLL_DEADLINE > ПРЕДЕЛ_ШЛЮЗА,
            "клиент не имеет права сдаваться раньше сервера"
        );
    }

    /// Регресс на удалённый потолок: раньше это было 360 попыток по 10 секунд,
    /// ровно 60 минут, и вторая дорожка часовой встречи в него не влезала.
    #[test]
    fn предела_в_шестьдесят_минут_больше_нет() {
        assert!(POLL_DEADLINE > Duration::from_secs(60 * 60));
    }
}
