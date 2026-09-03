# Выбор микрофона, месячные папки и переименование — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Записывать в явно выбранный микрофон, раскладывать записи по месячным папкам, переименовывать их из окна приложения и видеть, что микрофон реально слышит владельца.

**Architecture:** Ядро (`meeting_recorder`) получает выбор устройства как значение и ничего не сериализует; GUI (`meeting-recorder-gui`) владеет конфигом на диске, файловыми операциями над готовыми записями и окном. Вся новая логика, которую можно сломать незаметно, вынесена в чистые функции (`pick`, `split_name`, `rename_tail`, `imbalance`) и покрыта тестами без железа.

**Tech Stack:** Rust + Tauri v2, `cpal` (WASAPI), `hound` (WAV), `chrono`, `serde` + `serde_json` (конфиг).

**Спека:** `docs/2026-07-30-device-folders-rename-design.md`. Читать до начала работы — здесь только «как», обоснования «почему» там.

## Global Constraints

- **Таргет только `x86_64-pc-windows-msvc`.** Код не собирается под Linux/WSL. Все сборки и тесты — Windows-тулчейном через интероп: `cargo.exe test --workspace`, `cargo.exe build --release -p meeting-recorder-gui`. Обычный `cargo` соберёт Linux-бинарь и упадёт.
- **Код живёт на Windows-диске:** `/mnt/c/Users/<username>/Projects/meeting-recorder`. Не переносить в WSL-ФС.
- **Комментарии, имена тестов и сообщения коммитов — на русском**, как в остальном коде проекта.
- **Формат аудио неизменен:** 16000 Гц, моно, 16 бит PCM, WAV, две раздельные дорожки.
- **Корень записей:** `C:\Users\<username>\Recordings`. Никогда не внутри vault — там git-автокоммиты.
- **Аудио не коммитится.** `.gitignore` уже покрывает `*.wav`.
- **Инвариант микрофона сохраняется:** потоки захвата открываются только на детекте, ручном старте или явном включении проверки; закрываются сразу при `DiscardRing`, `CloseFile` и выключении проверки в `Idle`. Держать их открытыми «на всякий случай» запрещено.
- **Порог дисбаланса:** 20 дБ по RMS. Пол уровня: −120 dBFS.
- **Ширина префикса имени:** ровно 17 символов, `YYYY-MM-DD_HH-MM_`.

## File Structure

| Файл | Ответственность | Действие |
|---|---|---|
| `src/capture/mod.rs` | Выбор и открытие устройств, ресемплинг | Modify: `DeviceChoice`, `pick`, `list_input_devices`, `resolve_input`, разделение `build_capture` |
| `src/storage.rs` | Имена файлов и месячные папки, `WavSink` | Modify: `month_dir`, `split_name`, `rename_tail`, `sanitize_tail` |
| `src/app.rs` | Оркестратор: машина, кольцо, синки, уровни | Modify: корень вместо каталога, `set_mic_device`, `device_warning`, `set_monitor`, `levels`; расширение `ФейкAudio` в тестах |
| `src/main.rs` | Консольный отладочный бинарь | Modify: корень + `DeviceChoice::Default` |
| `src-tauri/src/config.rs` | Конфиг на диске | Create |
| `src-tauri/src/rename.rs` | Переименование готовой записи на диске | Create |
| `src-tauri/src/imbalance.rs` | RMS по готовому файлу и сравнение дорожек | Create |
| `src-tauri/src/main.rs` | Tauri-команды, обход каталога, склейка записей | Modify |
| `src-tauri/src/audio.rs` | Аудио-цикл, `Ctl`, таймаут проверки | Modify |
| `src-tauri/src/status.rs` | Разделяемая истина о состоянии | Modify: поле `device_warning` |
| `ui/index.html`, `ui/main.js` | Окно | Modify: выпадашка, полоски уровня, переименование, пометки |
| `scripts/migrate-to-month-folders.sh` | Разовая миграция | Create |

---

### Task 1: Чистый матчинг устройства

> **Частично отменена задачей Task 2a.** Текст ниже оставлен как есть — это запись того, что было сделано. Матчинг по имени заменён на матчинг по `DeviceId`: ревью Task 2 показало, что посылка «cpal не даёт стабильных идентификаторов» ложна. Тесты и сигнатуры этой задачи переписаны в Task 2a; реализовывать её текст заново не нужно.

Единственная часть выбора устройства, которую можно проверить без железа. Ошибка здесь (сравнение подстрокой вместо равенства, перепутанный порядок) даёт не отказ, а тихо не то устройство — то есть ровно тот класс бага, из-за которого спека и появилась.

**Files:**
- Modify: `src/capture/mod.rs`

**Interfaces:**
- Produces: `pub enum DeviceChoice { Default, Named(String) }`; `fn pick(names: &[String], choice: &DeviceChoice) -> Option<usize>`

- [ ] **Step 1: Написать падающий тест**

В конец `src/capture/mod.rs`, в существующий `mod tests`:

```rust
    fn имена() -> Vec<String> {
        vec![
            "Microphone Array (Intel SST)".to_string(),
            "Headset (Boss Bose)".to_string(),
            "Headset (Boss Bose Hands-Free)".to_string(),
        ]
    }

    #[test]
    fn дефолт_не_выбирает_никого_из_списка() {
        assert_eq!(
            pick(&имена(), &DeviceChoice::Default),
            None,
            "Default означает «спросить систему», а не «взять первое из списка»"
        );
    }

    #[test]
    fn именованное_устройство_ищется_точным_совпадением() {
        assert_eq!(
            pick(&имена(), &DeviceChoice::Named("Headset (Boss Bose)".into())),
            Some(1)
        );
    }

    /// Подстрока — не совпадение. "Headset (Boss Bose)" является префиксом
    /// "Headset (Boss Bose Hands-Free)", и матчинг по `contains`/`starts_with`
    /// выбрал бы узкополосный HFP-профиль вместо нормального.
    #[test]
    fn подстрока_не_считается_совпадением() {
        assert_eq!(
            pick(&имена(), &DeviceChoice::Named("Headset (Boss".into())),
            None
        );
    }

    #[test]
    fn отсутствующее_устройство_даёт_none() {
        assert_eq!(pick(&имена(), &DeviceChoice::Named("Yeti".into())), None);
    }

    /// cpal не даёт стабильных идентификаторов, поэтому два одинаковых имени
    /// различить нечем. Берём первое — детерминированно и объяснимо.
    #[test]
    fn дубликат_имени_разрешается_в_пользу_первого() {
        let names = vec!["Yeti".to_string(), "Yeti".to_string()];
        assert_eq!(pick(&names, &DeviceChoice::Named("Yeti".into())), Some(0));
    }

    #[test]
    fn пустой_список_не_паникует() {
        assert_eq!(pick(&[], &DeviceChoice::Named("Yeti".into())), None);
        assert_eq!(pick(&[], &DeviceChoice::Default), None);
    }
```

- [ ] **Step 2: Прогнать тест и убедиться, что он падает**

Run: `cd /mnt/c/Users/<username>/Projects/meeting-recorder && cargo.exe test -p meeting-recorder pick 2>&1 | tail -20`

Expected: ошибка компиляции — `cannot find function 'pick'`, `cannot find type 'DeviceChoice'`.

- [ ] **Step 3: Написать минимальную реализацию**

В `src/capture/mod.rs`, рядом с `enum Source`:

```rust
/// Какой микрофон брать. `Default` — тот, что выбран в системе.
///
/// Отдельный тип, а не `Option<String>`: `None` в вызывающем коде читается как
/// «не задано», а здесь это осмысленный выбор «спросить систему», и путать эти
/// два смысла нельзя — от них зависит, писать ли предупреждение о подмене.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DeviceChoice {
    #[default]
    Default,
    Named(String),
}

/// Индекс выбранного устройства в списке имён.
///
/// Сравнение строго по равенству. `contains`/`starts_with` здесь были бы багом:
/// "Headset (Boss Bose)" — префикс "Headset (Boss Bose Hands-Free)", и нестрогий
/// матчинг молча выбрал бы узкополосный HFP-профиль.
fn pick(names: &[String], choice: &DeviceChoice) -> Option<usize> {
    match choice {
        DeviceChoice::Default => None,
        DeviceChoice::Named(want) => names.iter().position(|n| n == want),
    }
}
```

- [ ] **Step 4: Прогнать тесты и убедиться, что они проходят**

Run: `cargo.exe test -p meeting-recorder pick 2>&1 | tail -20`

Expected: `test result: ok. 6 passed`.

- [ ] **Step 5: Закоммитить**

```bash
git add src/capture/mod.rs
git commit -m "feat(capture): DeviceChoice и матчинг устройства по точному имени"
```

---

### Task 2: Разрешение устройства и разделение build_capture

> **Частично отменена задачей Task 2a.** Разделение `build_capture` на две функции остаётся в силе; `list_input_devices` и `resolve_input` переписаны в Task 2a на идентификаторы вместо имён. Текст ниже — запись сделанного, реализовывать заново не нужно.

Сейчас `build_capture` — одна функция с тремя `match source` внутри; выбор устройства касается только микрофона и дал бы четвёртый матч с `unreachable` в одной ветке.

**Files:**
- Modify: `src/capture/mod.rs`
- Modify: `src/app.rs:20` (импорт), `src/app.rs:185-187` (вызовы)

**Interfaces:**
- Consumes: `DeviceChoice`, `pick` (Task 1)
- Produces:
  - `pub fn list_input_devices() -> Result<Vec<String>, CaptureError>`
  - `pub struct Resolved { pub device: cpal::Device, pub fell_back_from: Option<String> }`
  - `pub fn resolve_input(choice: &DeviceChoice) -> Result<Resolved, CaptureError>`
  - `pub fn build_mic_capture(choice: &DeviceChoice, sink: Sender<Vec<i16>>) -> Result<(PendingCapture, Option<String>), CaptureError>`
  - `pub fn build_loopback_capture(sink: Sender<Vec<i16>>) -> Result<PendingCapture, CaptureError>`

- [ ] **Step 1: Написать реализацию**

В `src/capture/mod.rs` заменить `build_capture` на две функции и добавить разрешение устройства:

```rust
/// Имена всех доступных устройств записи — для выпадашки в UI.
///
/// Устройство, у которого имя не читается, пропускается: `Device::description()`
/// ходит в WASAPI и может отказать на отдельном эндпоинте, и ронять из-за него
/// весь список неправильно — остальные устройства выбрать по-прежнему можно.
///
/// Имя берётся через `description()`, а НЕ через `Device::name()`: в закреплённой
/// здесь cpal 0.18.1 метода `name()` у `Device` нет, он заменён на
/// `description() -> Result<DeviceDescription, _>` с `DeviceDescription::name()`.
pub fn list_input_devices() -> Result<Vec<String>, CaptureError> {
    let host = cpal::default_host();
    Ok(host
        .input_devices()?
        .filter_map(|d| d.description().ok().map(|desc| desc.name().to_string()))
        .collect())
}

/// Устройство записи плюс признак того, что взяли не то, о чём просили.
pub struct Resolved {
    pub device: cpal::Device,
    /// `Some(имя)` — просили это, не нашли, взяли системный дефолт.
    pub fell_back_from: Option<String>,
}

/// Находит устройство по выбору, откатываясь на системный дефолт.
///
/// Фолбэк, а не ошибка: цена несимметрична. Испорченная дорожка чинится вторым
/// дублем или усилением, потерянная встреча не чинится ничем. Но молчать о
/// подмене нельзя — за этим и нужен `fell_back_from`.
pub fn resolve_input(choice: &DeviceChoice) -> Result<Resolved, CaptureError> {
    let host = cpal::default_host();
    if let DeviceChoice::Named(want) = choice {
        let devices: Vec<cpal::Device> = host.input_devices()?.collect();
        let names: Vec<String> = devices
            .iter()
            .map(|d| {
                d.description()
                    .map(|desc| desc.name().to_string())
                    .unwrap_or_default()
            })
            .collect();
        if let Some(i) = pick(&names, choice) {
            return Ok(Resolved {
                device: devices.into_iter().nth(i).expect("индекс из pick валиден"),
                fell_back_from: None,
            });
        }
        let device = host
            .default_input_device()
            .ok_or(CaptureError::NoDevice(Source::Mic))?;
        return Ok(Resolved {
            device,
            fell_back_from: Some(want.clone()),
        });
    }
    let device = host
        .default_input_device()
        .ok_or(CaptureError::NoDevice(Source::Mic))?;
    Ok(Resolved {
        device,
        fell_back_from: None,
    })
}

/// Открывает микрофон, **не запуская** поток (см. [`PendingCapture`]).
///
/// Возвращает вместе с потоком признак подмены устройства: сообщить о ней
/// должен тот, кто умеет говорить с пользователем, а не этот модуль.
pub fn build_mic_capture(
    choice: &DeviceChoice,
    sink: Sender<Vec<i16>>,
) -> Result<(PendingCapture, Option<String>), CaptureError> {
    let Resolved {
        device,
        fell_back_from,
    } = resolve_input(choice)?;
    let supported = device.default_input_config()?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let pending = build_stream_for_format(&device, config, sample_format, sink)?;
    Ok((pending, fell_back_from))
}

/// Открывает системный loopback, **не запуская** поток.
///
/// WASAPI включает loopback прозрачно: cpal видит `data_flow == eRender` и сам
/// добавляет `AUDCLNT_STREAMFLAGS_LOOPBACK` при инициализации входного потока.
/// Отдельного «loopback-устройства» перебирать не нужно — проверено по
/// исходникам cpal 0.18.1 (`src/host/wasapi/device.rs`).
///
/// Устройство всегда системное по умолчанию: собеседников слушают через тот же
/// выход, на который идёт звук. Родной формат читается из output-конфига — у
/// render-устройства список input-конфигов пуст по построению.
pub fn build_loopback_capture(sink: Sender<Vec<i16>>) -> Result<PendingCapture, CaptureError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(CaptureError::NoDevice(Source::SystemLoopback))?;
    let supported = device.default_output_config()?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    build_stream_for_format(&device, config, sample_format, sink)
}
```

- [ ] **Step 2: Обновить вызовы в app.rs**

`src/app.rs:20` — заменить импорт:

```rust
use crate::capture::{build_loopback_capture, build_mic_capture, start_silence, DeviceChoice};
```

`src/app.rs:185-187` — заменить два вызова. Поле `mic` добавляется в `CpalAudio` в Task 3; пока подставляем `DeviceChoice::Default`, чтобы компилировалось:

```rust
        let (mic_pending, fell_back) = build_mic_capture(&DeviceChoice::Default, tx_mic)?;
        let t2 = Instant::now();
        let sys_pending = build_loopback_capture(tx_sys)?;
        let t3 = Instant::now();
        let _ = fell_back; // проводка появится в Task 3
```

- [ ] **Step 3: Проверить, что всё собирается и старые тесты зелёные**

Run: `cargo.exe test --workspace 2>&1 | tail -20`

Expected: сборка проходит, `test result: ok` — 107 существующих тестов плюс 6 из Task 1.

- [ ] **Step 4: Закоммитить**

```bash
git add src/capture/mod.rs src/app.rs
git commit -m "refactor(capture): разделить build_capture на mic и loopback, добавить resolve_input"
```

---

### Task 2a: Перевод выбора устройства с имени на DeviceId

Задача-исправление, заведённая по находке ревью Task 2. Первая редакция спеки опиралась на утверждение «cpal не даёт стабильных идентификаторов, имя — единственное, что есть». Оно ложно: в cpal 0.18.1 есть `DeviceTrait::id() -> Result<DeviceId, Error>`, на Windows это `IMMDevice::GetId()`, а докблок типа предписывает персистить его через `Display`/`FromStr` — ровно наш сценарий.

Матчинг по имени ломается, когда устройство переименовали в системе или когда два эндпоинта названы одинаково, и ломается тихо — откатом на дефолт. Это тот же класс отказа, ради которого писалась вся правка.

**Files:**
- Modify: `src/capture/mod.rs`

**Interfaces:**
- Заменяет: `DeviceChoice::Named(String)` → `DeviceChoice::Id(String)`
- Заменяет: `pick(names: &[String], …)` → `pick(ids: &[String], …)`
- Заменяет: `list_input_devices() -> Result<Vec<String>, _>` → `-> Result<Vec<InputDevice>, _>`
- Produces: `pub struct InputDevice { pub id: String, pub name: String }`

- [ ] **Step 1: Переписать тесты под идентификаторы**

Заменить шесть тестов, добавленных в Task 1, на эти. Идентификаторы взяты в формате реальных WASAPI-эндпоинтов, чтобы тест читался как настоящий случай, а не как абстрактная строка:

