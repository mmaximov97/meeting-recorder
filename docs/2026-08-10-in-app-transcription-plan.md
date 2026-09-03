# Транскрипция в приложении — план реализации

> **Для агентов-исполнителей:** ОБЯЗАТЕЛЬНЫЙ САБ-СКИЛЛ: используйте `superpowers:subagent-driven-development` (рекомендуется) или `superpowers:executing-plans` для выполнения плана по задачам. Шаги отмечены чекбоксами (`- [ ]`).

**Цель:** приложение само транскрибирует запись (обе дорожки, слитые в один диалог по времени) через настраиваемый шлюз — URL и ключ пользователь задаёт в UI, а не в зашитом пути к конкретному серверу.

**Архитектура:** новый модуль `src-tauri/src/transcribe.rs` — HTTP-клиент шлюза (submit+poll на дорожку) и чистая функция слияния сегментов по времени. Оркестрация двух параллельных задач и запись результата — в `src-tauri/src/main.rs`, тем же событийным паттерном, что уже несёт `state`/`levels`/`error` из аудио-потока. Настройки — два новых поля в уже существующем `Config`.

**Стек:** Rust, `reqwest` (новая зависимость — HTTP-клиент с multipart), `tokio` (новая прямая зависимость `src-tauri` — до сих пор его тянул только Tauri транзитивно), `thiserror` (уже есть в корневом пакете, добавляется явно и в `src-tauri`).

## Глобальные ограничения

- Существующий тест-сьют (`cargo.exe test --workspace`) обязан оставаться зелёным после каждой задачи ниже.
- Модель и язык захардкожены: `Systran/faster-whisper-large-v3`, `ru`. В UI не выносятся.
- `diarize=true` запрашивается на ОБЕИХ дорожках — без этого шлюз не отдаёт сегменты с таймкодами, а хронологический мерж требует именно их (см. дизайн).
- Метки в объединённом транскрипте — только «Владелец» (дорожка mic) и «Собеседники» (дорожка system); `SPEAKER_00`/`01` от шлюза внутри дорожки system схлопываются в одну метку, не разносятся по людям.
- Одновременно — только одна транскрипция; вторая попытка получает немедленную ошибку, не встаёт в очередь.
- Результат — `{dir}/{base}.transcript/{base}.md` и `{base}.txt`, тот же путь и формат, которые уже понимает `rename.rs`.
- Ключ хранится как есть в `config.json` (не в Keychain/Credential Manager) — решение зафиксировано в дизайне, не пересматривается в рамках этого плана.
- Каждый Cargo-пакет декларирует свои зависимости явно — `src-tauri` не наследует зависимости корневого пакета `meeting-recorder` транзитивно (см. Task 2: `reqwest`/`tokio`/`thiserror` нужны именно в `src-tauri/Cargo.toml`, а не только в корневом).

---

## Task 1: Настройки — `Config`, команды, блок в UI

Расширяет уже существующий `Config` двумя полями и заводит их путь до UI и обратно — самодостаточно, без единого HTTP-запроса.

**Файлы:**
- Изменить: `src-tauri/src/config.rs`
- Изменить: `src-tauri/src/main.rs`
- Изменить: `ui/index.html`
- Изменить: `ui/main.js`

**Интерфейсы:**
- Производит: `Config.stt_gateway_url: Option<String>`, `Config.stt_api_key: Option<String>`; команда `set_transcribe_config(gateway_url: Option<String>, api_key: Option<String>)`; `get_config` (уже существует) теперь возвращает и эти поля без изменения сигнатуры.

- [ ] **Шаг 1: падающий тест на новые поля `Config`**

В `src-tauri/src/config.rs`, в `#[cfg(test)] mod tests`:

```rust
#[test]
fn настройки_транскрипции_переживают_сериализацию() {
    let c = Config {
        mic_device_id: None,
        mic_device_name: None,
        stt_gateway_url: Some("http://localhost:8080".to_string()),
        stt_api_key: Some("test_key_xxx".to_string()),
    };
    let json = serde_json::to_string(&c).unwrap();
    assert_eq!(Config::from_str(&json), c);
}

#[test]
fn старый_конфиг_без_настроек_транскрипции_даёт_none() {
    let c = Config::from_str(r#"{"mic_device_id":"{id}"}"#);
    assert_eq!(c.stt_gateway_url, None);
    assert_eq!(c.stt_api_key, None);
}
```

- [ ] **Шаг 2: запустить, убедиться, что не компилируется** (полей ещё нет в структуре)

```bash
cargo.exe test --workspace
```

Ожидается: ошибка компиляции — `Config` не имеет полей `stt_gateway_url`/`stt_api_key`.

- [ ] **Шаг 3: добавить поля в `Config`**

```rust
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Eq, Debug)]
#[serde(default)]
pub struct Config {
    pub mic_device_id: Option<String>,
    pub mic_device_name: Option<String>,
    /// Базовый URL шлюза, например `http://localhost:8080` — без хвоста
    /// `/v1/...`, его дописывает клиент транскрипции.
    pub stt_gateway_url: Option<String>,
    pub stt_api_key: Option<String>,
}
```

`#[serde(default)]` на структуре уже стоит — старый `config.json` без этих двух полей читается как есть, оба становятся `None`. `PartialEq, Eq` остаются выводимыми без изменений: оба новых поля — `Option<String>`, `Eq` тут ничего не ломает.

- [ ] **Шаг 4: прогнать тесты, убедиться, что новые проходят**

```bash
cargo.exe test --workspace
```

Ожидается: 177 тестов (175 прежних + 2 новых), 0 упавших.

- [ ] **Шаг 5: команда `set_transcribe_config` — и попутный фикс скрытого бага в `set_mic_device`**

`src-tauri/src/main.rs`. Сегодня `set_mic_device` строит `Config` С НУЛЯ (`Config { mic_device_id: id, mic_device_name: name }`) и сохраняет — это стирает любые другие поля, которые уже лежали в файле. Пока полей было только два и оба относились к микрофону, бага не было видно; как только появляются `stt_*`-поля, смена микрофона молча обнулит настройки транскрипции (и наоборот). Правильный паттерн — читать текущий конфиг, менять только свои поля, сохранять:

```rust
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
```

Зарегистрировать `set_transcribe_config` в `invoke_handler![...]` рядом с `set_mic_device`.

- [ ] **Шаг 6: тест на то, что `set_mic_device` больше не стирает другие поля**

В `src-tauri/src/main.rs` (или там, где уже есть тесты на конфиг — если для `main.rs` отдельного `#[cfg(test)] mod tests` с доступом к `Config` нет, добавить unit-тест прямо в `src-tauri/src/config.rs`, вызывая `Config::load`/`save` на временном `AppHandle`-моке невозможно без Tauri-рантайма — вместо этого тест на уровне `Config` фиксирует **инвариант**, а не саму команду: что `Config` с уже установленными `stt_*`-полями, при создании нового значения через `..cfg.clone()`-подобный паттерн смены только mic-полей, sохраняет `stt_*`):

```rust
// src-tauri/src/config.rs, там же, где остальные тесты Config
#[test]
fn смена_только_микрофонных_полей_не_трогает_остальные() {
    let mut cfg = Config {
        mic_device_id: None,
        mic_device_name: None,
        stt_gateway_url: Some("http://localhost:8080".to_string()),
        stt_api_key: Some("secret".to_string()),
    };
    cfg.mic_device_id = Some("{new-id}".to_string());
    cfg.mic_device_name = Some("Новый микрофон".to_string());
    assert_eq!(cfg.stt_gateway_url.as_deref(), Some("http://localhost:8080"));
    assert_eq!(cfg.stt_api_key.as_deref(), Some("secret"));
}
```

