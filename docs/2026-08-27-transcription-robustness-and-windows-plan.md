# Устойчивость транскрипции и Windows — план реализации

> **Для агентов:** ОБЯЗАТЕЛЬНЫЙ САБ-СКИЛЛ — `superpowers:subagent-driven-development`
> (рекомендуется) или `superpowers:executing-plans`. Шаги отмечаются чекбоксами.

**Цель:** транскрипция перестаёт умирать на записях длиннее часа, а редизайн 0.2.0
собирается и ведёт себя правильно на Windows.

**Архитектура:** клиент перестаёт отправлять обе дорожки разом (шлюз всё равно
сериализует их) и перестаёт сдаваться раньше шлюза: ожидание идёт по живости
задачи с абсолютным пределом, а не по числу попыток. Всплывашка на не-macOS
поднимается штатным `always_on_top`. Детект на двух системах остаётся разным —
чинится только отображение имени звонилки.

**Стек:** Rust 2021, Tauri 2, `reqwest` 0.12, `tokio` 1, ванильный JS в `ui/`,
GitHub Actions.

**Спека:** `docs/2026-08-27-transcription-robustness-and-windows-design.md`

**Второй репозиторий:** задача 10 требует, чтобы был выкачен
`DELETE /v1/jobs/:id` из плана `ai-lab/docs/2026-08-27-job-cancellation-plan.md`.
Задачи 1-9 от него не зависят.

## Global Constraints

- Шаг поллинга — **10 секунд**, как сейчас.
- Абсолютный предел ожидания — **3 часа на дорожку**, считается по `Instant`
  (настенные часы), а не по числу итераций.
- Допуск на подряд идущие неудачи опроса — **6**, счётчик обнуляется на любом
  удачном опросе.
- **Клиент не имеет права сдаваться раньше сервера.** У ai-lab лимит 2 часа на
  запрос; наш предел заведомо больше, чтобы причину отказа называл шлюз.
- Дорожки отправляются **последовательно**: `mic`, затем `system`.
- На macOS `always_on_top` для всплывашки **включать нельзя** — причина в
  докблоке `show_ask_popup`, `src-tauri/src/audio.rs:160-166`.
- В `src/detector/windows.rs` **нельзя** заводить список известных процессов:
  там детект по настоящим сессиям WASAPI, и список стал бы регрессией.
- Базовый прогон до начала работ — `cargo test --workspace`, 307 зелёных
  (175 в ядре, 132 в GUI). Ни одна задача не имеет права уменьшить это число.
- Комментарии и тексты — по-русски, как во всём репозитории. Тексты интерфейса —
  по словарю канона, `docs/2026-08-26-ui-design-system.md`, раздел 4.2.

---

## Структура файлов

| Файл | Ответственность | Задачи |
|---|---|---|
| `.github/workflows/check.yml` | создаётся: сборка и тесты на push/PR | 1 |
| `src-tauri/src/transcribe.rs` | HTTP-клиент шлюза: отправка, ожидание, слияние | 3, 4, 5, 10 |
| `src-tauri/src/main.rs` | очередь, воркер, команды Tauri, `run_transcription` | 4, 6, 10 |
| `src-tauri/Cargo.toml` | фичи `reqwest`, новый `tokio-util` | 5 |
| `ui/main.js` | список записей, стадии расшифровки | 6, 8 |
| `src-tauri/src/audio.rs` | всплывашка: показ, позиция, имя источника | 7, 8 |
| `src/detector/mod.rs` | канонизация имени источника, общая для систем | 8 |
| `ui/ask.js` | всплывашка: имя и знак звонилки | 8 |
| `LICENSE`, `.gitignore`, `README.md` | гигиена | 9 |

---

## Отступление от спеки, требующее решения

Спека в §5.3 предлагала канонические имена и для интерфейса, и для имени файла.
При написании плана выяснилось, что это меняло бы **имена файлов и на macOS**
(`zoom-us` → `zoom`), а не только на Windows: `recording_filename`
(`src/storage.rs:22`) прогоняет сырое имя процесса через `sanitize_source`.

Задача 8 поэтому канонизирует **только то, что уходит в интерфейс**, а имена
файлов не трогает вовсе. Следствие: таблица в `ui/main.js` остаётся
«историческим» словарём имён из файлов и должна принимать оба написания, а
`ui/ask.js` переходит на канонические идентификаторы. Разделение зафиксировано
комментариями в обоих файлах.

Если владелец предпочтёт единые имена и в файлах — задача 8 переписывается, и
к ней добавляется миграция уже записанного.

---

### Task 1: CI собирает Windows на каждый push

**Files:**
- Create: `.github/workflows/check.yml`

**Interfaces:**
- Consumes: ничего
- Produces: зелёный прогон на `windows-latest` и `macos-latest`, на который
  опираются все следующие задачи

- [ ] **Step 1: Создать workflow**

```yaml
name: check

# Релизный workflow (release.yml) срабатывает только на тег v*, поэтому за весь
# редизайн 0.2.0 Windows не собирался ни разу. Этот — на каждый push и каждый
# pull request: дешевле поймать несобираемость на PR, чем на релизе.
on:
  push:
  pull_request:

jobs:
  test:
    strategy:
      # Падение одной системы не должно скрывать состояние другой: нам важно
      # видеть обе, а не первую упавшую.
      fail-fast: false
      matrix:
        platform: ['windows-latest', 'macos-latest']

    runs-on: ${{ matrix.platform }}
    steps:
      - uses: actions/checkout@v4

      - name: install Rust stable
        uses: dtolnay/rust-toolchain@stable

      - uses: Swatinem/rust-cache@v2
        with:
          workspaces: '. -> target'

      - name: install frontend dependencies
        run: npm install

      - name: tests
        run: cargo test --workspace

      # Без бандла: нам нужен факт, что GUI-крейт линкуется и фронтенд
      # подхватывается, а не установщик. Бандл на macOS вдобавок требует
      # подписи и заметно дольше.
      - name: build (no bundle)
        run: npx tauri build --no-bundle
```