```rust
    fn идентификаторы() -> Vec<String> {
        vec![
            "{0.0.1.00000000}.{a1b2c3d4-0000-0000-0000-000000000001}".to_string(),
            "{0.0.1.00000000}.{a1b2c3d4-0000-0000-0000-000000000002}".to_string(),
            "{0.0.1.00000000}.{a1b2c3d4-0000-0000-0000-000000000003}".to_string(),
        ]
    }

    #[test]
    fn дефолт_не_выбирает_никого_из_списка() {
        assert_eq!(
            pick(&идентификаторы(), &DeviceChoice::Default),
            None,
            "Default означает «спросить систему», а не «взять первое из списка»"
        );
    }

    #[test]
    fn устройство_ищется_точным_совпадением_идентификатора() {
        assert_eq!(
            pick(&идентификаторы(), &DeviceChoice::Id(идентификаторы()[1].clone())),
            Some(1)
        );
    }

    /// Префикс — не совпадение. Эндпоинт-идентификаторы WASAPI различаются
    /// хвостом GUID, и нестрогий матчинг (`starts_with`/`contains`) выбрал бы
    /// первое попавшееся устройство того же контейнера — молча и не то.
    #[test]
    fn префикс_идентификатора_не_считается_совпадением() {
        assert_eq!(
            pick(&идентификаторы(), &DeviceChoice::Id("{0.0.1.00000000}".into())),
            None
        );
    }

    #[test]
    fn отсутствующее_устройство_даёт_none() {
        assert_eq!(
            pick(&идентификаторы(), &DeviceChoice::Id("{0.0.1.00000000}.{нет-такого}".into())),
            None
        );
    }

    /// Пустая строка приходит из `resolve_input`, когда у устройства не
    /// прочитался дескриптор. Она обязана не совпасть ни с чем, а не выбрать
    /// случайного соседа.
    #[test]
    fn пустой_идентификатор_ни_с_чем_не_совпадает() {
        assert_eq!(pick(&идентификаторы(), &DeviceChoice::Id(String::new())), None);
    }

    #[test]
    fn пустой_список_не_паникует() {
        assert_eq!(pick(&[], &DeviceChoice::Id("что-нибудь".into())), None);
        assert_eq!(pick(&[], &DeviceChoice::Default), None);
    }
```

- [ ] **Step 2: Прогнать тесты и убедиться, что они падают**

Run: `cd /mnt/c/Users/<username>/Projects/meeting-recorder && cargo.exe test -p meeting-recorder pick 2>&1 | tail -20`

Expected: ошибка компиляции — `no variant named 'Id' found for enum 'DeviceChoice'`.

- [ ] **Step 3: Переписать типы и разрешение устройства**

```rust
/// Какой микрофон брать. `Default` — тот, что выбран в системе.
///
/// Идентификатор, а не имя. `DeviceTrait::id()` на Windows отдаёт
/// `IMMDevice::GetId()` — эндпоинт-идентификатор WASAPI, стабильный между
/// перезапусками и переименованиями; докблок `DeviceId` прямо предписывает
/// персистить его через `Display`/`FromStr`. Имя же меняется в настройках
/// системы и не уникально, а промах по нему выглядит как тихий откат на
/// системный дефолт — тот самый отказ, который эта фича и устраняет.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DeviceChoice {
    #[default]
    Default,
    Id(String),
}

/// Устройство записи для выпадашки: чем искать и что показать.
///
/// Имя здесь — исключительно для глаз. Матчинг по нему не идёт нигде, иначе
/// хрупкость вернулась бы через чёрный ход.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDevice {
    pub id: String,
    pub name: String,
}

/// Индекс выбранного устройства в списке идентификаторов.
///
/// Сравнение строго по равенству: эндпоинт-идентификаторы разделяют префикс
/// контейнера, и нестрогий матчинг выбрал бы соседнее устройство.
fn pick(ids: &[String], choice: &DeviceChoice) -> Option<usize> {
    match choice {
        DeviceChoice::Default => None,
        DeviceChoice::Id(want) => ids.iter().position(|id| id == want),
    }
}
```

Чтение идентификатора и имени — одним хелпером, чтобы `list_input_devices` и `resolve_input` не разошлись:

```rust
/// Идентификатор устройства строкой. `None` — дескриптор не читается.
///
/// `DeviceId` персистится через `Display`, поэтому строка — это и есть его
/// каноническая форма, а не наше изобретение.
fn device_id(d: &cpal::Device) -> Option<String> {
    d.id().ok().map(|id| id.to_string())
}
```

`list_input_devices` отдаёт пары. Устройство, у которого не читается идентификатор ИЛИ имя, пропускается: выбрать его всё равно нельзя, а показать в списке — значит предложить то, что не запомнится.

```rust
pub fn list_input_devices() -> Result<Vec<InputDevice>, CaptureError> {
    let host = cpal::default_host();
    Ok(host
        .input_devices()?
        .filter_map(|d| {
            let id = device_id(&d)?;
            let name = d.description().ok()?.name().to_string();
            Some(InputDevice { id, name })
        })
        .collect())
}
```

`resolve_input` ищет по идентификаторам. Длины `ids` и `devices` обязаны совпадать, поэтому здесь `map` с `unwrap_or_default`, а не `filter_map`: нечитаемый идентификатор превращается в пустую строку, которая не совпадёт ни с чем (это сторожит тест `пустой_идентификатор_ни_с_чем_не_совпадает`).

```rust
pub fn resolve_input(choice: &DeviceChoice) -> Result<Resolved, CaptureError> {
    let host = cpal::default_host();
    if let DeviceChoice::Id(want) = choice {
        let devices: Vec<cpal::Device> = host.input_devices()?.collect();
        let ids: Vec<String> = devices
            .iter()
            .map(|d| device_id(d).unwrap_or_default())
            .collect();
        if let Some(i) = pick(&ids, choice) {
            return Ok(Resolved {
                device: devices.into_iter().nth(i).expect("индекс из pick валиден"),
                fell_back_from: None,
            });
        }
        let device = host
            .default_input_device()
            .ok_or(CaptureError::NoDevice(Source::Mic))?;
        return Ok(Resolved {
            device,
            fell_back_from: Some(want.clone()),
        });
    }
    let device = host
        .default_input_device()
        .ok_or(CaptureError::NoDevice(Source::Mic))?;
    Ok(Resolved {
        device,
        fell_back_from: None,
    })
}
```

Докблок `Resolved::fell_back_from` поправить: он несёт **идентификатор**, а человеческое имя подставляет GUI из конфига.

- [ ] **Step 4: Прогнать тесты**

Run: `cargo.exe test --workspace 2>&1 | tail -20`

Expected: `test result: ok`, шесть переписанных тестов зелёные, остальные не тронуты. Заглушка в `src/app.rs` (`DeviceChoice::Default`) продолжает компилироваться — вариант `Default` не менялся.

- [ ] **Step 5: Закоммитить**

```bash
git add src/capture/mod.rs
git commit -m "fix(capture): искать устройство по стабильному DeviceId, а не по имени

Имя ломается при переименовании устройства в системе и при двух одинаково
названных эндпоинтах, причём тихо — откатом на дефолт. cpal 0.18.1 отдаёт
IMMDevice::GetId() через DeviceTrait::id и предписывает персистить его."
```

---

### Task 3: App принимает выбор устройства и отдаёт предупреждение о подмене

**Files:**
- Modify: `src/app.rs` (трейт `AudioIo`, `CpalAudio`, `App`)
- Modify: `src/main.rs:35-37`

**Interfaces:**
- Consumes: `DeviceChoice`, `build_mic_capture` (Task 2)
- Produces:
  - `AudioIo::set_mic_device(&mut self, choice: DeviceChoice)` — дефолтная пустая реализация
  - `AudioIo::fell_back_from(&self) -> Option<String>` — дефолтный `None`
  - `App::new(root: PathBuf, mic: DeviceChoice) -> App`
  - `App::set_mic_device(&mut self, choice: DeviceChoice)`
  - `App::device_warning(&self) -> Option<String>`

- [ ] **Step 1: Расширить существующий фейк**

В `mod tests` файла `src/app.rs` уже есть `ФейкAudio` (строка ~898) и хелпер `стенд(журнал, поломка, звук)` (строка ~934). Второй фейк не заводить — добавить два поля в существующий:

```rust
    struct ФейкAudio {
        журнал: Журнал,
        открыт: bool,
        /// Что отдавать по каждому вызову drain, по порядку.
        очередь: Vec<(Vec<i16>, Vec<i16>)>,
        /// Куда фейк кладёт последний выбор устройства — тест смотрит сюда.
        выбор: Rc<RefCell<Option<DeviceChoice>>>,
        /// Что вернуть из `fell_back_from`: `Some` — притворяемся, что просили
        /// это устройство и не нашли.
        подмена: Option<String>,
    }
```

В `impl AudioIo for ФейкAudio` добавить две реализации:

```rust
        fn set_mic_device(&mut self, choice: DeviceChoice) {
            *self.выбор.borrow_mut() = Some(choice);
        }

        fn fell_back_from(&self) -> Option<String> {
            self.подмена.clone()
        }
```

В `стенд` дополнить конструктор новыми полями:

```rust
            Box::new(ФейкAudio {
                журнал: журнал.clone(),
                открыт: false,
                очередь: звук,
                выбор: Rc::new(RefCell::new(None)),
                подмена: None,
            }),
```

И добавить второй хелпер рядом со `стенд`:

```rust
    /// Стенд, у которого захват можно расспросить про устройство: возвращает
    /// App и общую ссылку на то, что фейку сказали выбрать.
    fn стенд_с_устройством(
        журнал: &Журнал,
        подмена: Option<&str>,
    ) -> (App, Rc<RefCell<Option<DeviceChoice>>>) {
        let выбор = Rc::new(RefCell::new(None));
        let app = App::with_backends(
            PathBuf::from("."),
            Box::new(ФейкAudio {
                журнал: журнал.clone(),
                открыт: false,
                очередь: Vec::new(),
                выбор: выбор.clone(),
                подмена: подмена.map(str::to_string),
            }),
            Box::new(ФейкSinks {
                журнал: журнал.clone(),
                поломка: Поломка::Нет,
            }),
        );
        (app, выбор)
    }
```

- [ ] **Step 2: Написать падающий тест**

```rust
    #[test]
    fn выбор_устройства_доезжает_до_захвата() {
        let ж = журнал();
        let (mut app, выбор) = стенд_с_устройством(&ж, None);
        app.set_mic_device(DeviceChoice::Id("{0.0.1.00000000}.{guid}".into()));
        assert_eq!(
            *выбор.borrow(),
            Some(DeviceChoice::Id("{0.0.1.00000000}.{guid}".into())),
            "App не хранит выбор сам — он обязан уехать в AudioIo"
        );
    }

    #[test]
    fn подмена_устройства_видна_снаружи() {
        let ж = журнал();
        let (app, _) = стенд_с_устройством(&ж, Some("Headset (Boss Bose)"));
        assert_eq!(app.device_warning().as_deref(), Some("Headset (Boss Bose)"));
    }

    #[test]
    fn без_подмены_предупреждения_нет() {
        let ж = журнал();
        let (app, _) = стенд_с_устройством(&ж, None);
        assert_eq!(app.device_warning(), None);
    }
```

- [ ] **Step 3: Прогнать тест и убедиться, что он падает**

Run: `cargo.exe test -p meeting-recorder устройств 2>&1 | tail -20`

Expected: ошибка компиляции — `no method named 'set_mic_device'` / `'device_warning'`.

- [ ] **Step 4: Написать реализацию**

В трейт `AudioIo` (`src/app.rs:64-79`) добавить два метода с дефолтами — так фейки в существующих тестах не придётся править:

```rust
    /// Сменить микрофон. Применяется к следующему `open()`: менять устройство
    /// под уже идущей записью значило бы порвать дорожку посередине.
    fn set_mic_device(&mut self, _choice: DeviceChoice) {}

    /// `Some(имя)` — при последнем `open()` просили это устройство, не нашли и
    /// взяли системный дефолт.
    fn fell_back_from(&self) -> Option<String> {
        None
    }
```

В `CpalAudio` — два поля и их заполнение:

```rust
struct CpalAudio {
    streams: Option<Streams>,
    mic: DeviceChoice,
    fell_back: Option<String>,
    timing: bool,
    opened_at: Option<Instant>,
    logged_mic: bool,
    logged_sys: bool,
}

impl CpalAudio {
    fn new(mic: DeviceChoice) -> Self {
        Self {
            streams: None,
            mic,
            fell_back: None,
            timing: std::env::var_os("MR_DEBUG_TIMING").is_some(),
            opened_at: None,
            logged_mic: false,
            logged_sys: false,
        }
    }
}
```

В `impl AudioIo for CpalAudio` — заменить строку из Task 2 Step 2 и добавить методы:

```rust
        let (mic_pending, fell_back) = build_mic_capture(&self.mic, tx_mic)?;
        self.fell_back = fell_back;
```

```rust
    fn set_mic_device(&mut self, choice: DeviceChoice) {
        self.mic = choice;
    }

    fn fell_back_from(&self) -> Option<String> {
        self.fell_back.clone()
    }
```

В `App`:

```rust
    /// Микрофон здесь НЕ открывается. Потоки поднимаются только по детекту,
    /// ручному старту или явной проверке — см. `Action::StartRingBuffer`.
    pub fn new(root: PathBuf, mic: DeviceChoice) -> Self {
        Self::with_backends(root, Box::new(CpalAudio::new(mic)), Box::new(WavSinks))
    }

    /// Сменить микрофон. Вступает в силу со следующего открытия потоков.
    pub fn set_mic_device(&mut self, choice: DeviceChoice) {
        self.audio.set_mic_device(choice);
    }

    /// `Some(имя)` — писали не тем микрофоном, о котором просили.
    pub fn device_warning(&self) -> Option<String> {
        self.audio.fell_back_from()
    }
```

Никаких `#[cfg(test)]`-геттеров в `App` не добавлять: `mod tests` лежит в том же файле, поэтому приватное поле `audio` тестам и так доступно напрямую — существующие тесты privacy-инвариантов уже пишут `app.audio.is_open()`.

- [ ] **Step 5: Обновить консольный бинарь**

`src/main.rs:35-37`:

```rust
    let root = PathBuf::from(r"C:\Users\<username>\Recordings");
    let det = WindowsDetector::new()?;
    // Консоль — отладочный инструмент ядра, конфига у неё нет: всегда системный
    // дефолт. Выбор устройства живёт в GUI, где его есть где хранить.
    let mut app = App::new(root, meeting_recorder::capture::DeviceChoice::Default);
```

- [ ] **Step 6: Обновить GUI-вызов, чтобы собиралось**

`src-tauri/src/audio.rs:205` — временно, конфиг появится в Task 4:

```rust
    let mut app = App::new(dir, meeting_recorder::capture::DeviceChoice::Default);
```

- [ ] **Step 7: Прогнать тесты**

Run: `cargo.exe test --workspace 2>&1 | tail -20`

Expected: `test result: ok`, все прежние тесты плюс три новых.

- [ ] **Step 8: Закоммитить**

```bash
git add src/app.rs src/main.rs src-tauri/src/audio.rs
git commit -m "feat(app): выбор микрофона доезжает до захвата, подмена видна снаружи"
```

---

### Task 4: Конфиг на диске и выпадашка устройств в окне

После этой задачи исходная проблема решена: запись идёт в выбранный микрофон, выбор переживает перезапуск.

**Files:**
- Create: `src-tauri/src/config.rs`
- Modify: `src-tauri/Cargo.toml` (добавить `serde_json`)
- Modify: `src-tauri/src/main.rs`, `src-tauri/src/audio.rs`
- Modify: `ui/index.html`, `ui/main.js`

**Interfaces:**
- Consumes: `App::set_mic_device`, `DeviceChoice` (Task 3), `list_input_devices` (Task 2)
- Produces:
  - `Config { pub mic_device_id: Option<String>, pub mic_device_name: Option<String> }`, `Config::load(&AppHandle) -> Config`, `Config::save(&self, &AppHandle) -> Result<(), String>`
  - Tauri-команды `list_mic_devices() -> Result<Vec<MicDevice>, String>` (где `MicDevice { id, name }`), `get_config() -> Config`, `set_mic_device(id: Option<String>, name: Option<String>) -> Result<(), String>`
  - `Ctl::SetMicDevice(DeviceChoice)`

- [ ] **Step 1: Добавить зависимость**

В `src-tauri/Cargo.toml`, в `[dependencies]`:

```toml
serde_json = "1"
```

- [ ] **Step 2: Написать падающий тест конфига**

