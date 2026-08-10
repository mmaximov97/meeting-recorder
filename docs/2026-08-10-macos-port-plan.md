# Порт на macOS — план реализации

> **Для агентов-исполнителей:** ОБЯЗАТЕЛЬНЫЙ САБ-СКИЛЛ: используйте `superpowers:subagent-driven-development` (рекомендуется) или `superpowers:executing-plans` для выполнения плана по задачам. Шаги отмечены чекбоксами (`- [ ]`).

**Цель:** приложение пишет встречи (мик + системный звук) и детектит их начало на macOS 14.4+ с тем же UX, что и на Windows (трей, хоткей, тост, список записей), без деградации функциональности.

**Архитектура:** платформенный код прячется за уже существующими трейтами `AudioIo` (`src/app.rs`) и `MeetingDetector` (`src/detector/mod.rs`). Системный звук — Core Audio Process Tap (macOS 14.2+ API), живёт как ресурс на весь процесс, а не на одну запись. Детект — `sysinfo` по именам процессов, отдельно от Core Audio; сигнал активности звука подмешивается на уровне `audio::run()` через уже существующий `App::levels()`.

**Стек:** Rust, Tauri 2, `cpal` (микрофон на обеих ОС), `sysinfo`, `objc2-core-audio`/ручной FFI (только macOS), `windows-rs` (только Windows).

## Глобальные ограничения

- Минимальная поддерживаемая версия — macOS 14.4. Ниже — приложение не запускается, явное сообщение, без деградации.
- Ad-hoc подпись сборки обязательна для macOS: без неё системный диалог разрешения на Process Tap не появляется вообще.
- Хоткей остаётся Ctrl+Shift+R на обеих ОС.
- `~/Recordings`, не `~/Documents/...` — вне iCloud-синка.
- Никакой нотаризации/App Store, sandbox выключен.
- Существующие Windows-тесты (170+, см. README) обязаны оставаться зелёными после каждой задачи ниже — большинство задач Windows не касаются вовсе.

---

## Task 1: Кросс-платформенная реструктуризация `capture/`

Ядро (`meeting_recorder`, крейт `src/`) сейчас физически не компилируется без `windows-rs` — `[dependencies.windows]` в корневом `Cargo.toml` безусловна. Эта задача выносит Windows-специфичный захват системного звука в отдельный модуль под `cfg`, и делает крейт впервые собираемым нативно на Linux/macOS. Чисто механический рефакторинг, поведение не меняется.

**Файлы:**
- Изменить: `Cargo.toml` (корневой пакет)
- Изменить: `src/capture/mod.rs`
- Создать: `src/capture/windows.rs`

**Интерфейсы:**
- Потребляет: ничего нового.
- Производит: `capture::build_loopback_capture`, `capture::start_silence` — те же публичные имена и сигнатуры, что и сегодня, просто физически определены в `capture::windows` и реэкспортированы из `capture::mod` только под `#[cfg(target_os = "windows")]`. Общие `Resampler`, `downmix_to_mono_f32`/`downmix_to_mono_i16`, `build_mic_capture`, `resolve_input`, `list_input_devices`, `DeviceChoice`, `InputDevice`, `CaptureError`, `Source`, `PendingCapture` остаются в `capture::mod` без изменений.

- [ ] **Шаг 1: создать `src/capture/windows.rs`**

Перенести в новый файл (вырезать из `src/capture/mod.rs`) функции `fill_silence`, `build_silence_for_format`, `build_silence`, `build_loopback_capture`, `start_silence` — вместе с их докблоками, без изменений в теле. Импорты нового файла:

```rust
//! Захват системного звука на Windows: WASAPI loopback + тихий render-поток,
//! который не даёт loopback-эндпоинту простаивать. Специфика этого модуля —
//! WASAPI-only, у macOS другой механизм (`capture::macos`).

use std::sync::mpsc::Sender;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample};

use super::{build_stream_for_format, CaptureError, PendingCapture, Source};
```

(Докблоки функций переносятся как есть — они уже написаны под Windows/WASAPI специфику, ничего в них поправлять не нужно.)

В конец файла — блок тестов, перенесённый из `capture::mod` без изменений:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn тишина_затирает_буфер_целиком() {
        let mut buf = [0.7f32, -0.3, 0.9, 0.1];
        fill_silence(&mut buf);
        assert_eq!(buf, [0.0; 4]);
    }

    #[test]
    fn тишина_для_знаковых_форматов_это_ноль() {
        let mut i = [7i16, -9, 32_000];
        fill_silence(&mut i);
        assert_eq!(i, [0; 3]);

        let mut j = [7i32, -9, 2_000_000];
        fill_silence(&mut j);
        assert_eq!(j, [0; 3]);
    }

    #[test]
    fn тишина_для_u8_это_середина_шкалы_а_не_ноль() {
        let mut u = [0u8, 255, 3];
        fill_silence(&mut u);
        assert_eq!(
            u,
            [128; 3],
            "нули в u8-потоке — это не тишина, а постоянное смещение на полшкалы"
        );
    }

    #[test]
    fn пустой_буфер_вывода_не_паникует() {
        let mut buf: [f32; 0] = [];
        fill_silence(&mut buf);
        assert!(buf.is_empty());
    }
}
```

- [ ] **Шаг 2: удалить перенесённое из `src/capture/mod.rs`**

Из `src/capture/mod.rs` убрать: сами функции `fill_silence`/`build_silence_for_format`/`build_silence`/`build_loopback_capture`/`start_silence`, а из блока `#[cfg(test)] mod tests` — четыре теста, перечисленных в шаге 1 (`тишина_*`, `пустой_буфер_вывода_не_паникует`). `build_stream_for_format` и `build_stream` остаются в `mod.rs` (их использует `build_mic_capture`, который общий), но теперь должны быть видны из `capture::windows` — они уже не помечены `pub`, а по правилам видимости Rust приватный айтем родительского модуля виден его дочерним модулям автоматически, так что `capture::windows` сможет звать `super::build_stream_for_format(...)` без дополнительных правок видимости.

В начало `src/capture/mod.rs` (после существующих `use`) добавить:

```rust
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::{build_loopback_capture, start_silence};
```

- [ ] **Шаг 3: вынести Windows-зависимость в target-specific секцию `Cargo.toml`**

В корневом `Cargo.toml` заменить:

```toml
[dependencies.windows]
version = "0.58"
features = [
    "Win32_Media_Audio",
    "Win32_System_Com",
    "Win32_Foundation",
]
```

на:

```toml
[target.'cfg(windows)'.dependencies.windows]
version = "0.58"
features = [
    "Win32_Media_Audio",
    "Win32_System_Com",
    "Win32_Foundation",
]
```

`cpal`, `sysinfo`, `thiserror`, `hound`, `chrono` остаются безусловными зависимостями — все они уже кросс-платформенные.