- [ ] **Step 2: Проверить синтаксис локально**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/check.yml')); print('ok')"`
Expected: `ok`

- [ ] **Step 3: Коммит**

```bash
git add .github/workflows/check.yml
git commit -m "ci: сборка и тесты на Windows и macOS на каждый push"
```

- [ ] **Step 4: Влить в master и дождаться прогона**

```bash
git push -u origin HEAD
gh pr create --base master --title "ci: проверка на push и PR" --body "Релизный workflow идёт только по тегу, поэтому Windows не собирался за весь редизайн."
gh pr checks --watch
```
Expected: оба job зелёные на текущем master.

**Если Windows красный уже здесь** — это находка, а не помеха: master сломан до
редизайна. Остановиться, показать лог, не чинить в этой задаче.

---

### Task 2: Влить редизайн в master

**Files:** ничего не редактируется — это git-операция с CI в роли проверки.

**Interfaces:**
- Consumes: `check.yml` из задачи 1
- Produces: master с 0.2.0, база для задач 3-9

- [ ] **Step 1: Открыть PR из ветки Ани**

```bash
gh pr create --base master --head chore/ui-release \
  --title "Редизайн интерфейса 0.2.0" \
  --body "См. docs/2026-08-26-ui-redesign-plan.md и docs/2026-08-26-ui-release-open-tasks.md. Windows проверяется впервые — check.yml добавлен отдельно."
```

- [ ] **Step 2: Дождаться CI**

Run: `gh pr checks --watch`
Expected: `windows-latest` и `macos-latest` зелёные.

**Если Windows красный** — не чинить вслепую и не отключать job. Прочитать лог,
записать причину в `docs/2026-08-26-ui-release-open-tasks.md` и починить
отдельным коммитом в ту же ветку, повторив прогон.

- [ ] **Step 3: Мердж**

```bash
gh pr merge --merge
git checkout master && git pull
```

- [ ] **Step 4: Зафиксировать базовый прогон**

Run: `cargo test --workspace 2>&1 | tail -5`
Expected: 307 passed, 0 failed. Это число — контрольное для задач 3-9.

---

### Task 3: Ожидание по живости задачи вместо счётчика попыток

**Files:**
- Modify: `src-tauri/src/transcribe.rs:43-57` (варианты ошибки), `:114-156`
  (`submit_and_poll`)
- Test: `src-tauri/src/transcribe.rs`, модуль `tests` в том же файле

**Interfaces:**
- Consumes: `parse_job_response`, `JobOutcome` — существующие
- Produces:
  - `pub enum PollFailure { Transient, Terminal(&'static str) }`
  - `pub fn classify_poll_status(status: u16) -> PollFailure`
  - `pub const POLL_INTERVAL: Duration`, `POLL_DEADLINE: Duration`,
    `MAX_CONSECUTIVE_POLL_FAILURES: u32`

- [ ] **Step 1: Написать падающие тесты**

Добавить в модуль `tests` файла `src-tauri/src/transcribe.rs`:

```rust
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
```

- [ ] **Step 2: Прогнать и убедиться, что падает**

Run: `cargo test -p meeting-recorder-gui transcribe:: 2>&1 | tail -20`
Expected: FAIL — `cannot find function classify_poll_status`, `cannot find value POLL_DEADLINE`.

- [ ] **Step 3: Реализовать классификацию и константы**

В `src-tauri/src/transcribe.rs`, рядом с `TranscribeError`:

```rust
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
```

- [ ] **Step 4: Прогнать тесты**

Run: `cargo test -p meeting-recorder-gui transcribe:: 2>&1 | tail -20`
Expected: PASS, пять новых тестов зелёные.

- [ ] **Step 5: Коммит**

```bash
git add src-tauri/src/transcribe.rs
git commit -m "feat: правила ожидания задачи — предел по времени, а не по числу попыток"
```

---

### Task 4: Дорожки по очереди, id задачи наружу

**Files:**
- Modify: `src-tauri/src/transcribe.rs:114-156` — `submit_and_poll` разделяется
- Modify: `src-tauri/src/main.rs:886-980` — `run_transcription`
- Modify: `src-tauri/src/main.rs:95-127` — `Pending`, `:128-232` — `TranscribeQueue`
- Test: `src-tauri/src/main.rs`, модуль `tests`

**Interfaces:**
- Consumes: `POLL_INTERVAL`, `POLL_DEADLINE`, `MAX_CONSECUTIVE_POLL_FAILURES`,
  `classify_poll_status`, `PollFailure` из задачи 3
- Produces:
  - `pub async fn submit(client: &reqwest::Client, gateway: &str, key: &str, wav_path: &Path) -> Result<String, TranscribeError>`
  - `pub async fn poll_until_done(client: &reqwest::Client, gateway: &str, key: &str, job_id: &str, label: Label) -> Result<TrackResult, TranscribeError>`
  - `TranscribeQueue::note_job(&self, job_id: String)`
  - `TranscribeQueue::take_jobs(&self) -> Vec<String>`
  - событие `transcribe-track` с телом `{ folder, base, track }`, где `track` —
    `"mic"` или `"system"` (принимает задача 6)

- [ ] **Step 1: Написать падающие тесты на учёт id задач**

Добавить в модуль `tests` файла `src-tauri/src/main.rs`, рядом с тестами очереди:

```rust
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
```

- [ ] **Step 2: Прогнать и убедиться, что падает**

Run: `cargo test -p meeting-recorder-gui note_job 2>&1 | tail -20`
Expected: FAIL — `no method named note_job`.

- [ ] **Step 3: Добавить учёт id в очередь**

В `src-tauri/src/main.rs`, в `struct Pending` — новое поле:

```rust
    /// Идентификаторы задач шлюза, уже отправленных для `items[0]`.
    ///
    /// Живут под тем же локом, что `running` и `cancel`, по той же причине:
    /// отмена решает «снять из очереди или прервать на ходу» и «что гасить на
    /// шлюзе» одним снимком. Читайся они порознь, отмена успела бы взять id
    /// уже следующей записи.
    jobs: Vec<String>,
```

В `impl TranscribeQueue` — два метода:

```rust
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
```

В `finish_front` — очистка рядом с существующими сбросами:

```rust
        pending.jobs.clear();
```

- [ ] **Step 4: Прогнать тесты очереди**

Run: `cargo test -p meeting-recorder-gui -- очеред note_job id_задач 2>&1 | tail -20`
Expected: PASS, три новых зелёные, десять старых тестов очереди не тронуты.

- [ ] **Step 5: Разделить `submit_and_poll` на отправку и ожидание**

В `src-tauri/src/transcribe.rs` заменить `submit_and_poll` на две функции:

```rust
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
```

Туда же — два новых варианта ошибки и новый исход, в `enum TranscribeError`:

```rust
    #[error("связь со шлюзом потеряна, задача {0} осталась на нём")]
    PollLost(String),
    #[error("задача {0} отменена")]
    JobCancelled(String),
```

и в `enum JobOutcome`:

```rust
    /// Отменена — на шлюзе или нами. Отдельно от `Failed`: отмену человек
    /// сделал сам, и красное «не удалось» на своё же действие читается как сбой.
    Cancelled,
```

В `parse_job_response` вынести `"cancelled"` из общей ветки отказа:

```rust
        "cancelled" => Ok(JobOutcome::Cancelled),
        "failed" | "error" => {
            Ok(JobOutcome::Failed(resp.error.unwrap_or_else(|| "неизвестная ошибка".to_string())))
        }
```

Существующий тест `ответ_error_и_cancelled_тоже_считаются_отказом` при этом
перестаёт быть верным — он разделяется на два:

```rust
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
```

- [ ] **Step 6: Переписать `run_transcription` на последовательные дорожки**

В `src-tauri/src/main.rs` заменить блок с `tokio::join!` (`:917-928`) на:

```rust
    // Дорожки ПО ОЧЕРЕДИ, а не через join!.
    //
    // Ускорения параллельность не давала никогда: воркер шлюза работает с
    // concurrency: 1 и всё равно выстраивает задачи друг за другом. Зато
    // клиентские часы у обеих тикали одновременно, и вторая дорожка тратила
    // свой бюджет ожидания, стоя в чужой очереди, — ровно поэтому часовые
    // встречи не доезжали. См. docs/2026-08-27-...-design.md, раздел 2.
    let queue = app.state::<TranscribeQueue>();

    emit_transcribe_track(app, folder, base, "mic");
    let mic_res = дорожка_целиком(&client, &url, &key, &mic_path, transcribe::Label::Owner, &queue).await;
    emit_transcribe_track(app, folder, base, "system");
    let sys_res = дорожка_целиком(&client, &url, &key, &sys_path, transcribe::Label::Others, &queue).await;
```

Рядом с остальными `emit_*` (`src-tauri/src/main.rs:808`) — само событие. Его
принимает интерфейс в задаче 6, но шлётся оно отсюда: только здесь известно,
какая дорожка пошла.

```rust
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
```

И свободную функцию для одной дорожки:

```rust
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
```

- [ ] **Step 7: Прогнать всё**

Run: `cargo test --workspace 2>&1 | tail -5`
Expected: 316 passed (312 после задачи 3, плюс 3 теста очереди и плюс 1 от разделения теста про `cancelled`), 0 failed.

- [ ] **Step 8: Коммит**

```bash
git add src-tauri/src/transcribe.rs src-tauri/src/main.rs
git commit -m "fix: дорожки транскрибируются по очереди, а отмена знает id задач

Обе дорожки уходили на шлюз одновременно, но воркер шлюза работает с
concurrency: 1 и обслуживал их по одной. Клиентские часы при этом тикали у
обеих сразу, поэтому вторая дорожка тратила свой бюджет ожидания, стоя в
чужой очереди, и на часовой встрече умирала по таймауту."
```

---

### Task 5: Файл стримится, а не читается в память

**Files:**
- Modify: `src-tauri/Cargo.toml:29` — фичи `reqwest`, новая зависимость
- Modify: `src-tauri/src/transcribe.rs` — тело `submit`
- Test: `src-tauri/src/transcribe.rs`

**Interfaces:**
- Consumes: `submit` из задачи 4
- Produces: та же сигнатура `submit`, без чтения файла целиком

- [ ] **Step 1: Написать падающий тест на длину**

```rust
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
```

- [ ] **Step 2: Прогнать и убедиться, что падает**

Run: `cargo test -p meeting-recorder-gui длина_дорожки 2>&1 | tail -20`
Expected: FAIL — `cannot find function длина_файла`.

- [ ] **Step 3: Добавить зависимости**

В `src-tauri/Cargo.toml`:

```toml
# `stream` — ради отправки дорожки потоком: без неё тело собирается в памяти
# целиком, а двухчасовая запись это ~230 МБ на дорожку (16 кГц, моно, i16).
reqwest = { version = "0.12", features = ["json", "multipart", "stream"] }
# Мост между `tokio::fs::File` и потоком, который принимает reqwest. Крейт уже
# приезжает транзитивно, здесь он объявляется явно ради фичи `io`.
tokio-util = { version = "0.7", features = ["io"] }
```

- [ ] **Step 4: Переписать тело `submit`**

```rust
/// Размер дорожки. Отдельной функцией, потому что его проверяет тест: тело
/// `submit` без живого шлюза не проверить.
async fn длина_файла(path: &Path) -> Result<u64, TranscribeError> {
    Ok(tokio::fs::metadata(path).await?.len())
}
```

и в `submit` заменить первые строки (`tokio::fs::read` + `Part::bytes`) на:

```rust
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
```

- [ ] **Step 5: Прогнать тесты**

Run: `cargo test --workspace 2>&1 | tail -5`
Expected: 317 passed (316 + 1 новый), 0 failed.

- [ ] **Step 6: Коммит**

```bash
git add src-tauri/Cargo.toml src-tauri/src/transcribe.rs Cargo.lock
git commit -m "perf: дорожка уходит на шлюз потоком, а не через память целиком"
```

---

### Task 6: Человек видит, какая дорожка идёт и сколько уже

**Files:**
- Modify: `src-tauri/src/main.rs:808-813` — рядом с `emit_transcribe_progress`
- Modify: `ui/main.js` — карта состояний расшифровки и её отрисовка
- Test: ручная проверка на стенде состояний, `docs/preview/serve.py`

**Interfaces:**
- Consumes: событие `transcribe-track` — шлётся из `run_transcription`, задача 4
- Produces: ничего, чем пользуется Rust

- [ ] **Step 1: Принять событие в интерфейсе**

В `ui/main.js`, рядом с картой `транскрипции`:

```js
// key -> { дорожка: "mic" | "system", с: миллисекунды }.
//
// Отдельная карта, а не поле в `транскрипции`: та хранит строку стадии, её
// читают `подпись_стадии` и `стадия_идёт`, и менять их сигнатуры ради
// подробности, которая к стадии не относится, значит трогать разбор
// `queued:N` без нужды.
const детали_расшифровки = new Map();

const ДОРОЖКА_НОМЕР = { mic: 1, system: 2 };

// «12 мин» вместо «743 с»: точность до секунды здесь никому не нужна, а
// растущее число секунд читается как счётчик ошибки.
function сколько_идёт(с) {
  const минут = Math.floor((Date.now() - с) / 60000);
  return минут < 1 ? "меньше минуты" : `${минут} мин`;
}
```

и подписка рядом с остальными:

```js
  await listen("transcribe-track", (e) => {
    детали_расшифровки.set(ключ_транскрипции(e.payload.folder, e.payload.base), {
      дорожка: e.payload.track,
      с: Date.now(),
    });
    обновить_список();
  });
```

- [ ] **Step 2: Чистить карту вместе с остальными**

Во всех трёх местах, где сейчас чистится `транскрипции` — в слушателях
`transcribe-done`, `transcribe-cancelled`, `transcribe-error` — добавить рядом:

```js
    детали_расшифровки.delete(ключ);
```

(в `transcribe-cancelled` переменной `ключ` нет — завести её так же, как в
соседних слушателях).

- [ ] **Step 3: Показать это в строке записи**

В месте, где строится подпись стадии (`ui/main.js`, около `:525`), после
получения подписи:

```js
  const детали = детали_расшифровки.get(ключ);
  const подпись =
    детали && стадия_идёт(стадия)
      ? `${подпись_стадии(стадия)} дорожка ${ДОРОЖКА_НОМЕР[детали.дорожка] ?? 1} из 2, ${сколько_идёт(детали.с)}`
      : подпись_стадии(стадия);
```

- [ ] **Step 4: Проверить на стенде состояний**

Run: `python3 docs/preview/serve.py`
Открыть http://127.0.0.1:3015/ и пройти состояния расшифровки.
Expected: строка читается «Расшифровываю… дорожка 1 из 2, 3 мин», у записи в
очереди подпись прежняя («В очереди №2»), полоска прогресса ведёт себя как раньше.

- [ ] **Step 5: Коммит**

```bash
git add ui/main.js
git commit -m "feat: у идущей расшифровки видно дорожку и сколько она уже идёт"
```

---

### Task 7: Всплывашка на Windows поверх окна звонка

**Files:**
- Modify: `src-tauri/src/audio.rs:195-196` и `:221-222` — не-macOS ветки показа
- Modify: `src-tauri/src/audio.rs:134-138` — комментарий у `ASK_WIDTH`
- Test: `src-tauri/src/audio.rs`

**Interfaces:**
- Consumes: ничего
- Produces: `fn показать_обычным_окном(w: &tauri::WebviewWindow)`

- [ ] **Step 1: Написать падающий тест на константы**

```rust
    /// Комментарий у ASK_WIDTH годами утверждал, что ширина дублируется в
    /// tauri.conf.json. Она там другая: 876 — это ширина ОКНА, с полем справа
    /// под уезжающую плашку, а ASK_WIDTH — ширина самой плашки. Тест держит
    /// оба числа сверенными с конфигом, чтобы комментарий больше не врал.
    #[test]
    fn ширина_плашки_и_окна_это_разные_числа() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("конфиг");
        let окно = conf["app"]["windows"]
            .as_array()
            .expect("список окон")
            .iter()
            .find(|w| w["label"] == "ask")
            .expect("окно ask");

        assert_eq!(окно["height"].as_f64(), Some(ASK_HEIGHT));
        assert!(
            окно["width"].as_f64().expect("ширина окна") > ASK_WIDTH,
            "окно шире плашки: справа поле, в которое она уезжает при смахивании"
        );
    }
```

- [ ] **Step 2: Прогнать и убедиться, что падает**

Run: `cargo test -p meeting-recorder-gui ширина_плашки 2>&1 | tail -20`
Expected: FAIL — тест новый, компиляции ещё нет либо утверждение не выполнено.

- [ ] **Step 3: Починить комментарий**

```rust
/// Ширина ПЛАШКИ — того, что человек видит. Окно `ask` в
/// `src-tauri/tauri.conf.json` намеренно шире (876): справа от плашки поле, в
/// которое она уезжает при смахивании, и оно висит за краем экрана. Третье
/// место — `ui/ask.js` (`ПЛАШКА` и `ШИРИНА_ОКНА`). Сверено тестом
/// `ширина_плашки_и_окна_это_разные_числа`.
const ASK_WIDTH: f64 = 380.0;
```