Создать `src-tauri/src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn пустой_конфиг_это_системный_дефолт() {
        assert_eq!(Config::default().choice(), DeviceChoice::Default);
    }

    #[test]
    fn идентификатор_устройства_превращается_в_id() {
        let c = Config {
            mic_device_id: Some("{0.0.1.00000000}.{guid}".into()),
            mic_device_name: Some("Headset (Boss Bose)".into()),
        };
        assert_eq!(c.choice(), DeviceChoice::Id("{0.0.1.00000000}.{guid}".into()));
    }

    /// Имя — только для показа. Конфиг с одним именем и без идентификатора
    /// матчить нечем, и притворяться, что устройство выбрано, нельзя.
    #[test]
    fn одно_имя_без_идентификатора_это_дефолт() {
        let c = Config {
            mic_device_id: None,
            mic_device_name: Some("Headset (Boss Bose)".into()),
        };
        assert_eq!(c.choice(), DeviceChoice::Default);
    }

    /// Битый JSON — это потерянная настройка, а не потерянная запись.
    #[test]
    fn битый_json_даёт_дефолт_а_не_панику() {
        assert_eq!(Config::from_str("{ это не json"), Config::default());
    }

    #[test]
    fn незнакомые_поля_не_ломают_разбор() {
        let c = Config::from_str(r#"{"mic_device_id":"{id}","что_то_новое":42}"#);
        assert_eq!(c.mic_device_id.as_deref(), Some("{id}"));
        assert_eq!(c.mic_device_name, None, "отсутствующее поле — не ошибка");
    }
}
```

- [ ] **Step 3: Прогнать тест и убедиться, что он падает**

Run: `cargo.exe test -p meeting-recorder-gui config 2>&1 | tail -20`

Expected: ошибка компиляции — модуль не объявлен, `Config` не существует.

- [ ] **Step 4: Написать реализацию конфига**

В начало `src-tauri/src/config.rs`:

```rust
//! Настройки, переживающие перезапуск.
//!
//! Живут в GUI, а не в ядре, намеренно: `App` тестируется без файловой системы,
//! и чтение конфига внутри него отняло бы эту способность. Ядро принимает выбор
//! устройства как значение — где оно хранится, его не касается.

use meeting_recorder::capture::DeviceChoice;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Eq, Debug)]
#[serde(default)]
pub struct Config {
    /// Идентификатор эндпоинта (`DeviceId` строкой). `None` — системный дефолт.
    pub mic_device_id: Option<String>,
    /// Имя на момент выбора. Только для показа: подставляется в выпадашку и в
    /// предупреждение о подмене, чтобы пользователь видел «Headset (Boss Bose)»,
    /// а не `{0.0.1.00000000}.{guid}`. Матчинг по нему не идёт нигде.
    pub mic_device_name: Option<String>,
}

impl Config {
    pub fn choice(&self) -> DeviceChoice {
        match &self.mic_device_id {
            Some(id) => DeviceChoice::Id(id.clone()),
            None => DeviceChoice::Default,
        }
    }

    /// Разбор без единого способа упасть: битый или чужой JSON даёт дефолт.
    /// Потерять настройку неприятно, не записать встречу — хуже.
    pub fn from_str(s: &str) -> Self {
        serde_json::from_str(s).unwrap_or_default()
    }

    fn path(app: &AppHandle) -> Result<PathBuf, String> {
        let dir = app
            .path()
            .app_config_dir()
            .map_err(|e| format!("не найден каталог конфига: {e}"))?;
        Ok(dir.join("config.json"))
    }

    pub fn load(app: &AppHandle) -> Self {
        match Self::path(app).and_then(|p| std::fs::read_to_string(p).map_err(|e| e.to_string())) {
            Ok(s) => Self::from_str(&s),
            // Файла нет при первом запуске — это не ошибка.
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, app: &AppHandle) -> Result<(), String> {
        let path = Self::path(app)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("не удалось создать {}: {e}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, json).map_err(|e| format!("не удалось записать {}: {e}", path.display()))
    }
}
```

- [ ] **Step 5: Прогнать тесты**

Run: `cargo.exe test -p meeting-recorder-gui config 2>&1 | tail -20`

Expected: `test result: ok. 4 passed`.

- [ ] **Step 6: Подключить конфиг к аудио-потоку и командам**

`src-tauri/src/audio.rs` — расширить `Ctl` и обработать новую команду:

```rust
pub enum Ctl {
    Event(Event),
    Toggle,
    /// Сменить микрофон. Вступает в силу со следующего открытия потоков —
    /// менять устройство под идущей записью значило бы порвать дорожку.
    SetMicDevice(meeting_recorder::capture::DeviceChoice),
    Shutdown,
}
```

В `ctl_to_event` — новая ветка. Она не событие машины, поэтому обрабатывается до неё, в `drain_ctl`:

```rust
fn drain_ctl(
    app: &mut App,
    rx: &Receiver<Ctl>,
    active: Option<&MicSession>,
    mut feed: impl FnMut(&mut App, Event, Option<&MicSession>),
) -> bool {
    for c in rx.try_iter() {
        // Не событие машины: состояние от смены устройства не меняется.
        if let Ctl::SetMicDevice(choice) = c {
            app.set_mic_device(choice);
            continue;
        }
        let (e, quit) = ctl_to_event(&c, app.state());
        feed(app, e, if quit { None } else { active });
        if quit {
            return true;
        }
    }
    false
}
```

Сигнатура `run` получает выбор устройства при старте. Дополнить импорты в начале файла:

```rust
use meeting_recorder::capture::DeviceChoice;
```

```rust
pub fn run(handle: AppHandle, rx: Receiver<Ctl>, root: PathBuf, mic: DeviceChoice) {
    ...
    let mut app = App::new(root, mic);
```

Тогда вариант `Ctl::SetMicDevice` пишется коротко — `SetMicDevice(DeviceChoice)`, без полного пути.

- [ ] **Step 7: Добавить команды в main.rs**

`src-tauri/src/main.rs` — объявить модуль, добавить три команды и передать конфиг в поток:

```rust
mod config;

use config::Config;

/// Доступные микрофоны для выпадашки: идентификатор и что показать.
///
/// `InputDevice` уже `Serialize`? Нет — он в ядре, где serde не подключён.
/// Поэтому здесь своя DTO: тащить serde в ядро ради одной структуры значило бы
/// расширить его зависимости под нужду GUI.
#[derive(serde::Serialize)]
struct MicDevice {
    id: String,
    name: String,
}

#[tauri::command]
fn list_mic_devices() -> Result<Vec<MicDevice>, String> {
    Ok(meeting_recorder::capture::list_input_devices()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|d| MicDevice {
            id: d.id,
            name: d.name,
        })
        .collect())
}

#[tauri::command]
fn get_config(app: AppHandle) -> Config {
    Config::load(&app)
}

/// `id: None` — вернуться на системный дефолт.
///
/// Имя приходит вместе с идентификатором и сохраняется рядом: когда устройства
/// не окажется в системе, показать пользователю будет нечего, кроме него.
#[tauri::command]
fn set_mic_device(
    id: Option<String>,
    name: Option<String>,
    state: tauri::State<Cmd>,
    app: AppHandle,
) -> Result<(), String> {
    let cfg = Config {
        mic_device_id: id,
        mic_device_name: name,
    };
    cfg.save(&app)?;
    state
        .send(Ctl::SetMicDevice(cfg.choice()))
        .inspect_err(|_| status::fatal(&app, status::DEAD.to_string()))
}
```

В `invoke_handler` добавить `list_mic_devices, get_config, set_mic_device`.

В `setup` — прочитать конфиг до запуска потока:

```rust
            let mic = Config::load(&handle).choice();
            std::thread::spawn(move || audio::run(handle, rx, recordings_dir(), mic));
```

- [ ] **Step 8: Добавить выпадашку в окно**

`ui/index.html` — перед блоком `<div class="row">` с кнопками:

```html
    <h2>Микрофон</h2>
    <div class="row">
      <select id="mic"></select>
    </div>
```

и стиль рядом с `button`:

```css
      select {
        font: inherit;
        flex: 1;
        padding: 7px 8px;
        border-radius: 6px;
        border: 1px solid var(--line);
        background: transparent;
        color: var(--fg);
      }
```

`ui/main.js` — заполнение и сохранение:

```js
// Значение <option> — идентификатор эндпоинта, подпись — имя. Пользователь
// выбирает глазами по имени, приложение запоминает идентификатор: имя может
// поменяться в настройках Windows, идентификатор — нет.
async function обновить_устройства() {
  try {
    const [устройства, конфиг] = await Promise.all([
      invoke("list_mic_devices"),
      invoke("get_config"),
    ]);
    const sel = $("mic");
    sel.innerHTML = "";
    const дефолт = document.createElement("option");
    дефолт.value = "";
    дефолт.textContent = "Системный по умолчанию";
    sel.append(дефолт);
    for (const у of устройства) {
      const o = document.createElement("option");
      o.value = у.id;
      o.textContent = у.name;
      sel.append(o);
    }
    // Сохранённое устройство может отсутствовать прямо сейчас (гарнитура
    // выключена). Показываем его как выбранное всё равно — иначе выпадашка
    // молча «забыла» бы настройку, которая на самом деле цела. Подпись берём
    // из конфига: имени в системе сейчас нет, спросить его не у кого.
    const сохранён = конфиг.mic_device_id ?? "";
    if (сохранён && !устройства.some((у) => у.id === сохранён)) {
      const o = document.createElement("option");
      o.value = сохранён;
      o.textContent = `${конфиг.mic_device_name ?? сохранён} (сейчас недоступен)`;
      sel.append(o);
    }
    sel.value = сохранён;
  } catch (e) {
    показать_ошибку(String(e));
  }
}

$("mic").addEventListener("change", async (e) => {
  const выбран = e.target.selectedOptions[0];
  const id = e.target.value || null;
  try {
    await invoke("set_mic_device", {
      id,
      // Дефолт («Системный по умолчанию») — не устройство, имя ему не нужно.
      name: id ? выбран.textContent.replace(" (сейчас недоступен)", "") : null,
    });
    показать_ошибку("");
  } catch (err) {
    показать_ошибку(String(err));
  }
});
```

Вызвать `обновить_устройства()` в `старт()` рядом с `обновить_список()` и в обработчике `window.addEventListener("focus", ...)` — устройства подключают и отключают, пока окно закрыто.

- [ ] **Step 9: Собрать и проверить руками**

Run: `cargo.exe test --workspace 2>&1 | tail -5 && cargo.exe build --release -p meeting-recorder-gui 2>&1 | tail -5`

Expected: тесты зелёные, сборка успешна.

Проверить вручную: запустить `target\release\meeting-recorder-gui.exe`, выбрать в выпадашке `Headset (Boss Bose)`, записать 10 секунд речи, остановить, затем из WSL:

```bash
ffmpeg -hide_banner -i "/mnt/c/Users/<username>/Recordings/<файл>.mic.wav" -af volumedetect -f null /dev/null 2>&1 | grep mean_volume
```

Expected: mean_volume около −25…−35 дБ, а не −55…−60. Перезапустить приложение и убедиться, что выбор сохранился.

- [ ] **Step 10: Закоммитить**

```bash
git add src-tauri/Cargo.toml src-tauri/src/config.rs src-tauri/src/main.rs src-tauri/src/audio.rs ui/index.html ui/main.js
git commit -m "feat(gui): выбор микрофона в окне с сохранением между запусками"
```

---

### Task 5: Предупреждение о подмене устройства

**Files:**
- Modify: `src-tauri/src/status.rs`, `src-tauri/src/audio.rs`, `ui/index.html`, `ui/main.js`

**Interfaces:**
- Consumes: `App::device_warning` (Task 3)
- Produces: `Status::set_device_warning(&self, w: Option<&str>) -> bool`, поле `Snapshot::device_warning`, событие `device-warning`

- [ ] **Step 1: Написать падающий тест**

В `mod tests` файла `src-tauri/src/status.rs`:

```rust
    #[test]
    fn предупреждение_об_устройстве_сообщается_только_об_изменении() {
        let s = Status::default();
        assert!(s.set_device_warning(Some("Headset (Boss Bose)")));
        assert!(!s.set_device_warning(Some("Headset (Boss Bose)")));
        assert!(s.set_device_warning(None), "снятие — тоже изменение");
        assert_eq!(s.snapshot().device_warning, None);
    }

    /// Тот, кто открыл окно посреди записи, обязан увидеть предупреждение —
    /// ровно та же причина, по которой в снимке живёт fatal.
    #[test]
    fn предупреждение_видно_в_снимке() {
        let s = Status::default();
        s.set_device_warning(Some("Yeti"));
        assert_eq!(s.snapshot().device_warning.as_deref(), Some("Yeti"));
    }
```

- [ ] **Step 2: Прогнать тест и убедиться, что он падает**

Run: `cargo.exe test -p meeting-recorder-gui предупреждение 2>&1 | tail -20`

Expected: `no method named 'set_device_warning'`.

- [ ] **Step 3: Написать реализацию**

В `Snapshot`:

```rust
    /// `Some(имя)` — просили этот микрофон, не нашли, пишем в системный дефолт.
    /// Не фатально (запись идёт), но молчать нельзя.
    pub device_warning: Option<String>,
```

В `impl Status`:

```rust
    /// `true` — изменилось, есть о чём сообщать. Снятие предупреждения — тоже
    /// изменение: устройство могли вернуть, и висящий баннер врал бы.
    pub fn set_device_warning(&self, w: Option<&str>) -> bool {
        let mut g = self.lock();
        let new = w.map(str::to_string);
        if g.device_warning == new {
            return false;
        }
        g.device_warning = new;
        true
    }
```

В `src-tauri/src/audio.rs`, функция `sync` — добавить после блока с состоянием:

```rust
    // Наверх идёт ИДЕНТИФИКАТОР эндпоинта: аудио-поток знает только его.
    // Человеческое имя подставляет тот, у кого есть конфиг, — окно (для баннера)
    // и код ниже (для тоста, который до окна не доходит).
    let warn = app.device_warning();
    if status.set_device_warning(warn.as_deref()) {
        let _ = handle.emit("device-warning", warn.clone());
        if let Some(id) = warn {
            // Тост показывается и при закрытом окне, поэтому имя ему нужно
            // здесь; конфиг читается только в момент изменения, а не каждый тик.
            let имя = crate::config::Config::load(handle)
                .mic_device_name
                .unwrap_or(id);
            let _ = handle
                .notification()
                .builder()
                .title("Пишется не тот микрофон")
                .body(format!(
                    "«{имя}» недоступен — запись идёт с системного по умолчанию."
                ))
                .show();
        }
    }
```

- [ ] **Step 4: Показать предупреждение в окне**

`ui/index.html` — после блока `#ask`:

```html
    <div id="devwarn"></div>
```

и стиль:

```css
      #devwarn {
        display: none;
        border: 1px solid #d0761a;
        border-radius: 8px;
        padding: 10px 12px;
        margin-bottom: 12px;
        color: #d0761a;
      }
      #devwarn.on {
        display: block;
      }
```

`ui/main.js`:

```js
// Приходит идентификатор эндпоинта — показывать его пользователю бессмысленно.
// Имя берём из конфига: в системе устройства сейчас нет, спросить не у кого.
async function показать_предупреждение_устройства(id) {
  const el = $("devwarn");
  el.classList.toggle("on", Boolean(id));
  if (!id) {
    el.textContent = "";
    return;
  }
  let имя = id;
  try {
    const конфиг = await invoke("get_config");
    if (конфиг.mic_device_id === id && конфиг.mic_device_name) {
      имя = конфиг.mic_device_name;
    }
  } catch {
    // Не смогли прочитать конфиг — покажем идентификатор. Предупреждение
    // важнее его читаемости: молчать здесь нельзя.
  }
  el.textContent = `Микрофон «${имя}» недоступен — пишется системный по умолчанию.`;
}
```

Подписка в `старт()` — до запроса снимка, вместе с остальными:

```js
  await listen("device-warning", (e) => показать_предупреждение_устройства(e.payload));
```

и в применении снимка:

```js
    показать_предупреждение_устройства(снимок.device_warning);
```

- [ ] **Step 5: Прогнать тесты и собрать**

Run: `cargo.exe test --workspace 2>&1 | tail -5 && cargo.exe build --release -p meeting-recorder-gui 2>&1 | tail -5`

Expected: тесты зелёные, сборка успешна.

- [ ] **Step 6: Закоммитить**

```bash
git add src-tauri/src/status.rs src-tauri/src/audio.rs ui/index.html ui/main.js
git commit -m "feat(gui): громкое предупреждение, когда пишется не выбранный микрофон"
```

---

### Task 6: Месячные папки

**Files:**
- Modify: `src/storage.rs`, `src/app.rs`, `src/main.rs`, `src-tauri/src/main.rs`

**Interfaces:**
- Produces: `pub fn month_dir(root: &Path, started: DateTime<Local>) -> PathBuf`
- Меняет: `App` хранит `root`, а не `dir`; синки открываются в `month_dir(&self.root, self.started)`