- [ ] **Шаг 4: гейтить `CpalAudio` в `src/app.rs` — иначе крейт всё ещё не соберётся на не-Windows**

`capture::build_loopback_capture`/`capture::start_silence` теперь видны только под `cfg(target_os = "windows")` (шаг 2), но `src/app.rs` — часть той же библиотечной цели (`src/lib.rs` подключает `pub mod app;` безусловно) — использует их безусловно: `struct CpalAudio` и `impl AudioIo for CpalAudio` вызывают `start_silence()` и `build_loopback_capture(tx_sys)` внутри `CpalAudio::open()`, а `impl App { pub fn new(...) }` безусловно строит `CpalAudio::new(mic)`. Без этого шага `cargo build --lib -p meeting-recorder` упадёт с `E0432` (unresolved import) на любой не-Windows платформе — ровно то, что должен уметь именно этот таск.

В начале `src/app.rs` заменить:

```rust
use crate::capture::{build_loopback_capture, build_mic_capture, start_silence, DeviceChoice};
```

на:

```rust
use crate::capture::{build_mic_capture, DeviceChoice};
#[cfg(target_os = "windows")]
use crate::capture::{build_loopback_capture, start_silence};
```

(`build_mic_capture`/`DeviceChoice` остаются безусловными — микрофонная дорожка общая для обеих ОС, `build_mic_capture` начнёт использоваться на macOS в Task 4, в промежутке между этим шагом и Task 4 возможно предупреждение о неиспользуемом импорте на не-Windows — это ожидаемо и самоустранится в Task 4, не годная тому, что стоит подавлять сейчас.)

Добавить `#[cfg(target_os = "windows")]` непосредственно над каждым из трёх мест — без изменений тела:

```rust
#[cfg(target_os = "windows")]
struct CpalAudio {
    // ... как сейчас, без изменений ...
}

#[cfg(target_os = "windows")]
impl CpalAudio {
    // ... как сейчас, без изменений ...
}

#[cfg(target_os = "windows")]
impl AudioIo for CpalAudio {
    // ... как сейчас, без изменений ...
}
```

И у самого метода `App::new` (не у всего `impl App` — блок используется и другими, платформенно-нейтральными методами):

```rust
impl App {
    #[cfg(target_os = "windows")]
    pub fn new(root: PathBuf, mic: DeviceChoice) -> Self {
        Self::with_backends(root, Box::new(CpalAudio::new(mic)), Box::new(WavSinks))
    }

    // остальные методы impl App — без cfg, как сейчас
    ...
}
```

Публичный конструктор для macOS (`App::new_with_audio` или аналог, принимающий готовый `Box<dyn AudioIo>`) сюда не добавляется — вводить его раньше, чем появится реальный macOS-потребитель (`MacAudio` из Task 4), значит писать мёртвый код. Собственные тесты `src/app.rs` не пострадают: они конструируют `App` через `with_backends` с подставными `AudioIo`, а не через `App::new`, и `with_backends` остаётся приватным/видимым только тестам — это уже так сегодня.

- [ ] **Шаг 5: проверить, что крейт впервые собирается нативно на Linux**

```bash
cargo test --lib -p meeting-recorder
```

Ожидается: компилируется и проходит (раньше это было физически невозможно без Windows-таргета — `windows-rs` и WASAPI-типы были в безусловных зависимостях). Флаг `--lib` намеренно ограничивает сборку библиотечной целью — бинарь `meeting-recorder-cli` (`src/main.rs`) всё ещё зовёт `WindowsDetector` безусловно и на Linux/macOS пока не соберётся; это чинится в Task 6.

Если в этом самом окружении `cargo test` падает не на этапе типизации Rust-кода, а на этапе линковки нативной библиотеки ALSA (`libasound`) — это не относится к этой правке: `cpal` на Linux требует системные ALSA dev-заголовки, которых может не быть в свежем окружении, и на реальную цель порта (macOS, там `cpal` линкуется с CoreAudio, ALSA не участвует) это никак не влияет. Если это единственная причина отказа, зафиксировать её отдельной строкой в отчёте и как минимум подтвердить `cargo build --lib -p meeting-recorder 2>&1 | grep -i error` не показывает ошибок типизации/неразрешённых импортов Rust-уровня — это и есть цель шага.

- [ ] **Шаг 6: проверить, что Windows-сборка не сломалась**

```bash
cargo.exe build --workspace
cargo.exe test --workspace
```

Ожидается: тот же результат, что и до правки (175 тестов зелёные, 118 в библиотеке + 57 в GUI-крейте) — это чистый рефакторинг, поведение не менялось.

- [ ] **Шаг 7: commit**

```bash
git add Cargo.toml src/capture/mod.rs src/capture/windows.rs src/app.rs
git commit -m "refactor(capture): вынести WASAPI loopback в capture::windows, собираться кросс-платформенно"
```

---

## Task 2: Кросс-платформенная реструктуризация `detector/`

`detector/mod.rs` уже физически разнесён на `mod.rs` + `windows.rs` — не хватает только `cfg`-гейта. Самая маленькая задача плана.

**Файлы:**
- Изменить: `src/detector/mod.rs`

**Интерфейсы:**
- Производит: `detector::WindowsDetector` остаётся публичным реэкспортом, но теперь только под `#[cfg(target_os = "windows")]`. `MeetingDetector`, `MicSession`, `POLL_INTERVAL` — без изменений, кросс-платформенные. `DetectError` тоже остаётся кросс-платформенным типом (нужен будущему `MacDetector` в Task 5, который тоже возвращает `Result<Vec<MicSession>, DetectError>`), но его единственный сегодняшний вариант `Com` гейтится сам — см. шаг 1.

- [ ] **Шаг 1: добавить `cfg` на модуль, реэкспорт и Windows-специфичный вариант ошибки**

В `src/detector/mod.rs` заменить:

```rust
pub mod windows;
```

на:

```rust
#[cfg(target_os = "windows")]
pub mod windows;
```

и:

```rust
pub use self::windows::WindowsDetector;
```

на:

```rust
#[cfg(target_os = "windows")]
pub use self::windows::WindowsDetector;
```

`DetectError` сегодня не так платформенно-нейтрален, как кажется на первый взгляд: его единственный вариант `Com` ссылается на `::windows::core::Error` безусловно. Гейтить весь enum (или тем более сам трейт `MeetingDetector`) нельзя — оба нужны будущему `MacDetector` (Task 5) на любой ОС. Правильная граница — вариант, а не тип целиком; Rust это разрешает:

```rust
#[derive(Debug, thiserror::Error)]
pub enum DetectError {
    #[cfg(target_os = "windows")]
    #[error("ошибка COM/WASAPI: {0}")]
    Com(#[from] ::windows::core::Error),
}
```

