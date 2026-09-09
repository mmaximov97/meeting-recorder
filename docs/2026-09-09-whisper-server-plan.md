# whisper-server как второй тип сервера расшифровки — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** MeetRec умеет расшифровывать записи через `whisper-server` из whisper.cpp, поднятый на этом же компьютере, тем же меню и той же очередью, что и через шлюз selfhost-ai-lab.

**Architecture:** В `transcribe::Mode` появляется третий вариант `WhisperCpp`; конфиг получает поле `transcribe_server`; новый модуль `src-tauri/src/whisper_cpp.rs` делает один синхронный `POST /inference` и разбирает `verbose_json` в те же `Segment`/`TrackResult`; настройки получают выпадашку «Тип сервера», README — инструкцию по установке сервера на Windows и macOS.

**Tech Stack:** Rust (Tauri 2, reqwest 0.12 multipart, tokio, serde_json, thiserror), vanilla JS (`ui/main.js`), `ui/i18n/strings.json`.

**Spec:** `docs/2026-09-09-whisper-server-design.md` — план аргументирует от неё, исполнитель читает обе.

## Global Constraints

- Код, комментарии, докблоки и имена тестов — по-русски, как во всём репозитории. Имена функций в `main.rs` бывают кириллическими (`ключи_шлюза`) — это норма проекта.
- Каждая правка — сначала падающий тест, потом код (TDD).
- **GUI-крейт (`src-tauri`) под Linux не собирается и тесты его здесь не запускаются.** Проверка на этой машине — компиляция вместе с тестами:
  ```bash
  export RUSTUP_HOME=/data/cypher-ai-lab/rust/rustup CARGO_HOME=/data/cypher-ai-lab/rust/cargo \
         XWIN_CACHE_DIR=/data/cypher-ai-lab/rust/xwin CARGO_TARGET_DIR=/data/cypher-ai-lab/rust/mr-target
  LLVM=/data/cypher-ai-lab/rust/llvm/root/usr/lib/llvm-18
  export LD_LIBRARY_PATH="$LLVM/lib:/data/cypher-ai-lab/rust/llvm/root/usr/lib/x86_64-linux-gnu"
  export PATH="$LLVM/bin:$CARGO_HOME/bin:$PATH"
  flock -w 3600 /tmp/cypher-heavy.lock cargo xwin check -p meeting-recorder-gui --target x86_64-pc-windows-msvc --all-targets -j 4
  ```
  «Тест падает» на этой машине означает: не компилируется из-за отсутствующего символа. Запуск тестов — `cargo test --workspace` на macOS/Windows или GitHub Actions `check.yml` после пуша ветки. На Mac/Windows шаги «Run» ниже выполняются буквально.
- Тяжёлое (`cargo`, сборка whisper.cpp) — только под `flock -w 3600 /tmp/cypher-heavy.lock`, перед запуском `cat /proc/loadavg`, при 1-минутной нагрузке выше 12 — ждать.
- Коммиты без `Co-Authored-By`. Сообщения по-русски, префиксы `feat:`/`fix:`/`docs:`/`test:`.
- Ветка: `feat/whisper-server` (уже есть, от `origin/master`, в ней лежит спека).
- Ничего в `local.rs` и в скрытом режиме «На этом компьютере» не менять.
- Существующие тесты остаются зелёными; количество тестов не уменьшается.

---

## Карта файлов

| Файл | Что меняется |
|---|---|
| `src-tauri/src/transcribe.rs` | `Mode::Server` → `Mode::Gateway`, новый `Mode::WhisperCpp`, `effective_mode` с двумя аргументами, ветка в `transcribe_track`, вариант `TranscribeError::NoTimestamps` |
| `src-tauri/src/whisper_cpp.rs` | **новый**: `parse_response`, `transcribe` |
| `src-tauri/src/config.rs` | поле `transcribe_server` |
| `src-tauri/src/main.rs` | `mod whisper_cpp`, команда `set_transcribe_server`, `ключи_шлюза` → `параметры_сервера`, стадия в `дорожка_целиком` |
| `src-tauri/Cargo.toml` | tokio feature `net` (для тестового HTTP-стаба) |
| `src-tauri/fixtures/whisper_cpp_inference.json` | **новый**: живой ответ whisper-server |
| `ui/index.html`, `ui/main.js`, `ui/i18n/strings.json` | выпадашка типа сервера, подсказка, ссылка на инструкцию |
| `README.ru.md`, `README.md` | раздел «Свой whisper-сервер на этом компьютере» |
| `docs/2026-09-02-local-transcription-spec.md` | абзац: встроенный движок отложен |

---

### Task 1: Третий режим в развилке и тип сервера в конфиге

**Files:**
- Modify: `src-tauri/src/transcribe.rs:14-40` (enum `Mode`, `effective_mode`), `:454-460` (match в `transcribe_track`), тесты `:1131-1145`
- Modify: `src-tauri/src/config.rs:43-49` (поле), четыре литерала `Config { ... }` в тестах, тесты в конце файла
- Modify: `src-tauri/src/main.rs:1394` (вызов `effective_mode`), `:1578` (`Mode::Server`), `:1147-1152` (соседняя команда), список `generate_handler!`, тесты `:2689-2707`

**Interfaces:**
- Produces: `transcribe::Mode { Gateway, WhisperCpp, Local }`; `transcribe::effective_mode(cfg_mode: Option<&str>, cfg_server: Option<&str>) -> Mode`; `Config.transcribe_server: Option<String>`; команда Tauri `set_transcribe_server(kind: Option<String>)`.

- [ ] **Step 1: Падающие тесты на `effective_mode` с двумя аргументами**

В `src-tauri/src/transcribe.rs` заменить два существующих теста (`отсутствие_поля_и_незнакомое_значение_дают_сервер`, `local_в_конфиге_ведёт_на_сервер_пока_движка_нет`) на:

```rust
    #[test]
    fn отсутствие_полей_и_незнакомые_значения_дают_шлюз() {
        assert_eq!(effective_mode(None, None), Mode::Gateway);
        assert_eq!(effective_mode(Some("что-то незнакомое"), None), Mode::Gateway);
        assert_eq!(effective_mode(Some("server"), None), Mode::Gateway);
        assert_eq!(effective_mode(None, Some("gateway")), Mode::Gateway);
        assert_eq!(effective_mode(None, Some("что-то незнакомое")), Mode::Gateway);
    }

    #[test]
    fn тип_сервера_whisper_cpp_разбирается_явно() {
        assert_eq!(effective_mode(None, Some("whisper_cpp")), Mode::WhisperCpp);
        assert_eq!(effective_mode(Some("server"), Some("whisper_cpp")), Mode::WhisperCpp);
    }

    /// Пока движка нет, «local» в конфиге — это сервер того типа, что выбран:
    /// человек, у которого значение осталось от прежней выпадашки, должен
    /// получить расшифровку, а не «движок не подключён». Тест обязан
    /// развернуться, когда флаг поднимут.
    #[test]
    fn local_в_конфиге_ведёт_на_выбранный_сервер_пока_движка_нет() {
        assert!(!LOCAL_MODE_AVAILABLE, "движок подключили — верни режим и переверни тест");
        assert_eq!(effective_mode(Some("local"), None), Mode::Gateway);
        assert_eq!(effective_mode(Some("local"), Some("whisper_cpp")), Mode::WhisperCpp);
    }
```

- [ ] **Step 2: Убедиться, что не компилируется**

Run: команда `cargo xwin check` из Global Constraints.
Expected: `error[E0061]: this function takes 1 argument but 2 arguments were supplied` на `effective_mode` и `error[E0599]: no variant named Gateway`.

- [ ] **Step 3: Реализация в `transcribe.rs`**

Заменить блок `Mode` + `effective_mode` (`transcribe.rs:14-40`) на:

```rust
/// Где считается расшифровка. Разбор из конфига — `effective_mode`; сам
/// конфиг про сеть, очередь и движок ничего не знает (см. докблоки полей
/// `transcribe_mode` и `transcribe_server` в `config.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Шлюз selfhost-ai-lab: async-задача, опрос, диаризация, ключ.
    Gateway,
    /// `whisper-server` из whisper.cpp: один синхронный POST, без ключа,
    /// без диаризации. Обычно на этом же компьютере.
    WhisperCpp,
    /// Встроенный движок — честная заглушка, спрятана (`LOCAL_MODE_AVAILABLE`).
    Local,
}

/// Движок локальной расшифровки ещё не вписан — `local::transcribe_local`
/// честная заглушка (`docs/2026-09-02-local-transcription-spec.md`). Пока так,
/// режим спрятан целиком: из настроек его не выбрать (`ui/main.js`,
/// `ЛОКАЛЬНЫЙ_РЕЖИМ_ДОСТУПЕН`), а `"local"`, оставшийся в конфиге с тех пор,
/// как выпадашка его предлагала, ведёт на выбранный сервер, а не в заглушку с
/// «движок не подключён». Когда движок появится — поставить `true` здесь и в
/// `main.js`, больше ничего возвращать не надо: каркас вокруг режима на месте.
pub const LOCAL_MODE_AVAILABLE: bool = false;

/// Из двух полей конфига — один режим.
///
/// `transcribe_mode == "local"` побеждает, но только при поднятом
/// `LOCAL_MODE_AVAILABLE`. Иначе решает тип сервера: `"whisper_cpp"` —
/// whisper-server, всё остальное, включая `None` (поля ещё не было в конфиге,
/// когда он появился на диске) и незнакомые строки — шлюз, как было всегда.
pub fn effective_mode(cfg_mode: Option<&str>, cfg_server: Option<&str>) -> Mode {
    match (cfg_mode, cfg_server) {
        (Some("local"), _) if LOCAL_MODE_AVAILABLE => Mode::Local,
        (_, Some("whisper_cpp")) => Mode::WhisperCpp,
        _ => Mode::Gateway,
    }
}
```

В `transcribe_track` (`transcribe.rs:454-460`) переименовать ветку `Mode::Server` в `Mode::Gateway` и **временно** добавить ветку-заглушку, чтобы `match` был полным (её заменит Task 5):

```rust
        Mode::WhisperCpp => unreachable!("ветка появится в Task 5"),