Это не полный regression-тест на саму команду `set_mic_device` (для этого нужен реальный `AppHandle`, которого юнит-тесты этого файла не поднимают — ровно та же причина, по которой сегодняшние тесты `Config` тестируют `Config::from_str`/поля напрямую, а не Tauri-команды), но фиксирует сам инвариант «частичное изменение не должно задевать остальные поля», который и был источником бага. Реальная проверка команды — на живом приложении, см. шаг 9.

```bash
cargo.exe test --workspace
```

Ожидается: 178 тестов, 0 упавших.

- [ ] **Шаг 7: блок настроек в `ui/index.html`**

В `<style>`, расширить существующее правило для `select` — те же поля ввода нужны и для текста/пароля:

```css
select,
input[type="text"],
input[type="password"] {
  font: inherit;
  font-family: var(--mono);
  flex: 1;
  min-width: 0;
  padding: 7px 8px;
  border-radius: 6px;
  border: 1px solid var(--line);
  background: var(--bg);
  color: var(--fg);
}
```

(Заменить существующий одиночный селектор `select { ... }` на групповой выше — тело правила не меняется, кроме списка селекторов.)

В разметку, после блока «Микрофон» и перед `<h2>Записи</h2>`:

```html
<h2>Транскрипция</h2>
<div class="row">
  <input type="text" id="stt-url" placeholder="URL шлюза, например http://localhost:8080" />
</div>
<div class="row">
  <input type="password" id="stt-key" placeholder="Ключ" />
</div>
```

- [ ] **Шаг 8: сохранение и загрузка настроек в `ui/main.js`**

Сохранение по потере фокуса поля — тот же принцип, что и у остальных настроек (сразу, без отдельной кнопки «Сохранить»):

```js
async function сохранить_настройки_транскрипции() {
  try {
    await invoke("set_transcribe_config", {
      gatewayUrl: $("stt-url").value || null,
      apiKey: $("stt-key").value || null,
    });
    показать_ошибку("");
  } catch (e) {
    показать_ошибку(String(e));
  }
}

$("stt-url").addEventListener("blur", сохранить_настройки_транскрипции);
$("stt-key").addEventListener("blur", сохранить_настройки_транскрипции);
```

Загрузка текущих значений — `обновить_устройства()` уже делает `invoke("get_config")` для микрофона, довести до конца тем же вызовом, без второго похода за конфигом:

```js
async function обновить_устройства() {
  try {
    const [устройства, конфиг] = await Promise.all([
      invoke("list_mic_devices"),
      invoke("get_config"),
    ]);
    // ...существующее тело до конца без изменений...
    $("stt-url").value = конфиг.stt_gateway_url ?? "";
    $("stt-key").value = конфиг.stt_api_key ?? "";
  } catch (e) {
    показать_ошибку(String(e));
  }
}
```

(Добавить две новые строки в конец `try`-блока существующей функции — сигнатура и остальное тело не меняются.)

- [ ] **Шаг 9: ручная проверка**

Собрать и запустить GUI (`cargo.exe build -p meeting-recorder-gui` и запустить exe, или через `cargo tauri dev`, если настроен). Ввести URL и ключ, переключить окно фокуса или перезапустить — значения должны сохраниться и подставиться в поля. Сменить микрофон в выпадашке, снова открыть настройки — URL/ключ обязаны остаться на месте (это и есть регресс-проверка бага из шага 5, недоступная юнит-тестам без Tauri-рантайма).

- [ ] **Шаг 10: commit**

```bash
git add src-tauri/src/config.rs src-tauri/src/main.rs ui/index.html ui/main.js
git commit -m "feat: настройки транскрипции (URL шлюза, ключ) в UI и config.json"
```

---

## Task 2: HTTP-клиент шлюза — submit, поллинг, разбор ответа

**Файлы:**
- Создать: `src-tauri/src/transcribe.rs`
- Изменить: `src-tauri/Cargo.toml`
- Изменить: `src-tauri/src/main.rs` (добавить `mod transcribe;`)