- [ ] **Step 4: Поднять окно на не-macOS**

Добавить рядом с `показать_поверх`:

```rust
/// Показать всплывашку на системах, где обычному окну не отказывают в верхнем
/// уровне.
///
/// На macOS так нельзя, и причина в докблоке `show_ask_popup`: приложение
/// живёт в строке меню как `Accessory`, а окна плавающего уровня у неактивного
/// приложения система не показывает вовсе. На Windows этой беды нет — там это
/// обычный `HWND_TOPMOST`, и без него плашка вылезает ПОД окном звонка, то
/// есть не вылезает.
#[cfg(not(target_os = "macos"))]
fn показать_обычным_окном(w: &tauri::WebviewWindow) {
    let _ = w.set_always_on_top(true);
    let _ = w.show();
}
```

и заменить обе не-macOS ветки (`:195-196` и `:221-222`) на:

```rust
        #[cfg(not(target_os = "macos"))]
        показать_обычным_окном(&w);
```

- [ ] **Step 5: Прогнать тесты**

Run: `cargo test --workspace 2>&1 | tail -5`
Expected: 318 passed (317 + 1 новый), 0 failed.

- [ ] **Step 6: Посмотреть плашку на Windows глазами**

Шрифт всплывашки — `-apple-system, system-ui, "SF Pro Text"` при 13px/1.3 в
окне высотой 96 (`ui/ask.html:13`). На Windows это Segoe UI, метрики другие, и
проверяется это только зрением: автотеста на «не разъехалось» нет.

Собрать на Windows, дождаться вопроса о записи.
Expected: заголовок и имя источника не обрезаны и не наезжают друг на друга,
кнопка «Записать» целиком в плашке, полоска отсчёта видна снизу. Если поехало —
чинить здесь же отдельным коммитом, а не заводить новую задачу.

- [ ] **Step 7: Коммит**

```bash
git add src-tauri/src/audio.rs
git commit -m "fix(windows): всплывашка поднимается поверх окна звонка

На macOS её поднимает NSPanel, на остальных системах оставался голый show(),
а alwaysOnTop в конфиге выключен — на Windows плашка появлялась под окном
звонилки. Заодно починен комментарий у ASK_WIDTH: 876 в конфиге это ширина
окна, а не плашки."
```

---

### Task 8: Звонилка узнаётся по имени и на Windows

**Files:**
- Modify: `src/detector/mod.rs` — канонизация, общая для систем
- Modify: `src-tauri/src/audio.rs:936` — что уходит в событие `ask`
- Modify: `ui/ask.js:25-30` — ключи на канонические идентификаторы
- Modify: `ui/main.js:341-352` — исторические имена из файлов, оба написания
- Test: `src/detector/mod.rs`

**Interfaces:**
- Consumes: `MicSession` — существующий
- Produces: `pub fn canonical_source(raw: &str) -> &'static str`, возвращает
  `"zoom" | "teams" | "slack" | "discord" | "unknown"`

**ВНИМАНИЕ, нужны внешние данные.** Точные имена процессов на Windows
снимаются с живой машины, а не выдумываются: `ms-teams.exe` (новый Teams)
против `Teams.exe` (классический) зависит от установки. До того как список
снят, задачу не начинать. Команда на Windows во время звонка:

```powershell
Get-Process | Where-Object { $_.MainWindowTitle -or $_.ProcessName -match 'zoom|teams|slack|discord' } | Select-Object ProcessName | Sort-Object ProcessName -Unique
```

Имена из вывода подставляются в таблицу шага 3 вместо перечисленных ниже
кандидатов, а тест шага 1 дополняется реально увиденными.

- [ ] **Step 1: Написать падающие тесты**