- [ ] **Step 1: Написать падающий тест**

В `mod tests` файла `src/storage.rs`:

```rust
    #[test]
    fn месячная_папка_из_даты_записи() {
        assert_eq!(
            month_dir(Path::new(r"C:\Recordings"), момент()),
            Path::new(r"C:\Recordings").join("2026-07")
        );
    }

    /// Месяц берётся из времени НАЧАЛА записи. Встреча, начатая 31 июля в 23:50
    /// и законченная 1 августа, целиком лежит в июле — иначе пара дорожек
    /// разъехалась бы по разным папкам, если бы папку считали при закрытии.
    #[test]
    fn граница_месяца_берётся_по_началу_записи() {
        let конец_июля = chrono::Local
            .with_ymd_and_hms(2026, 7, 31, 23, 50, 0)
            .unwrap();
        assert_eq!(
            month_dir(Path::new(r"C:\R"), конец_июля),
            Path::new(r"C:\R").join("2026-07")
        );
    }

    #[test]
    fn однозначный_месяц_дополняется_нулём() {
        let январь = chrono::Local.with_ymd_and_hms(2027, 1, 5, 9, 0, 0).unwrap();
        assert_eq!(
            month_dir(Path::new(r"C:\R"), январь),
            Path::new(r"C:\R").join("2027-01")
        );
    }
```

- [ ] **Step 2: Прогнать тест и убедиться, что он падает**

Run: `cargo.exe test -p meeting-recorder месячн 2>&1 | tail -20`

Expected: `cannot find function 'month_dir'`.

- [ ] **Step 3: Написать реализацию**

В `src/storage.rs`:

```rust
/// Папка записи по времени её НАЧАЛА: `<root>/2026-07`.
///
/// Именно по началу, а не по моменту закрытия файла: встреча, начатая 31 июля
/// в 23:50, должна целиком лежать в июле. Считай мы папку при финализации,
/// дорожки такой встречи могли бы разъехаться по разным месяцам.
///
/// Гранулярность месяц, а не день: при 1-3 встречах в день это 30-60 файлов на
/// папку — читаемо, тогда как папка на каждый день дала бы сотни папок по паре
/// файлов.
pub fn month_dir(root: &Path, started: DateTime<Local>) -> PathBuf {
    root.join(started.format("%Y-%m").to_string())
}
```

- [ ] **Step 4: Прогнать тест**

Run: `cargo.exe test -p meeting-recorder месячн 2>&1 | tail -10`

Expected: `test result: ok. 3 passed`.

- [ ] **Step 5: Перевести App на корень**

`src/app.rs` — переименовать поле и считать каталог на месте использования:

```rust
pub struct App {
    machine: SessionMachine,
    /// Корень записей. Конкретная папка считается из `started` — см. `month_dir`.
    root: PathBuf,
    ...
}
```

`with_backends` — `root` вместо `dir`. В `open_sinks`:

```rust
    fn open_sinks(&mut self) -> Res {
        let src = self.current_source.clone();
        let dir = month_dir(&self.root, self.started);
        let (mic, sys) = free_name_pair(&dir, self.started, &src)?;
        // Порядок важен: если вторая дорожка не открылась, первую надо закрыть,
        // иначе на диске останется осиротевший mic-файл.
        let sink_mic = self.sinks.create(&dir, &mic)?;
        match self.sinks.create(&dir, &sys) {
            Ok(sink_sys) => {
                self.sink_mic = Some(sink_mic);
                self.sink_sys = Some(sink_sys);
                Ok(())
            }
            Err(e) => {
                let _ = sink_mic.finalize();
                let _ = std::fs::remove_file(dir.join(&mic));
                Err(e)
            }
        }
    }
```

Импорт в `src/app.rs:24` дополнить `month_dir`.

- [ ] **Step 6: Обновить оба бинаря**

`src-tauri/src/main.rs:23-25` — переименовать функцию, чтобы имя не врало:

```rust
/// Корень записей. Тот же, что у консольного бинаря (`src/main.rs`): разъедься
/// эти два пути, GUI перестал бы показывать записи, сделанные консолью, — а
/// консоль остаётся инструментом отладки того же ядра.
///
/// Конкретная запись ложится в месячную подпапку, см. `storage::month_dir`.
fn recordings_root() -> PathBuf {
    PathBuf::from(r"C:\Users\<username>\Recordings")
}
```

Заменить все вызовы `recordings_dir()` на `recordings_root()` (в `list_recordings`, `open_folder`, `setup`). В `src/main.rs` переменная уже названа `root` после Task 3.

- [ ] **Step 7: Прогнать тесты**

Run: `cargo.exe test --workspace 2>&1 | tail -10`

Expected: `test result: ok`. Существующие тесты `free_name_pair` работают на `ScratchDir` напрямую и не затронуты.

- [ ] **Step 8: Закоммитить**

```bash
git add src/storage.rs src/app.rs src/main.rs src-tauri/src/main.rs
git commit -m "feat(storage): записи ложатся в месячные папки"
```

---

### Task 7: Список записей из подпапок

**Files:**
- Modify: `src-tauri/src/main.rs`

**Interfaces:**
- Produces: `Recording { name, folder: Option<String>, mic, system, size }`; `group_recordings(files: impl IntoIterator<Item = (Option<String>, String, u64)>) -> Vec<Recording>`

- [ ] **Step 1: Написать падающий тест**

Заменить в `mod tests` файла `src-tauri/src/main.rs` хелперы и добавить тесты:

```rust
    fn rec(name: &str, folder: Option<&str>, mic: bool, system: bool, size: u64) -> Recording {
        Recording {
            name: name.to_string(),
            folder: folder.map(str::to_string),
            mic,
            system,
            size,
            imbalance_db: None,
        }
    }

    fn group(files: &[(Option<&str>, &str, u64)]) -> Vec<Recording> {
        group_recordings(
            files
                .iter()
                .map(|(f, n, s)| (f.map(str::to_string), n.to_string(), *s)),
        )
    }

    #[test]
    fn записи_из_подпапки_и_из_корня_живут_в_одном_списке() {
        let list = group(&[
            (Some("2026-07"), "2026-07-30_13-03_chrome.mic.wav", 10),
            (Some("2026-07"), "2026-07-30_13-03_chrome.system.wav", 10),
            (None, "2026-06-01_10-00_zoom.mic.wav", 5),
        ]);
        assert_eq!(
            list,
            vec![
                rec("2026-07-30_13-03_chrome", Some("2026-07"), true, true, 20),
                rec("2026-06-01_10-00_zoom", None, true, false, 5),
            ],
            "порядок хронологический независимо от папки"
        );
    }

    #[test]
    fn папка_записи_запоминается() {
        let list = group(&[(Some("2026-07"), "2026-07-30_13-03_chrome.mic.wav", 1)]);
        assert_eq!(list[0].folder.as_deref(), Some("2026-07"));
    }
```

Существующие тесты (`пара_дорожек_склеивается_в_одну_запись`, `одинокая_дорожка_видна_как_неполная`, `чужие_файлы_в_каталоге_не_наше_дело`, `свежее_сверху_независимо_от_порядка_обхода`, `повтор_в_ту_же_минуту_это_отдельная_запись`, `пустой_каталог_это_пустой_список_а_не_ошибка`) обновить под новую сигнатуру `group`: добавить `None` первым элементом кортежа и `None` в `rec`.

- [ ] **Step 2: Прогнать тест и убедиться, что он падает**

Run: `cargo.exe test -p meeting-recorder-gui group 2>&1 | tail -20`

Expected: ошибка компиляции — у `Recording` нет полей `folder` и `imbalance_db`.

- [ ] **Step 3: Написать реализацию**

```rust
/// Одна запись: пара дорожек под общим именем.
///
/// `Eq` из производных убран: появилось поле `f32`, на котором он не выводится.
/// `assert_eq!` в тестах работает и на одном `PartialEq`.
#[derive(Serialize, PartialEq, Debug)]
struct Recording {
    /// `2026-07-17_14-30_zoom` — общая основа обеих дорожек.
    name: String,
    /// Месячная папка или `None` для корня (записи до перехода на папки).
    folder: Option<String>,
    mic: bool,
    system: bool,
    /// Суммарный размер дорожек в байтах.
    size: u64,
    /// Насколько mic-дорожка тише system, в дБ. Заполняется в `list_recordings`
    /// после группировки — считать это здесь значило бы тащить в чистую
    /// функцию чтение файлов.
    #[serde(skip_serializing_if = "Option::is_none")]
    imbalance_db: Option<f32>,
}
```

Сама группировка:

> **Поправлено финальным ревью перед мержем.** Версия ниже была реализацией
> Task 7 и держалась на посылке «дата в основе однозначно задаёт месячную
> папку, поэтому одна запись не может лежать в двух папках сразу». Посылка
> оказалась ложной: прерванная миграция (`migrate-to-month-folders.sh`
> переносит ПОФАЙЛОВО и не откатывает уже перенесённые члены тройки на
> конфликте) и коллизия имён между корнем и месячной папкой (`free_name_pair`
> не видит файлы корня) — оба достижимы прямо сейчас. Ключ группировки стал
> `(base, folder)` вместо одной `base`; сортировка по-прежнему идёт по `base`
> (`folder` — только тайбрейк, он второй компонент кортежа). Финальный код —
> в `src-tauri/src/main.rs`, тест на регресс —
> `одна_основа_в_двух_папках_даёт_две_неполные_записи_а_не_одну_целую`.

```rust
fn group_recordings(
    files: impl IntoIterator<Item = (Option<String>, String, u64)>,
) -> Vec<Recording> {
    let mut found: BTreeMap<(String, Option<String>), Recording> = BTreeMap::new();
    for (folder, file, size) in files {
        let (base, is_mic) = match (file.strip_suffix(".mic.wav"), file.strip_suffix(".system.wav"))
        {
            (Some(b), _) => (b.to_string(), true),
            (_, Some(b)) => (b.to_string(), false),
            // Не наша дорожка — чужой файл в каталоге, не наше дело.
            _ => continue,
        };
        // Ключ — база И папка: одна и та же основа может лежать в двух
        // папках сразу (прерванная миграция, коллизия имён корень/месяц —
        // см. примечание выше). Половинки должны остаться видны как ДВЕ
        // неполные записи, а не молча слиться в одну.
        let key = (base.clone(), folder.clone());
        let rec = found.entry(key).or_insert(Recording {
            name: base,
            folder,
            mic: false,
            system: false,
            size: 0,
            imbalance_db: None,
        });
        if is_mic {
            rec.mic = true;
        } else {
            rec.system = true;
        }
        rec.size += size;
    }
    found.into_values().rev().collect()
}
```

Обход каталога:

```rust
/// Похоже ли имя папки на месячную (`2026-07`).
fn is_month_folder(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 7
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..].iter().all(u8::is_ascii_digit)
}

/// Файлы корня плюс файлы месячных подпапок. Глубина ровно два уровня:
/// предсказуемо и не засасывает чужое дерево, если рядом окажется постороннее.
fn collect_files(root: &Path) -> Result<Vec<(Option<String>, String, u64)>, String> {
    fn read(dir: &Path, folder: Option<&str>, out: &mut Vec<(Option<String>, String, u64)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                out.push((
                    folder.map(str::to_string),
                    e.file_name().to_string_lossy().into_owned(),
                    e.metadata().map(|m| m.len()).unwrap_or(0),
                ));
            }
        }
    }

    let mut out = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        // Каталога нет — записей просто ещё не было. Это не ошибка.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("не удалось прочитать {}: {e}", root.display())),
    };
    let mut months: Vec<PathBuf> = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        match e.file_type() {
            Ok(t) if t.is_dir() && is_month_folder(&name) => months.push(e.path()),
            Ok(t) if t.is_file() => out.push((
                None,
                name,
                e.metadata().map(|m| m.len()).unwrap_or(0),
            )),
            _ => {}
        }
    }
    for m in months {
        let folder = m.file_name().map(|n| n.to_string_lossy().into_owned());
        read(&m, folder.as_deref(), &mut out);
    }
    Ok(out)
}

#[tauri::command]
fn list_recordings() -> Result<Vec<Recording>, String> {
    Ok(group_recordings(collect_files(&recordings_root())?))
}
```

Добавить тест для `is_month_folder`:

```rust
    #[test]
    fn месячной_папкой_считается_только_yyyy_mm() {
        assert!(is_month_folder("2026-07"));
        assert!(!is_month_folder("2026-7"));
        assert!(!is_month_folder("2026-07-30"));
        assert!(!is_month_folder("архив"));
        assert!(!is_month_folder(""));
    }
```

- [ ] **Step 4: Показать папку в окне**

`ui/main.js`, в `обновить_список`, после строки с дорожками:

```js
      const части = [дорожки.join(" + "), размер(з.size)];
      if (з.folder) части.unshift(з.folder);
      мета.textContent = части.join(" · ");
```

- [ ] **Step 5: Прогнать тесты**

Run: `cargo.exe test --workspace 2>&1 | tail -10`

Expected: `test result: ok`.

- [ ] **Step 6: Закоммитить**

```bash
git add src-tauri/src/main.rs ui/main.js
git commit -m "feat(gui): список собирает записи из месячных папок и из корня"
```

---

### Task 8: Разовая миграция и обновление скилла

**Files:**
- Create: `scripts/migrate-to-month-folders.sh`
- Modify: `~/.claude/skills/meeting-to-vault/SKILL.md` (вне репозитория, коммитить отдельно там, где он лежит)

**Interfaces:** нет — скрипт запускается руками один раз.

- [ ] **Step 1: Написать скрипт**

Создать `scripts/migrate-to-month-folders.sh`:

```bash
#!/usr/bin/env bash
# Разовая раскладка записей из корня по месячным папкам.
#
# Переносит ГРУППОЙ по общей основе имени: обе дорожки и .transcript-папка
# обязаны уехать вместе, иначе транскрипт развяжется с записью.
#
# Дата берётся из начала имени (^\d{4}-\d{2}-\d{2}) — без требования времени,
# чтобы переехало и положенное руками (например .mp4 с демо). Всё, у чего даты
# в имени нет, остаётся в корне.
#
# Идемпотентен ПРИ УСПЕХЕ: повторный запуск по уже разложенному дереву ничего
# не делает — в корне пусто. Но если в целевой месячной папке уже лежит объект
# с тем же именем (например, после прерванного прошлого прогона) — перенос
# этого конкретного объекта считается КОНФЛИКТОМ: он остаётся в корне, это
# репортится громко (stderr + именованное сообщение), НЕ засчитывается в
# "перенесено" и ведёт к ненулевому коду возврата. Так тройка mic+system+
# .transcript никогда не считается перенесённой наполовину молча — по выводу
# видно, что часть осталась, и какая именно. Расколоть тройку молча хуже,
# чем не перенести вообще.
#
# Имена переменных ASCII (moved/skipped/conflicts/month), а не кириллицей:
# bash не поддерживает нелатинские идентификаторы ни в одной локали
# (проверено — `перенесено=0` падает как `command not found` даже под
# LC_ALL=ru_RU.UTF-8). Это ограничение самого bash, а не окружения. Текст
# сообщений — по-русски, как везде в проекте.
set -euo pipefail

ROOT="${1:-/mnt/c/Users/<username>/Recordings}"
DRY="${DRY_RUN:-0}"

cd "$ROOT"
moved=0
skipped=0
conflicts=0

for entry in *; do
  [ -e "$entry" ] || continue
  [ -d "$entry" ] && [[ "$entry" =~ ^[0-9]{4}-[0-9]{2}$ ]] && continue

  if [[ "$entry" =~ ^([0-9]{4})-([0-9]{2})-[0-9]{2} ]]; then
    month="${BASH_REMATCH[1]}-${BASH_REMATCH[2]}"

    if [ -e "$month/$entry" ]; then
      echo "  КОНФЛИКТ: $month/$entry уже существует — $entry остаётся в корне, НЕ перенесён" >&2
      conflicts=$((conflicts + 1))
      continue
    fi

    if [ "$DRY" = "1" ]; then
      echo "  $entry -> $month/"
    else
      mkdir -p "$month"
      mv -n -- "$entry" "$month/"
      if [ -e "$entry" ]; then
        # mv -n молча отказывается от переноса при коллизии имён и всё равно
        # выходит с кодом 0 — сюда попадаем, только если коллизия возникла
        # между проверкой выше и самим mv (гонка) либо иначе не сработала.
        echo "  КОНФЛИКТ: $entry не исчез из корня после mv — НЕ перенесён" >&2
        conflicts=$((conflicts + 1))
        continue
      fi
    fi
    moved=$((moved + 1))
  else
    echo "  оставлено в корне (нет даты в имени): $entry"
    skipped=$((skipped + 1))
  fi
done

echo "перенесено: $moved, оставлено: $skipped, конфликтов: $conflicts"

if [ "$conflicts" -gt 0 ]; then
  exit 1
fi
```

