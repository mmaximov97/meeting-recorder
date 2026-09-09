# Локальный whisper-сервер как второй тип сервера расшифровки — дизайн

Дата: 09.09.2026.
База: `origin/master` после PR #7 (`bfdafc9`) — ссылки на файлы и строки по ней.
Заменяет план «встроенный движок» из `docs/2026-09-02-local-transcription-spec.md`:
решение владельца от 09.09 — движок в приложение не вшивать, а подключать
whisper-сервер, поднятый рядом, как ещё один сервер.

## Задача

Аня, 06.09: «А ты проверил, что локальная модель работает? Что на комп
устанавливаешь модель и она нормально транскрибирует?» Ответ на сегодня —
нет: режим «На этом компьютере» был заглушкой и в PR #7 спрятан.

Нужен способ расшифровывать встречу, не отправляя звук за пределы машины,
без Docker и без GPU-сервера, и чтобы приложение при этом не тащило в себя
C++-движок с моделью.

Решение: приложение умеет говорить не только со шлюзом selfhost-ai-lab, но и
с `whisper-server` из whisper.cpp — тем же экраном настроек, тем же меню
«Расшифровать встречу», той же очередью. Сервер человек ставит сам по
инструкции из README и вписывает `http://127.0.0.1:8178` в адрес.

## Что проверено про whisper-server (09.09)

- **Windows.** В официальном релизе whisper.cpp (`b4938`,
  `whisper-bin-x64.zip`, 7 МБ) лежит готовый `Release/whisper-server.exe` и
  все нужные DLL. Ничего собирать не надо.
- **macOS.** `brew install whisper-cpp` (1.9.2) собирается с
  `-DWHISPER_BUILD_SERVER=OFF` — сервера в нём **нет**, только `whisper-cli`.
  Сервер на Mac — сборка из исходников: `brew install cmake`, `git clone`,
  `cmake -B build && cmake --build build -j`; в стандартной сборке
  `WHISPER_BUILD_SERVER` включён по умолчанию, Metal подхватывается сам.
  Альтернатива без сборки — selfhost-ai-lab в Docker, он уже описан в README.
- **Протокол.** `POST {url}/inference`, multipart: `file` (WAV 16 кГц моно —
  ровно то, что пишет `src/storage.rs`, конвертация не нужна), `language`
  (`auto` допустим), `response_format=verbose_json`, `temperature`. Ответ:
  `{"text": "...", "language": "...", "segments": [{"id", "start", "end",
  "text", ...}]}`, `start`/`end` в секундах. Ошибки — 400 (нет файла, не
  WAV), 500 (движок), 499 (клиент оборвал соединение).
- **Один запрос за раз.** Сервер держит один контекст модели под мьютексом;
  второй запрос ждёт первого. Отмены запроса нет: оборванное соединение
  сервер замечает только по завершении расчёта.
- **Ключа нет.** Никакой аутентификации; лишний `Authorization` игнорируется.
- **VAD.** Включается флагом при запуске сервера (`--vad -vm <модель>`),
  запросом его тоже можно попросить (`vad=true`), но без загруженной при
  старте VAD-модели это ошибка. Клиент про VAD молчит — это забота запуска.
- **Модели.** `ggml-large-v3-turbo-q5_0.bin` — 574 041 195 байт,
  `https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin`;
  VAD `ggml-silero-v5.1.2.bin` — 885 098 байт,
  `https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v5.1.2.bin`.

---

## 1. Тип сервера в конфиге

`src-tauri/src/config.rs`, `Config`: новое поле

```rust
/// `"gateway" | "whisper_cpp"`. Отсутствие поля (старый конфиг) и `None` —
/// `"gateway"`: шлюз selfhost-ai-lab, как было всегда. Разбор в действующий
/// протокол живёт в `transcribe::effective_server`, не здесь.
pub transcribe_server: Option<String>,
```

Тем же приёмом, что `transcribe_mode` и `language`: конфиг хранит строку,
разбирает её модуль-потребитель. Четыре литерала `Config { ... }` в тестах
`config.rs` получают `transcribe_server: None`.

Команда `set_transcribe_server(kind: Option<String>)` в `main.rs` рядом с
`set_transcribe_mode` (`main.rs:1147`): загрузить, подменить поле, сохранить.

## 2. Развилка

`src-tauri/src/transcribe.rs`. Сейчас режим один — `Mode { Server, Local }`
(`transcribe.rs:17`), развилка в `transcribe_track` (`transcribe.rs:435`).
Становится:

```rust
pub enum Mode {
    /// Шлюз selfhost-ai-lab: async-задача, опрос, диаризация. Как было.
    Gateway,
    /// whisper.cpp `whisper-server`: один синхронный POST, без ключа.
    WhisperCpp,
    /// Заглушка встроенного движка, спрятана (`LOCAL_MODE_AVAILABLE`).
    Local,
}

/// Из двух полей конфига — один режим. `transcribe_mode == "local"` при
/// поднятом `LOCAL_MODE_AVAILABLE` побеждает; иначе смотрим тип сервера.
pub fn effective_mode(cfg_mode: Option<&str>, cfg_server: Option<&str>) -> Mode
```