В `src/detector/mod.rs`, новый модуль `tests`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn имена_macos_канонизируются() {
        assert_eq!(canonical_source("zoom.us"), "zoom");
        assert_eq!(canonical_source("Microsoft Teams"), "teams");
        assert_eq!(canonical_source("Slack"), "slack");
        assert_eq!(canonical_source("Discord"), "discord");
    }

    /// Гвоздь задачи: на Windows имена другие, и до этой правки ни одно из них
    /// не совпадало ни с одной таблицей в интерфейсе — у каждого звонка был
    /// общий значок микрофона и сырое «.exe» в строке взвода.
    #[test]
    fn имена_windows_канонизируются_в_то_же_самое() {
        assert_eq!(canonical_source("Zoom.exe"), "zoom");
        assert_eq!(canonical_source("ms-teams.exe"), "teams");
        assert_eq!(canonical_source("Teams.exe"), "teams");
        assert_eq!(canonical_source("slack.exe"), "slack");
        assert_eq!(canonical_source("Discord.exe"), "discord");
    }

    /// Регистр имени процесса между системами и версиями не постоянен, а
    /// узнавание от него зависеть не должно.
    #[test]
    fn регистр_не_влияет() {
        assert_eq!(canonical_source("ZOOM.EXE"), "zoom");
        assert_eq!(canonical_source("slack"), "slack");
    }

    /// Незнакомое имя — это «встреча, программу не узнали», а не пустота:
    /// интерфейс на этом идентификаторе рисует общий значок микрофона.
    #[test]
    fn незнакомое_имя_даёт_unknown() {
        assert_eq!(canonical_source("Finder"), "unknown");
        assert_eq!(canonical_source("explorer.exe"), "unknown");
        assert_eq!(canonical_source(""), "unknown");
    }

    /// Вспомогательные процессы Discord не должны выдавать себя за звонилку —
    /// то же правило, что уже зафиксировано в детекторе macOS.
    #[test]
    fn хелперы_не_звонилка() {
        assert_eq!(canonical_source("Discord Helper"), "unknown");
        assert_eq!(canonical_source("Google Chrome Helper (Renderer)"), "unknown");
    }

    /// Фиксирует решение, а не поведение: встреча в Meet — вкладка браузера, и
    /// имя процесса у неё то же, что у почты и у YouTube. Если этот тест
    /// «починят», добавив браузер, приложение начнёт спрашивать «записать
    /// встречу?» на каждом видео.
    #[test]
    fn браузер_не_звонилка() {
        for b in ["Google Chrome", "chrome.exe", "Arc", "Safari", "msedge.exe"] {
            assert_eq!(
                canonical_source(b),
                "unknown",
                "{b} — браузер, а не звонилка"
            );
        }
    }
}
```

- [ ] **Step 2: Прогнать и убедиться, что падает**

Run: `cargo test -p meeting-recorder canonical_source 2>&1 | tail -20`
Expected: FAIL — `cannot find function canonical_source`.

- [ ] **Step 3: Реализовать канонизацию**

В `src/detector/mod.rs`:

```rust
/// Сырое имя процесса → идентификатор звонилки для интерфейса.
///
/// Одна таблица на обе системы, потому что имена процессов у них разные, а
/// звонилка одна и та же: `zoom.us` на macOS и `Zoom.exe` на Windows — это
/// Zoom, и логотип у него общий.
///
/// **Это НЕ детект.** Детект на двух системах устроен по-разному и правильно:
/// на macOS список имён и есть детект (кто держит микрофон, система без лишних
/// прав не показывает), а на Windows `WindowsDetector` видит настоящие сессии
/// захвата WASAPI. Заводить этот список в детекте Windows нельзя — перестанут
/// ловиться Zoom во вкладке браузера, Webex и всё, чего в списке нет.
///
/// **Имена файлов эта функция не трогает.** `recording_filename`
/// (`src/storage.rs:22`) как прогонял сырое имя через `sanitize_source`, так и
/// прогоняет: канонизация имён на диске переименовала бы и записи macOS, а
/// таблица в `ui/main.js` всё равно обязана понимать всё, что когда-либо было
/// записано.
pub fn canonical_source(raw: &str) -> &'static str {
    let имя = raw.trim().trim_end_matches(".exe").trim_end_matches(".EXE");
    let имя = имя.to_ascii_lowercase();
    match имя.as_str() {
        "zoom.us" | "zoom" => "zoom",
        "microsoft teams" | "ms-teams" | "teams" => "teams",
        "slack" => "slack",
        "discord" => "discord",
        _ => "unknown",
    }
}
```

- [ ] **Step 4: Прогнать тесты ядра**

Run: `cargo test -p meeting-recorder canonical 2>&1 | tail -20`
Expected: PASS, шесть новых зелёных.

- [ ] **Step 5: Слать в интерфейс канонический идентификатор**

В `src-tauri/src/audio.rs:936` заменить

```rust
                            ask(&handle, &s.process_name);
```

на

```rust
                            // В интерфейс уходит канонический идентификатор, а
                            // не сырое имя процесса: на Windows оно другое
                            // (`Zoom.exe` против `zoom.us`), и таблицы во
                            // всплывашке на него не отзывались.
                            ask(&handle, meeting_recorder::detector::canonical_source(&s.process_name));
```

Сигнатура `ask(handle: &AppHandle, source: &str)` не меняется.

- [ ] **Step 6: Перевести всплывашку на канонические ключи**

В `ui/ask.js:25-30`:

```js
// Ключи — КАНОНИЧЕСКИЕ идентификаторы звонилок, как их шлёт событие `ask`
// (`canonical_source` в src/detector/mod.rs). Сырые имена процессов сюда
// класть нельзя: на macOS и Windows они разные.
const ИСТОЧНИКИ = {
  zoom: { имя: "Zoom", класс: "zoom" },
  teams: { имя: "Teams", класс: "teams" },
  slack: { имя: "Slack", класс: "slack" },
  discord: { имя: "Discord", класс: "discord" },
};
```

и в обработчике `listen("ask", ...)` заменить сравнение

```js
    источник === "zoom.us" ? ZOOM_SVG : ...
```

на

```js
    источник === "zoom" ? ZOOM_SVG : ...
```

- [ ] **Step 7: Научить список понимать имена из файлов Windows**

В `ui/main.js:341-352` — ключи остаются от `sanitize_source`, добавляются
написания Windows:

```js
// Ключи — источник ИЗ ИМЕНИ ФАЙЛА, после `sanitize_source`
// (`src/storage.rs:124`). Это исторический словарь: имена на диске не
// переименовываются, поэтому здесь обязаны жить оба написания — macOS
// (`zoom-us`) и Windows (`zoom-exe`), — иначе у записей, сделанных на другой
// системе, пропадёт логотип. Живые идентификаторы звонилок — в `ui/ask.js`,
// это другая таблица и другой формат.
const ИСТОЧНИКИ = {
  "zoom-us": { лого: "zoom", имя: "Zoom" },
  "zoom-exe": { лого: "zoom", имя: "Zoom" },
  "microsoft-teams": { лого: "teams", имя: "Teams" },
  "ms-teams-exe": { лого: "teams", имя: "Teams" },
  "teams-exe": { лого: "teams", имя: "Teams" },
  slack: { лого: "slack", имя: "Slack" },
  "slack-exe": { лого: "slack", имя: "Slack" },
  discord: { лого: null, имя: "Discord" },
  "discord-exe": { лого: null, имя: "Discord" },
  manual: { лого: null, имя: "Запись" },
  unknown: { лого: null, имя: "Встреча" },
};
```

- [ ] **Step 8: Прогнать всё**

Run: `cargo test --workspace 2>&1 | tail -5`
Expected: 324 passed (318 + 6 новых), 0 failed.

- [ ] **Step 9: Коммит**

```bash
git add src/detector/mod.rs src-tauri/src/audio.rs ui/ask.js ui/main.js
git commit -m "fix(windows): звонилка узнаётся по имени процесса и там тоже

