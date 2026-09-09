//! Клиент `whisper-server` из whisper.cpp — второй тип сервера расшифровки
//! рядом со шлюзом selfhost-ai-lab (`transcribe.rs`).
//!
//! Протокол проверен по исходникам `examples/server/server.cpp` 09.09.2026:
//! один синхронный `POST {url}/inference`, multipart с `file`, `language`,
//! `response_format`, `temperature`; ответ приходит после полного расчёта.
//! Ключа нет, отмены нет, один запрос за раз. Подробности и что из этого
//! следует для интерфейса — `docs/2026-09-09-whisper-server-design.md`.

use crate::transcribe::{Label, Segment, TrackResult, TranscribeError, POLL_DEADLINE};
use serde::Deserialize;
use std::path::Path;

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

/// Сколько символов тела ошибки показать человеку. Тела whisper-server
/// однострочные; предел — на случай HTML-страницы от чужого сервера по
/// этому адресу.
const ERROR_BODY_CHARS: usize = 200;

/// Одна дорожка целиком: отправить, дождаться, разобрать.
///
/// Таймаута на сам запрос нет: ответ приходит только после полного расчёта,
/// а это на CPU — до часа на часовую дорожку. Граница одна, общая на весь
/// вызов, — `POLL_DEADLINE`, тот же предел, что у ожидания задачи на шлюзе:
/// клиент не сдаётся раньше, чем сдался бы там.
///
/// Отмена (`select!` в `spawn_transcribe_worker`) бросает эту future, reqwest
/// закрывает соединение — для приложения дорожка отменена сразу. Сервер при
/// этом ДОСЧИТЫВАЕТ запрос до конца и только потом замечает обрыв (он
/// однопоточный, API отмены у него нет); следующая дорожка встанет за ним.
/// Это записано в README, обойти нельзя.
pub async fn transcribe(
    client: &reqwest::Client,
    url: &str,
    wav_path: &Path,
    label: Label,
) -> Result<TrackResult, TranscribeError> {
    let track = wav_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("audio.wav")
        .to_string();
    // `Part::file` — потоком через открытый дескриптор, тот же приём, что
    // `transcribe::submit`: дорожка на часовую встречу — ~115 МБ.
    let part = reqwest::multipart::Part::file(wav_path)
        .await?
        .file_name(track.clone())
        .mime_str("audio/wav")?;
    let form = reqwest::multipart::Form::new()
        .part("file", part)
        // `auto` — язык встречи не связан с языком интерфейса.
        .text("language", "auto")
        .text("response_format", "verbose_json")
        .text("temperature", "0.0");

    let запрос = async {
        let resp = client.post(format!("{url}/inference")).multipart(form).send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        Ok::<_, TranscribeError>((status, body))
    };
    let (status, body) = match tokio::time::timeout(POLL_DEADLINE, запрос).await {
        Ok(r) => r?,
        Err(_) => return Err(TranscribeError::Timeout(track)),
    };
    if !status.is_success() {
        let короткое: String = body.chars().take(ERROR_BODY_CHARS).collect();
        return Err(TranscribeError::JobFailed(track, format!("{status}: {}", короткое.trim())));
    }
    parse_response(&body, label)
}

/// Стаб whisper-server и тестовый WAV-файл — вынесены из `mod tests` в
/// отдельный `pub(crate)` модуль, потому что нужны не только сетевым тестам
/// этого файла, но и тесту развилки в `transcribe.rs`: там нужен настоящий
/// ответ whisper-server (`verbose_json`), чтобы отличить свою ветку от
/// ветки шлюза, а не просто получить сетевую ошибку — её дала бы и ветка
/// шлюза на том же адресе.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Стаб whisper-server: принимает ОДИН запрос, дочитывает его тело по
    /// `Content-Length` (иначе reqwest увидит обрыв посреди отправки файла)
    /// и отвечает заданным статусом и телом. Возвращает адрес.
    pub(crate) async fn стаб(status: &'static str, body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let (mut header_end, mut content_length) = (None, 0usize);
            loop {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if header_end.is_none() {
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = Some(pos + 4);
                        let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                        content_length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                    }
                }
                if let Some(he) = header_end {
                    if buf.len() - he >= content_length {
                        break;
                    }
                }
            }
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.shutdown().await.ok();
        });
        addr
    }

    /// Каталог уникален на каждый вызов (счётчик + pid, тот же приём, что
    /// `ScratchDir` в `local.rs`): три теста — это `#[tokio::test]`,
    /// выполняются в одном процессе конкурентно, и один путь на всех дал бы
    /// гонку — `WavWriter::create` в одном тесте обрезает файл, который
    /// `Part::file` в другом уже открыл и с которого снял длину, а стаб потом
    /// вечно ждёт байты по уже нечестному `Content-Length`.
    pub(crate) fn wav_файл() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("mr-wcpp-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mic.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..1600 {
            w.write_sample(0i16).unwrap();
        }
        w.finalize().unwrap();
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::test_support::{стаб, wav_файл};

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

    /// Живой ответ whisper-server b4938+ на русскую речь (09.09.2026), без
    /// токенов. Если формат `verbose_json` у сервера поменяется, первым
    /// упадёт этот тест, а не расшифровка у человека.
    const ЖИВОЙ_ОТВЕТ: &str = include_str!("../fixtures/whisper_cpp_inference.json");

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

    #[test]
    fn живой_ответ_сервера_разбирается() {
        let r = parse_response(ЖИВОЙ_ОТВЕТ, Label::Owner).unwrap();
        assert!(!r.segments.is_empty());
        assert!(r.text.to_lowercase().contains("модел"), "{}", r.text);
        assert!(r.segments.windows(2).all(|w| w[0].start <= w[1].start), "таймкоды по возрастанию");
    }

    #[tokio::test]
    async fn успешный_ответ_разбирается_в_дорожку() {
        let url = стаб("200 OK", ОТВЕТ).await;
        let client = reqwest::Client::new();
        let r = transcribe(&client, &url, &wav_файл(), Label::Owner).await.unwrap();
        assert_eq!(r.segments.len(), 2);
        assert_eq!(r.text, "Привет. Это проверка.");
    }

    /// 400/500 от whisper-server — короткий человеческий текст («no 'file'
    /// field», «failed to read WAV»); он и уходит в карточку записи, вместе
    /// со статусом. Никакого «проверьте ключ»: ключа у этого сервера нет.
    #[tokio::test]
    async fn ошибка_сервера_уходит_с_её_текстом_и_без_совета_про_ключ() {
        let url = стаб("500 Internal Server Error", "failed to read WAV").await;
        let client = reqwest::Client::new();
        let err = transcribe(&client, &url, &wav_файл(), Label::Owner).await.unwrap_err();
        let текст = err.to_string();
        assert!(текст.contains("500"), "{текст}");
        assert!(текст.contains("failed to read WAV"), "{текст}");
        assert!(!текст.contains("ключ"), "{текст}");
        assert!(matches!(err, TranscribeError::JobFailed(ref track, _) if track == "mic.wav"), "{err}");
    }

    /// Сервер не запущен — сетевая ошибка reqwest как есть: «connection
    /// refused» на localhost и есть честный диагноз.
    #[tokio::test]
    async fn недоступный_сервер_это_сетевая_ошибка() {
        let client = reqwest::Client::new();
        let err = transcribe(&client, "http://127.0.0.1:1", &wav_файл(), Label::Owner).await.unwrap_err();
        assert!(matches!(err, TranscribeError::Network(_)), "{err}");
    }
}