`Mode::Server` переименовывается в `Mode::Gateway` по всему крейту — чтобы
«сервер» перестало означать одновременно «не локально» и «шлюз».
Переименование механическое, поведение шлюза не меняется.

`transcribe_track` получает третью ветку:

```rust
Mode::WhisperCpp => whisper_cpp::transcribe(client, url, path, label).await,
```

`on_job` в этой ветке не зовётся: у whisper-server нет id задачи и нечего
класть в `Pending::jobs` для отмены.

## 3. Клиент whisper-server

Новый модуль `src-tauri/src/whisper_cpp.rs`. Две части: чистая и сетевая.

**Чистая** — разбор ответа, тестируется без сети:

```rust
pub fn parse_response(body: &str, label: Label) -> Result<TrackResult, TranscribeError>
```

`verbose_json` → `TrackResult { segments, text }`: `start` берётся как есть
(секунды), `text` обрезается по краям (whisper отдаёт с ведущим пробелом),
`speaker: None`. Сегмент без `start` (сервер запущен с `-nt`) — ошибка
разбора с понятным текстом, а не нули: без таймкодов `merge_markdown` не
сможет свести дорожки. Пустой список сегментов при непустом `text` — не
ошибка: одна дорожка целиком тишина, так бывает.

**Сетевая**:

```rust
pub async fn transcribe(
    client: &reqwest::Client, url: &str, wav_path: &Path, label: Label,
) -> Result<TrackResult, TranscribeError>
```

- `POST {url}/inference`, multipart: `file` через `Part::file` (потоком, как
  `submit` в `transcribe.rs:246`), `language=auto`,
  `response_format=verbose_json`, `temperature=0.0`. Без `Authorization`.
- Таймаута на запрос **нет** — ответ приходит только после полного расчёта,
  а это на CPU может быть час. Общая граница — `tokio::time::timeout` на
  весь вызов с тем же `POLL_DEADLINE` (3 часа, `transcribe.rs:107`), исход —
  `TranscribeError::Timeout` с именем дорожки вместо id задачи.
- Не-2xx → `TranscribeError::JobFailed("<дорожка>", "<статус>: <тело до 200
  символов>")`. Тело whisper-server короткое и человеческое («no 'file'
  field», «failed to read WAV»), ключом здесь не поможешь, и текст
  `SubmitRejected` («проверьте ключ») не годится.
- Сетевая ошибка → `TranscribeError::Network` как есть: «сервер не
  запущен» на localhost выглядит именно так, и текст reqwest про отказ
  соединения читаем.

## 4. Ключ и адрес

`ключи_шлюза` (`main.rs:1527`) сегодня требует и адрес, и ключ. Становится
`параметры_сервера(mode, url, key)`:

- `Gateway` — как было: оба непустые, иначе «настройте URL и ключ шлюза»;
- `WhisperCpp` — только адрес; ключ игнорируется даже если введён;
  сообщение при пустом адресе — «настройте адрес whisper-сервера»;
- `Local` — как было, пустые строки.

Хвостовой `/` у адреса срезается для обоих типов, как сейчас.

## 5. Ход работы, отмена, очередь

- **Стадии.** В `дорожка_целиком` (`main.rs:1559`) для `WhisperCpp` —
  сразу `"polling"` («Расшифровываю…»), как у `Local`: «Отправляю…» длится
  секунду на localhost и врёт про суть — сервер считает, а не принимает.
  Счётчик времени по дорожке уже растёт сам (`transcribe.progress`).
- **Отмена.** `select!` в `spawn_transcribe_worker` (`main.rs:322`) бросает
  future, reqwest закрывает соединение — для приложения дорожка отменена
  честно и сразу. Сервер при этом **досчитывает** запрос до конца и только
  потом видит обрыв; следующая дорожка встанет за ним. Записать в докблок
  `whisper_cpp::transcribe` и в README, обойти нельзя: у whisper-server нет
  API отмены.
- **Очередь.** Без изменений: одна запись за раз, дорожки по очереди — это
  и так единственный режим, который whisper-server переживёт.
- **Диаризации нет.** `speaker: None` на всех сегментах; `merge_markdown`
  тогда выводит «Владелец»/«Собеседники» без номеров — как для дорожки, где
  шлюз диаризацию не прислал (`transcribe.rs:523`). Ничего дописывать не
  надо.

## 6. Настройки

`ui/index.html`, раздел «Расшифровка встреч» (`index.html:1844`). Выпадашка
режима `#transcribe-mode` после PR #7 держит один видимый пункт «На сервере»
— выпадашка из одного пункта выглядит как поломка. Пока
`ЛОКАЛЬНЫЙ_РЕЖИМ_ДОСТУПЕН` опущен, её строка целиком прячется (`hidden`), а
на её месте стоит выпадашка типа сервера, тем же контролом `.row.mode`:

```html
<select id="transcribe-server">
  <option value="gateway"     data-i18n="server.gateway">Шлюз selfhost-ai-lab</option>
  <option value="whisper_cpp" data-i18n="server.whisperCpp">whisper.cpp server</option>
</select>
```