Обе таблицы имён в интерфейсе были завязаны на имена процессов macOS, поэтому
на Windows у каждого звонка был общий значок микрофона и сырое .exe в строке
взвода. Канонизация только для интерфейса: имена файлов не трогаем, иначе
переименовались бы и записи macOS."
```

---

### Task 9: Гигиена репозитория

**Files:**
- Create: `LICENSE`
- Modify: `.gitignore`, `README.md:4` и `:264`,
  `docs/2026-08-26-ui-redesign-plan.md`,
  `docs/2026-08-10-in-app-transcription-design.md`,
  `docs/2026-08-10-in-app-transcription-plan.md`

**Interfaces:**
- Consumes: ничего
- Produces: ничего, чем пользуется код

**Открытый вопрос к владельцу перед началом:** README остаётся русским (0.2.0,
ветка редизайна) или берётся английский с `prepare-public-release`? Задача
написана под первый вариант. При втором — английский README переносится
черри-пиком коммита `1a7b17b` и дописывается разделом про 0.2.0.

- [ ] **Step 1: Вернуть лицензию**

```bash
git show prepare-public-release:LICENSE > LICENSE
head -3 LICENSE
```
Expected: `MIT License`.

- [ ] **Step 2: Закрыть `.env`**

В `.gitignore`, рядом с блоком про аудио:

```
# Ключи шлюза транскрипции и всё прочее локальное окружение
.env
.env.*
```

- [ ] **Step 3: Убрать личные пути**

```bash
sed -i 's|`C:\\Users\\<username>\\Recordings`|каталог записей Windows|g; s|`C:\\Users\\<username>\\Recordings\\YYYY-MM\\`|`<каталог записей>\\YYYY-MM\\`|g' README.md
sed -i 's|/Users/<username>/Projects/|<каталог проектов>/|g' docs/2026-08-26-ui-redesign-plan.md
sed -i 's|10\.0\.0\.3:8080|ai-lab.example:8080|g' docs/2026-08-10-in-app-transcription-design.md docs/2026-08-10-in-app-transcription-plan.md
```

- [ ] **Step 4: Проверить, что не осталось**

Run:
```bash
grep -rn "C:\\\\Users\\\\<username>\|/Users/<username>\|10\.0\.0\.3" --include="*.md" --include="*.rs" --include="*.js" --include="*.json" . | grep -v node_modules
```
Expected: пусто.

- [ ] **Step 5: Прогнать тесты**

Run: `cargo test --workspace 2>&1 | tail -5`
Expected: 324 passed, 0 failed. Тест `минимальная_версия_macos_в_бандле_осталась_11_0` читает конфиг — правки в README на него не влияют, но прогон обязателен.

- [ ] **Step 6: Коммит**

```bash
git add LICENSE .gitignore README.md docs/
git commit -m "chore: лицензия, .env в gitignore и вычистка личных путей

Всё это было сделано на ветке prepare-public-release, но она так и не влилась,
а master и редизайн ушли вперёд без неё."
```

---

### Task 10: Отмена гасит задачу на шлюзе

**ЗАВИСИМОСТЬ:** требует выкаченного `DELETE /v1/jobs/:id` — план
`ai-lab/docs/2026-08-27-job-cancellation-plan.md`, задачи 1-5. Проверить
до начала:

```bash
curl -s -o /dev/null -w '%{http_code}\n' -X DELETE \
  -H "Authorization: Bearer $AILAB_KEY" "$AILAB_URL/v1/jobs/несуществующая"
```
Expected: `404`, а не `404` от «route not found». Если возвращается 405 —
маршрута нет, задачу не начинать.

**Files:**
- Modify: `src-tauri/src/transcribe.rs` — новая функция отмены
- Modify: `src-tauri/src/main.rs:861-884` — `cancel_transcription`
- Test: `src-tauri/src/main.rs`

**Interfaces:**
- Consumes: `TranscribeQueue::take_jobs` из задачи 4, `Cancelled` — существующий
- Produces: `pub async fn cancel_job(client: &reqwest::Client, gateway: &str, key: &str, job_id: &str) -> Result<(), TranscribeError>`

- [ ] **Step 1: Написать падающий тест**

```rust
    /// Локальная отмена обязана сработать, даже если шлюз недоступен: человек
    /// нажал кнопку, и кнопка не имеет права зависнуть от чужой сети. Неудача
    /// уходит в лог, а не на экран.
    #[test]
    fn отмена_идущей_забирает_id_для_гашения_на_шлюзе() {
        let (queue, _rx) = TranscribeQueue::new();
        let item = qi("встреча");
        queue.enqueue(item.clone()).expect("постановка");
        queue.start_front(&item).expect("старт");
        queue.note_job("job_mic".to_string());

        assert_eq!(queue.cancel(&item).expect("отмена"), Cancelled::Stopped);
        assert_eq!(
            queue.take_jobs(),
            vec!["job_mic".to_string()],
            "id обязаны пережить отмену — иначе гасить на шлюзе будет нечего"
        );
    }