На не-Windows платформах `DetectError` временно остаётся типом без единого варианта (сконструировать нечем) — это легальный Rust, и `Result<_, DetectError>` в сигнатуре `MeetingDetector::poll` продолжает типизироваться без проблем; пустота исчезнет сама, когда у macOS появится свой вариант ошибки (если вообще появится — `MacDetector` по плану Task 5 не падает).

`MicSession`, `POLL_INTERVAL`, `MeetingDetector` — трейт и типы ниже в файле — остаются без `cfg` целиком, они действительно платформенно-нейтральны.

- [ ] **Шаг 2: проверить сборку**

```bash
cargo test --lib -p meeting-recorder
```

Ожидается: тот же результат, что после Task 1 (детектор пока не даёт новых тестов — `WindowsDetector` и так не был юнит-тестируемым, только гейтится по платформе).

```bash
cargo.exe test --workspace
```

Ожидается: без изменений, зелёные.

- [ ] **Шаг 3: commit**

```bash
git add src/detector/mod.rs
git commit -m "refactor(detector): гейтить WindowsDetector по cfg(target_os = \"windows\")"
```

---

## Task 3: Спайк — подтвердить рецепт Core Audio Process Tap на реальной машине

**Требует физический Mac (14.4+) с Xcode command line tools.** Не выполняется в Linux/WSL-окружении — здесь нет `CoreAudio.framework`. Цель — de-risk самый неопределённый кусок порта ДО того, как он обрастёт обвязкой (`capture::macos`, каналы, `AudioIo`), чтобы ошибиться в API дёшево, на 80 строках, а не посреди большого модуля.

**Файлы:**
- Создать: `examples/mac_tap_spike.rs` (автообнаруживаемый пример в корневом пакете, `cargo run --example mac_tap_spike`)
- Изменить: `Cargo.toml` (корневой пакет) — добавить macOS-specific зависимости

**Подтверждённая независимыми источниками (2026) последовательность вызовов** (сверено по гайду capturing-system-audio-on-macos-2026 и открытому инструменту AudioTee — см. `docs/2026-08-10-macos-port-design.md`, раздел «Rust-биндинг»):

1. `CATapDescription` — тап на весь микс (`stereoGlobalTapButExcludeProcesses: []`, **не трогать `isExclusive` после инициализации этим конструктором** — это флаг направления, а не переключатель «эксклюзивности», трогать его вручную инвертирует смысл и даёт тишину).
2. `AudioHardwareCreateProcessTap(&tapDescription, &mut tapID)` — здесь всплывает системный TCC-диалог (только на подписанном бинарнике, см. Task 8).
3. Узнать UID текущего дефолтного устройства вывода: `kAudioHardwarePropertyDefaultOutputDevice` → `kAudioDevicePropertyDeviceUID`.
4. Собрать словарь агрегированного устройства: `kAudioAggregateDeviceMainSubDeviceKey` = UID реального устройства вывода (тап **не может** быть главным саб-девайсом — задокументированная ловушка, даёт тишину), `kAudioAggregateDeviceSubDeviceListKey` = [{UID вывода}], `kAudioAggregateDeviceTapListKey` = [{`kAudioSubTapUIDKey`: UID тапа, `kAudioSubTapDriftCompensationKey`: true}], `kAudioAggregateDeviceTapAutoStartKey`: true, `kAudioAggregateDeviceIsPrivateKey`: true.
5. `AudioHardwareCreateAggregateDevice(&dict, &mut aggregateID)`.
6. Чтение сэмплов — **не через `AVAudioEngine`** (не ретаргетится на произвольный HAL-девайс, `kAudioOutputUnitProperty_CurrentDevice` вернёт `noErr`, но тихо продолжит читать системный дефолтный вход — задокументированная ловушка), а через `AudioDeviceCreateIOProcIDWithBlock(&mut ioProcID, aggregateID, queue, block)` на самом агрегированном устройстве. Очередь диспетчеризации обязана быть не-nil.
7. `AudioDeviceStart(aggregateID, ioProcID)` / на завершение — `AudioDeviceStop` → `AudioDeviceDestroyIOProcID` → `AudioHardwareDestroyAggregateDevice` → `AudioHardwareDestroyProcessTap`.

**Референсы для сверки точных сигнатур** (открытый код, не пересказ): `github.com/insidegui/AudioCap` (Swift, каноничный пример от бывшего инженера Apple, файл `ProcessTapRecorder.swift`/`ProcessTap.swift`) и `AudioTee` (Swift CLI-инструмент с ровно этой задачей, `AudioTapManager`/`AudioRecorder`). Крейт `objc2-core-audio` уже экспонирует `CATapDescription` — первый шаг ниже выясняет, покрывает ли он и сами функции.

- [ ] **Шаг 1: проверить покрытие `objc2-core-audio`**

```bash
cargo add --target 'cfg(target_os = "macos")' objc2-core-audio objc2-core-foundation
cargo doc -p objc2-core-audio --open
```

Найти в сгенерированной документации: `AudioHardwareCreateProcessTap`, `AudioHardwareCreateAggregateDevice`, `AudioHardwareDestroyProcessTap`, `AudioHardwareDestroyAggregateDevice`, `AudioDeviceCreateIOProcIDWithBlock`, `AudioDeviceStart`/`AudioDeviceStop`/`AudioDeviceDestroyIOProcID`. Если все есть — использовать их напрямую. Если каких-то не хватает — объявить недостающие вручную как `extern "C"` над `CoreAudio.framework`/`AudioToolbox.framework` (сигнатуры взять из `<CoreAudio/AudioHardware.h>` в SDK, установленном вместе с Xcode command line tools — `xcrun --show-sdk-path`, или один в один списать типы аргументов из `ProcessTap.swift` в `insidegui/AudioCap`).

- [ ] **Шаг 2: написать спайк**

`examples/mac_tap_spike.rs` — реализует последовательность из семи пунктов выше, пишет 5 секунд захваченных сэмплов в WAV через уже существующий `meeting_recorder::storage::WavSink` (используется как есть, никакой новой логики записи не пишем):

```rust
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("этот пример только для macOS");
}

#[cfg(target_os = "macos")]
fn main() {
    use meeting_recorder::storage::WavSink;
    use std::path::Path;
    use std::time::Duration;

    // Реализация по семи пунктам из докблока задачи — CATapDescription,
    // AudioHardwareCreateProcessTap, поиск UID дефолтного вывода,
    // AudioHardwareCreateAggregateDevice, AudioDeviceCreateIOProcIDWithBlock.
    // Колбэк IOProc складывает сэмплы в общий буфер за Mutex; main спит 5с,
    // потом останавливает поток и пишет накопленное через WavSink::create/
    // write/finalize (частота дискретизации — та, что реально отдал тап,
    // WavSink её не подгоняет).

    println!("Играй что-нибудь на 5 секунд — Spotify, YouTube, что угодно...");
    std::thread::sleep(Duration::from_secs(2));
    // ... открыть тап и аггрегат, запустить IOProc, спать 5 секунд, остановить ...
    println!("Записано в mac_tap_spike.wav — прослушай и подтверди, что там системный звук");
}
```