- [ ] **Step 2: Прогнать вхолостую и проверить глазами**

Run:
```bash
chmod +x /mnt/c/Users/<username>/Projects/meeting-recorder/scripts/migrate-to-month-folders.sh
DRY_RUN=1 /mnt/c/Users/<username>/Projects/meeting-recorder/scripts/migrate-to-month-folders.sh
```

Expected: список вида `2026-07-30_13-03_chrome.mic.wav -> 2026-07/`, включая `.transcript`-папки и `.mp4`. Ничего не перемещено. Убедиться, что пар дорожек, у которых одна половина уезжает, а вторая остаётся, в выводе нет.

- [ ] **Step 3: Прогнать по-настоящему**

Run: `/mnt/c/Users/<username>/Projects/meeting-recorder/scripts/migrate-to-month-folders.sh`

Expected: `перенесено: N, оставлено: 0, конфликтов: 0` и код возврата 0.

Ненулевой счётчик конфликтов означает, что часть записи осталась в корне: скрипт не перезаписывает существующие файлы и говорит об этом громко, но уже перенесённых членов группы назад не откатывает. В этом случае оставшееся в корне разбирается руками, а не повторным запуском.

Проверить: `ls /mnt/c/Users/<username>/Recordings/` — только папки `2026-07` и подобные; `ls /mnt/c/Users/<username>/Recordings/2026-07/ | head` — файлы и `.transcript`-папки на месте.

- [ ] **Step 4: Обновить скилл meeting-to-vault**

В `~/.claude/skills/meeting-to-vault/SKILL.md`, раздел «Входные данные» — заменить описание раскладки:

```markdown
## Входные данные

Записи в `C:\Users\<username>\Recordings` = `/mnt/c/Users/<username>/Recordings`, разложены
по месячным подпапкам `YYYY-MM/` (свежая встреча — в папке текущего месяца). Обычно
пара синхронных дорожек одной встречи: `<штамп>.mic.wav` (микрофон = Михаил Максимов)
и `<штамп>.system.wav` (остальные участники). Штамп — общий префикс имён целиком,
включая суффикс приложения (`2026-07-21_12-00_chrome`). Проверь ffprobe: одинаковый
штамп и длительность ±10 с = одна встреча.

Искать последнюю запись — по всему дереву, а не только в корне:

    ls -t /mnt/c/Users/<username>/Recordings/*/*.mic.wav | head
```

В разделе «Пайплайн», шаг 1 — заменить команду микса на версию с усилением:

```markdown
1. **Смикшируй дорожки в один файл** (в scratchpad, не в Recordings). Порядок
   обязателен: сначала замер, потом выбор варианта команды — не копируй микс
   не глядя на результат замера, `+16 дБ` на уже сбалансированной записи даёт
   клиппинг mic и забивает собеседника.

   Шаг 1a — замерь баланс обеих дорожек:
   ```bash
   ffmpeg -i <mic.wav> -af volumedetect -f null /dev/null    # смотри mean_volume
   ffmpeg -i <system.wav> -af volumedetect -f null /dev/null # смотри mean_volume
   ```

   Шаг 1b — по разнице mean_volume выбери ОДИН из двух вариантов микса:

   - Разница **больше ~10 дБ** (типично для записей, сделанных ДО появления
     выбора устройства: там mic на 22-37 дБ тише system, и faster-whisper по
     VAD выбрасывает реплики владельца как тишину — обрывки в транскрипте,
     отказ выглядит как успех) → подтяни mic на измеренную разницу:
     ```bash
     ffmpeg -y -v error -i <mic.wav> -i <system.wav> \
       -filter_complex "[0:a]volume=NdB[a0];[a0][1:a]amix=inputs=2:duration=longest:normalize=0[out]" \
       -map "[out]" -ac 1 -ar 16000 -c:a pcm_s16le <scratchpad>/<штамп>_meeting.mixed.wav
     ```
     где `N` — измеренная разница mean_volume в дБ (округли вверх), а не
     значение по умолчанию.

   - Разница **до ~10 дБ** (обычно записи после появления выбора устройства,
     дорожки уже сопоставимы) → микс без усиления:
     ```bash
     ffmpeg -y -v error -i <mic.wav> -i <system.wav> \
       -filter_complex "amix=inputs=2:duration=longest:normalize=0" \
       -ac 1 -ar 16000 -c:a pcm_s16le <scratchpad>/<штамп>_meeting.mixed.wav
     ```
```

В таблицу «Грабли» добавить строку:

```markdown
| В транскрипте реплики Миши обрывочны, у собеседника — связный текст | Не подтянут уровень mic. Проверь `volumedetect` по обеим дорожкам и перемикшируй с `volume=NdB` |
```

- [ ] **Step 5: Закоммитить**

```bash
cd /mnt/c/Users/<username>/Projects/meeting-recorder
git add scripts/migrate-to-month-folders.sh
git commit -m "chore(scripts): разовая раскладка записей по месячным папкам"
```

Скилл живёт вне репозитория (`~/.claude/skills/`) — если это git-репозиторий, закоммитить там отдельно; если нет, изменения просто остаются на диске.

---

### Task 9: Чистая логика переименования

**Files:**
- Modify: `src/storage.rs`

**Interfaces:**
- Produces:
  - `pub fn split_name(base: &str) -> Option<(String, String)>`
  - `pub fn rename_tail(base: &str, new_tail: &str) -> Option<String>`
  - `fn sanitize_tail(raw: &str) -> Option<String>` (внутренняя; `sanitize_source` становится обёрткой)

- [ ] **Step 1: Написать падающий тест**

В `mod tests` файла `src/storage.rs`:

```rust
    #[test]
    fn имя_разбирается_на_префикс_и_хвост() {
        assert_eq!(
            split_name("2026-07-30_13-03_chrome"),
            Some(("2026-07-30_13-03_".to_string(), "chrome".to_string()))
        );
    }

    #[test]
    fn суффикс_повтора_это_часть_хвоста() {
        assert_eq!(
            split_name("2026-07-30_13-03_chrome_2"),
            Some(("2026-07-30_13-03_".to_string(), "chrome_2".to_string()))
        );
    }

    #[test]
    fn чужое_имя_не_разбирается() {
        assert_eq!(split_name("заметки"), None);
        assert_eq!(split_name("2026-07-30_13-03_"), None, "пустой хвост");
        assert_eq!(split_name("2026-07-30 13-03_chrome"), None, "пробел вместо _");
        assert_eq!(split_name("20260730_1303_chrome"), None, "нет дефисов");
        assert_eq!(split_name(""), None);
    }

    /// Первые 17 символов префикса — ASCII, но хвост может быть кириллицей.
    /// Разбор обязан резать по границе символа, а не по байту.
    #[test]
    fn кириллический_хвост_разбирается_без_паники() {
        assert_eq!(
            split_name("2026-07-30_13-03_разговор"),
            Some(("2026-07-30_13-03_".to_string(), "разговор".to_string()))
        );
    }

    #[test]
    fn переименование_меняет_только_хвост() {
        assert_eq!(
            rename_tail("2026-07-30_13-03_chrome", "Разговор с Артемом"),
            Some("2026-07-30_13-03_разговор-с-артемом".to_string())
        );
    }

    #[test]
    fn суффикс_повтора_переименованием_стирается() {
        assert_eq!(
            rename_tail("2026-07-30_13-03_chrome_2", "демо"),
            Some("2026-07-30_13-03_демо".to_string()),
            "хвост заменяется целиком, вместе с номером повтора"
        );
    }

    /// Пустой хвост — ошибка, а не подстановка `unknown`. При автоименовании
    /// источник берёт машина и подставить туда нечего; здесь поле стёр человек
    /// и обязан это увидеть.
    #[test]
    fn пустой_новый_хвост_это_ошибка() {
        assert_eq!(rename_tail("2026-07-30_13-03_chrome", ""), None);
        assert_eq!(rename_tail("2026-07-30_13-03_chrome", "---"), None);
        assert_eq!(rename_tail("2026-07-30_13-03_chrome", "   "), None);
    }

    #[test]
    fn слишком_длинный_хвост_обрезается() {
        let длинный = "я".repeat(200);
        let out = rename_tail("2026-07-30_13-03_chrome", &длинный).expect("хвост непустой");
        let (_, хвост) = split_name(&out).expect("результат разбирается обратно");
        assert_eq!(хвост.chars().count(), MAX_SOURCE_LEN);
    }

    #[test]
    fn чужое_имя_не_переименовывается() {
        assert_eq!(rename_tail("заметки", "новое"), None);
    }
```

- [ ] **Step 2: Прогнать тест и убедиться, что он падает**

Run: `cargo.exe test -p meeting-recorder split_name 2>&1 | tail -20`

Expected: `cannot find function 'split_name'`.

- [ ] **Step 3: Написать реализацию**

В `src/storage.rs`, рядом с `sanitize_source`:

```rust
/// Ширина префикса `YYYY-MM-DD_HH-MM_` — 17 символов, все ASCII.
pub const PREFIX_LEN: usize = 17;

/// Форма префикса: `d` — цифра, остальное — литерал.
const PREFIX_SHAPE: &[u8] = b"dddd-dd-dd_dd-dd_";

fn prefix_ok(b: &[u8]) -> bool {
    b.len() >= PREFIX_LEN
        && PREFIX_SHAPE
            .iter()
            .zip(b)
            .all(|(shape, c)| match shape {
                b'd' => c.is_ascii_digit(),
                lit => c == lit,
            })
}

/// Разбирает основу имени на неизменяемый префикс и редактируемый хвост.
///
/// Префикс — несущая конструкция: по нему идёт сортировка списка, склейка пары
/// дорожек и выбор месячной папки. Поэтому он фиксированной ширины и проверяется
/// по форме, а не «до последнего подчёркивания» — иначе `zoom_2` разъехался бы
/// на префикс `..._zoom_` и хвост `2`.
///
/// `None` — имя не наше (чужой файл в каталоге) или хвост пуст.
pub fn split_name(base: &str) -> Option<(String, String)> {
    if !prefix_ok(base.as_bytes()) {
        return None;
    }
    // Резать по PREFIX_LEN безопасно: первые 17 байт проверены как ASCII,
    // значит граница символа здесь совпадает с границей байта. Хвост при этом
    // может быть каким угодно юникодом.
    let (prefix, tail) = base.split_at(PREFIX_LEN);
    if tail.is_empty() {
        return None;
    }
    Some((prefix.to_string(), tail.to_string()))
}

/// Новая основа имени с заменённым хвостом.
///
/// `None` — исходное имя не разбирается или новый хвост после очистки пуст.
pub fn rename_tail(base: &str, new_tail: &str) -> Option<String> {
    let (prefix, _) = split_name(base)?;
    let tail = sanitize_tail(new_tail)?;
    Some(format!("{prefix}{tail}"))
}
```

`sanitize_source` разделить на две функции, сохранив поведение для автоименования:

```rust
/// Очистка произвольной строки под хвост имени файла. `None` — после очистки
/// ничего не осталось.
///
/// Юникодные буквы сохраняются: имена процессов на кириллице (у «Яндекс.Телемост»)
/// реальны, и человек тоже вправе назвать запись по-русски.
fn sanitize_tail(raw: &str) -> Option<String> {
    let stem = raw.strip_suffix(".exe").unwrap_or(raw);
    let cleaned: String = stem
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    // схлопнуть повторы дефисов и обрезать по краям
    let mut out = String::with_capacity(cleaned.len());
    for c in cleaned.chars() {
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    let out = out.trim_matches('-');
    // обрезаем по границе символа, а не байта — хвост может быть кириллицей
    let truncated: String = out.chars().take(MAX_SOURCE_LEN).collect();
    let truncated = truncated.trim_end_matches('-');
    if truncated.is_empty() {
        None
    } else {
        Some(truncated.to_string())
    }
}

/// То же, но для автоименования: пустой результат заменяется на `unknown`,
/// чтобы источник в имени файла не терялся молча.
fn sanitize_source(source: &str) -> String {
    sanitize_tail(source).unwrap_or_else(|| "unknown".to_string())
}
```

- [ ] **Step 4: Прогнать тесты**

Run: `cargo.exe test -p meeting-recorder 2>&1 | tail -10`

Expected: `test result: ok` — новые тесты плюс все существующие тесты `sanitize_source` (`один_только_exe_даёт_unknown`, `источник_из_одних_дефисов_даёт_unknown`, `пустой_источник_даёт_unknown`) остаются зелёными.

- [ ] **Step 5: Закоммитить**

```bash
git add src/storage.rs
git commit -m "feat(storage): разбор имени на фиксированный префикс и редактируемый хвост"
```

---

### Task 10: Переименование записи на диске

**Files:**
- Create: `src-tauri/src/rename.rs`
- Modify: `src-tauri/src/main.rs`, `ui/index.html`, `ui/main.js`

**Interfaces:**
- Consumes: `rename_tail`, `split_name` (Task 9), `month_dir` (Task 6)
- Produces: `pub fn rename_recording(dir: &Path, base: &str, new_tail: &str) -> Result<String, String>`; Tauri-команда `rename_recording(folder: Option<String>, base: String, new_tail: String) -> Result<String, String>`

- [ ] **Step 1: Написать падающий тест**

Создать `src-tauri/src/rename.rs` с тестами:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Уникальный временный каталог без внешних зависимостей. Удаляется в Drop,
    /// даже если тест упал.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path =
                std::env::temp_dir().join(format!("mr-rename-{tag}-{pid}-{nanos}-{n}"));
            std::fs::create_dir_all(&path).expect("создать временный каталог");
            Self(path)
        }
    }

    impl std::ops::Deref for ScratchDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn файл(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"x").expect("создать файл");
    }

    #[test]
    fn переименовываются_обе_дорожки() {
        let dir = ScratchDir::new("pair");
        файл(&dir, "2026-07-30_13-03_chrome.mic.wav");
        файл(&dir, "2026-07-30_13-03_chrome.system.wav");

        let новое = rename_recording(&dir, "2026-07-30_13-03_chrome", "артем").expect("переименование");

        assert_eq!(новое, "2026-07-30_13-03_артем");
        assert!(dir.join("2026-07-30_13-03_артем.mic.wav").exists());
        assert!(dir.join("2026-07-30_13-03_артем.system.wav").exists());
        assert!(!dir.join("2026-07-30_13-03_chrome.mic.wav").exists());
    }

    /// Транскрипт кладётся рядом с записью внешним скриптом расшифровки. Оставить
    /// его со старым именем — значит развязать транскрипт и запись.
    #[test]
    fn папка_транскрипта_едет_вместе_с_записью() {
        let dir = ScratchDir::new("transcript");
        файл(&dir, "2026-07-30_13-03_chrome.mic.wav");
        файл(&dir, "2026-07-30_13-03_chrome.system.wav");
        std::fs::create_dir(dir.join("2026-07-30_13-03_chrome.transcript")).unwrap();
        файл(
            &dir.join("2026-07-30_13-03_chrome.transcript"),
            "выжимка.md",
        );

        rename_recording(&dir, "2026-07-30_13-03_chrome", "артем").expect("переименование");

        assert!(dir
            .join("2026-07-30_13-03_артем.transcript")
            .join("выжимка.md")
            .exists());
    }

    #[test]
    fn отсутствие_транскрипта_не_ломает_переименование() {
        let dir = ScratchDir::new("no-transcript");
        файл(&dir, "2026-07-30_13-03_chrome.mic.wav");
        assert!(rename_recording(&dir, "2026-07-30_13-03_chrome", "артем").is_ok());
        assert!(dir.join("2026-07-30_13-03_артем.mic.wav").exists());
    }

    /// Автоподбор `_2` уместен там, где имя выбирает машина. Здесь его выбрал
    /// человек, и подмена за спиной означала бы, что запись потом ищут не там.
    #[test]
    fn занятое_имя_это_ошибка_а_не_автоподбор() {
        let dir = ScratchDir::new("taken");
        файл(&dir, "2026-07-30_13-03_chrome.mic.wav");
        файл(&dir, "2026-07-30_13-03_артем.mic.wav");

        let err = rename_recording(&dir, "2026-07-30_13-03_chrome", "артем").unwrap_err();

        assert!(err.contains("занято"), "текст ошибки: {err}");
        assert!(
            dir.join("2026-07-30_13-03_chrome.mic.wav").exists(),
            "при отказе на диске ничего не меняется"
        );
        assert!(!dir.join("2026-07-30_13-03_артем.system.wav").exists());
    }

    #[test]
    fn пустой_хвост_отвергается_до_касания_диска() {
        let dir = ScratchDir::new("empty");
        файл(&dir, "2026-07-30_13-03_chrome.mic.wav");
        assert!(rename_recording(&dir, "2026-07-30_13-03_chrome", "  ").is_err());
        assert!(dir.join("2026-07-30_13-03_chrome.mic.wav").exists());
    }

    #[test]
    fn переименование_в_то_же_имя_это_не_ошибка() {
        let dir = ScratchDir::new("same");
        файл(&dir, "2026-07-30_13-03_chrome.mic.wav");
        assert_eq!(
            rename_recording(&dir, "2026-07-30_13-03_chrome", "chrome").expect("то же имя"),
            "2026-07-30_13-03_chrome"
        );
        assert!(dir.join("2026-07-30_13-03_chrome.mic.wav").exists());
    }
}
```

- [ ] **Step 2: Прогнать тест и убедиться, что он падает**

Run: `cargo.exe test -p meeting-recorder-gui rename 2>&1 | tail -20`

Expected: модуль не объявлен, `rename_recording` не существует.

- [ ] **Step 3: Написать реализацию**

В начало `src-tauri/src/rename.rs`:

```rust
//! Переименование готовой записи на диске.
//!
//! Живёт в GUI, а не в ядре: ядро отвечает за то, как запись СОЗДАЁТСЯ, а это
//! управление уже созданным. Чистая арифметика имён при этом в ядре
//! (`storage::rename_tail`) — она нужна обоим и тестируется без файлов.