**Интерфейсы:**
- Производит:
  ```rust
  pub enum Label { Owner, Others }
  pub struct Segment { pub start: f64, pub label: Label, pub text: String }
  pub struct TrackResult { pub segments: Vec<Segment>, pub text: String }   // Default
  pub enum TranscribeError { /* Display через thiserror */ }

  pub enum JobOutcome { Pending, Succeeded(TrackResult), Failed(String) }
  pub fn parse_job_response(body: &str, label: Label) -> Result<JobOutcome, TranscribeError>;

  pub async fn submit_and_poll(
      client: &reqwest::Client, gateway: &str, key: &str, wav_path: &Path, label: Label,
  ) -> Result<TrackResult, TranscribeError>;
  ```

- [ ] **Шаг 1: добавить зависимости в `src-tauri/Cargo.toml`**

```toml
[dependencies]
# ...существующие строки без изменений...
reqwest = { version = "0.12", features = ["json", "multipart"] }
tokio = { version = "1", features = ["time", "fs"] }
thiserror = "2"
```

`tokio` — явно, а не транзитивно через Tauri: Cargo не даёт пакету доступ к чужим прямым зависимостям, даже в одном воркспейсе (см. Global Constraints и находку в соседнем macOS-плане про `sysinfo`, ровно та же ловушка). `thiserror` версии `"2"` — той же мажорной версии, что уже в корневом `Cargo.toml`.

- [ ] **Шаг 2: падающие тесты на разбор ответа шлюза**

`src-tauri/src/transcribe.rs`:

```rust
//! HTTP-клиент шлюза транскрипции: submit одной дорожки, поллинг задачи,
//! слияние двух дорожек по времени. Контракт API — тот же, что уже проверен
//! внешним скриптом расшифровки (`POST /v1/audio/transcriptions/async` +
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
```

- [ ] **Шаг 3: запустить, убедиться, что падает**

```bash
cargo.exe test --workspace transcribe::
```

Ожидается: ошибка компиляции — `parse_job_response` не существует.

- [ ] **Шаг 4: реализовать `parse_job_response`**

```rust
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
```

- [ ] **Шаг 5: прогнать тесты, убедиться, что проходят**

```bash
cargo.exe test --workspace transcribe::
```

Ожидается: все восемь новых тестов зелёные.

- [ ] **Шаг 6: `submit_and_poll` — не юнит-тестируется, реализуется по контракту**

```rust
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
    // внешнем скрипте расшифровки, не изобретается заново.
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
```

- [ ] **Шаг 7: подключить модуль**

В `src-tauri/src/main.rs`, рядом с существующими `mod audio; mod config; ...`:

```rust
mod transcribe;
```

- [ ] **Шаг 8: прогнать полный сьют**

```bash
cargo.exe test --workspace
```

Ожидается: 178 (после Task 1) + 7 новых = 185, 0 упавших (семь `#[test]`-функций из шага 2 — два из них перебирают по два статуса циклом внутри одного теста, это не восемь отдельных тестов). `submit_and_poll` компилируется, но не покрыт юнит-тестом — как и `WindowsDetector`/`build_loopback_capture` в этом же проекте, реальный HTTP-клиент проверяется вручную, не в CI.

- [ ] **Шаг 9: ручная проверка на реальном шлюзе**

С настроенными в UI (Task 1) URL и ключом, временно вызвать `submit_and_poll` на реальном `.wav`-файле (например, через `#[cfg(test)]`-независимый маленький `main`-скретч или интерактивно) — убедиться, что задача уходит, поллинг доходит до `succeeded`, `segments` заполнены при `diarize=true`. Не обязательно автоматизировать — цель шага ровно та же, что у ручной проверки Process Tap в соседнем macOS-плане: подтвердить контракт на реальном сервере до того, как он обрастёт оркестрацией (Task 4).