(Тело `#[cfg(target_os = "macos")]`-ветки дописывается по месту на реальной машине по семи пунктам выше — здесь фиксирован контракт входа/выхода и то, что WAV-запись переиспользует существующий `storage::WavSink`, а не изобретает свою.)

- [ ] **Шаг 3: ручная проверка на Mac**

```bash
cargo run --example mac_tap_spike
```

Ожидается: (1) при первом запуске появляется системный диалог разрешения — подтвердить; (2) файл `mac_tap_spike.wav` создаётся; (3) при воспроизведении файла плеером слышен тот звук, что играл эти 5 секунд. Если тишина — свериться с двумя ловушками из шага «последовательность вызовов» (тап как главный саб-девайс вместо саб-тапа; `isExclusive`, тронутый вручную).

- [ ] **Шаг 4: commit**

```bash
git add examples/mac_tap_spike.rs Cargo.toml
git commit -m "spike: подтверждённый вручную рецепт Core Audio Process Tap"
```

---

## Task 4: `capture::macos` — системный тап как ресурс уровня процесса

Оборачивает подтверждённый в Task 3 рецепт в модуль с публичным API, пригодным для встраивания в `src-tauri/src/audio.rs`. Ключевое архитектурное отличие от Windows: тап живёт всё время работы приложения (см. `docs/2026-08-10-macos-port-design.md`, «Жизненный цикл тапа...»), а не открывается на `Action::StartRingBuffer` — иначе сигналу активности звука (Task 5) не на чем было бы работать до первого детекта.

**Файлы:**
- Создать: `src/capture/macos.rs`
- Изменить: `src/capture/mod.rs`
- Изменить: `src/app.rs` (не только `src-tauri/src/audio.rs` — `AudioIo`/`CpalAudio` живут в ядре, `src/app.rs`; см. правку Task 1, добавленную после ревью)
- Изменить: `src-tauri/src/audio.rs` (точка вызова — где строится `App` в `run()`)

**Интерфейсы:**
- Потребляет: рецепт из Task 3 (тот же вызов функций, обёрнутый в тип).
- Производит:
  ```rust
  // src/capture/macos.rs, реэкспорт из src/capture/mod.rs
  pub struct SystemTap { /* ... */ }
  impl SystemTap {
      pub fn start() -> Result<Self, CaptureError>;   // открывает тап+аггрегат+IOProc один раз
      pub fn drain(&mut self) -> Vec<i16>;             // всё, что накопилось с прошлого вызова, 16 кГц моно i16
  }

  // src/app.rs
  pub struct MacAudio { /* ... */ }   // impl AudioIo for MacAudio
  impl MacAudio {
      pub fn new(mic: DeviceChoice, system_tap: Rc<RefCell<SystemTap>>) -> Self;
  }
  impl App {
      pub fn new_with_audio(root: PathBuf, audio: Box<dyn AudioIo>) -> Self;
  }
  ```
  `drain()` возвращает сэмплы уже в формате хранения — переиспользует `Resampler`/`downmix_to_mono_i16` из `capture::mod`, как и `build_mic_capture` сегодня.

- [ ] **Шаг 1: перенести спайк в постоянный модуль**

`src/capture/macos.rs` — та же логика открытия тапа/аггрегата/IOProc, что в `examples/mac_tap_spike.rs`, но колбэк IOProc не копит в локальный `Vec`, а шлёт чанки через `std::sync::mpsc::Sender<Vec<i16>>` — тот же паттерн, что уже применяют `build_mic_capture`/`build_loopback_capture` (даунмикс + `Resampler::process_to_i16` внутри колбэка, отправка через канал). `SystemTap::drain()` читает из соответствующего `Receiver` через `try_iter().flatten().collect()` — идентично тому, как `Streams::rx_mic`/`rx_sys` дренируются в `CpalAudio::drain()` (`src-tauri/src/audio.rs`).

Добавить в `src/capture/mod.rs`:

```rust
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::SystemTap;
```

- [ ] **Шаг 2: завести macOS-реализацию `AudioIo` в `src/app.rs`**

В `src/app.rs`, рядом с `CpalAudio` (которая теперь под `#[cfg(target_os = "windows")]`, см. Task 1), добавить `#[cfg(target_os = "macos")]` реализацию, структурно параллельную `CpalAudio`, но с системной дорожкой поверх уже запущенного `SystemTap`, а не открываемой заново на каждый `open()`:

```rust
// pub — MacAudio::new вызывается из src-tauri/src/audio.rs, другого крейта;
// поля при этом остаются приватными, наружу торчит только конструктор.
#[cfg(target_os = "macos")]
pub struct MacAudio {
    mic: Option<cpal::Stream>,
    mic_rx: Option<std::sync::mpsc::Receiver<Vec<i16>>>,
    mic_choice: DeviceChoice,
    fell_back: Option<String>,
    /// Живёт всё время процесса — сконструирован снаружи и передан сюда,
    /// а не создаётся в open()/close(). См. докблок задачи.
    system_tap: std::rc::Rc<std::cell::RefCell<crate::capture::SystemTap>>,
}

#[cfg(target_os = "macos")]
impl MacAudio {
    /// `system_tap` — общий на весь процесс, конструируется и передаётся
    /// вызывающим (`audio::run()`), а не здесь: это ресурс уровня процесса,
    /// а не уровня одной записи, см. докблок задачи.
    pub fn new(mic: DeviceChoice, system_tap: std::rc::Rc<std::cell::RefCell<crate::capture::SystemTap>>) -> Self {
        Self {
            mic: None,
            mic_rx: None,
            mic_choice: mic,
            fell_back: None,
            system_tap,
        }
    }
}

#[cfg(target_os = "macos")]
impl AudioIo for MacAudio {
    fn open(&mut self) -> Res {
        // Открывает ТОЛЬКО микрофон через build_mic_capture — системная
        // дорожка уже течёт из system_tap независимо от этого вызова.
        if self.mic.is_some() { return Ok(()); }
        let (tx, rx) = std::sync::mpsc::channel();
        // build_mic_capture уже импортирована безусловно в начале файла
        // (правка Task 1) — на macOS у неё наконец появляется вызывающий.
        let (pending, fell_back) = build_mic_capture(&self.mic_choice, tx)?;
        self.fell_back = fell_back;
        self.mic = Some(pending.play()?);
        self.mic_rx = Some(rx);
        Ok(())
    }

    fn close(&mut self) {
        self.mic = None;
        self.mic_rx = None;
    }

    fn is_open(&self) -> bool {
        // Про микрофон — как и докблок трейта требует ("горит ли индикатор").
        // Системная дорожка сюда не входит: у неё нет privacy-индикатора,
        // и она не гейтится этим методом.
        self.mic.is_some()
    }

    fn drain(&mut self) -> (Vec<i16>, Vec<i16>) {
        let mic = self.mic_rx.as_ref()
            .map(|rx| rx.try_iter().flatten().collect())
            .unwrap_or_default();
        let sys = self.system_tap.borrow_mut().drain();
        (mic, sys)
    }

    fn set_mic_device(&mut self, choice: DeviceChoice) {
        self.mic_choice = choice;
    }

    fn fell_back_from(&self) -> Option<String> {
        self.fell_back.clone()
    }
}
```