use meeting_recorder::storage::rename_tail;
use std::path::{Path, PathBuf};

/// Что переезжает вместе с записью: обе дорожки и папка транскрипта.
const SUFFIXES: [&str; 3] = [".mic.wav", ".system.wav", ".transcript"];

/// Переименовывает запись целиком. Возвращает новую основу имени.
///
/// Сначала проверяются ВСЕ целевые имена, и только потом что-либо двигается:
/// иначе отказ на второй дорожке оставил бы половину записи переименованной, а
/// половину нет — состояние, из которого пользователь не выберется руками, не
/// зная правил склейки.
///
/// Переименование не меняет месячную папку: дата в префиксе неизменяема, а
/// именно она эту папку и задаёт.
pub fn rename_recording(dir: &Path, base: &str, new_tail: &str) -> Result<String, String> {
    let new_base = rename_tail(base, new_tail)
        .ok_or_else(|| format!("не удалось построить имя из «{new_tail}»: пустое или имя записи чужое"))?;
    if new_base == base {
        return Ok(new_base);
    }

    // Что существует и куда поедет.
    let pairs: Vec<(PathBuf, PathBuf)> = SUFFIXES
        .iter()
        .map(|s| (dir.join(format!("{base}{s}")), dir.join(format!("{new_base}{s}"))))
        .filter(|(from, _)| from.exists())
        .collect();

    if pairs.is_empty() {
        return Err(format!("запись «{base}» не найдена в {}", dir.display()));
    }
    for (_, to) in &pairs {
        if to.exists() {
            return Err(format!(
                "имя «{new_base}» занято: {} уже существует",
                to.display()
            ));
        }
    }

    let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (from, to) in &pairs {
        match std::fs::rename(from, to) {
            Ok(()) => done.push((from.clone(), to.clone())),
            Err(e) => {
                // Откат: вернуть уже переименованное. Ошибку самого отката
                // здесь МОЛЧА не проглатываем: если она есть, наверх должна
                // уйти не только первая причина, но и факт, что откат не
                // довёл дело до конца — иначе человек прочитает «не удалось
                // переименовать» и решит, что диск не тронут, хотя часть
                // дорожек могла остаться висеть под новым именем.
                let rollback_failures: Vec<String> = done
                    .iter()
                    .rev()
                    .filter_map(|(from_done, to_done)| {
                        std::fs::rename(to_done, from_done).err().map(|re| {
                            format!("{} → {}: {re}", to_done.display(), from_done.display())
                        })
                    })
                    .collect();
                let reason = format!("не удалось переименовать {}: {e}", from.display());
                return Err(if rollback_failures.is_empty() {
                    reason
                } else {
                    format!(
                        "{reason}; ОТКАТ НЕ УДАЛСЯ, на диске остался частично переименованный набор: {}",
                        rollback_failures.join("; ")
                    )
                });
            }
        }
    }
    Ok(new_base)
}
```

> Ревью Task 10 (находка 3): черновой вариант этого блока глушил ошибку
> самого отката (`let _ = std::fs::rename(...)`). Версия выше — исправленная;
> см. отчёт `task-10-report.md`, секция «Фикс по ревью».

Дополнительно к тестам Step 1 нужен тест, реально доводящий до ветки отката
(находка 2 того же ревью) — все шесть тестов из Step 1 падают на upfront-
проверке `to.exists()`, ни один не вызывает настоящий `fs::rename` дважды.
Добавить в `mod tests`:

```rust
    /// Ревью Task 10, находка 2: все прежние тесты падали на upfront-проверке
    /// `to.exists()`, ни один не доходил до настоящего `fs::rename`, значит
    /// ветку отката не проверял никто. Здесь первая дорожка (`.mic.wav`)
    /// переименовывается по-настоящему, а вторая (`.system.wav`) держится
    /// заблокированной хендлом без `FILE_SHARE_DELETE`: обычный
    /// `std::fs::File::open` эту дорожку не заблокировал бы — Rust на Windows
    /// включает `FILE_SHARE_DELETE` в шаринг по умолчанию именно для того,
    /// чтобы файл можно было переименовать/удалить, пока где-то открыт его
    /// хендл. Здесь шаринг сознательно урезан до одного лишь чтения, и
    /// `MoveFileExW` внутри `fs::rename` откажет с sharing violation уже
    /// ПОСЛЕ того, как первая дорожка успешно переехала — ровно та ветка,
    /// которую нужно было проверить.
    #[test]
    fn отказ_второй_дорожки_откатывает_первую() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x0000_0001;

        let dir = ScratchDir::new("rollback");
        файл(&dir, "2026-07-30_13-03_chrome.mic.wav");
        файл(&dir, "2026-07-30_13-03_chrome.system.wav");

        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ) // ни FILE_SHARE_WRITE, ни FILE_SHARE_DELETE
            .open(dir.join("2026-07-30_13-03_chrome.system.wav"))
            .expect("открыть system-дорожку с эксклюзивной (без delete) блокировкой");

        let err = rename_recording(&dir, "2026-07-30_13-03_chrome", "артем").unwrap_err();
        drop(lock); // снять блокировку сразу — дальше идёт только чтение файловой системы

        assert!(
            err.contains("не удалось переименовать"),
            "текст ошибки: {err}"
        );
        assert!(
            dir.join("2026-07-30_13-03_chrome.mic.wav").exists(),
            "первая дорожка обязана откатиться на старое имя"
        );
        assert!(
            !dir.join("2026-07-30_13-03_артем.mic.wav").exists(),
            "новое имя первой дорожки не должно остаться висеть после отката"
        );
        assert!(
            dir.join("2026-07-30_13-03_chrome.system.wav").exists(),
            "вторая дорожка так и не переехала — она и была заблокирована"
        );
    }
```

- [ ] **Step 4: Прогнать тесты**

Run: `cargo.exe test -p meeting-recorder-gui rename 2>&1 | tail -10`

Expected: `test result: ok. 7 passed` (шесть исходных плюс тест отката).

- [ ] **Step 5: Добавить команду**

`src-tauri/src/main.rs`:

```rust
mod rename;

/// Переименовать запись. `folder` — месячная папка или `None` для корня.
#[tauri::command]
fn rename_recording(
    folder: Option<String>,
    base: String,
    new_tail: String,
) -> Result<String, String> {
    let dir = match folder {
        Some(f) => recordings_root().join(f),
        None => recordings_root(),
    };
    rename::rename_recording(&dir, &base, &new_tail)
}
```

Добавить `rename_recording` в `invoke_handler`.

- [ ] **Step 6: Добавить переименование в окно**

`ui/index.html` — стиль для кнопки в строке списка:

```css
      li {
        padding: 8px 0;
        border-bottom: 1px solid var(--line);
        display: flex;
        align-items: center;
        gap: 8px;
      }
      li > .grow {
        flex: 1;
        min-width: 0;
      }
      .rename {
        padding: 4px 8px;
        font-size: 12px;
      }
      .tail-input {
        font: inherit;
        width: 100%;
        padding: 4px 6px;
        border-radius: 4px;
        border: 1px solid var(--accent);
        background: transparent;
        color: var(--fg);
      }
```

`ui/main.js` — в `обновить_список`, вместо `li.append(имя, мета)`:

```js
      const колонка = document.createElement("div");
      колонка.className = "grow";
      колонка.append(имя, мета);

      const кнопка = document.createElement("button");
      кнопка.className = "rename";
      кнопка.textContent = "Переименовать";
      кнопка.addEventListener("click", () => начать_переименование(з, имя, кнопка));

      li.append(колонка, кнопка);
```

и новая функция:

```js
// Префикс с датой не редактируется: по нему идёт сортировка списка, склейка
// пары дорожек и выбор месячной папки. Поле правит только хвост.
const ДЛИНА_ПРЕФИКСА = 17;

function начать_переименование(запись, узел_имени, кнопка) {
  const префикс = запись.name.slice(0, ДЛИНА_ПРЕФИКСА);
  const хвост = запись.name.slice(ДЛИНА_ПРЕФИКСА);
  узел_имени.textContent = префикс;
  const поле = document.createElement("input");
  поле.className = "tail-input";
  поле.value = хвост;
  узел_имени.append(поле);
  поле.focus();
  поле.select();
  кнопка.disabled = true;

  // обновить_список() ниже перерисовывает <ul> целиком — list.innerHTML = ""
  // синхронно выбрасывает из документа ещё сфокусированное поле, а удаление
  // сфокусированного элемента само порождает событие blur. Без флага и без
  // снятия слушателя это било дважды: Escape вместо отмены применял текущее
  // (не отменённое) значение поля через blur-обработчик, а обычное
  // применение через Enter задваивалось тем же blur'ом секундой позже.
  // Флаг закрывает вход и в третьем случае — быстрый повторный Enter или
  // Tab/клик до ответа сервера тоже не даёт второй invoke.
  let завершено = false;

  const отписаться = () => поле.removeEventListener("blur", на_потерю_фокуса);

  const применить = async () => {
    if (завершено) return;
    завершено = true;
    отписаться();
    поле.disabled = true;
    try {
      await invoke("rename_recording", {
        folder: запись.folder ?? null,
        base: запись.name,
        newTail: поле.value,
      });
      показать_ошибку("");
    } catch (e) {
      показать_ошибку(String(e));
    }
    обновить_список();
  };

  const отменить = () => {
    if (завершено) return;
    завершено = true;
    отписаться();
    обновить_список();
  };

  const на_потерю_фокуса = () => применить();

  поле.addEventListener("keydown", (e) => {
    if (e.key === "Enter") применить();
    if (e.key === "Escape") отменить();
  });
  поле.addEventListener("blur", на_потерю_фокуса);
}
```

> Ревью Task 10 (находки 1 и 4): черновой вариант звал `применить` напрямую
> из `blur` и вызывал `обновить_список()` из ветки Escape без снятия этого
> слушателя. Поскольку `обновить_список()` удаляет ещё сфокусированное поле
> из DOM, а удаление сфокусированного элемента само рождает `blur`, Escape на
> самом деле ПРИМЕНЯЛ правку вместо отмены (Critical), а обычное применение
> через Enter могло задвоиться тем же механизмом (Important). Версия выше —
> исправленная; см. отчёт `task-10-report.md`, секция «Фикс по ревью».

- [ ] **Step 7: Собрать и проверить руками**

Run: `cargo.exe test --workspace 2>&1 | tail -5 && cargo.exe build --release -p meeting-recorder-gui 2>&1 | tail -5`

Проверить вручную: запустить GUI, переименовать запись в «тест переименования», убедиться в проводнике, что обе дорожки и `.transcript` (если есть) переименованы; попробовать переименовать вторую запись в то же имя — должна появиться ошибка «имя занято», файлы не тронуты.

- [ ] **Step 8: Закоммитить**

```bash
git add src-tauri/src/rename.rs src-tauri/src/main.rs ui/index.html ui/main.js
git commit -m "feat(gui): переименование записи вместе с обеими дорожками и транскриптом"
```

---

### Task 11: Пометка «микрофон тише системы»

**Files:**
- Create: `src-tauri/src/imbalance.rs`
- Modify: `src-tauri/Cargo.toml` (добавить `hound`), `src-tauri/src/main.rs`, `ui/main.js`

**Interfaces:**
- Produces:
  - `pub const FLOOR_DBFS: f32 = -120.0`, `pub const THRESHOLD_DB: f32 = 20.0`
  - `pub fn sampled_rms_dbfs(path: &Path, windows: usize) -> Result<f32, hound::Error>`
  - `pub fn imbalance(mic_dbfs: f32, sys_dbfs: f32) -> Option<f32>`
  - `pub struct Cache` с методом `pub fn rms(&self, path: &Path) -> Option<f32>`

Спека писала сигнатуру через `io::Result`; берём `hound::Error`, потому что `WavReader` возвращает именно его, а он уже включает `io::Error` вариантом.

- [ ] **Step 1: Добавить зависимость**

В `src-tauri/Cargo.toml`, в `[dependencies]`:

```toml
hound = "3.5"
```

- [ ] **Step 2: Написать падающий тест**

Создать `src-tauri/src/imbalance.rs` с тестами:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ровные_дорожки_не_дают_пометки() {
        assert_eq!(imbalance(-24.0, -24.0), None);
        assert_eq!(imbalance(-30.0, -24.0), None, "6 дБ — обычная разница");
        assert_eq!(imbalance(-43.9, -24.0), None, "19.9 дБ — ещё под порогом");
    }

    #[test]
    fn микрофон_тише_на_порог_даёт_пометку() {
        let diff = imbalance(-60.0, -24.0).expect("36 дБ — это пометка");
        assert!((diff - 36.0).abs() < 0.01, "разница: {diff}");
    }

    /// Ровно порог не считается: сравнение строгое, чтобы граничное значение
    /// не мигало пометкой туда-сюда между пересчётами.
    #[test]
    fn ровно_порог_не_считается() {
        assert_eq!(imbalance(-44.0, -24.0), None);
    }

    /// Обе дорожки в тишине — сравнивать нечего, предупреждать не о чем.
    #[test]
    fn тишина_в_обеих_дорожках_не_даёт_ложной_пометки() {
        assert_eq!(imbalance(FLOOR_DBFS, FLOOR_DBFS), None);
        assert_eq!(imbalance(-118.0, -119.0), None);
    }

    /// Полностью молчащий микрофон при говорящей системе — худший случай,
    /// и он обязан ловиться, а не отбрасываться как «нечего сравнивать».
    #[test]
    fn молчащий_микрофон_при_говорящей_системе_ловится() {
        assert!(imbalance(FLOOR_DBFS, -24.0).is_some());
    }

    /// Микрофон громче системы — не наша проблема: пометка только про «тише».
    #[test]
    fn микрофон_громче_системы_не_помечается() {
        assert_eq!(imbalance(-20.0, FLOOR_DBFS), None);
        assert_eq!(imbalance(-10.0, -60.0), None);
    }

    #[test]
    fn rms_синуса_половинной_амплитуды_около_минус_девяти_дб() {
        let dir = std::env::temp_dir().join(format!("mr-imb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sine.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..16_000 {
            let v = (i as f32 / 16_000.0 * 440.0 * std::f32::consts::TAU).sin() * 0.5;
            w.write_sample((v * i16::MAX as f32) as i16).unwrap();
        }
        w.finalize().unwrap();

        // RMS синуса = амплитуда / sqrt(2) = 0.3536 → 20*log10(0.3536) ≈ -9.03 дБ
        let db = sampled_rms_dbfs(&path, 200).unwrap();
        assert!((db + 9.03).abs() < 0.5, "получили {db} дБ");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn файл_короче_выборки_читается_целиком_а_не_падает() {
        let dir = std::env::temp_dir().join(format!("mr-imb-short-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("short.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..100 {
            w.write_sample(i16::MAX / 2).unwrap();
        }
        w.finalize().unwrap();

        let db = sampled_rms_dbfs(&path, 200).unwrap();
        assert!((db + 6.02).abs() < 0.5, "получили {db} дБ");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn пустой_файл_даёт_пол_а_не_ошибку() {
        let dir = std::env::temp_dir().join(format!("mr-imb-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        hound::WavWriter::create(&path, spec).unwrap().finalize().unwrap();

        assert_eq!(sampled_rms_dbfs(&path, 200).unwrap(), FLOOR_DBFS);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
```