```

- [ ] **Step 2: Прогнать и убедиться, что падает**

Run: `cargo test -p meeting-recorder-gui отмена_идущей_забирает 2>&1 | tail -20`
Expected: FAIL — если `cancel` чистит `jobs`, вернётся пустой вектор.

- [ ] **Step 3: Добавить вызов отмены в клиент шлюза**

В `src-tauri/src/transcribe.rs`:

```rust
/// Погасить задачу на шлюзе.
///
/// 404 и 409 — не ошибка: задача могла закончиться сама между нажатием и этим
/// вызовом, и «не нашли, что отменять» здесь означает ровно то, чего человек
/// и хотел.
pub async fn cancel_job(
    client: &reqwest::Client,
    gateway: &str,
    key: &str,
    job_id: &str,
) -> Result<(), TranscribeError> {
    let url = format!("{gateway}/v1/jobs/{job_id}");
    let resp = client.delete(&url).bearer_auth(key).send().await?;
    match resp.status().as_u16() {
        200 | 202 | 404 | 409 => Ok(()),
        code => Err(TranscribeError::SubmitRejected(code.to_string())),
    }
}
```

- [ ] **Step 4: Позвать её из команды отмены**

В `src-tauri/src/main.rs`, в ветке `Cancelled::Stopped` команды
`cancel_transcription`:

```rust
        Cancelled::Stopped => {
            // Задача на шлюзе живёт своей жизнью и держит GPU: воркер там
            // работает с concurrency: 1, и пока брошенная задача не погашена,
            // следующая в НАШЕЙ очереди не двинется. Гасим её явно.
            let jobs = queue.take_jobs();
            if !jobs.is_empty() {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let cfg = Config::load(&app);
                    let (Some(url), Some(key)) = (cfg.stt_gateway_url, cfg.stt_api_key) else {
                        return;
                    };
                    let url = url.trim().trim_end_matches('/').to_string();
                    let Ok(client) = reqwest::Client::builder().build() else {
                        return;
                    };
                    for job in jobs {
                        // Неудача не всплывает на экран: локальная отмена уже
                        // сработала, и красное сообщение о чужой сети поверх
                        // собственного успешного действия только пугает.
                        if let Err(e) = transcribe::cancel_job(&client, &url, key.trim(), &job).await {
                            log::warn!("не удалось погасить задачу {job} на шлюзе: {e}");
                        }
                    }
                });
            }
        }
```

- [ ] **Step 5: Прогнать всё**

Run: `cargo test --workspace 2>&1 | tail -5`
Expected: 325 passed (324 + 1 новый), 0 failed.

- [ ] **Step 6: Проверить живьём**

1. Поставить в очередь две записи длиннее двадцати минут.
2. Отменить идущую.
3. Смотреть `GET /v1/jobs/<id>` первой задачи.

Expected: статус `cancelled`, и вторая запись **стартует сразу**, а не
досиживает чужую задачу. Это и есть смысл всей задачи — без шага 6 она не
считается сделанной.

- [ ] **Step 7: Коммит**

```bash
git add src-tauri/src/transcribe.rs src-tauri/src/main.rs
git commit -m "fix: отмена расшифровки гасит задачу на шлюзе, а не только ожидание

Раньше отмена снимала только наше ожидание: задача на шлюзе доживала и держала
GPU, поэтому следующая в очереди не двигалась. Человек нажимал «отменить» и
ждал ровно столько же."
```

---

### Task 11: Каталог записей на Windows — у текущего пользователя, а не у автора

Задача заведена по ходу исполнения, а не при написании плана: находка всплыла в
задаче 9, когда grep искал остатки личных путей.

**Files:**
- Modify: `src/main.rs:38-41` — Windows-ветка `recordings_root`, и новый `mod tests`
- Modify: `src-tauri/src/main.rs:35-38` — то же самое, и тест в существующий `mod tests`
- Test: оба файла, в них же

**Interfaces:**
- Consumes: ничего
- Produces: ничего нового наружу — `recordings_root()` сохраняет сигнатуру

**Почему это баг, а не гигиена.** Под macOS каталог записей выводится из `$HOME`.
Под Windows он прибит гвоздём: `PathBuf::from(r"C:\Users\<username>\Recordings")`. На
машине любого другого человека приложение пишет записи в чужой домашний каталог,
а не имея туда прав — не пишет вовсе. Не всплывало это ровно потому, что у автора
пользователь и назывался `Cypher`.

Оба файла правятся вместе и обязаны остаться одинаковыми: докблок в
`src-tauri/src/main.rs:30-34` прямо требует, чтобы корень записей у GUI и у
консольного бинаря совпадал — разъедься они, GUI перестал бы показывать записи,
сделанные консолью.

- [ ] **Step 1: Написать падающие тесты**

В `src/main.rs` завести модуль (его там сейчас нет вовсе), а в
`src-tauri/src/main.rs` дописать в существующий `mod tests`. Текст одинаковый для
обоих файлов:

```rust
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
```

В `src/main.rs` модуль оформляется целиком:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // (сюда оба теста из блока выше)
}
```

- [ ] **Step 2: Прогнать и убедиться, что падает**

Прогон идёт в CI (локального тулчейна нет).
Expected: на `windows-latest` тест падает — слева `C:\Users\<username>\Recordings`,
справа профиль раннера. На `macos-latest` проходит сразу: там код уже верный, и
этот тест только фиксирует существующее поведение.

- [ ] **Step 3: Починить Windows-ветку в обоих файлах**

Одинаково в `src/main.rs` и в `src-tauri/src/main.rs`:

```rust
#[cfg(target_os = "windows")]
fn recordings_root() -> PathBuf {
    let home = std::env::var("USERPROFILE").expect("%USERPROFILE% обязан быть установлен");
    PathBuf::from(home).join("Recordings")
}
```

`expect`, а не `unwrap_or`, — симметрично macOS-ветке рядом: без домашнего
каталога приложению всё равно некуда писать, и молча подставить что-то другое
хуже, чем сказать об этом вслух.

- [ ] **Step 4: Прогнать в CI**

Expected: зелено на обеих системах, тестов на 2 больше.

- [ ] **Step 5: Проверить, что README не разъехался с кодом**

Задача 9 уже переписала README на «в домашний каталог `Recordings` на Windows».
До этой задачи это было неправдой, после — стало правдой. Убедиться, что так и
читается, и что литерала `C:\Users\<username>` в README не осталось.

- [ ] **Step 6: Коммит**

```bash
git add src/main.rs src-tauri/src/main.rs
git commit -m "fix(windows): записи ложатся в домашний каталог текущего пользователя

Под Windows корень записей был прибит к C:\\Users\\<username>\\Recordings — пути
автора. На чужой машине приложение писало в чужой домашний каталог, а без прав
туда не писало вовсе. Под macOS тот же корень всё это время честно выводился из
\$HOME; теперь ветки симметричны, и обе покрыты тестом."
```