Рядом, в `src/app.rs`, добавить публичный конструктор для не-Windows платформ — `App::new` (Task 1) остался `#[cfg(target_os = "windows")]` и жёстко строит `CpalAudio`, поэтому macOS нужен свой вход, принимающий уже готовый `AudioIo` вместо конкретного типа:

```rust
impl App {
    /// Для платформ, где `AudioIo` собирается снаружи (macOS — `MacAudio`
    /// с общим на весь процесс `SystemTap`, которого `DeviceChoice` одного
    /// не описывает). `SinkFactory` при этом всегда `WavSinks` в продакшене,
    /// как и в `App::new` — варьируется только `AudioIo`.
    pub fn new_with_audio(root: PathBuf, audio: Box<dyn AudioIo>) -> Self {
        Self::with_backends(root, audio, Box::new(WavSinks))
    }
}
```

В `src-tauri/src/audio.rs::run()` заменить безусловное `let mut app = App::new(root, mic);` на:

```rust
#[cfg(target_os = "windows")]
let mut app = App::new(root, mic);

#[cfg(target_os = "macos")]
let mut app = {
    let system_tap = std::rc::Rc::new(std::cell::RefCell::new(
        meeting_recorder::capture::SystemTap::start().expect("Process Tap не поднялся"),
    ));
    App::new_with_audio(root, Box::new(meeting_recorder::app::MacAudio::new(mic, system_tap)))
};
```

`MacAudio`/`MacAudio::new` в `src/app.rs` должны быть `pub` (сама структура и конструктор, поля — нет), чтобы быть видимыми из `src-tauri/src/audio.rs` — это внешний по отношению к ядру крейт. `.expect(...)` здесь, а не пробрасывание ошибки через `status::fatal`, как у `WindowsDetector::new()` чуть выше по этому же файлу, — временное упрощение ЭТОЙ задачи: единообразную обработку отказа детектора/тапа на старте наводит Task 7 заодно с guard'ом версии ОС, здесь достаточно не притворяться, что тап поднялся, если это не так.

- [ ] **Шаг 3: ручная проверка на Mac**

Временно (для проверки, не коммитить) собрать GUI, включить `MR_DEBUG_TIMING=1`, запустить ручную запись через хоткей на 10 секунд с играющим на фоне звуком, остановить, прослушать `*.system.wav` в `~/Recordings/YYYY-MM/`. Ожидается: файл содержит именно то, что играло за эти 10 секунд, без обрезанного начала (в отличие от Windows, где на холодном loopback-эндпоинте без `start_silence()` возможен пустой старт — на macOS такого быть не должно, тап уже тёплый).

- [ ] **Шаг 4: commit**

```bash
git add src/capture/mod.rs src/capture/macos.rs src/app.rs src-tauri/src/audio.rs
git commit -m "feat(capture): системная дорожка на macOS через Core Audio Process Tap"
```

---

## Task 5: `detector::macos` — известные процессы + активность звука

**Файлы:**
- Создать: `src/detector/macos.rs`
- Изменить: `src/detector/mod.rs`
- Изменить: `src-tauri/src/audio.rs`

**Интерфейсы:**
- Производит:
  ```rust
  pub struct MacDetector { /* держит sysinfo::System */ }
  impl MacDetector { pub fn new() -> Self; }
  impl MeetingDetector for MacDetector { fn poll(&self) -> Result<Vec<MicSession>, DetectError>; }

  pub fn is_known_meeting_process(name: &str) -> bool;   // чистая, без sysinfo
  ```
  и в `src-tauri/src/audio.rs`:
  ```rust
  fn mac_should_arm(process_detected: bool, system_level: f32) -> bool;   // чистая
  ```

- [ ] **Шаг 1: написать падающий тест на список известных процессов**

`src/detector/macos.rs`:

```rust
//! Детект на macOS: известные процессы звонилок. Активность звука сюда не
//! входит намеренно — сигнал подмешивается в audio::run() поверх уже
//! посчитанного App::levels(), см. docs/2026-08-10-macos-port-design.md.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_известная_звонилка() {
        assert!(is_known_meeting_process("zoom.us"));
    }

    #[test]
    fn teams_известная_звонилка() {
        assert!(is_known_meeting_process("Microsoft Teams"));
    }

    #[test]
    fn slack_известная_звонилка() {
        assert!(is_known_meeting_process("Slack"));
    }

    #[test]
    fn обычный_процесс_не_звонилка() {
        assert!(!is_known_meeting_process("Finder"));
        assert!(!is_known_meeting_process("Safari"));
    }

    /// Браузеры сознательно не в списке: они открыты почти всегда независимо
    /// от звонков (см. «Отказы» в дизайн-документе) — включи их, и эвристика
    /// «известный процесс» перестанет что-либо фильтровать.
    #[test]
    fn браузеры_не_считаются_известной_звонилкой() {
        for b in ["Google Chrome", "Safari", "Firefox", "Arc"] {
            assert!(!is_known_meeting_process(b), "{b} не должен считаться звонилкой");
        }
    }

    #[test]
    fn пустое_имя_не_звонилка() {
        assert!(!is_known_meeting_process(""));
    }
}
```

- [ ] **Шаг 2: запустить, убедиться, что падает**

```bash
cargo test --lib -p meeting-recorder is_known_meeting_process
```

Ожидается: FAIL — `is_known_meeting_process` не существует.

- [ ] **Шаг 3: реализовать**

```rust
const KNOWN_MEETING_PROCESSES: &[&str] = &["zoom.us", "Microsoft Teams", "Slack"];

pub fn is_known_meeting_process(name: &str) -> bool {
    KNOWN_MEETING_PROCESSES.contains(&name)
}
```

- [ ] **Шаг 4: прогнать тесты**

```bash
cargo test --lib -p meeting-recorder is_known_meeting_process
```

Ожидается: PASS, все шесть.

- [ ] **Шаг 5: написать падающий тест на гейт активности звука**