> Правка по итогам ревью Task 11 (2026-07-30, Important): исходный набор из
> девяти тестов ни разу не заходил в ветку `seek`/`windows > 1` — оба
> WAV-теста укладываются в порог полного чтения (`total <= windows * WINDOW`).
> Добавлен десятый тест `разреженная_выборка_с_seek_совпадает_с_полным_чтением`
> — файл длиннее `windows * WINDOW` (200 × 4096 = 819 200 сэмплов), первые 10%
> тишина, остальные 90% синус известной амплитуды; страйд подобран так, что
> пропорция окон (20 тишина / 180 синус) точно повторяет пропорцию файла,
> поэтому разреженная выборка обязана дать почти тот же RMS, что и полное
> чтение. Проверено, что тест краснеет при сломанном `seek` (см.
> `task-11-report.md`, секция «Фикс по ревью»). Итоговый счёт для Step 5 —
> **10 passed**, а не 9.

- [ ] **Step 3: Прогнать тест и убедиться, что он падает**

Run: `cargo.exe test -p meeting-recorder-gui imbalance 2>&1 | tail -20`

Expected: модуль не объявлен, функции не существуют.

- [ ] **Step 4: Написать реализацию**

В начало `src-tauri/src/imbalance.rs`:

```rust
//! Сравнение громкости дорожек готовой записи.
//!
//! Существует потому, что выбор устройства не доказывает, что устройство слышит
//! владельца. Отказ здесь маскируется под успех: файл есть, весит сколько
//! положено, открывается и играет — просто голоса в нём почти нет, и обнаружится
//! это только на транскрипте, через день.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// Пол уровня. Всё, что тише, считается этим значением.
///
/// Пол, а не `-inf`, чтобы арифметика сравнения оставалась обычной: с
/// бесконечностями пришлось бы отдельно разбирать случай «мик молчал совсем»,
/// который как раз и есть самый важный.
pub const FLOOR_DBFS: f32 = -120.0;

/// Насколько mic должна быть тише system, чтобы это считалось проблемой.
///
/// 20 дБ выбраны между нормальной разницей близких уровней (до ~10 дБ) и
/// реальным отказом: у записи 30.07 расхождение по RMS было 36 дБ. По пикам оно
/// составляло всего 16 дБ — поэтому здесь именно RMS, пиковый порог этот случай
/// не поймал бы.
pub const THRESHOLD_DB: f32 = 20.0;

/// Сколько сэмплов в одном окне выборки.
const WINDOW: usize = 4096;

/// Дочитать до `take` сэмплов из текущей позиции `reader`, накопив их в
/// `sum_sq`/`counted`.
///
/// Свободная функция, а не замыкание внутри [`sampled_rms_dbfs`]: она не
/// захватывает ничего снаружи (всё приходит параметрами), поэтому
/// `let mut accumulate = |...| { ... }` компилятор справедливо помечал
/// `unused_mut` — мутируемым должно быть то, что меняется через `&mut`
/// параметры, а не сам биндинг. Вынос убирает предупреждение и заодно делает
/// вызывающий код короче на сигнатуру замыкания.
///
/// > Правка по итогам ревью Task 11 (2026-07-30, Important): исходная версия
/// > объявляла эту логику как `let mut accumulate = |...|` внутри
/// > `sampled_rms_dbfs` — единственная задача в плане, сданная с
/// > `unused_mut`-предупреждением. См. `task-11-report.md`, секция «Фикс по
/// > ревью».
fn accumulate(
    reader: &mut hound::WavReader<std::io::BufReader<std::fs::File>>,
    take: usize,
    sum_sq: &mut f64,
    counted: &mut usize,
) -> Result<(), hound::Error> {
    for s in reader.samples::<i16>().take(take) {
        let v = s? as f64;
        *sum_sq += v * v;
        *counted += 1;
    }
    Ok(())
}

/// RMS файла в dBFS по разреженной выборке.
///
/// Читается `windows` окон по [`WINDOW`] сэмплов, равномерно по файлу: для
/// 38-минутной записи это ~1.6 МБ вместо 73 МБ. Файл короче суммарной выборки
/// читается целиком — выборка вырождается в полный проход, а не в ошибку.
pub fn sampled_rms_dbfs(path: &Path, windows: usize) -> Result<f32, hound::Error> {
    let mut reader = hound::WavReader::open(path)?;
    let total = reader.len() as usize;
    if total == 0 {
        return Ok(FLOOR_DBFS);
    }

    let mut sum_sq = 0f64;
    let mut counted = 0usize;

    if windows == 0 || total <= windows * WINDOW {
        accumulate(&mut reader, total, &mut sum_sq, &mut counted)?;
    } else {
        let stride = total / windows;
        for i in 0..windows {
            reader.seek((i * stride) as u32)?;
            accumulate(&mut reader, WINDOW, &mut sum_sq, &mut counted)?;
        }
    }

    if counted == 0 {
        return Ok(FLOOR_DBFS);
    }
    let rms = (sum_sq / counted as f64).sqrt() / i16::MAX as f64;
    if rms <= 0.0 {
        return Ok(FLOOR_DBFS);
    }
    Ok((20.0 * rms.log10()).max(FLOOR_DBFS as f64) as f32)
}

/// Насколько mic тише system, если это тянет на проблему.
///
/// `None` — либо разница в пределах нормы, либо mic громче. Обратный перекос не
/// помечается: система тише микрофона — это просто тихий собеседник, а не
/// сломанная запись.
pub fn imbalance(mic_dbfs: f32, sys_dbfs: f32) -> Option<f32> {
    let diff = sys_dbfs - mic_dbfs;
    (diff > THRESHOLD_DB).then_some(diff)
}

/// Кеш посчитанных уровней.
///
/// Ключ включает размер и время модификации, поэтому заменённый или дописанный
/// файл пересчитывается сам. Без кеша список, обновляющийся на каждый фокус
/// окна, перечитывал бы диск заново.
#[derive(Default)]
pub struct Cache(Mutex<HashMap<(PathBuf, u64, SystemTime), f32>>);

impl Cache {
    /// `None` — файла нет или он не читается как WAV. Это не ошибка: пометки
    /// просто не будет.
    pub fn rms(&self, path: &Path) -> Option<f32> {
        let meta = std::fs::metadata(path).ok()?;
        let key = (path.to_path_buf(), meta.len(), meta.modified().ok()?);
        if let Ok(g) = self.0.lock() {
            if let Some(v) = g.get(&key) {
                return Some(*v);
            }
        }
        let v = sampled_rms_dbfs(path, 200).ok()?;
        if let Ok(mut g) = self.0.lock() {
            g.insert(key, v);
        }
        Some(v)
    }
}
```

- [ ] **Step 5: Прогнать тесты**

Run: `cargo.exe test -p meeting-recorder-gui imbalance 2>&1 | tail -10`

Expected: `test result: ok. 10 passed` (после правки по ревью — было 9, см. сноску у Step 2).

- [ ] **Step 6: Заполнить поле в списке**

`src-tauri/src/main.rs`:

```rust
mod imbalance;

use imbalance::Cache;
```

Зарегистрировать кеш в `main()` рядом с `Status`:

```rust
        .manage(Cache::default())
```

и заполнять поле после группировки:

> **Поправлено финальным ревью перед мержем.** `list_recordings` осталась
> единственной командой без докблока. Финальный код несёт докблок,
> объясняющий, почему обход/склейка и разметка дисбаланса разделены
> по-разному (обход и склейка — чистые функции ради теста без диска, разметка
> дисбаланса собрана прямо в команде, потому что читает файлы) — см.
> `src-tauri/src/main.rs`.

```rust
#[tauri::command]
fn list_recordings(cache: tauri::State<Cache>) -> Result<Vec<Recording>, String> {
    let root = recordings_root();
    let mut list = group_recordings(collect_files(&root)?);
    for r in &mut list {
        // Пометка имеет смысл только для полной пары: одинокая дорожка уже
        // помечена как неполная, и второе предупреждение о ней ничего не добавит.
        if !(r.mic && r.system) {
            continue;
        }
        let dir = match &r.folder {
            Some(f) => root.join(f),
            None => root.clone(),
        };
        let mic = cache.rms(&dir.join(format!("{}.mic.wav", r.name)));
        let sys = cache.rms(&dir.join(format!("{}.system.wav", r.name)));
        if let (Some(m), Some(s)) = (mic, sys) {
            r.imbalance_db = imbalance::imbalance(m, s);
        }
    }
    Ok(list)
}
```

- [ ] **Step 7: Показать пометку в окне**

`ui/main.js`, в `обновить_список`, после блока с `warn`:

```js
      if (з.imbalance_db != null) {
        мета.classList.add("warn");
        мета.textContent += ` · микрофон тише системы на ${Math.round(з.imbalance_db)} дБ`;
      }
```

- [ ] **Step 8: Собрать и проверить на реальных записях**

Run: `cargo.exe test --workspace 2>&1 | tail -5 && cargo.exe build --release -p meeting-recorder-gui 2>&1 | tail -5`

Проверить вручную: запустить GUI. Записи июля (до правки) должны получить пометку про 20-40 дБ; запись, сделанная после Task 4 с выбранной гарнитурой, — не должна.

- [ ] **Step 9: Закоммитить**

```bash
git add src-tauri/Cargo.toml src-tauri/src/imbalance.rs src-tauri/src/main.rs ui/main.js
git commit -m "feat(gui): пометка записей, где микрофон тише системной дорожки"
```

---

### Task 12: Уровень и режим проверки в ядре

**Files:**
- Modify: `src/app.rs`

**Interfaces:**
- Produces:
  - `pub struct Levels { pub mic: f32, pub system: f32 }`
  - `App::levels(&self) -> Levels`
  - `App::set_monitor(&mut self, on: bool) -> Res`
  - `App::is_monitoring(&self) -> bool`

- [ ] **Step 1: Написать падающий тест**

В `mod tests` файла `src/app.rs`:

Новых фейков не заводить: `стенд(журнал, поломка, звук)` уже принимает очередь сэмплов, а `ФейкAudio` отдаёт её по одному элементу на `drain`. Доступ к приватным полям (`app.audio`, `app.ring_mic`) у тестов есть — они лежат в том же файле.

```rust
    /// Инвариант микрофона: в Idle потоки открыты, только если явно попросили
    /// их послушать.
    #[test]
    fn проверка_открывает_и_закрывает_микрофон_в_idle() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::Нет, Vec::new());
        assert!(!app.audio.is_open(), "в Idle микрофон отпущен");
        app.set_monitor(true).expect("включение проверки");
        assert!(app.audio.is_open());
        app.set_monitor(false).expect("выключение проверки");
        assert!(!app.audio.is_open());
    }

    /// Если за время проверки пришёл детект, потоки уже принадлежат записи.
    /// Погасить их по выключению проверки значило бы оборвать встречу.
    #[test]
    fn выключение_проверки_не_гасит_идущую_запись() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::Нет, Vec::new());
        app.set_monitor(true).expect("включение проверки");
        app.on_event(Event::SessionAppeared, Some(&сессия(42, "zoom.exe")))
            .expect("детект");
        app.set_monitor(false).expect("выключение проверки");
        assert!(
            app.audio.is_open(),
            "машина в Armed — микрофон обязан остаться открытым"
        );
    }

    #[test]
    fn уровень_растёт_от_громких_сэмплов_и_затухает() {
        let ж = журнал();
        let mut app = стенд(
            &ж,
            Поломка::Нет,
            vec![(vec![i16::MAX / 2], Vec::new()), (Vec::new(), Vec::new())],
        );
        assert_eq!(app.levels().mic, 0.0, "до прокачки уровня нет");
        // Захват отдаёт очередь только открытым — иначе drain вернёт пустоту.
        app.set_monitor(true).expect("включение проверки");
        app.pump_audio().expect("прокачка");
        let первый = app.levels().mic;
        assert!(первый > 0.4, "уровень: {первый}");
        app.pump_audio().expect("прокачка на тишине");
        assert!(
            app.levels().mic < первый,
            "без сигнала уровень обязан затухать"
        );
    }

    #[test]
    fn проверка_не_пишет_ни_в_кольцо_ни_в_файл() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::Нет, vec![(vec![100; 1000], vec![100; 1000])]);
        app.set_monitor(true).expect("включение проверки");
        app.pump_audio().expect("прокачка");
        assert_eq!(
            app.ring_mic.len(),
            0,
            "в Idle кольцо не крутится, даже когда микрофон открыт"
        );
        assert!(
            !записано(&ж).iter().any(|s| s.starts_with("create:")),
            "проверка не открывает файлов: {:?}",
            записано(&ж)
        );
    }
```

Используемые хелперы уже есть в `mod tests` файла `src/app.rs`: `журнал()` (строка ~816), `сессия(pid, name)` (~692), `записано(&журнал)` (~949), `стенд(журнал, поломка, звук)` (~934). Новых не заводить.

- [ ] **Step 2: Прогнать тест и убедиться, что он падает**

Run: `cargo.exe test -p meeting-recorder проверка 2>&1 | tail -20`

Expected: `no method named 'set_monitor'` / `'levels'`.

- [ ] **Step 3: Написать реализацию**

В `src/app.rs`:

```rust
/// Пиковый уровень по обеим дорожкам, 0..1.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Levels {
    pub mic: f32,
    pub system: f32,
}

/// Во сколько раз уровень падает за тик без сигнала. При тике 200 мс полоска
/// опускается примерно за полсекунды — глазу видно движение, но не мерцание.
const LEVEL_DECAY: f32 = 0.7;

fn peak(samples: &[i16]) -> f32 {
    samples
        .iter()
        .map(|s| (*s as f32 / i16::MAX as f32).abs())
        .fold(0.0, f32::max)
        .min(1.0)
}
```

> **Поправлено финальным ревью перед мержем.** Масштаб — `i16::MAX`, поэтому
> `i16::MIN` без зажима давал 1.0000305 — за пределами документированного
> диапазона `Levels` (0..1). Зажим стоял только в UI; `.min(1.0)` переносит
> контракт «0..1» в само ядро. Тест — `peak_зажимается_к_единице_на_i16_min`.

В `App` — три поля:

```rust
    /// Включена ли явная проверка микрофона. Не состояние машины: она про
    /// запись, а это про «дай послушать».
    monitor: bool,
    level_mic: f32,
    level_sys: f32,
```

Инициализировать нулями в `with_backends`. Методы:

```rust
    pub fn levels(&self) -> Levels {
        Levels {
            mic: self.level_mic,
            system: self.level_sys,
        }
    }

    pub fn is_monitoring(&self) -> bool {
        self.monitor
    }

    /// Явная проверка микрофона: открыть потоки, ничего не записывая.
    ///
    /// Инвариант «микрофон открыт только когда мы слушаем» сохраняется по
    /// смыслу: слушаем именно потому, что попросили. Индикатор Windows при этом
    /// честно горит.
    ///
    /// Выключение гасит потоки ТОЛЬКО в `Idle`. Если за время проверки пришёл
    /// детект, потоки уже принадлежат записи — закрыть их здесь значило бы
    /// оборвать встречу тем, что пользователь выключил проверку.
    pub fn set_monitor(&mut self, on: bool) -> Res {
        self.monitor = on;
        if !matches!(self.machine.state(), State::Idle) {
            return Ok(());
        }
        if on {
            self.audio.open()?;
        } else {
            self.audio.close();
            self.level_mic = 0.0;
            self.level_sys = 0.0;
        }
        Ok(())
    }
```

> **БЛОКЕР, поправлено финальным ревью перед мержем.** Версия выше ставит
> `self.monitor = on` ДО `self.audio.open()?`. При отказе `open()` (устройство
> занято или исчезло) флаг остаётся `true` при закрытых потоках — в тот же тик
> в окно уходит `{mic: 0, system: 0, monitoring: true}`: кнопка «Остановить
> проверку», полоски на нуле, неотличимо от «микрофон не слышит», ровно то, от
> чего обещает защищать докблок `drain_ctl`. Финальный код присваивает
> `self.monitor` только ПОСЛЕ успешного `open()`/`close()` — см.
> `src/app.rs`, тест `провал_включения_проверки_не_оставляет_флаг_включённым`.

Геттеров для тестов не добавлять: `mod tests` лежит в том же файле, `app.audio` и `app.ring_mic` ему доступны напрямую, а `RingBuffer::len()` уже публичный (`src/ringbuf.rs:37`).

В `pump_audio` — считать уровень до разбора состояния:

```rust
    pub fn pump_audio(&mut self) -> Res {
        let (mic, sys) = self.drain_channels();
        // Уровень считается всегда, когда что-то течёт: в записи он даровой
        // (данные и так проходят здесь), в проверке — единственный смысл.
        // Затухание, а не мгновенный ноль: иначе полоска мигала бы на паузах
        // между словами.
        self.level_mic = peak(&mic).max(self.level_mic * LEVEL_DECAY);
        self.level_sys = peak(&sys).max(self.level_sys * LEVEL_DECAY);
        match self.machine.state() {
            State::Armed => {
                self.ring_mic.push_slice(&mic);
                self.ring_sys.push_slice(&sys);
            }
            State::Recording(_) => {
                if let Some(w) = self.sink_mic.as_mut() {
                    w.write(&mic)?;
                }
                if let Some(w) = self.sink_sys.as_mut() {
                    w.write(&sys)?;
                }
            }
            // Idle и Finalizing: сэмплы дренированы и отброшены. В проверке
            // это и требуется — послушать, ничего не сохранив.
            _ => {}
        }
        Ok(())
    }
```

