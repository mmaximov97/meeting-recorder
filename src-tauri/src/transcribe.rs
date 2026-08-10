//! HTTP-клиент шлюза транскрипции: submit одной дорожки, поллинг задачи,
//! слияние двух дорожек по времени. Контракт API — тот же, что уже проверен
//! скиллом `ailab-transcribe` (`POST /v1/audio/transcriptions/async` +
//! `GET /v1/jobs/:id`), здесь не изобретается заново.

use serde::Deserialize;
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
}

pub enum JobOutcome {
    Pending,
    Succeeded(TrackResult),
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
                .map(|s| Segment { start: s.start, label, text: s.text })
                .collect();
            Ok(JobOutcome::Succeeded(TrackResult { segments, text: result.text }))
        }
        "failed" | "error" | "cancelled" => {
            Ok(JobOutcome::Failed(resp.error.unwrap_or_else(|| "неизвестная ошибка".to_string())))
        }
        _ => Ok(JobOutcome::Pending),
    }
}

pub async fn submit_and_poll(
    client: &reqwest::Client,
    gateway: &str,
    key: &str,
    wav_path: &Path,
    label: Label,
) -> Result<TrackResult, TranscribeError> {
    let bytes = tokio::fs::read(wav_path).await?;
    let file_name = wav_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("audio.wav")
        .to_string();
    let part = reqwest::multipart::Part::bytes(bytes)
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

    let job_url = format!("{gateway}/v1/jobs/{}", submit.id);
    // 360 попыток по 10с — тот же лимит (~60 минут), что уже проверен в
    // ailab-transcribe/scripts/transcribe.sh, не изобретается заново.
    for _ in 0..360 {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let body = client.get(&job_url).bearer_auth(key).send().await?.text().await?;
        match parse_job_response(&body, label)? {
            JobOutcome::Pending => continue,
            JobOutcome::Succeeded(result) => return Ok(result),
            JobOutcome::Failed(msg) => return Err(TranscribeError::JobFailed(submit.id, msg)),
        }
    }
    Err(TranscribeError::Timeout(submit.id))
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

pub fn merge_markdown(mic: &TrackResult, system: &TrackResult) -> String {
    let mut blocks: Vec<String> = chronological_segments(mic, system)
        .into_iter()
        .map(|s| format!("**[{}]** _{}_ {}", fmt_ts(s.start), s.label.title(), s.text))
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

    #[test]
    fn speaker_id_от_шлюза_не_попадает_в_segment() {
        let body = r#"{"status":"succeeded","result":{"text":"текст","segments":[{"start":0.0,"speaker":"SPEAKER_03","text":"текст"}]}}"#;
        let outcome = parse_job_response(body, Label::Others).unwrap();
        match outcome {
            JobOutcome::Succeeded(r) => assert_eq!(r.segments[0].label, Label::Others),
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
    fn ответ_error_и_cancelled_тоже_считаются_отказом() {
        for status in ["error", "cancelled"] {
            let body = format!(r#"{{"status":"{status}"}}"#);
            let outcome = parse_job_response(&body, Label::Owner).unwrap();
            assert!(matches!(outcome, JobOutcome::Failed(_)), "status={status}");
        }
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
        Segment { start, label, text: text.to_string() }
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
}