В `src-tauri/src/audio.rs`, рядом с существующим блоком `#[cfg(test)] mod tests`:

```rust
#[cfg(target_os = "macos")]
#[test]
fn известный_процесс_и_звук_дают_детект() {
    assert!(mac_should_arm(true, 0.05));
}

#[cfg(target_os = "macos")]
#[test]
fn известный_процесс_без_звука_не_детектится() {
    assert!(!mac_should_arm(true, 0.0));
}

#[cfg(target_os = "macos")]
#[test]
fn звук_без_известного_процесса_не_детектится() {
    assert!(!mac_should_arm(false, 0.5));
}

#[cfg(target_os = "macos")]
#[test]
fn порог_не_ловит_шум_на_грани_тишины() {
    assert!(!mac_should_arm(true, 0.001));
}
```

- [ ] **Шаг 6: запустить — упадёт компиляцией** (эти тесты собираются только под `cfg(target_os = "macos")`, поэтому на Linux/Windows шаг 6-7 не выполняются в CI этой задачи — проверяются на Mac вместе с Task 4's шагом 3; здесь фиксируем ожидаемый код).

- [ ] **Шаг 7: реализовать**

```rust
/// Порог подобран по аналогии с `peak()` в этом же файле — 0..1, где 1.0 —
/// полная шкала. 0.01 отсекает цифровой шум тишины, но ловит любую реальную
/// речь/музыку, которая на пике даёт на порядки больше.
#[cfg(target_os = "macos")]
const MEETING_AUDIO_THRESHOLD: f32 = 0.01;

#[cfg(target_os = "macos")]
fn mac_should_arm(process_detected: bool, system_level: f32) -> bool {
    process_detected && system_level > MEETING_AUDIO_THRESHOLD
}
```

Место вызова — в цикле `run()`, там же, где сегодня `let (found, event) = poll_to_event(was_active, sessions, me);`: на macOS решение о `SessionAppeared` дополнительно фильтруется через `mac_should_arm(active.is_some(), app.levels().system)` перед тем, как звать `feed`/`ask` (на Windows эта строка не меняется — `poll_to_event` остаётся единственным источником решения).

- [ ] **Шаг 8: `MacDetector` — обёртка над sysinfo**

```rust
use sysinfo::{ProcessRefreshKind, RefreshKind, System};

/// Без полей: `sysinfo::System::refresh_processes` требует `&mut self`, а
/// `MeetingDetector::poll` даёт только `&self` — хранить `System` между
/// опросами и мутировать его через `RefCell` ради этого не стоит: пришлось
/// бы городить внутреннюю изменяемость ради экономии, которая на POLL_INTERVAL
/// в 2с не измерима. `new()` при этом не лишний: он даёт точку конструирования,
/// симметричную `WindowsDetector::new()`, и именно её зовёт `audio::run()`.
pub struct MacDetector;

impl MacDetector {
    pub fn new() -> Self {
        Self
    }
}

impl super::MeetingDetector for MacDetector {
    fn poll(&self) -> Result<Vec<super::MicSession>, super::DetectError> {
        let sys = System::new_with_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::new()));
        Ok(sys
            .processes()
            .values()
            .filter(|p| is_known_meeting_process(&p.name().to_string_lossy()))
            .map(|p| super::MicSession {
                pid: p.pid().as_u32(),
                process_name: p.name().to_string_lossy().to_string(),
            })
            .collect())
    }
}
```

- [ ] **Шаг 9: гейт модуля в `detector/mod.rs`**

```rust
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub use self::macos::MacDetector;
```

- [ ] **Шаг 10: прогнать все тесты**

```bash
cargo test --lib -p meeting-recorder
```

Ожидается: все зелёные, включая шесть новых `is_known_meeting_process`.

- [ ] **Шаг 11: ручная проверка на Mac**

Собрать GUI, запустить Zoom без звонка — тост не должен появиться. Начать реальный звонок с говорящим собеседником — тост должен появиться в пределах ~`POLL_INTERVAL` (2с) после первого звука.

- [ ] **Шаг 12: commit**

```bash
git add src/detector/mod.rs src/detector/macos.rs src-tauri/src/audio.rs
git commit -m "feat(detector): детект звонка на macOS — известные процессы + активность звука"
```

---

## Task 6: Хранилище записей, открытие папки и консольный бинарь — кросс-платформенно

**Файлы:**
- Изменить: `src-tauri/src/main.rs`
- Изменить: `src/main.rs`

**Интерфейсы:**
- Потребляет: `detector::WindowsDetector`/`detector::MacDetector` (Task 2, Task 5), `App::new` (без изменений).
- Производит: ничего нового наружу — обе точки входа (`meeting-recorder-cli`, `meeting-recorder-gui`) становятся собираемыми на своей платформе.

- [ ] **Шаг 1: `recordings_root()` в `src-tauri/src/main.rs`**

Заменить:

```rust
fn recordings_root() -> PathBuf {
    PathBuf::from(r"C:\Users\<username>\Recordings")
}
```

на:

```rust
#[cfg(target_os = "windows")]
fn recordings_root() -> PathBuf {
    PathBuf::from(r"C:\Users\<username>\Recordings")
}

#[cfg(target_os = "macos")]
fn recordings_root() -> PathBuf {
    let home = std::env::var("HOME").expect("$HOME обязан быть установлен");
    PathBuf::from(home).join("Recordings")
}
```

- [ ] **Шаг 2: `open_folder()` в `src-tauri/src/main.rs`**

Заменить тело на cfg-ветки:

```rust
#[tauri::command]
fn open_folder() -> Result<(), String> {
    let dir = recordings_root();
    std::fs::create_dir_all(&dir).map_err(|e| format!("не удалось создать {}: {e}", dir.display()))?;
    #[cfg(target_os = "windows")]
    let mut cmd = std::process::Command::new("explorer.exe");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    cmd.arg(&dir);
    cmd.spawn().map_err(|e| format!("не удалось открыть Finder/проводник: {e}"))?;
    Ok(())
}
```

- [ ] **Шаг 3: конструирование детектора в `src-tauri/src/audio.rs`**

Место, где сегодня `let det = match WindowsDetector::new() { ... };` — обернуть в cfg:

```rust
#[cfg(target_os = "windows")]
let det = WindowsDetector::new();
#[cfg(target_os = "macos")]
let det: Result<meeting_recorder::detector::MacDetector, meeting_recorder::detector::DetectError> =
    Ok(meeting_recorder::detector::MacDetector::new());
let det = match det {
    Ok(d) => d,
    Err(e) => {
        status::fatal(&handle, format!("детектор не поднялся: {e}"));
        return;
    }
};
```

(`MacDetector::new()` не падает, `Result` вокруг него — только чтобы обе ветки давали один и тот же тип выражения для `match` ниже; альтернатива — развести весь блок на два `#[cfg]`-варианта целиком, что читается хуже при таком маленьком теле.)

Импорт вверху файла — сегодня `use meeting_recorder::detector::{MeetingDetector, MicSession, WindowsDetector, POLL_INTERVAL};` — разбить:

```rust
use meeting_recorder::detector::{MeetingDetector, MicSession, POLL_INTERVAL};
#[cfg(target_os = "windows")]
use meeting_recorder::detector::WindowsDetector;
#[cfg(target_os = "macos")]
use meeting_recorder::detector::MacDetector;
```

- [ ] **Шаг 4: то же самое для консольного бинаря `src/main.rs`**

```rust
use meeting_recorder::detector::{MeetingDetector, MicSession, POLL_INTERVAL};
#[cfg(target_os = "windows")]
use meeting_recorder::detector::WindowsDetector;
#[cfg(target_os = "macos")]
use meeting_recorder::detector::MacDetector;
```

```rust
#[cfg(target_os = "windows")]
fn recordings_root() -> PathBuf {
    PathBuf::from(r"C:\Users\<username>\Recordings")
}

#[cfg(target_os = "macos")]
fn recordings_root() -> PathBuf {
    let home = std::env::var("HOME").expect("$HOME обязан быть установлен");
    PathBuf::from(home).join("Recordings")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = recordings_root();
    #[cfg(target_os = "windows")]
    let det = WindowsDetector::new()?;
    #[cfg(target_os = "macos")]
    let det = MacDetector::new();
    // ... остальное тело без изменений ...
}
```

- [ ] **Шаг 5: проверить сборку на обеих платформах**

```bash
cargo build --workspace   # теперь впервые собирается целиком на Linux/macOS, не только --lib
cargo test --workspace
```

```bash
cargo.exe build --workspace
cargo.exe test --workspace
```

Ожидается: оба зелёные. Начиная с этой задачи `--lib`-ограничение из Task 1 больше не нужно — весь воркспейс собирается нативно на любой платформе (кроме `src-tauri`-пакета, если Task 7/8 к этому моменту ещё не сделаны — Tauri сам по себе кросс-платформенный, так что должно быть ок уже сейчас).

- [ ] **Шаг 6: commit**

```bash
git add src-tauri/src/main.rs src-tauri/src/audio.rs src/main.rs
git commit -m "feat: кросс-платформенные recordings_root/open_folder/детектор в обоих бинарях"
```

---

## Task 7: Отказ на старте при macOS < 14.4

**Файлы:**
- Изменить: `src-tauri/src/audio.rs` (или новый `src-tauri/src/platform.rs`, если версия-чек обрастёт деталями — начинаем с `audio.rs`, где уже есть `status::fatal`)

**Интерфейсы:**
- Производит: `fn macos_version_supported(major: u32, minor: u32) -> bool;` (чистая).

- [ ] **Шаг 1: падающий тест**

```rust
#[cfg(target_os = "macos")]
#[test]
fn версия_14_4_поддерживается() {
    assert!(macos_version_supported(14, 4));
}

#[cfg(target_os = "macos")]
#[test]
fn более_новая_минорная_версия_поддерживается() {
    assert!(macos_version_supported(14, 9));
}

#[cfg(target_os = "macos")]
#[test]
fn следующий_мажор_поддерживается() {
    assert!(macos_version_supported(15, 0));
}

#[cfg(target_os = "macos")]
#[test]
fn версия_ниже_14_4_не_поддерживается() {
    assert!(!macos_version_supported(14, 3));
    assert!(!macos_version_supported(13, 9));
}
```

- [ ] **Шаг 2: запустить, убедиться, что падает**

```bash
cargo test --lib -p meeting-recorder-gui macos_version_supported 2>&1 || true
```

(Пакет `meeting-recorder-gui` — это `src-tauri`; если тесты в `src-tauri` не запускались этим способом раньше, см. корневой `Cargo.toml`: `default-members` уже включает `src-tauri`, `cargo test --workspace` тоже сработает.)

Ожидается: FAIL — функции не существует.

- [ ] **Шаг 3: реализовать**

```rust
#[cfg(target_os = "macos")]
fn macos_version_supported(major: u32, minor: u32) -> bool {
    (major, minor) >= (14, 4)
}
```

- [ ] **Шаг 4: прогнать тесты, убедиться, что проходят**

```bash
cargo test --workspace
```

- [ ] **Шаг 5: падающий тест на разбор строки версии**

```rust
#[cfg(target_os = "macos")]
#[test]
fn разбор_обычной_версии() {
    assert_eq!(parse_major_minor("14.5"), Some((14, 5)));
}

#[cfg(target_os = "macos")]
#[test]
fn разбор_версии_с_патчем() {
    assert_eq!(parse_major_minor("14.5.1"), Some((14, 5)));
}

#[cfg(target_os = "macos")]
#[test]
fn разбор_версии_без_минорной_части() {
    assert_eq!(parse_major_minor("15"), None);
}

#[cfg(target_os = "macos")]
#[test]
fn разбор_мусора_даёт_none() {
    assert_eq!(parse_major_minor("garbage"), None);
    assert_eq!(parse_major_minor(""), None);
    assert_eq!(parse_major_minor("14.x"), None);
}
```

- [ ] **Шаг 6: запустить, убедиться, что падает, затем реализовать**

```rust
/// `sysinfo::System::os_version()` на macOS отдаёт `"14.5"`/`"14.5.1"` —
/// мажор и минор обязательны, патч (если есть) отбрасывается: он ни на что
/// в этом сравнении не влияет.
#[cfg(target_os = "macos")]
fn parse_major_minor(v: &str) -> Option<(u32, u32)> {
    let mut parts = v.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}
```

```bash
cargo test --workspace
```

Ожидается: все семь новых тестов (четыре на `macos_version_supported` из шага 1 плюс четыре здесь, итого их фактически восемь — пересчитать по факту) зелёные.

- [ ] **Шаг 7: получить реальную версию ОС и вызвать проверку в `run()`**

Место — **до** конструирования детектора (та же точка, где сегодня падает `WindowsDetector::new()`):

```rust
#[cfg(target_os = "macos")]
{
    let unsupported = sysinfo::System::os_version()
        .and_then(|v| parse_major_minor(&v))
        .map(|(major, minor)| !macos_version_supported(major, minor))
        .unwrap_or(true); // не смогли определить версию — не рискуем, отказываем
    if unsupported {
        status::fatal(
            &handle,
            "нужна macOS 14.4 или новее — используется Core Audio Process Tap API".to_string(),
        );
        return;
    }
}
```

- [ ] **Шаг 8: commit**

```bash
git add src-tauri/src/audio.rs
git commit -m "feat: явный отказ на старте при macOS < 14.4"
```

---

## Task 8: Упаковка — иконка, Info.plist, ad-hoc подпись, bundle targets

**Требует физический Mac.** Файлы:
- Изменить: `src-tauri/tauri.conf.json`
- Создать: `src-tauri/icons/icon.icns`

- [ ] **Шаг 1: иконка**

Сгенерировать `.icns` из существующего `icons/128x128@2x.png` (например, `iconutil` через промежуточный `.iconset`, или онлайн/CLI-конвертер — механический шаг, конкретный инструмент не принципиален). Положить в `src-tauri/icons/icon.icns`.

- [ ] **Шаг 2: `tauri.conf.json` — bundle targets и иконки per-platform**

```json
{
  "bundle": {
    "active": true,
    "targets": ["nsis", "app", "dmg"],
    "icon": [
      "icons/32x32.png",
      "icons/128x128.png",
      "icons/128x128@2x.png",
      "icons/icon.ico",
      "icons/icon.icns"
    ],
    "macOS": {
      "signingIdentity": "-",
      "entitlements": null,
      "minimumSystemVersion": "14.4"
    }
  }
}
```

`signingIdentity: "-"` — ad-hoc подпись средствами `codesign` (бесплатная, без Apple Developer аккаунта); без неё TCC-диалог на `AudioHardwareCreateProcessTap` не появится вообще (см. `docs/2026-08-10-macos-port-design.md`, раздел «Упаковка Tauri»). Точный ключ конфигурации сверить с версией Tauri в `src-tauri/Cargo.toml` (`tauri = "2"`) на момент выполнения — в Tauri v2 signing настраивается через `bundle.macOS.signingIdentity`, но конкретное имя поля стоит перепроверить по `tauri info`/официальной схеме `$schema` в начале файла, если сборка ругнётся на неизвестный ключ.

- [ ] **Шаг 3: `Info.plist` — usage descriptions**

Tauri генерирует `Info.plist` из `tauri.conf.json` (`bundle.macOS.infoPlist` — точный путь конфигурации тоже сверить по схеме) плюс собственные ключи. Добавить:

```json
{
  "bundle": {
    "macOS": {
      "info": {
        "NSMicrophoneUsageDescription": "Запись микрофона нужна, чтобы сохранить вашу часть разговора на встрече.",
        "NSAudioCaptureUsageDescription": "Захват системного звука нужен, чтобы записать голоса собеседников во время встречи."
      }
    }
  }
}
```

Если у используемой версии `tauri-cli` нет прямой поддержки произвольных `Info.plist`-ключей через конфиг — добавить `src-tauri/Info.plist` (или `Info.macos.plist`, в зависимости от того, что подхватывает бандлер) с этими двумя ключами вручную; проверить по официальной документации Tauri v2 bundler на момент выполнения (страница `Configuration` → `bundle.macOS`), не полагаться на память.

- [ ] **Шаг 4: собрать и проверить на Mac**

```bash
cargo tauri build
```

Ожидается: собирается `.app` и `.dmg` в `src-tauri/target/release/bundle/macos/` и `.../dmg/`. Запустить `.app` напрямую, начать первую запись (или явную проверку микрофона) — должны появиться ДВА системных диалога разрешения (микрофон — `NSMicrophoneUsageDescription`, системный звук — `NSAudioCaptureUsageDescription`), каждый с текстом из шага 3, а не с общей/пустой формулировкой.

Заодно проверить то, что design-документ отметил как «ожидается рабочим без переписывания логики» (раздел «Упаковка Tauri»), но что раньше никогда не запускалось на macOS: значок в трее появляется и открывает окно со списком записей; хоткей **Ctrl+Shift+R** переключает запись при свёрнутом окне; тост «Похоже, встреча» появляется поверх других окон и не блокируется чем-то вроде Do Not Disturb; закрытие окна крестиком прячет его, а не завершает процесс (процесс остаётся в трее). Любое расхождение — заводить как отдельный баг, не блокируя эту задачу, если сама сборка/подпись/permissions работают.

- [ ] **Шаг 5: commit**

```bash
git add src-tauri/tauri.conf.json src-tauri/icons/icon.icns
git commit -m "build(macos): bundle targets, ad-hoc подпись, usage descriptions для микрофона и системного звука"
```

---

## Task 9: README — инструкция сборки на macOS

**Файлы:**
- Изменить: `README.md`

- [ ] **Шаг 1: обновить разделы «Стек» и «Сборка»**

В разделе «Стек» заменить фразу «macOS заложен точкой расширения... но не проработан» на описание того, что реализовано: Core Audio Process Tap для системного звука, `sysinfo`-детект по известным процессам + активности звука, минимальная версия 14.4.

Добавить в раздел «Сборка» отдельный macOS-блок рядом с существующим Windows/WSL-блоком:

```markdown
### Сборка (на macOS)

Нативно, без WSL-обвязки — Xcode command line tools и Rust-тулчейн достаточно:

\`\`\`bash
cargo build --workspace            # debug
cargo tauri build                  # релизный .app/.dmg, см. src-tauri/tauri.conf.json
cargo test --workspace
\`\`\`

Минимальная версия — macOS 14.4 (нужен Core Audio Process Tap API для записи
системного звука). На более старой версии приложение откажется запускаться с
понятным сообщением, а не тихо потеряет дорожку собеседников.

Первый запуск спросит два системных разрешения — на микрофон и на захват
системного звука; без обоих запись будет неполной.
```

- [ ] **Шаг 2: перечитать весь README целиком**

Пройти по всему файлу и убедиться, что ни одно оставшееся упоминание `C:\Users\<username>\Recordings`/`explorer.exe`/`cargo.exe` не выглядит теперь как единственный способ — где формулировка была общей («приложение пишет...», «файлы: две WAV-дорожки в ...»), уточнить, что путь платформенный (`C:\Users\<username>\Recordings` на Windows, `~/Recordings` на macOS), не переписывая разделы, специфичные для одной ОС (например, «Не запускать GUI из WSL-терминала» — это по-прежнему верно и относится только к Windows-сборке).

- [ ] **Шаг 3: commit**

```bash
git add README.md
git commit -m "docs: инструкция сборки и статус порта на macOS"
```

---

## Порядок выполнения и что можно проверить прямо сейчас

Задачи 1–2 и часть задачи 6–7 (пишущиеся как чистые функции) не требуют Mac и полностью проверяемы в этом Linux/WSL-окружении прямо сейчас — `cargo`/`rustc` здесь уже есть. Задачи 3, 4 (частично), 5 (частично), 8 требуют физического Mac 14.4+ и не выполнимы без него. Рекомендованный порядок — как пронумеровано: 1 → 2 (де-риск и подготовка среды, можно сделать уже сегодня) → 3 (спайк, первое, что нужно на Mac) → 4 → 5 → 6 → 7 → 8 → 9.