Дополнить `Action::DiscardRing` и `Action::CloseFile`: там зовётся `self.audio.close()`, который погасит и проверку. Это верно — по окончании записи проверка тоже кончилась; сбросить флаг:

```rust
            Action::DiscardRing => {
                self.ring_mic.drain_to_vec();
                self.ring_sys.drain_to_vec();
                self.audio.close(); // отказ — отпускаем микрофон немедленно
                self.monitor = false;
            }
```

и аналогично после `self.audio.close()` в `Action::CloseFile` и в `reset_to_idle`.

- [ ] **Step 4: Прогнать тесты**

Run: `cargo.exe test --workspace 2>&1 | tail -10`

Expected: `test result: ok` — в том числе существующие тесты privacy-инвариантов, которые проверяют `is_open` после отказа.

- [ ] **Step 5: Закоммитить**

```bash
git add src/app.rs
git commit -m "feat(app): режим проверки микрофона и пиковый уровень по дорожкам"
```

---

### Task 13: Проверка микрофона и полоски уровня в окне

**Files:**
- Modify: `src-tauri/src/audio.rs`, `src-tauri/src/status.rs`, `src-tauri/src/main.rs`, `ui/index.html`, `ui/main.js`

**Interfaces:**
- Consumes: `App::set_monitor`, `App::levels`, `App::is_monitoring` (Task 12)
- Produces: `Ctl::Monitor(bool)`; Tauri-команда `set_monitor(on: bool)`; событие `levels` с `{ mic, system, monitoring }`

- [ ] **Step 1: Написать падающий тест проводки**

В `mod tests` файла `src-tauri/src/audio.rs`:

```rust
    /// Проверка микрофона — не событие машины: состояние записи от неё не
    /// меняется, и в `feed` ничего уходить не должно.
    #[test]
    fn монитор_не_кормит_машину_событиями() {
        let (tx, rx) = channel();
        tx.send(Ctl::Monitor(true)).unwrap();
        tx.send(Ctl::Monitor(false)).unwrap();

        let mut seen = Vec::new();
        let quit = drain_ctl(&mut app(), &rx, None, |_, e, _| seen.push(e), |_| {});

        assert!(!quit);
        assert!(seen.is_empty(), "уехало в машину: {seen:?}");
    }

    #[test]
    fn shutdown_после_монитора_всё_равно_финализирует() {
        let (tx, rx) = channel();
        tx.send(Ctl::Monitor(true)).unwrap();
        tx.send(Ctl::Shutdown).unwrap();

        let mut seen = Vec::new();
        assert!(drain_ctl(
            &mut app(),
            &rx,
            None,
            |_, e, _| seen.push(e),
            |_| {}
        ));
        assert_eq!(seen, vec![Event::ManualStop]);
    }

Третьего теста — на то, что отказ проверки уходит в канал ошибок, — здесь НЕ писать. Стенд `app()` в тестах `audio.rs` строится на настоящем `CpalAudio`, заставить который отказать по команде нечем: откроется микрофон или нет, зависит от машины. Тест на «сообщений либо нет, либо они про проверку» проходит на пустом списке и потому не проверяет ничего. Канал ошибок здесь верифицируется ручной проверкой в Step 5 (выключить гарнитуру, нажать «Проверить»), а не зелёным ассертом, который не может покраснеть.
```

- [ ] **Step 2: Прогнать тест и убедиться, что он падает**

Run: `cargo.exe test -p meeting-recorder-gui монитор 2>&1 | tail -20`

Expected: `no variant named 'Monitor'`.

- [ ] **Step 3: Написать реализацию**

`src-tauri/src/audio.rs` — расширить `Ctl`:

```rust
    /// Включить/выключить проверку микрофона. Как и `SetMicDevice`, это не
    /// событие машины: состояние записи от проверки не меняется.
    Monitor(bool),
```

В `drain_ctl` — рядом с веткой `SetMicDevice`:

```rust
Отказ проверки обязан дойти до окна, а не только в stderr: в релизе консоли нет, и молчаливый отказ выглядел бы как «нажал Проверить, полоска стоит» — то есть неотличимо от «микрофон не слышит», ровно того, что эта кнопка и должна различать. Но `drain_ctl` не знает про `AppHandle` (и не должен: на нём держатся тесты проводки без Tauri), поэтому канал ошибки приходит параметром:

```rust
fn drain_ctl(
    app: &mut App,
    rx: &Receiver<Ctl>,
    active: Option<&MicSession>,
    mut feed: impl FnMut(&mut App, Event, Option<&MicSession>),
    mut on_error: impl FnMut(String),
) -> bool {
    for c in rx.try_iter() {
        // Не события машины: состояние записи от них не меняется.
        if let Ctl::SetMicDevice(choice) = c {
            app.set_mic_device(choice);
            continue;
        }
        if let Ctl::Monitor(on) = c {
            if let Err(e) = app.set_monitor(on) {
                on_error(format!("проверка микрофона: {e}"));
            }
            continue;
        }
        let (e, quit) = ctl_to_event(&c, app.state());
        feed(app, e, if quit { None } else { active });
        if quit {
            return true;
        }
    }
    false
}
```

> **Поправлено финальным ревью перед мержем.** `ctl_to_event` держала два
> `unreachable!()` на ветках `SetMicDevice`/`Monitor` — рабочих (`drain_ctl`
> перехватывает оба варианта раньше и сюда их не пропускает), но паника здесь
> убивает единственный поток, который умеет писать на диск, причём тихо:
> `status::fatal` от паники не срабатывает, а «поток мёртв» всплывает только
> на следующей команде из GUI. Сигнатура стала `Option<(Event, bool)>`, обе
> ветки отдают `None` вместо паники, а вызов в `drain_ctl` — деструктуризация
> с `continue` вместо `let (e, quit) = ...`:
>
> ```rust
>         let Some((e, quit)) = ctl_to_event(&c, app.state()) else {
>             continue;
>         };
>         feed(app, e, if quit { None } else { active });
>         if quit {
>             return true;
>         }
> ```
>
> Финальный код — в `src-tauri/src/audio.rs`.

Вызов в `run`:

```rust
        let quit = drain_ctl(
            &mut app,
            &rx,
            active.as_ref(),
            |a, e, src| feed(a, &handle, e, src),
            |msg| {
                let _ = handle.emit("error", msg.clone());
                eprintln!("{msg}");
            },
        );
```

Существующие тесты `drain_ctl` в `mod tests` (`shutdown_кормит_машину_и_только_потом_выходит`, `команды_до_shutdown_доходят_после_него_нет`, `пустой_канал_это_не_повод_выходить`, `shutdown_идёт_без_источника_обычная_команда_с_ним`) получают пятым аргументом `|_| {}` — им ошибки не интересны, их предмет другой.
```

Таймаут и рассылка уровней — в `run`. Рядом с `last_poll` завести:

```rust
    // Проверка, забытая включённой, держала бы микрофон бесконечно. Таймер
    // считает здесь, а не в webview: окно можно закрыть, и выключать проверку
    // стало бы некому.
    let mut monitor_since: Option<Instant> = None;
```

В начале тела цикла, ДО опроса детектора и `drain_ctl`, снять снимок состояния
проверки на начало тика:

```rust
    loop {
        // Снимок ДО обработки этого тика — единственный способ увидеть
        // переход «было включено → стало выключено» уже после того, как он
        // случился. См. докблок `should_emit_levels`.
        let was_monitoring = app.is_monitoring();
```

Важно: снимок берётся ДО `drain_ctl` и ДО блока таймаута ниже, потому что оба
могут выключить проверку за этот же тик (`Ctl::Monitor(false)` в `drain_ctl`,
автовыключение по таймауту здесь же, а ещё и конец записи — `Action::CloseFile`
в ядре гасит флаг проверки как побочный эффект, и этот путь тоже проходит через
`drain_ctl`/детект раньше этой строки). Если снимок взять позже — уже после
одной из этих мутаций, — он покажет то же самое, что и `is_monitoring()` в
конце тика, и весь смысл его существования потеряется.

После `drain_ctl` в теле цикла:

```rust
        // Взводим/снимаем таймер по факту состояния, а не по команде: так копия
        // «включена ли проверка» не может разъехаться с истиной в App.
        match (app.is_monitoring(), monitor_since) {
            (true, None) => monitor_since = Some(Instant::now()),
            (false, Some(_)) => monitor_since = None,
            (true, Some(t)) if t.elapsed() >= MONITOR_TIMEOUT => {
                if let Err(e) = app.set_monitor(false) {
                    eprintln!("автовыключение проверки: {e}");
                }
                monitor_since = None;
            }
            _ => {}
        }
```

Константа рядом с `TICK`:

```rust
/// Через сколько проверка микрофона выключается сама.
const MONITOR_TIMEOUT: Duration = Duration::from_secs(60);
```

Решение «слать ли `levels` в этом тике» — чистая функция, а не голое условие
внутри `run`: она читается тестом отдельно от Tauri и от таймера.

```rust
/// Решение «слать ли `levels` в этом тике».
///
/// Без простоя было бы 5 событий в секунду в пустоту, поэтому в тишине (не
/// проверяем и не пишем) событие не шлётся. Но `was_monitoring` — отдельный
/// параметр, а не то же самое, что `is_monitoring`: за один тик проверка может
/// успеть выключиться (`Ctl::Monitor(false)`, автовыключение по таймауту или
/// конец записи), и к моменту, когда вызывающий спрашивает это условие,
/// `is_monitoring()` уже вернёт `false`, а `state()` уже может быть `Idle`.
/// Без `was_monitoring` это финальное «проверка кончилась» терялось бы
/// навсегда: окно не получило бы событие с `monitoring: false`, а кнопка и
/// полоски там остались бы висеть в состоянии «идёт проверка» до следующей
/// настоящей записи.
///
/// НЕ писать здесь `if app.is_monitoring() || app.state() != State::Idle` —
/// именно эта версия (без `was_monitoring`) была в первой реализации задачи и
/// не отправляла последнее событие ни разу: см. ревью Task 13.
fn should_emit_levels(was_monitoring: bool, is_monitoring: bool, state: State) -> bool {
    was_monitoring || is_monitoring || state != State::Idle
}
```

Рассылка уровней — в конце тела цикла, перед `sleep`:

```rust
        if should_emit_levels(was_monitoring, app.is_monitoring(), app.state()) {
            let l = app.levels();
            let _ = handle.emit(
                "levels",
                serde_json::json!({
                    "mic": l.mic,
                    "system": l.system,
                    "monitoring": app.is_monitoring(),
                }),
            );
        }
```

Тест на `should_emit_levels` пишется как чистая логика над двумя булевыми и
`State`, отдельно от `run` и от Tauri — минимум один регресс-тест, ловящий
именно потерю финального события:

```rust
#[test]
fn финальное_выключение_проверки_всё_равно_шлёт_событие() {
    // Без `was_monitoring` в OR: is_monitoring() уже false, state() уже Idle —
    // условие было бы ложным, и это событие терялось бы навсегда.
    assert!(should_emit_levels(true, false, State::Idle));
}
```

Для этого добавить в `src-tauri/Cargo.toml` уже подключённый в Task 4 `serde_json` (проверить, что он есть).

`src-tauri/src/main.rs` — команда:

```rust
/// Включить/выключить проверку микрофона.
#[tauri::command]
fn set_monitor(on: bool, state: tauri::State<Cmd>, app: AppHandle) -> Result<(), String> {
    state
        .send(Ctl::Monitor(on))
        .inspect_err(|_| status::fatal(&app, status::DEAD.to_string()))
}
```

Добавить `set_monitor` в `invoke_handler`.

- [ ] **Step 4: Добавить полоски и кнопку в окно**

`ui/index.html` — в секцию «Микрофон», после `<select>`:

```html
    <div class="row">
      <select id="mic"></select>
      <button id="check">Проверить</button>
    </div>
    <div class="meters">
      <div class="meter"><span>Вы</span><div class="bar"><i id="lvl-mic"></i></div></div>
      <div class="meter"><span>Система</span><div class="bar"><i id="lvl-sys"></i></div></div>
    </div>
```

Стили:

```css
      .meters {
        margin-top: 8px;
      }
      .meter {
        display: flex;
        align-items: center;
        gap: 8px;
        margin-bottom: 4px;
        font-size: 12px;
        color: var(--muted);
      }
      .meter span {
        width: 56px;
        flex: none;
      }
      .bar {
        flex: 1;
        height: 8px;
        border-radius: 4px;
        background: var(--line);
        overflow: hidden;
      }
      .bar > i {
        display: block;
        height: 100%;
        width: 0;
        background: var(--accent);
        transition: width 0.12s linear;
      }
```

`ui/main.js`:

```js
let проверка_идёт = false;

$("check").addEventListener("click", async () => {
  try {
    await invoke("set_monitor", { on: !проверка_идёт });
    показать_ошибку("");
  } catch (e) {
    показать_ошибку(String(e));
  }
});

function применить_уровни(l) {
  $("lvl-mic").style.width = `${Math.min(100, l.mic * 100)}%`;
  $("lvl-sys").style.width = `${Math.min(100, l.system * 100)}%`;
  проверка_идёт = Boolean(l.monitoring);
  $("check").textContent = проверка_идёт ? "Остановить проверку" : "Проверить";
}
```

Подписка в `старт()`:

```js
  await listen("levels", (e) => применить_уровни(e.payload));
```

И гасить полоски, когда всё кончилось, — в `применить_состояние`:

```js
  if (s === "idle" && !проверка_идёт) {
    применить_уровни({ mic: 0, system: 0, monitoring: false });
  }
```

- [ ] **Step 5: Собрать и проверить руками**

Run: `cargo.exe test --workspace 2>&1 | tail -5 && cargo.exe build --release -p meeting-recorder-gui 2>&1 | tail -5`

Проверить вручную:
1. Нажать «Проверить», говорить — полоска «Вы» должна двигаться. Индикатор микрофона в трее Windows горит.
2. Нажать «Остановить проверку» — полоска гаснет, индикатор Windows гаснет.
3. Включить проверку и оставить на минуту — она должна выключиться сама.
4. Переключить устройство на `Microphone Array` и повторить проверку: полоска должна двигаться заметно слабее при той же громкости речи. Это и есть та разница, из-за которой всё затевалось.
5. Выключить гарнитуру, выбрать её в выпадашке и нажать «Проверить» — в окне обязано появиться сообщение про проверку микрофона или предупреждение о подмене устройства, а не молчание. Это единственная проверка канала ошибок: юнит-тестом её не сделать, отказ `CpalAudio` по команде не вызывается.

- [ ] **Step 6: Обновить README**

В `README.md`, раздел «Как пользоваться», добавить пункты:

```markdown
- выпадашка **«Микрофон»** — выбор устройства записи; выбор сохраняется между
  запусками, а если устройство недоступно, запись идёт с системного по умолчанию
  и в окне висит предупреждение;
- кнопка **«Проверить»** — открывает микрофон и показывает уровень по обеим
  дорожкам, чтобы убедиться, что пишется тот микрофон, в который говорят;
  выключается сама через минуту;
- файлы: две WAV-дорожки в `C:\Users\<username>\Recordings\YYYY-MM\`, папка месяца
  создаётся автоматически;
- **«Переименовать»** в строке записи меняет хвост имени (дата и время
  зафиксированы), переименовывая обе дорожки и папку транскрипта разом.
```

- [ ] **Step 7: Закоммитить**

```bash
git add src-tauri/src/audio.rs src-tauri/src/main.rs ui/index.html ui/main.js README.md
git commit -m "feat(gui): проверка микрофона с полосками уровня и автовыключением"
```

---

## Проверка по завершении

Прогнать целиком и убедиться, что ничего не отвалилось:

```bash
cd /mnt/c/Users/<username>/Projects/meeting-recorder
cargo.exe test --workspace 2>&1 | tail -20
cargo.exe build --release -p meeting-recorder-gui 2>&1 | tail -5
```

Затем сквозной сценарий на живой встрече: выбрать гарнитуру, проверить уровень кнопкой, записать 2-3 минуты, остановить, убедиться что файлы легли в `C:\Users\<username>\Recordings\<текущий месяц>\`, переименовать запись из окна, прогнать внешний скрипт расшифровки и убедиться, что реплики владельца в транскрипте связные, а не обрывочные.