- [ ] **Шаг 10: commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/transcribe.rs src-tauri/src/main.rs
git commit -m "feat(transcribe): HTTP-клиент шлюза — submit, поллинг, разбор ответа"
```

---

## Task 3: Слияние дорожек в единый транскрипт

**Файлы:**
- Изменить: `src-tauri/src/transcribe.rs`

**Интерфейсы:**
- Производит:
  ```rust
  pub fn merge_markdown(mic: &TrackResult, system: &TrackResult) -> String;
  pub fn merge_plain(mic: &TrackResult, system: &TrackResult) -> String;
  ```

- [ ] **Шаг 1: падающие тесты**

Добавить в `mod tests` того же файла:

```rust
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
```

- [ ] **Шаг 2: запустить, убедиться, что не компилируется**

```bash
cargo.exe test --workspace transcribe::
```

- [ ] **Шаг 3: реализовать**

```rust
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
```

- [ ] **Шаг 4: прогнать тесты, убедиться, что проходят**

```bash
cargo.exe test --workspace
```

Ожидается: 185 (после Task 2) + 8 новых = 193, 0 упавших.

- [ ] **Шаг 5: commit**

```bash
git add src-tauri/src/transcribe.rs
git commit -m "feat(transcribe): слияние двух дорожек по времени в единый транскрипт"
```

---

## Task 4: Оркестрация — две дорожки параллельно, guard, запись результата

**Файлы:**
- Изменить: `src-tauri/src/main.rs`

**Интерфейсы:**
- Потребляет: `transcribe::{Label, submit_and_poll, merge_markdown, merge_plain, TranscribeError}` (Task 2, Task 3), `Config::load` (Task 1), `recordings_root()` (уже существует).
- Производит: команда `transcribe_recording(folder: Option<String>, base: String)`; события `transcribe-progress`/`transcribe-done`/`transcribe-error` с пейлоадом `{ folder, base, ... }`.

- [ ] **Шаг 1: состояние «идёт ли сейчас транскрипция»**

Рядом с существующими `struct Cmd(...)`:

```rust
/// `Some(base)` — какая запись сейчас транскрибируется. Один слот на всё
/// приложение: одновременно — только одна транскрипция, см. дизайн.
struct Transcribing(Mutex<Option<String>>);
```

В `fn main()`, рядом с `.manage(Cmd(...))`/`.manage(Status::default())`/`.manage(Cache::default())`:

```rust
.manage(Transcribing(Mutex::new(None)))
```

- [ ] **Шаг 2: вспомогательные функции emit**

`app.emit(...)` требует трейт `Emitter` в области видимости — сегодня `src-tauri/src/main.rs` импортирует только `use tauri::{AppHandle, WindowEvent};`, без него. Без этой правки `app.emit(...)` ниже не скомпилируется (метод трейта, трейт не виден). Заменить на:

```rust
use tauri::{AppHandle, Emitter, WindowEvent};
```

(`audio.rs` уже импортирует `Emitter` для того же метода — `use tauri::{AppHandle, Emitter, Manager};` — здесь та же причина.)

```rust
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
```

- [ ] **Шаг 3: сама команда**

```rust
#[tauri::command]
async fn transcribe_recording(
    folder: Option<String>,
    base: String,
    app: AppHandle,
    state: tauri::State<'_, Transcribing>,
) -> Result<(), String> {
    {
        let mut current = state.0.lock().map_err(|e| e.to_string())?;
        if current.is_some() {
            return Err("уже идёт транскрипция другой записи".to_string());
        }
        *current = Some(base.clone());
    }

    let result = run_transcription(&folder, &base, &app).await;

    {
        let mut current = state.0.lock().map_err(|e| e.to_string())?;
        *current = None;
    }

    result
}

