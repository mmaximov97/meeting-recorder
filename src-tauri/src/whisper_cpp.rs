//! Клиент `whisper-server` из whisper.cpp — второй тип сервера расшифровки
//! рядом со шлюзом selfhost-ai-lab (`transcribe.rs`).
//!
//! Протокол проверен по исходникам `examples/server/server.cpp` 09.09.2026:
//! один синхронный `POST {url}/inference`, multipart с `file`, `language`,
//! `response_format`, `temperature`; ответ приходит после полного расчёта.
//! Ключа нет, отмены нет, один запрос за раз. Подробности и что из этого
//! следует для интерфейса — `docs/2026-09-09-whisper-server-design.md`.

use crate::transcribe::{Label, Segment, TrackResult, TranscribeError};
use serde::Deserialize;

/// Ровно те поля `verbose_json`, что нужны. Остальное (`id`, `end`, `tokens`,
/// `language`, вероятности языков) не читается.
#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    text: String,
    #[serde(default)]
    segments: Vec<RawSegment>,
}

#[derive(Deserialize)]
struct RawSegment {
    /// `Option`: сервер, запущенный с `-nt`/`--no-timestamps`, поля не шлёт.
    start: Option<f64>,
    text: String,
}

/// `verbose_json` → `TrackResult`.
///
/// Тексты обрезаются по краям: whisper отдаёт сегменты с ведущим пробелом.
/// `speaker` всегда `None` — whisper-server не различает говорящих, и
/// `merge_markdown` тогда подпишет дорожку целиком «Владелец»/«Собеседники».
/// Сегмент без `start` — ошибка, а не ноль: без таймкодов дорожки не свести.
/// Пустой список сегментов при непустом тексте — не ошибка (дорожка целиком
/// тишина, так бывает).
pub fn parse_response(body: &str, label: Label) -> Result<TrackResult, TranscribeError> {
    let resp: Response = serde_json::from_str(body)?;
    let mut segments = Vec::with_capacity(resp.segments.len());
    for s in resp.segments {
        let Some(start) = s.start else {
            return Err(TranscribeError::NoTimestamps);
        };
        segments.push(Segment { start, label, text: s.text.trim().to_string(), speaker: None });
    }
    Ok(TrackResult { segments, text: resp.text.trim().to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ОТВЕТ: &str = r#"{
        "task": "transcribe",
        "language": "russian",
        "duration": 4.2,
        "text": " Привет. Это проверка.",
        "segments": [
            {"id": 0, "start": 0.0, "end": 1.5, "text": " Привет.", "tokens": [50364, 8391]},
            {"id": 1, "start": 1.5, "end": 4.2, "text": " Это проверка.", "tokens": [1]}
        ]
    }"#;

    #[test]
    fn сегменты_разбираются_с_таймкодами_и_без_ведущих_пробелов() {
        let r = parse_response(ОТВЕТ, Label::Owner).unwrap();
        assert_eq!(r.text, "Привет. Это проверка.");
        assert_eq!(r.segments.len(), 2);
        assert_eq!(r.segments[0].start, 0.0);
        assert_eq!(r.segments[0].text, "Привет.");
        assert_eq!(r.segments[1].start, 1.5);
        assert_eq!(r.segments[1].text, "Это проверка.");
    }

    #[test]
    fn метка_дорожки_проставляется_а_говорящий_всегда_пуст() {
        let r = parse_response(ОТВЕТ, Label::Others).unwrap();
        assert!(r.segments.iter().all(|s| s.label == Label::Others));
        assert!(r.segments.iter().all(|s| s.speaker.is_none()));
    }

    /// Дорожка целиком тишина: whisper отдаёт пустой список и пустой текст.
    #[test]
    fn пустые_сегменты_не_ошибка() {
        let r = parse_response(r#"{"text": "", "segments": []}"#, Label::Owner).unwrap();
        assert!(r.segments.is_empty());
        assert_eq!(r.text, "");
    }

    /// Сервер запущен с `-nt`: таймкодов нет, свести дорожки нечем.
    #[test]
    fn сегмент_без_таймкода_это_ошибка_а_не_ноль() {
        let body = r#"{"text": " a", "segments": [{"id": 0, "text": " a"}]}"#;
        let err = parse_response(body, Label::Owner).unwrap_err();
        assert!(matches!(err, TranscribeError::NoTimestamps), "получили {err}");
    }

    #[test]
    fn мусор_вместо_json_это_ошибка_разбора() {
        let err = parse_response("<html>404</html>", Label::Owner).unwrap_err();
        assert!(matches!(err, TranscribeError::Parse(_)), "получили {err}");
    }
}