```

Заменить `Mode::Server` на `Mode::Gateway` во всех остальных местах: `transcribe.rs` (докблок `transcribe_track`, тесты), `main.rs:1578`, `main.rs:2689-2707` (тесты `ключи_шлюза`). Проверка: `grep -rn "Mode::Server" src-tauri/src` пуст.

- [ ] **Step 4: Падающие тесты на поле конфига**

В `src-tauri/src/config.rs`, в `mod tests`, после `режим_расшифровки_переживает_сериализацию`:

```rust
    /// Конфиг, записанный до появления типа сервера, обязан читаться как
    /// шлюз — то же требование, что и для режима расшифровки выше.
    #[test]
    fn старый_конфиг_без_типа_сервера_даёт_none() {
        let c = Config::from_str(r#"{"mic_device_id":"{id}"}"#);
        assert_eq!(c.transcribe_server, None);
    }

    #[test]
    fn тип_сервера_переживает_сериализацию() {
        let c = Config { transcribe_server: Some("whisper_cpp".to_string()), ..Config::default() };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(Config::from_str(&json), c);
    }
```

- [ ] **Step 5: Поле в `Config`**

После `update_skipped_version` (`config.rs`):

```rust
    /// `"gateway" | "whisper_cpp"`. Отсутствие поля (старый конфиг) и `None` —
    /// то же самое, что `"gateway"`: шлюз selfhost-ai-lab, как было всегда.
    /// Разбор значения в действующий режим живёт в `transcribe::effective_mode`,
    /// а не здесь — этот файл ничего не знает про протоколы серверов.
    pub transcribe_server: Option<String>,
```

Во всех четырёх литералах `Config { ... }` в тестах `config.rs` добавить строку `transcribe_server: None,` после `update_skipped_version: None,`:

```bash
sed -i 's/^            update_skipped_version: None,$/            update_skipped_version: None,\n            transcribe_server: None,/' src-tauri/src/config.rs
grep -c "transcribe_server: None" src-tauri/src/config.rs   # ожидается 4
```

- [ ] **Step 6: Вызов и команда в `main.rs`**

`main.rs:1394`:

```rust
    let mode = transcribe::effective_mode(cfg.transcribe_mode.as_deref(), cfg.transcribe_server.as_deref());
```

После `set_transcribe_mode` (`main.rs:1147-1152`):

```rust
/// Тип сервера расшифровки: `"gateway"` — шлюз selfhost-ai-lab, `"whisper_cpp"`
/// — whisper-server. Только сохраняет выбор: развилка читает конфиг при
/// каждой расшифровке (`run_transcription`), второй источник правды не нужен.
#[tauri::command]
fn set_transcribe_server(kind: Option<String>, app: AppHandle) -> Result<(), String> {
    let mut cfg = Config::load(&app);
    cfg.transcribe_server = kind;
    cfg.save(&app)
}
```

В `generate_handler![...]` после `set_transcribe_mode,` добавить `set_transcribe_server,`.

- [ ] **Step 7: Компиляция и тесты**

Run: `cargo xwin check` (Linux) — Expected: `Finished`, без `error`. Единственное новое предупреждение допустимо: `unreachable!` не даёт предупреждений.
Run (Mac/Windows или CI): `cargo test --workspace` — Expected: все зелёные, в том числе три теста Step 1 и два теста Step 4.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/transcribe.rs src-tauri/src/config.rs src-tauri/src/main.rs
git commit -m "feat: режим WhisperCpp в развилке и тип сервера в конфиге; Mode::Server переименован в Gateway"
```

---

### Task 2: Разбор ответа whisper-server

**Files:**
- Create: `src-tauri/src/whisper_cpp.rs`
- Modify: `src-tauri/src/transcribe.rs:73-102` (новый вариант ошибки)
- Modify: `src-tauri/src/main.rs:8-19` (`mod whisper_cpp;`)

**Interfaces:**
- Consumes: `transcribe::{Label, Segment, TrackResult, TranscribeError}` (существуют: `Segment { start: f64, label: Label, text: String, speaker: Option<String> }`, `TrackResult { segments: Vec<Segment>, text: String }`).
- Produces: `whisper_cpp::parse_response(body: &str, label: Label) -> Result<TrackResult, TranscribeError>`; `TranscribeError::NoTimestamps`.

- [ ] **Step 1: Модуль с падающими тестами**

Создать `src-tauri/src/whisper_cpp.rs`:

```rust
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
```

В `src-tauri/src/main.rs` после `mod update;` добавить `mod whisper_cpp;`.

- [ ] **Step 2: Убедиться, что не компилируется**

Run: `cargo xwin check`.
Expected: `error[E0599]: no variant or associated item named NoTimestamps found for enum TranscribeError`.

- [ ] **Step 3: Вариант ошибки**

В `transcribe.rs`, в `pub enum TranscribeError`, перед вариантом `Local`:

```rust
    /// whisper-server (`whisper_cpp.rs`) отдал сегменты без `start`: его
    /// запустили с `-nt`. Без таймкодов `merge_markdown` не сведёт дорожки,
    /// поэтому это отказ, а не расшифровка с нулями.
    #[error("whisper-server отдал текст без таймкодов — запустите его без -nt/--no-timestamps")]
    NoTimestamps,
```

- [ ] **Step 4: Компиляция и тесты**

Run: `cargo xwin check` — Expected: `Finished`; предупреждение `function parse_response is never used` допустимо до Task 5.
Run (Mac/Windows или CI): `cargo test -p meeting-recorder-gui whisper_cpp` — Expected: 5 passed.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/whisper_cpp.rs src-tauri/src/transcribe.rs src-tauri/src/main.rs
git commit -m "feat: разбор verbose_json от whisper-server в TrackResult"
```

---

### Task 3: Сетевой вызов whisper-server

**Files:**
- Modify: `src-tauri/src/whisper_cpp.rs`
- Modify: `src-tauri/Cargo.toml:40` (tokio features)

**Interfaces:**
- Consumes: `whisper_cpp::parse_response` (Task 2), `transcribe::POLL_DEADLINE` (существует, 3 часа).
- Produces: `whisper_cpp::transcribe(client: &reqwest::Client, url: &str, wav_path: &Path, label: Label) -> Result<TrackResult, TranscribeError>`.

- [ ] **Step 1: Тестовый HTTP-стаб и падающие тесты**

В `src-tauri/Cargo.toml` строку tokio заменить на:

```toml
# `net` — только ради тестов `whisper_cpp.rs`: стаб-сервер на `TcpListener`
# отвечает заранее заготовленным телом, чтобы проверить сетевой путь без
# настоящего whisper-server.
tokio = { version = "1", features = ["time", "fs", "macros", "io-util", "net"] }
```

В `mod tests` файла `whisper_cpp.rs` добавить:

```rust
    use std::path::Path;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Стаб whisper-server: принимает ОДИН запрос, дочитывает его тело по
    /// `Content-Length` (иначе reqwest увидит обрыв посреди отправки файла)
    /// и отвечает заданным статусом и телом. Возвращает адрес.
    async fn стаб(status: &'static str, body: &'static str) -> String {
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

    fn wav_файл() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mr-wcpp-{}", std::process::id()));
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
```

- [ ] **Step 2: Убедиться, что не компилируется**

Run: `cargo xwin check`.
Expected: `error[E0425]: cannot find function transcribe in this scope`.

- [ ] **Step 3: Реализация**

В `whisper_cpp.rs` после `parse_response`:

```rust
use crate::transcribe::POLL_DEADLINE;
use std::path::Path;

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
```

- [ ] **Step 4: Компиляция и тесты**

Run: `cargo xwin check` — Expected: `Finished`.
Run (Mac/Windows или CI): `cargo test -p meeting-recorder-gui whisper_cpp` — Expected: 8 passed.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/whisper_cpp.rs src-tauri/Cargo.toml Cargo.lock
git commit -m "feat: сетевой вызов whisper-server с общим пределом ожидания и честными ошибками"
```

---

### Task 4: Ветка в развилке, параметры сервера, стадии

**Files:**
- Modify: `src-tauri/src/transcribe.rs:454-460` (`transcribe_track`), тесты после `развилка_в_local_идёт_в_заглушку_а_не_в_сеть`
- Modify: `src-tauri/src/main.rs:1422` (вызов), `:1519-1542` (`ключи_шлюза`), `:1571-1584` (`дорожка_целиком`), тесты `:2670-2710`

**Interfaces:**
- Consumes: `whisper_cpp::transcribe` (Task 3), `Mode::WhisperCpp` (Task 1).
- Produces: `параметры_сервера(mode, url, key) -> Result<(String, String), &'static str>` вместо `ключи_шлюза`.

- [ ] **Step 1: Падающий тест на развилку**

В `transcribe.rs`, в `mod tests`, после `развилка_в_local_идёт_в_заглушку_а_не_в_сеть`:

```rust
    /// В режиме `WhisperCpp` развилка идёт в `whisper_cpp::transcribe`, а не в
    /// `submit` шлюза: `on_job` не зовётся (задачи с id у whisper-server
    /// нет), а недоступный адрес даёт сетевую ошибку, не `SubmitRejected`.
    #[tokio::test]
    async fn развилка_в_whisper_cpp_не_заводит_задачу_на_шлюзе() {
        let client = reqwest::Client::new();
        let err = transcribe_track(
            Mode::WhisperCpp,
            &client,
            "http://127.0.0.1:1",
            "",
            Path::new("/dev/null"),
            Label::Owner,
            |_| panic!("у whisper-server нет id задачи — on_job звать нечем"),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, TranscribeError::Network(_) | TranscribeError::Io(_)),
            "ожидали сеть или файл, получили: {err}"
        );
    }
```

- [ ] **Step 2: Убедиться, что тест падает**

На Linux: `cargo xwin check` компилируется (заглушка `unreachable!` из Task 1 на месте) — падение здесь только при запуске. На Mac/Windows: `cargo test -p meeting-recorder-gui развилка_в_whisper_cpp` — Expected: panic `ветка появится в Task 5`.

- [ ] **Step 3: Ветка**

В `transcribe_track` заменить заглушку:

```rust
        Mode::WhisperCpp => whisper_cpp::transcribe(client, url, path, label).await,
```

и в начале `transcribe.rs` рядом с `use crate::local;` добавить `use crate::whisper_cpp;`. Докблок `transcribe_track` дополнить абзацем:

```rust
/// `WhisperCpp` — один синхронный вызов `whisper_cpp::transcribe`; `key` и
/// `on_job` в этой ветке не нужны: у whisper-server нет ни ключа, ни id
/// задачи, класть в очередь отмены нечего.
```

- [ ] **Step 4: Падающие тесты на параметры сервера**

В `main.rs`, в `mod tests`, заменить три теста про `ключи_шлюза` (`:2676-2710`) на:

```rust
    /// Локальный режим считает на этой же машине и на шлюз не ходит, поэтому
    /// требовать его адрес и ключ нельзя.
    #[test]
    fn локальный_режим_не_требует_параметров_сервера() {
        assert_eq!(
            параметры_сервера(transcribe::Mode::Local, None, None),
            Ok((String::new(), String::new()))
        );
    }

    /// Шлюз без настроек — по-прежнему отказ, а не пустые строки: иначе
    /// запрос уйдёт в никуда и человек увидит сетевую ошибку вместо понятного
    /// «настройте шлюз».
    #[test]
    fn шлюз_без_настроек_отказывает() {
        assert!(параметры_сервера(transcribe::Mode::Gateway, None, None).is_err());
        assert!(параметры_сервера(
            transcribe::Mode::Gateway,
            Some("   ".to_string()),
            Some("k".to_string())
        )
        .is_err());
        assert!(параметры_сервера(
            transcribe::Mode::Gateway,
            Some("http://localhost:8080".to_string()),
            None
        )
        .is_err());
    }

    /// whisper-server ключа не имеет: нужен только адрес, введённый ключ
    /// игнорируется, а не уходит в запрос.
    #[test]
    fn whisper_cpp_требует_только_адрес() {
        assert_eq!(
            параметры_сервера(
                transcribe::Mode::WhisperCpp,
                Some("http://127.0.0.1:8178/".to_string()),
                None
            ),
            Ok(("http://127.0.0.1:8178".to_string(), String::new()))
        );
        assert_eq!(
            параметры_сервера(
                transcribe::Mode::WhisperCpp,
                Some("http://127.0.0.1:8178".to_string()),
                Some("лишний".to_string())
            ),
            Ok(("http://127.0.0.1:8178".to_string(), String::new()))
        );
        let err = параметры_сервера(transcribe::Mode::WhisperCpp, None, None).unwrap_err();
        assert!(err.contains("whisper"), "{err}");
    }

    /// Хвостовой слеш и пробелы срезаются здесь, а не у места вызова: путь
    /// (`/v1/...` или `/inference`) дописывает клиент, и `.../` дал бы двойной слеш.
    #[test]
    fn шлюз_чистит_пробелы_и_хвостовой_слеш() {
        assert_eq!(
            параметры_сервера(
                transcribe::Mode::Gateway,
                Some("  http://localhost:8080/  ".to_string()),
                Some("  секрет  ".to_string())
            ),
            Ok(("http://localhost:8080".to_string(), "секрет".to_string()))
        );
    }
```

- [ ] **Step 5: Убедиться, что не компилируется**

Run: `cargo xwin check`. Expected: `error[E0425]: cannot find function параметры_сервера`.

- [ ] **Step 6: `параметры_сервера`**

Заменить `ключи_шлюза` (`main.rs:1519-1542`, вместе с докблоком) на:

```rust
/// Адрес и ключ для выбранного режима, уже вычищенные.
///
/// `Gateway` требует и адрес, и ключ. `WhisperCpp` — только адрес: ключа у
/// whisper-server нет, введённый по привычке игнорируется, а не уезжает в
/// запрос. `Local` не ходит никуда — пустые строки, которые дальше по коду
/// никто не читает. Хвостовой `/` срезается здесь, потому что путь
/// (`/v1/...` у шлюза, `/inference` у whisper-server) дописывает клиент.
fn параметры_сервера(
    mode: transcribe::Mode,
    url: Option<String>,
    key: Option<String>,
) -> Result<(String, String), &'static str> {
    let чистый_адрес = url
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty());
    let чистый_ключ = key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
    match mode {
        transcribe::Mode::Local => Ok((String::new(), String::new())),
        transcribe::Mode::WhisperCpp => match чистый_адрес {
            Some(u) => Ok((u, String::new())),
            None => Err("настройте адрес whisper-сервера"),
        },
        transcribe::Mode::Gateway => match (чистый_адрес, чистый_ключ) {
            (Some(u), Some(k)) => Ok((u, k)),
            _ => Err("настройте URL и ключ шлюза"),
        },
    }
}
```

`main.rs:1422`: `ключи_шлюза(` → `параметры_сервера(`. Проверка: `grep -n "ключи_шлюза" src-tauri/src/main.rs` пуст.

- [ ] **Step 7: Стадия в `дорожка_целиком`**

`main.rs:1576-1579`:

```rust
    match mode {
        // «Отправляю…» здесь было бы враньём: локальный движок никуда не
        // шлёт, а whisper-server на localhost принимает файл за секунду и
        // дальше считает — суть происходящего «Расшифровываю…».
        transcribe::Mode::Local | transcribe::Mode::WhisperCpp => {
            emit_transcribe_progress(app, folder, base, "polling")
        }
        transcribe::Mode::Gateway => emit_transcribe_progress(app, folder, base, "uploading"),
    }
```

- [ ] **Step 8: Компиляция и тесты**

Run: `cargo xwin check` — Expected: `Finished`, предупреждение про неиспользуемый `parse_response` ушло.
Run (Mac/Windows или CI): `cargo test --workspace` — Expected: все зелёные.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/transcribe.rs src-tauri/src/main.rs
git commit -m "feat: расшифровка через whisper-server — ветка развилки, адрес без ключа, стадия «Расшифровываю…»"
```

---

### Task 5: Настройки — тип сервера

**Files:**
- Modify: `ui/index.html:1844-1878` (раздел «Расшифровка встреч»)
- Modify: `ui/i18n/strings.json` (новый раздел `server` перед `"local"`)
- Modify: `ui/main.js:1679-1690` (чтение конфига), `:1703-1720` (флаг и `применить_режим_расшифровки`), `:1815-1824` (обработчик `change`)

**Interfaces:**
- Consumes: команда `set_transcribe_server` (Task 1), `открыть_ссылку(url)` (существует, `main.js:2037`), `i18n.t`, `i18n.lang`.
- Produces: `#transcribe-server`, `#server-hint`, `#server-howto`, `применить_тип_сервера(тип)`.

- [ ] **Step 1: Строки**

В `ui/i18n/strings.json` перед строкой `  "local": {` вставить:

```json
  "server": {
    "gateway": {
      "ru": "Шлюз selfhost-ai-lab",
      "en": "selfhost-ai-lab gateway"
    },
    "whisperCpp": {
      "ru": "whisper.cpp server",
      "en": "whisper.cpp server"
    },
    "gatewayHint": {
      "ru": "Дорожки уходят на указанный адрес по вашему ключу. Различает собеседников между собой.",
      "en": "The tracks go to the address you set, with your key. Tells the other speakers apart."
    },
    "whisperCppHint": {
      "ru": "Сервер на этом компьютере или в локальной сети, без ключа. Собеседников между собой не различает. Отмена в приложении не останавливает расчёт на сервере.",
      "en": "A server on this machine or on the local network, no key. Won't tell the other speakers apart. Cancelling in the app does not stop the server's computation."
    },
    "whisperCppUrl": {
      "ru": "http://127.0.0.1:8178",
      "en": "http://127.0.0.1:8178"
    },
    "howTo": {
      "ru": "Как поднять whisper-сервер",
      "en": "How to run a whisper server"
    }
  },
```

Проверка: `python3 -c "import json;json.load(open('ui/i18n/strings.json'))"`.

- [ ] **Step 2: Разметка**

В `ui/index.html` у строки режима (`<div class="row mode">` перед `<select id="transcribe-mode">`) добавить `id="transcribe-mode-row"`. Сразу после закрывающего `</div>` этой строки (после `<span class="ic" id="local-about" ...>…</span>`) и перед комментарием про `#stt-url-row` вставить:

```html
            <!-- Тип сервера: шлюз selfhost-ai-lab или whisper-server из
                 whisper.cpp (`docs/2026-09-09-whisper-server-design.md`).
                 Пока встроенный движок спрятан, это единственная видимая
                 выпадашка раздела — строка режима выше скрыта из main.js
                 (`ЛОКАЛЬНЫЙ_РЕЖИМ_ДОСТУПЕН`). Подсказка под ней меняется по
                 типу, а не живёт в тултипе: у whisper-сервера есть свойство,
                 которое надо видеть до первой расшифровки, — отмена не
                 останавливает расчёт. -->
            <div class="row mode" id="transcribe-server-row">
              <span class="field">
                <select id="transcribe-server">
                  <option value="gateway" data-i18n="server.gateway">selfhost-ai-lab gateway</option>
                  <option value="whisper_cpp" data-i18n="server.whisperCpp">whisper.cpp server</option>
                </select>
                <svg class="chev" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6" /></svg>
              </span>
            </div>
            <div class="hint" id="server-hint"></div>
            <!-- Кнопка, а не <a href>: обычная ссылка в вебвью Tauri открылась
                 бы внутри окна (см. .contacts). Ведёт на раздел README, ссылка
                 по языку — в main.js. -->
            <div class="row" id="server-howto-row" hidden>
              <button class="sec" id="server-howto" data-i18n="server.howTo">How to run a whisper server</button>
            </div>
```

- [ ] **Step 3: JS**

В `ui/main.js`, в блоке флага (`if (!ЛОКАЛЬНЫЙ_РЕЖИМ_ДОСТУПЕН) { ... }`) добавить строку:

```js
  $("transcribe-mode-row").hidden = true;
```

После функции `применить_режим_расшифровки` добавить:

```js
// Ссылка на инструкцию по whisper-серверу — раздел README на языке
// интерфейса. Якорь GitHub строит из заголовка: строчные буквы, пробелы в
// дефисы; кириллица сохраняется.
const ИНСТРУКЦИЯ_WHISPER = {
  ru: "https://github.com/mmaximov97/meeting-recorder/blob/master/README.ru.md#свой-whisper-сервер-на-этом-компьютере",
  en: "https://github.com/mmaximov97/meeting-recorder/blob/master/README.md#your-own-whisper-server-on-this-machine",
};

// Тип сервера меняет три вещи: ключ не нужен whisper-серверу (строка ключа
// прячется, а не гаснет — тем же приёмом, что в локальном режиме), подсказка
// под выпадашкой говорит, чем типы отличаются, и появляется кнопка на
// инструкцию. Плейсхолдер адреса подсказывает типичный localhost-адрес.
function применить_тип_сервера(тип) {
  const whisper = тип === "whisper_cpp";
  const локально = $("transcribe-mode").value === "local";
  $("stt-key-row").hidden = локально || whisper;
  $("stt-url").placeholder = whisper ? i18n.t("server.whisperCppUrl") : i18n.t("settings.serverUrl");
  $("server-hint").textContent = i18n.t(whisper ? "server.whisperCppHint" : "server.gatewayHint");
  $("server-howto-row").hidden = !whisper;
}

$("transcribe-server").addEventListener("change", async (e) => {
  const значение = e.target.value;
  try {
    await invoke("set_transcribe_server", { kind: значение });
    применить_тип_сервера(значение);
    показать_ошибку("");
  } catch (err) {
    показать_ошибку(i18n.t("error.generic"), err);
  }
});

$("server-howto").addEventListener("click", () => {
  открыть_ссылку(ИНСТРУКЦИЯ_WHISPER[i18n.lang === "ru" ? "ru" : "en"]);
});
```

В чтении конфига (`main.js`, сразу после `применить_режим_расшифровки(режим);`):

```js
    const тип_сервера = конфиг.transcribe_server === "whisper_cpp" ? "whisper_cpp" : "gateway";
    $("transcribe-server").value = тип_сервера;
    применить_тип_сервера(тип_сервера);
```

Проверить, что `i18n.lang` — строка `"ru"`/`"en"`: `grep -n "lang" ui/i18n.js | head`. Если `lang` — функция, звать `i18n.lang()`.

- [ ] **Step 4: Проверка**

Run: `node --check ui/main.js` — Expected: тишина. `python3 -c "import json;json.load(open('ui/i18n/strings.json'))"` — тишина.
Run: `cargo xwin check` — `i18n.rs` вшивает `strings.json` через `include_str!` и разбирает в тесте; ожидается `Finished`.
Глазами (Mac/Windows, `npx tauri dev`): в настройках одна выпадашка «Шлюз selfhost-ai-lab / whisper.cpp server»; при выборе whisper.cpp строка ключа исчезает, подсказка меняется, появляется кнопка инструкции; после перезапуска выбор сохранён.

- [ ] **Step 5: Commit**

```bash
git add ui/index.html ui/main.js ui/i18n/strings.json
git commit -m "feat: выпадашка типа сервера в настройках, подсказка и ссылка на инструкцию"
```

---

### Task 6: README и старая спека

**Files:**
- Modify: `README.ru.md:75-84`, `README.md:75-84`
- Modify: `docs/2026-09-02-local-transcription-spec.md:7-10`

- [ ] **Step 1: README.ru.md**

Абзац «Режим, в котором расшифровка считается прямо на вашей машине … сейчас в работе.» заменить на:

````markdown
### Свой whisper-сервер на этом компьютере

Второй вариант — `whisper-server` из [whisper.cpp](https://github.com/ggml-org/whisper.cpp), запущенный на вашей же машине. Звук не покидает компьютер, ключ не нужен. Чего он не умеет: различать собеседников между собой — в расшифровке будут «Владелец» и «Собеседники», без номеров.

Модели, две, положить в одну папку:

- речь: [`ggml-large-v3-turbo-q5_0.bin`](https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin), 574 МБ — русский почти как у large-v3, в несколько раз быстрее;
- детектор речи: [`ggml-silero-v5.1.2.bin`](https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v5.1.2.bin), 0,9 МБ — без него whisper придумывает текст в паузах, а дорожка микрофона на встрече — это в основном паузы.

**Windows.** Скачать `whisper-bin-x64.zip` из [релизов whisper.cpp](https://github.com/ggml-org/whisper.cpp/releases/latest), распаковать, положить модели рядом с `whisper-server.exe` и запустить:

```
whisper-server.exe -m ggml-large-v3-turbo-q5_0.bin -l auto --vad -vm ggml-silero-v5.1.2.bin --port 8178 -t 8
```

`-t` — число потоков, ставьте по числу ядер. Без видеокарты часовая встреча считается около часа.

**macOS.** Пакет `brew install whisper-cpp` сервера не содержит, нужна сборка из исходников — три минуты:

```sh
brew install cmake git
git clone https://github.com/ggml-org/whisper.cpp
cd whisper.cpp
cmake -B build && cmake --build build -j
./build/bin/whisper-server -m ggml-large-v3-turbo-q5_0.bin -l auto --vad -vm ggml-silero-v5.1.2.bin --port 8178
```

Metal подхватывается сам: на Apple Silicon часовая встреча считается за несколько минут.

**В приложении.** Настройки → «Расшифровка встреч» → тип сервера «whisper.cpp server», адрес `http://127.0.0.1:8178`. Ключ не нужен.

Одна оговорка: «Отменить расшифровку» в приложении отпускает запись сразу, но сервер досчитывает уже принятую дорожку до конца — у него нет команды отмены. Следующая расшифровка встанет за ней.
````

- [ ] **Step 2: README.md**

Абзац «A local mode, where the audio is transcribed on your own machine and nothing leaves it, is in progress.» заменить на:

````markdown
### Your own whisper server on this machine

The second option is `whisper-server` from [whisper.cpp](https://github.com/ggml-org/whisper.cpp), running on the same machine. Audio never leaves the computer and no key is needed. What it can't do: tell the other speakers apart — the transcript will say "Owner" and "Others", without numbers.

Two models, put them in one folder:

- speech: [`ggml-large-v3-turbo-q5_0.bin`](https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin), 574 MB — close to large-v3 in quality, several times faster;
- voice activity detector: [`ggml-silero-v5.1.2.bin`](https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v5.1.2.bin), 0.9 MB — without it whisper makes up text in the pauses, and a microphone track of a meeting is mostly pauses.

**Windows.** Download `whisper-bin-x64.zip` from the [whisper.cpp releases](https://github.com/ggml-org/whisper.cpp/releases/latest), unpack it, put the models next to `whisper-server.exe` and run:

```
whisper-server.exe -m ggml-large-v3-turbo-q5_0.bin -l auto --vad -vm ggml-silero-v5.1.2.bin --port 8178 -t 8
```

`-t` is the thread count; match it to your cores. Without a GPU an hour-long meeting takes about an hour.

**macOS.** The `brew install whisper-cpp` package does not include the server, so build from source — about three minutes:

```sh
brew install cmake git
git clone https://github.com/ggml-org/whisper.cpp
cd whisper.cpp
cmake -B build && cmake --build build -j
./build/bin/whisper-server -m ggml-large-v3-turbo-q5_0.bin -l auto --vad -vm ggml-silero-v5.1.2.bin --port 8178
```

Metal is picked up automatically: on Apple Silicon an hour-long meeting takes a few minutes.

**In the app.** Settings → Transcription → server type "whisper.cpp server", address `http://127.0.0.1:8178`. No key.

One caveat: "Cancel transcription" in the app releases the recording immediately, but the server finishes the track it has already accepted — it has no cancel command. The next transcription queues behind it.
````

- [ ] **Step 3: Спека 02.09**

В `docs/2026-09-02-local-transcription-spec.md` после абзаца раздела «Режим спрятан до появления движка (09.09.2026)» добавить:

```markdown
Решение владельца от 09.09: движок в приложение не вшивать. Вместо этого приложение умеет говорить с `whisper-server` из whisper.cpp как со вторым типом сервера — `docs/2026-09-09-whisper-server-design.md`. Встроенный движок отложен без срока; всё ниже остаётся точкой встраивания на случай, если решение изменится.
```

- [ ] **Step 4: Проверка якорей**

Run: `grep -n "^### Свой whisper-сервер на этом компьютере" README.ru.md && grep -n "^### Your own whisper server on this machine" README.md` — обе строки найдены; они и есть якоря из `ИНСТРУКЦИЯ_WHISPER` (Task 5).

- [ ] **Step 5: Commit**

```bash
git add README.ru.md README.md docs/2026-09-02-local-transcription-spec.md
git commit -m "docs: как поднять whisper-server на Windows и macOS; встроенный движок отложен"
```

---

### Task 7: Живая проверка на Linux и фикстура ответа

**Files:**
- Create: `src-tauri/fixtures/whisper_cpp_inference.json`
- Modify: `src-tauri/src/whisper_cpp.rs` (тест на фикстуру)

Вне репозитория: сборка whisper.cpp в `/data/cypher-ai-lab/whisper.cpp`, модели в `/data/cypher-ai-lab/models/`.

- [ ] **Step 1: Собрать whisper-server**

```bash
cat /proc/loadavg
cd /data/cypher-ai-lab && git clone --depth 1 https://github.com/ggml-org/whisper.cpp
cd whisper.cpp && flock -w 3600 /tmp/cypher-heavy.lock bash -c 'cmake -B build -DWHISPER_BUILD_SERVER=ON -DWHISPER_BUILD_EXAMPLES=ON && cmake --build build -j 4 --target whisper-server'
ls -la build/bin/whisper-server
```

Expected: бинарь на месте. Если `cmake` ругается на компилятор — `g++` и `cmake` есть в `/usr/bin`.

- [ ] **Step 2: Модели**

```bash
mkdir -p /data/cypher-ai-lab/models && cd /data/cypher-ai-lab/models
curl -L -o ggml-large-v3-turbo-q5_0.bin https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin
curl -L -o ggml-silero-v5.1.2.bin https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v5.1.2.bin
ls -l   # 574041195 и 885098 байт
```

- [ ] **Step 3: Запустить сервер и подготовить русскую речь**

```bash
cd /data/cypher-ai-lab/whisper.cpp && flock -w 3600 /tmp/cypher-heavy.lock ./build/bin/whisper-server \
  -m /data/cypher-ai-lab/models/ggml-large-v3-turbo-q5_0.bin -l auto \
  --vad -vm /data/cypher-ai-lab/models/ggml-silero-v5.1.2.bin --port 8178 -t 4 &
```

Русская речь: голосовое из Telegram, уже расшифрованное ai-lab 09.09 («Это локальная модель, её как бы нет…»):

```bash
S=/tmp/claude-1000/-home-cypher/cc5a6fa0-630a-4766-bd2a-643dfeee472d/scratchpad
ffmpeg -y -i /home/cypher/downloads/telegram_-1004485570884_1709_1788882340.oga -ac 1 -ar 16000 -sample_fmt s16 $S/ru.wav
```

Если `ffmpeg` нет — `python3 -c "import soundfile"` и конвертация через `soundfile`/`resampy`; если и их нет — попросить у владельца любой `*.mic.wav` из `Recordings`.

- [ ] **Step 4: Запрос ровно той формы, что шлёт клиент**

```bash
curl -s http://127.0.0.1:8178/inference \
  -F "file=@$S/ru.wav" -F language=auto -F response_format=verbose_json -F temperature=0.0 \
  -o $S/inference.json
python3 -c "import json;d=json.load(open('$S/inference.json'));print(d['text'][:200]);print(len(d['segments']),'segments');print(d['segments'][0])"
```

Expected: текст про «локальную модель», сегменты с `start`/`end`. Сверить с транскриптом ai-lab по смыслу.

- [ ] **Step 5: Фикстура и тест**

```bash
mkdir -p src-tauri/fixtures
python3 - <<'EOF'
import json,os
S=os.environ.get('S','/tmp/claude-1000/-home-cypher/cc5a6fa0-630a-4766-bd2a-643dfeee472d/scratchpad')
d=json.load(open(f'{S}/inference.json'))
for s in d['segments']: s.pop('tokens',None)   # токены весят много и не читаются
json.dump(d,open('src-tauri/fixtures/whisper_cpp_inference.json','w'),ensure_ascii=False,indent=1)
EOF
```

В `whisper_cpp.rs`, `mod tests`:

```rust
    /// Живой ответ whisper-server b4938+ на русскую речь (09.09.2026), без
    /// токенов. Если формат `verbose_json` у сервера поменяется, первым
    /// упадёт этот тест, а не расшифровка у человека.
    const ЖИВОЙ_ОТВЕТ: &str = include_str!("../fixtures/whisper_cpp_inference.json");

    #[test]
    fn живой_ответ_сервера_разбирается() {
        let r = parse_response(ЖИВОЙ_ОТВЕТ, Label::Owner).unwrap();
        assert!(!r.segments.is_empty());
        assert!(r.text.to_lowercase().contains("модел"), "{}", r.text);
        assert!(r.segments.windows(2).all(|w| w[0].start <= w[1].start), "таймкоды по возрастанию");
    }
```

- [ ] **Step 6: Проверка, остановка сервера, commit**

Run: `cargo xwin check` — `Finished`. CI после пуша — тест зелёный.

```bash
kill %1 2>/dev/null || pkill -f whisper-server
git add src-tauri/fixtures/whisper_cpp_inference.json src-tauri/src/whisper_cpp.rs
git commit -m "test: живой ответ whisper-server как фикстура разбора"
```

---

### Task 8: Пуш, CI, PR

- [ ] **Step 1: Пуш и CI**

```bash
git push -u origin feat/whisper-server
gh run list --branch feat/whisper-server --limit 1
gh run watch <id> --exit-status
```

Expected: `test (windows-latest)` и `test (macos-latest)` зелёные.

- [ ] **Step 2: PR**

```bash
gh pr create --base master --head feat/whisper-server \
  --title "whisper-server как второй тип сервера расшифровки" \
  --body-file - <<'EOF'
Спека: docs/2026-09-09-whisper-server-design.md, план: docs/2026-09-09-whisper-server-plan.md.

- `Mode::Server` → `Mode::Gateway`, новый `Mode::WhisperCpp`, поле `transcribe_server` в конфиге.
- `src-tauri/src/whisper_cpp.rs`: один синхронный `POST /inference`, разбор `verbose_json`, общий предел 3 часа, ошибки с текстом сервера.
- Настройки: выпадашка «Тип сервера», ключ прячется для whisper.cpp, ссылка на инструкцию.
- README: установка whisper-server на Windows (zip) и macOS (из исходников), модели, команда запуска, оговорка про отмену.

Проверено: CI на macOS и Windows; живой ответ whisper-server на русскую речь лежит фикстурой в тесте.
Не проверено: расшифровка настоящей встречи через whisper-server на Mac и Windows — просьба к @blin_birka и владельцу.
EOF
```

---

## Self-review

**Покрытие спеки.** §1 конфиг — Task 1; §2 развилка и переименование — Task 1, Task 4; §3 клиент — Task 2, Task 3; §4 ключ и адрес — Task 4; §5 стадии/отмена/очередь — Task 4 (стадия), докблок `transcribe` в Task 3 (отмена), очередь без изменений; §6 настройки — Task 5; §7 README — Task 6; §8 спека 02.09 — Task 6; «Проверка» — тесты в Task 1–4, живой прогон Task 7, CI Task 8.

**Согласованность имён.** `effective_mode(cfg_mode, cfg_server)` — Task 1 и вызов в Task 1 Step 6; `whisper_cpp::transcribe(client, url, path, label)` — Task 3 и ветка Task 4; `параметры_сервера` — Task 4 Step 4 и Step 6; `set_transcribe_server(kind)` — Task 1 и `invoke(..., { kind })` в Task 5; `TranscribeError::NoTimestamps` — Task 2 Step 1 и Step 3; ключи строк `server.*` — Task 5 Step 1 и Step 3; якоря README — Task 5 `ИНСТРУКЦИЯ_WHISPER` и заголовки Task 6.