Под ней строка-подсказка `.hint` с текстом по типу: для шлюза —
`server.gatewayHint` («Асинхронно, различает собеседников между собой, нужен
ключ»), для whisper.cpp — `server.whisperCppHint` («На этом компьютере или в
локальной сети, без ключа, собеседников между собой не различает. Как
поднять — в README») и кнопка-ссылка «Инструкция», которая через `open_url`
открывает README на якоре раздела.

`#stt-key-row` скрывается (`hidden`) при `whisper_cpp` тем же приёмом, что
при локальном режиме (`main.js:1715`). Плейсхолдер адреса для whisper.cpp —
`http://127.0.0.1:8178`.

`main.js`: `применить_тип_сервера(тип)` рядом с
`применить_режим_расшифровки`; чтение из конфига в том же месте, где сейчас
читаются `stt_gateway_url`/`stt_api_key` (`main.js:1679`); `change` →
`invoke("set_transcribe_server")`.

Строки в `ui/i18n/strings.json`, новый раздел `server`: `gateway`,
`whisperCpp`, `gatewayHint`, `whisperCppHint`, `howTo`. Оба языка.

## 7. README

Оба README, раздел «Расшифровка и приватность» (`README.ru.md:75`,
`README.md:75`). Абзац «Режим… сейчас в работе» заменяется подразделом
**«Свой whisper-сервер на этом компьютере»**:

1. Что это даёт: звук не покидает машину, ключ не нужен, собеседники между
   собой не различаются.
2. Модель: две ссылки (turbo 574 МБ и VAD 0.9 МБ), куда положить.
3. Windows: zip из релизов whisper.cpp, распаковать, команда
   `whisper-server.exe -m ggml-large-v3-turbo-q5_0.bin -l auto --vad -vm ggml-silero-v5.1.2.bin --port 8178 -t 8`.
4. macOS: `brew install cmake git`, clone, `cmake -B build`,
   `cmake --build build -j`, та же команда с `./build/bin/whisper-server`.
   Честно: brew-пакет сервера не содержит.
5. В приложении: тип «whisper.cpp server», адрес `http://127.0.0.1:8178`.
6. Ожидания: Mac с Metal — быстрее реального времени в несколько раз;
   Windows без GPU — часовая встреча около часа; отмена в приложении не
   останавливает расчёт на сервере.

## 8. Что остаётся как есть

- `local.rs` и режим «На этом компьютере» — спрятаны, не трогаются. В
  `docs/2026-09-02-local-transcription-spec.md` дописать абзац: путь 09.09 —
  внешний сервер; встроенный движок отложен без срока.
- Протокол шлюза, очередь, отмена на шлюзе, слияние дорожек — без изменений.
- `Segment`, `TrackResult`, `merge_markdown` — без изменений.

---

## Проверка

**Тесты (GUI-крейт, гоняет CI на macOS и Windows; здесь — компиляция через
`cargo xwin check --all-targets`):**

- `whisper_cpp::parse_response`: полный ответ с тремя сегментами; ведущие
  пробелы обрезаны; `speaker` всегда `None`; пустые сегменты при непустом
  тексте; сегмент без `start` — ошибка; мусор вместо JSON — ошибка разбора.
- `effective_mode`: `(None, None)` → `Gateway`; `(None, "whisper_cpp")` →
  `WhisperCpp`; `("local", _)` → `Gateway`, пока флаг опущен; незнакомая
  строка → `Gateway`.
- `параметры_сервера`: whisper.cpp без ключа проходит, шлюз без ключа — нет;
  хвостовой `/` срезан.
- `config.rs`: старый JSON без `transcribe_server` читается, поле `None`.
- `transcribe_track` в `WhisperCpp` не зовёт `on_job` (тот же приём, что
  тест для `Local`, `transcribe.rs:1133`).

**Живая проверка на этой машине (Linux):** собрать whisper.cpp из исходников
с `whisper-server`, поднять с turbo-моделью на CPU, синтезировать русскую
фразу через ai-lab TTS в WAV 16 кГц моно и прогнать против сервера
`curl`-запросом ровно той формы, что шлёт клиент (multipart, `language=auto`,
`verbose_json`); сверить, что ответ разбирается `parse_response` — через
юнит-тест с этим же телом ответа, положенным в `tests/fixtures`.

**Что может проверить только человек:** Аня на Mac — сборка сервера по
README и расшифровка настоящей встречи; Максимов на Windows — zip и та же
встреча; отмена во время расчёта — строка возвращается сразу, сервер
дорабатывает.

## Порядок работ

1. `Mode::Server` → `Mode::Gateway`, `effective_mode` с двумя аргументами,
   поле конфига, команда `set_transcribe_server`. Тесты зелёные, поведение
   прежнее.
2. `whisper_cpp.rs`: `parse_response` с тестами, затем `transcribe`.
3. Ветка в `transcribe_track`, `параметры_сервера`, стадии в
   `дорожка_целиком`.
4. Настройки: разметка, строки, JS.
5. README на двух языках, абзац в спеке 02.09.
6. Живая проверка на Linux, фикстура ответа в тест.
