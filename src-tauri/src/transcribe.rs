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
}