async fn run_transcription(folder: &Option<String>, base: &str, app: &AppHandle) -> Result<(), String> {
    let cfg = Config::load(app);
    let (url, key) = match (cfg.stt_gateway_url, cfg.stt_api_key) {
        (Some(u), Some(k)) if !u.trim().is_empty() && !k.trim().is_empty() => (u, k),
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
    let client = reqwest::Client::new();
    let (mic_res, sys_res) = tokio::join!(
        transcribe::submit_and_poll(&client, &url, &key, &mic_path, transcribe::Label::Owner),
        transcribe::submit_and_poll(&client, &url, &key, &sys_path, transcribe::Label::Others),
    );

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
    let txt = transcribe::merge_plain(&mic, &sys);

    let out_dir = dir.join(format!("{base}.transcript"));
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    std::fs::write(out_dir.join(format!("{base}.md")), &md).map_err(|e| e.to_string())?;
    std::fs::write(out_dir.join(format!("{base}.txt")), &txt).map_err(|e| e.to_string())?;

    emit_transcribe_done(app, folder, base);
    Ok(())
}
```

Зарегистрировать `transcribe_recording` в `invoke_handler![...]`.

- [ ] **Шаг 4: тест на guard «одна транскрипция за раз»**

`Transcribing` — обёртка над `Mutex<Option<String>>`, саму команду (принимает `AppHandle`) без Tauri-рантайма не запустить, но логику guard'а можно проверить отдельно от Tauri-обвязки:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn второй_slot_не_занимается_пока_первый_не_освобождён() {
        let t = Transcribing(Mutex::new(None));
        {
            let mut slot = t.0.lock().unwrap();
            assert!(slot.is_none());
            *slot = Some("2026-08-10_10-00_zoom".to_string());
        }
        {
            let slot = t.0.lock().unwrap();
            assert!(slot.is_some(), "занятый слот должен остаться занятым");
        }
    }

    #[test]
    fn slot_освобождается_и_снова_доступен() {
        let t = Transcribing(Mutex::new(Some("занято".to_string())));
        *t.0.lock().unwrap() = None;
        assert!(t.0.lock().unwrap().is_none());
    }
}
```

(Тест на саму `#[tauri::command] async fn transcribe_recording` (что при занятом слоте она возвращает `Err` немедленно, не трогая сеть) требует поднятого `AppHandle` — не юнит-тестируется в этом файле по той же причине, что и остальные Tauri-команды здесь; проверяется вручную на шаге 6.)

```bash
cargo.exe test --workspace
```

Ожидается: 193 (после Task 3) + 2 новых = 195, 0 упавших.

- [ ] **Шаг 5: commit кода**

```bash
git add src-tauri/src/main.rs
git commit -m "feat(transcribe): оркестрация двух дорожек, guard на одну транскрипцию, запись .transcript"
```

- [ ] **Шаг 6: ручная проверка на реальной записи**

С настроенными URL/ключом, на реальной записи с обеими дорожками вызвать `transcribe_recording` (через временную кнопку/консоль разработчика webview, раз UI-кнопка появится только в Task 5) — убедиться: (а) второй одновременный вызов сразу получает ошибку про «уже идёт транскрипция»; (б) после завершения первого второй вызов снова проходит; (в) `{base}.transcript/{base}.md` и `.txt` появляются с содержимым, `rename_recording` для этой записи по-прежнему переименовывает `.transcript`-папку вместе с дорожками (уже покрыто существующими тестами `rename.rs`, но стоит увидеть глазами один раз на реальном файле).

---

## Task 5: Кнопка и статус в списке записей

**Файлы:**
- Изменить: `ui/index.html`
- Изменить: `ui/main.js`

**Интерфейсы:**
- Потребляет: команду `transcribe_recording` и события `transcribe-progress`/`-done`/`-error` (Task 4).

- [ ] **Шаг 1: CSS для состояния «занята»**

В `<style>` `ui/index.html`, рядом с правилом `.rename`:

```css
button:disabled {
  opacity: 0.5;
  cursor: default;
}
```

- [ ] **Шаг 2: состояние транскрипций и кнопка в `обновить_список()`**

В `ui/main.js`, в начало файла (рядом с другими module-level переменными вроде `проверка_идёт`):

```js
// key -> "uploading" | "polling" | "merging". Переживает перерисовку списка
// (list.innerHTML = "" на каждый обновить_список()) — состояние источник
// правды не DOM-узел кнопки, который может быть пересоздан, а эта карта.
const транскрипции = new Map();

const СТАДИЯ_ПОДПИСЬ = {
  uploading: "Загрузка…",
  polling: "Обработка…",
  merging: "Слияние…",
};

function ключ_транскрипции(folder, base) {
  return `${folder ?? ""}::${base}`;
}
```

В `обновить_список()`, внутри цикла `for (const з of записи)`, после создания кнопки `рename` (`кнопка`) и перед `li.append(колонка, кнопка)`:

```js
if (з.mic && з.system) {
  const кнопкаТранскрипции = document.createElement("button");
  кнопкаТранскрипции.className = "rename";
  const ключ = ключ_транскрипции(з.folder, з.name);
  const стадия = транскрипции.get(ключ);
  if (стадия) {
    кнопкаТранскрипции.textContent = СТАДИЯ_ПОДПИСЬ[стадия] ?? "Идёт транскрипция…";
    кнопкаТранскрипции.disabled = true;
  } else {
    кнопкаТранскрипции.textContent = "Транскрибировать";
    кнопкаТранскрипции.addEventListener("click", () => начать_транскрипцию(з));
  }
  li.append(колонка, кнопка, кнопкаТранскрипции);
} else {
  li.append(колонка, кнопка);
}
```

(Заменить существующую последнюю строку цикла `li.append(колонка, кнопка); list.append(li);` на этот блок — `list.append(li)` остаётся после блока без изменений.)

- [ ] **Шаг 3: запуск транскрипции и обработчики событий**

```js
async function начать_транскрипцию(запись) {
  try {
    await invoke("transcribe_recording", { folder: запись.folder ?? null, base: запись.name });
  } catch (e) {
    показать_ошибку(String(e));
  }
}
```

В функции `старт()`, рядом с остальными `await listen(...)`:

```js
await listen("transcribe-progress", (e) => {
  транскрипции.set(ключ_транскрипции(e.payload.folder, e.payload.base), e.payload.stage);
  обновить_список();
});
await listen("transcribe-done", (e) => {
  транскрипции.delete(ключ_транскрипции(e.payload.folder, e.payload.base));
  обновить_список();
});
await listen("transcribe-error", (e) => {
  транскрипции.delete(ключ_транскрипции(e.payload.folder, e.payload.base));
  показать_ошибку(e.payload.message);
  обновить_список();
});
```

- [ ] **Шаг 4: ручная проверка**

Собрать GUI, настроить URL/ключ (Task 1), запустить транскрипцию у реальной записи с обеими дорожками кнопкой в списке. Проверить: кнопка блокируется и меняет подпись по стадиям; переключение фокуса окна во время транскрипции (вызывает `обновить_список()` через существующий `window.addEventListener("focus", ...)`) не сбрасывает статус — кнопка остаётся неактивной с той же подписью; по завершении кнопка возвращается в «Транскрибировать»; `.transcript`-файлы читаемы. Отдельно — вызвать транскрипцию у двух записей подряд, не дожидаясь первой: у второй кнопки должна тут же появиться ошибка «уже идёт транскрипция другой записи» в `#err`.

- [ ] **Шаг 5: commit**

```bash
git add ui/index.html ui/main.js
git commit -m "feat(ui): кнопка «Транскрибировать» и статус транскрипции в списке записей"
```

---

## Порядок выполнения

Строго последовательный по номерам — каждая задача опирается на интерфейсы предыдущей (Task 3 использует типы из Task 2, Task 4 — функции из Task 2 и Task 3, Task 5 — команду и события из Task 4). Task 1 при этом полностью независим и самодостаточен — его можно проверить (настройки сохраняются и подставляются) даже если ничего из остального ещё не начато.
