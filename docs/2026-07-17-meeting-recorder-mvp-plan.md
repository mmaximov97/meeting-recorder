# meeting-recorder MVP — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Windows-приложение, которое замечает начало звонка, предлагает записать, и пишет две раздельные аудиодорожки (микрофон + системный звук) в `C:\Users\<username>\Recordings`.

**Architecture:** Ядро — две чистые, платформенно-независимые единицы (`ringbuf`, `session`), тестируемые без микрофона и без встречи. Вокруг них — три платформенных адаптера за трейтами (`detector`, `capture`, `storage`) и Tauri-оболочка (`ui`). Детект встречи сделан **поллингом** WASAPI audio-сессий раз в 2 секунды, не событиями.

**Tech Stack:** Rust + Tauri v2, `windows` (windows-rs) для WASAPI-детекта, `cpal` для захвата, `hound` для WAV, `sysinfo` для имени процесса по PID.

## Global Constraints

- **Таргет только `x86_64-pc-windows-msvc`.** Код не собирается и не запускается под Linux/WSL. Все сборки — Windows-тулчейном.
- **Код живёт на Windows-диске:** `C:\Users\<username>\Projects\meeting-recorder` (из WSL — `/mnt/c/Users/<username>/Projects/meeting-recorder`). Сборка Windows-овским `cargo.exe` через WSL-интероп. Не переносить в WSL-ФС: `cargo.exe`, читающий `\\wsl$\`, работает мучительно медленно.
- **Формат аудио:** 16000 Гц, моно, 16 бит PCM, WAV. Одинаково для обеих дорожек.
- **Каталог записей:** `C:\Users\<username>\Recordings`. **Никогда** не внутри vault (`C:\Users\<username>\Documents\vault\obsidian-vault`) — там git-автокоммиты.
- **Кольцевой буфер:** 30 секунд.
- **Интервал поллинга детектора:** 2 секунды.
- **Аудио никогда не коммитится.** `.gitignore` уже покрывает `*.wav` и `/recordings/`.
- Комментарии и коммиты — на русском, как в остальных проектах.

---

### Task 0: Тулчейн на Windows (выполняет Михаил вручную)

Это единственная задача, которую не может сделать агент: инсталляторы интерактивные и ставят Windows-софт.

**Files:** нет

- [ ] **Step 1: Поставить Visual Studio Build Tools**

Скачать [Build Tools for Visual Studio](https://visualstudio.microsoft.com/visual-cpp-build-tools/), в инсталляторе выбрать workload **«Desktop development with C++»**. Это несколько ГБ. Без него не слинкуется `msvc`-таргет — обойти нельзя, `windows-rs` и Tauri требуют его.

- [ ] **Step 2: Поставить Rust**

Скачать и запустить [rustup-init.exe](https://rustup.rs). Выбрать дефолт (`stable-x86_64-pc-windows-msvc`).

- [ ] **Step 3: Поставить Node LTS**

Скачать с [nodejs.org](https://nodejs.org/) (LTS). Нужен Tauri для фронтенда.

- [ ] **Step 4: Проверить, что WSL видит Windows-тулчейн**

Run (из WSL):
```bash
cargo.exe --version && rustc.exe --version && node.exe --version
```
Expected: три строки с версиями. Если `command not found` — перелогиниться в WSL (PATH подхватывается из Windows при старте сессии).

- [ ] **Step 5: Проверить, что MSVC-линковка реально работает**

Run:
```bash
cd /mnt/c/Users/<username>/Projects/meeting-recorder && cargo.exe new --bin linktest && cd linktest && cargo.exe run
```
Expected: `Hello, world!`. Если падает на `link.exe not found` — Build Tools встали без C++ workload, вернуться к шагу 1.

- [ ] **Step 6: Убрать проверочный проект**

```bash
cd /mnt/c/Users/<username>/Projects/meeting-recorder && rm -rf linktest
```

---

### Task 1: Каркас + спайк детектора

Идёт первой намеренно: детект — единственное место, где допущение может не подтвердиться. Если WASAPI не отдаёт то, что мы ждём, меняется весь дизайн, и лучше узнать это до того, как поверх построено что-либо.

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`
- Create: `src/detector/mod.rs`
- Create: `src/detector/windows.rs`

**Interfaces:**
- Consumes: ничего
- Produces: `MicSession { pid: u32, process_name: String }`; трейт `MeetingDetector` с методом `fn poll(&self) -> Result<Vec<MicSession>, DetectError>`; реализация `WindowsDetector::new() -> Result<Self, DetectError>`

- [ ] **Step 1: Создать `Cargo.toml`**

```toml
[package]
name = "meeting-recorder"
version = "0.1.0"
edition = "2021"

[dependencies]
sysinfo = "0.32"
thiserror = "2"

[dependencies.windows]
version = "0.58"
features = [
    "Win32_Media_Audio",
    "Win32_System_Com",
    "Win32_Foundation",
]
```

- [ ] **Step 2: Написать трейт и типы детектора**

Create `src/detector/mod.rs`:

```rust
pub mod windows;

/// Активная сессия захвата микрофона: какой-то процесс держит мик.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicSession {
    pub pid: u32,
    pub process_name: String,
}

#[derive(Debug, thiserror::Error)]
pub enum DetectError {
    #[error("ошибка COM/WASAPI: {0}")]
    Com(#[from] ::windows::core::Error),
}

/// Опрашивается раз в 2 секунды. Поллинг, а не события: перечисление каждый раз
/// видит все сессии, включая созданные до старта приложения.
pub trait MeetingDetector {
    fn poll(&self) -> Result<Vec<MicSession>, DetectError>;
}
```

- [ ] **Step 3: Реализовать WASAPI-детект**

Create `src/detector/windows.rs`:

```rust
use super::{DetectError, MeetingDetector, MicSession};
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};
use windows::core::Interface;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, AudioSessionStateActive, IAudioSessionControl2,
    IAudioSessionManager2, IMMDeviceEnumerator, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};

pub struct WindowsDetector {
    enumerator: IMMDeviceEnumerator,
}

impl WindowsDetector {
    pub fn new() -> Result<Self, DetectError> {
        unsafe {
            // Инициализация COM для этого потока. Игнорируем RPC_E_CHANGED_MODE:
            // COM мог быть уже инициализирован (например, Tauri).
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            Ok(Self { enumerator })
        }
    }
}

impl MeetingDetector for WindowsDetector {
    fn poll(&self) -> Result<Vec<MicSession>, DetectError> {
        let mut out = Vec::new();
        unsafe {
            // eCapture — устройство ЗАПИСИ. Именно на нём висят сессии тех,
            // кто держит микрофон.
            let device = self.enumerator.GetDefaultAudioEndpoint(eCapture, eConsole)?;
            let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
            let sessions = manager.GetSessionEnumerator()?;
            let count = sessions.GetCount()?;

            for i in 0..count {
                let ctl = sessions.GetSession(i)?;
                if ctl.GetState()? != AudioSessionStateActive {
                    continue;
                }
                let ctl2: IAudioSessionControl2 = ctl.cast()?;
                let pid = ctl2.GetProcessId()?;
                if pid == 0 {
                    continue; // системная сессия, не процесс
                }
                out.push(MicSession {
                    pid,
                    process_name: process_name_for(pid),
                });
            }
        }
        Ok(out)
    }
}

fn process_name_for(pid: u32) -> String {
    let sys = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::new()),
    );
    sys.process(Pid::from_u32(pid))
        .map(|p| p.name().to_string_lossy().to_string())
        .unwrap_or_else(|| format!("pid-{pid}"))
}
```

- [ ] **Step 4: Написать спайк-`main`, печатающий сессии**

Create `src/main.rs`:

```rust
mod detector;

use detector::{windows::WindowsDetector, MeetingDetector};
use std::{thread, time::Duration};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let det = WindowsDetector::new()?;
    println!("Слежу за mic-сессиями. Ctrl+C для выхода.");
    loop {
        match det.poll() {
            Ok(sessions) if sessions.is_empty() => println!("— тихо —"),
            Ok(sessions) => {
                for s in sessions {
                    println!("АКТИВНА: {} (pid {})", s.process_name, s.pid);
                }
            }
            Err(e) => eprintln!("ошибка: {e}"),
        }
        thread::sleep(Duration::from_secs(2));
    }
}
```

- [ ] **Step 5: Собрать**

Run: `cd /mnt/c/Users/<username>/Projects/meeting-recorder && cargo.exe build`
Expected: компилируется. **Вероятны правки под конкретную версию `windows` 0.58** — сигнатуры `CoInitializeEx`/`Activate` менялись между версиями крейта. Если не собирается, свериться с [docs.rs/windows](https://docs.rs/windows/0.58.0/windows/Win32/Media/Audio/index.html) для точной сигнатуры, не угадывать.

- [ ] **Step 6: Проверить главное допущение вручную**

Run: `cargo.exe run`

Проверить **три** сценария — третий и есть ответ на открытый вопрос №1 из спеки:

1. Ничего не запущено → `— тихо —`.
2. Открыть Zoom/Meet и войти в звонок → появляется `АКТИВНА: Zoom.exe` (или `chrome.exe` для Meet во вкладке).
3. **Убить приложение (Ctrl+C), оставив звонок идти, и запустить снова** → сессия должна найтись **сразу**. Это доказывает, что поллинг видит сессии, созданные до старта — то, чего не умеет `IAudioSessionNotification`.

Если сценарий 3 не работает — остановиться и пересмотреть дизайн детекта, не идти дальше.

- [ ] **Step 7: Коммит**

```bash
cd /mnt/c/Users/<username>/Projects/meeting-recorder
git add Cargo.toml Cargo.lock src/
git commit -m "feat(detector): поллинг mic-сессий через WASAPI"
```

---

### Task 2: Кольцевой буфер

Чистая логика, ноль зависимостей от железа — тестируется целиком юнит-тестами.

**Files:**
- Create: `src/ringbuf.rs`
- Modify: `src/main.rs` (добавить `mod ringbuf;`)

**Interfaces:**
- Consumes: ничего
- Produces: `RingBuffer::new(capacity_samples: usize) -> Self`, `push_slice(&mut self, samples: &[i16])`, `drain_to_vec(&mut self) -> Vec<i16>`, `len(&self) -> usize`

- [ ] **Step 1: Написать падающие тесты**

Create `src/ringbuf.rs`:

```rust
/// Кольцевой буфер семплов. Держит последние N семплов; старое вытесняется.
/// Нужен, чтобы не потерять начало встречи, пока пользователь думает над тостом.
pub struct RingBuffer {
    // реализация в следующем шаге
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn пустой_буфер_отдаёт_пусто() {
        let mut b = RingBuffer::new(4);
        assert_eq!(b.drain_to_vec(), Vec::<i16>::new());
    }

    #[test]
    fn отдаёт_то_что_положили_если_влезло() {
        let mut b = RingBuffer::new(4);
        b.push_slice(&[1, 2, 3]);
        assert_eq!(b.drain_to_vec(), vec![1, 2, 3]);
    }

    #[test]
    fn вытесняет_старое_при_переполнении() {
        let mut b = RingBuffer::new(3);
        b.push_slice(&[1, 2, 3, 4, 5]);
        // влезают только последние 3
        assert_eq!(b.drain_to_vec(), vec![3, 4, 5]);
    }

    #[test]
    fn кусок_длиннее_ёмкости_не_ломает_буфер() {
        let mut b = RingBuffer::new(2);
        b.push_slice(&[1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(b.drain_to_vec(), vec![6, 7]);
    }

    #[test]
    fn drain_опустошает() {
        let mut b = RingBuffer::new(4);
        b.push_slice(&[1, 2]);
        let _ = b.drain_to_vec();
        assert_eq!(b.len(), 0);
        assert_eq!(b.drain_to_vec(), Vec::<i16>::new());
    }
}
```

- [ ] **Step 2: Убедиться, что тесты падают**

Run: `cargo.exe test ringbuf`
Expected: FAIL — `no function or associated item named 'new' found`

- [ ] **Step 3: Реализовать**

Заменить заглушку `pub struct RingBuffer` в `src/ringbuf.rs` на:

```rust
use std::collections::VecDeque;

pub struct RingBuffer {
    buf: VecDeque<i16>,
    capacity: usize,
}

impl RingBuffer {
    pub fn new(capacity_samples: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity_samples),
            capacity: capacity_samples,
        }
    }

    pub fn push_slice(&mut self, samples: &[i16]) {
        // Кусок длиннее ёмкости: интересен только его хвост.
        let tail = if samples.len() > self.capacity {
            &samples[samples.len() - self.capacity..]
        } else {
            samples
        };
        for &s in tail {
            if self.buf.len() == self.capacity {
                self.buf.pop_front();
            }
            self.buf.push_back(s);
        }
    }

    pub fn drain_to_vec(&mut self) -> Vec<i16> {
        self.buf.drain(..).collect()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}
```

- [ ] **Step 4: Убедиться, что тесты проходят**

Run: `cargo.exe test ringbuf`
Expected: PASS, 5 тестов

- [ ] **Step 5: Коммит**

```bash
git add src/ringbuf.rs src/main.rs
git commit -m "feat(ringbuf): кольцевой буфер семплов с вытеснением"
```

---

### Task 3: Стейт-машина сессии

Самая хитрая логика проекта — и полностью чистая. Именно здесь живёт правило, что причина старта определяет правило остановки.

**Files:**
- Create: `src/session.rs`
- Modify: `src/main.rs` (добавить `mod session;`)

**Interfaces:**
- Consumes: ничего
- Produces: `enum Trigger { Auto, Manual }`, `enum State { Idle, Armed, Recording(Trigger), Finalizing }`, `enum Event { SessionAppeared, SessionGone, UserConfirmed, UserDeclined, ManualStart, ManualStop, FinalizeDone }`, `enum Action { StartRingBuffer, DiscardRing, FlushRingToFile, StartFileWrite, CloseFile, None }`, `SessionMachine::new()`, `handle(&mut self, e: Event) -> Action`, `state(&self) -> State`

- [ ] **Step 1: Написать падающие тесты**

Create `src/session.rs`:

```rust
// реализация в шаге 3

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn детект_переводит_в_armed_и_запускает_кольцо() {
        let mut m = SessionMachine::new();
        assert_eq!(m.handle(Event::SessionAppeared), Action::StartRingBuffer);
        assert_eq!(m.state(), State::Armed);
    }

    #[test]
    fn подтверждение_сбрасывает_кольцо_в_файл() {
        let mut m = SessionMachine::new();
        m.handle(Event::SessionAppeared);
        assert_eq!(m.handle(Event::UserConfirmed), Action::FlushRingToFile);
        assert_eq!(m.state(), State::Recording(Trigger::Auto));
    }

    #[test]
    fn отказ_выбрасывает_кольцо_и_ничего_не_пишет_на_диск() {
        let mut m = SessionMachine::new();
        m.handle(Event::SessionAppeared);
        assert_eq!(m.handle(Event::UserDeclined), Action::DiscardRing);
        assert_eq!(m.state(), State::Idle);
    }

    #[test]
    fn сессия_исчезла_до_ответа_выбрасывает_кольцо() {
        let mut m = SessionMachine::new();
        m.handle(Event::SessionAppeared);
        assert_eq!(m.handle(Event::SessionGone), Action::DiscardRing);
        assert_eq!(m.state(), State::Idle);
    }

    #[test]
    fn авто_запись_останавливается_когда_сессия_исчезла() {
        let mut m = SessionMachine::new();
        m.handle(Event::SessionAppeared);
        m.handle(Event::UserConfirmed);
        assert_eq!(m.handle(Event::SessionGone), Action::CloseFile);
        assert_eq!(m.state(), State::Finalizing);
    }

    #[test]
    fn ручной_старт_из_idle_минует_armed_и_кольцо() {
        let mut m = SessionMachine::new();
        assert_eq!(m.handle(Event::ManualStart), Action::StartFileWrite);
        assert_eq!(m.state(), State::Recording(Trigger::Manual));
    }

    /// Гвоздь всей задачи: у ручной записи может вообще не быть mic-сессии
    /// (разговор в комнате, телефон на громкой). Общее правило «сессия исчезла →
    /// стоп» убило бы такую запись на первой секунде.
    #[test]
    fn ручную_запись_исчезновение_сессии_не_останавливает() {
        let mut m = SessionMachine::new();
        m.handle(Event::ManualStart);
        assert_eq!(m.handle(Event::SessionGone), Action::None);
        assert_eq!(m.state(), State::Recording(Trigger::Manual));
    }

    #[test]
    fn ручной_стоп_останавливает_ручную_запись() {
        let mut m = SessionMachine::new();
        m.handle(Event::ManualStart);
        assert_eq!(m.handle(Event::ManualStop), Action::CloseFile);
        assert_eq!(m.state(), State::Finalizing);
    }

    #[test]
    fn ручной_стоп_останавливает_и_авто_запись() {
        let mut m = SessionMachine::new();
        m.handle(Event::SessionAppeared);
        m.handle(Event::UserConfirmed);
        assert_eq!(m.handle(Event::ManualStop), Action::CloseFile);
        assert_eq!(m.state(), State::Finalizing);
    }

    #[test]
    fn ручной_старт_из_armed_равен_подтверждению() {
        let mut m = SessionMachine::new();
        m.handle(Event::SessionAppeared);
        assert_eq!(m.handle(Event::ManualStart), Action::FlushRingToFile);
        assert_eq!(m.state(), State::Recording(Trigger::Auto));
    }

    #[test]
    fn финализация_возвращает_в_idle() {
        let mut m = SessionMachine::new();
        m.handle(Event::ManualStart);
        m.handle(Event::ManualStop);
        assert_eq!(m.handle(Event::FinalizeDone), Action::None);
        assert_eq!(m.state(), State::Idle);
    }

    #[test]
    fn повторный_детект_во_время_записи_ничего_не_делает() {
        let mut m = SessionMachine::new();
        m.handle(Event::ManualStart);
        assert_eq!(m.handle(Event::SessionAppeared), Action::None);
        assert_eq!(m.state(), State::Recording(Trigger::Manual));
    }
}
```

- [ ] **Step 2: Убедиться, что тесты падают**

Run: `cargo.exe test session`
Expected: FAIL — `cannot find type 'SessionMachine' in this scope`

- [ ] **Step 3: Реализовать**

Вставить в начало `src/session.rs` (перед блоком `#[cfg(test)]`):

```rust
/// Причина старта записи. От неё зависит правило остановки — см. handle().
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Auto,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    /// Детект сработал, пишем в кольцо, ждём ответа пользователя.
    Armed,
    Recording(Trigger),
    Finalizing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    SessionAppeared,
    SessionGone,
    UserConfirmed,
    UserDeclined,
    ManualStart,
    ManualStop,
    FinalizeDone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    StartRingBuffer,
    DiscardRing,
    FlushRingToFile,
    StartFileWrite,
    CloseFile,
    None,
}

pub struct SessionMachine {
    state: State,
}

impl SessionMachine {
    pub fn new() -> Self {
        Self { state: State::Idle }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn handle(&mut self, e: Event) -> Action {
        use Event::*;
        use State::*;

        match (self.state, e) {
            (Idle, SessionAppeared) => {
                self.state = Armed;
                Action::StartRingBuffer
            }
            (Idle, ManualStart) => {
                self.state = Recording(Trigger::Manual);
                Action::StartFileWrite
            }

            // Подтверждение и ручной старт из Armed — одно и то же:
            // кольцо уже набрано, сбрасываем его в файл.
            (Armed, UserConfirmed) | (Armed, ManualStart) => {
                self.state = Recording(Trigger::Auto);
                Action::FlushRingToFile
            }
            (Armed, UserDeclined) | (Armed, SessionGone) => {
                self.state = Idle;
                Action::DiscardRing
            }

            // Ключевое место: исчезновение mic-сессии останавливает ТОЛЬКО
            // авто-запись. У ручной сессии могло не быть вовсе.
            (Recording(Trigger::Auto), SessionGone) => {
                self.state = Finalizing;
                Action::CloseFile
            }
            (Recording(Trigger::Manual), SessionGone) => Action::None,

            (Recording(_), ManualStop) => {
                self.state = Finalizing;
                Action::CloseFile
            }

            (Finalizing, FinalizeDone) => {
                self.state = Idle;
                Action::None
            }

            // Всё остальное — шум (повторный детект во время записи и т.п.)
            _ => Action::None,
        }
    }
}

impl Default for SessionMachine {
    fn default() -> Self {
        Self::new()
    }
}
```

- [ ] **Step 4: Убедиться, что тесты проходят**

Run: `cargo.exe test session`
Expected: PASS, 12 тестов

- [ ] **Step 5: Коммит**

```bash
git add src/session.rs src/main.rs
git commit -m "feat(session): стейт-машина записи, стоп зависит от причины старта"
```

---

### Task 4: Хранилище — именование и запись WAV

**Files:**
- Create: `src/storage.rs`
- Modify: `Cargo.toml` (добавить `hound`, `chrono`)
- Modify: `src/main.rs` (добавить `mod storage;`)

**Interfaces:**
- Consumes: ничего
- Produces: `enum Track { Mic, System }`, `recording_filename(started: DateTime<Local>, source: &str, track: Track) -> String`, `WavSink::create(dir: &Path, filename: &str) -> Result<Self>`, `WavSink::write(&mut self, samples: &[i16]) -> Result<()>`, `WavSink::finalize(self) -> Result<PathBuf>`

- [ ] **Step 1: Добавить зависимости**

Modify `Cargo.toml`, в `[dependencies]`:

```toml
hound = "3.5"
chrono = "0.4"
```

- [ ] **Step 2: Написать падающие тесты именования**

Create `src/storage.rs`:

```rust
// реализация в шаге 4

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn момент() -> chrono::DateTime<chrono::Local> {
        chrono::Local.with_ymd_and_hms(2026, 7, 17, 14, 30, 0).unwrap()
    }

    #[test]
    fn имя_для_дорожки_микрофона() {
        assert_eq!(
            recording_filename(момент(), "zoom", Track::Mic),
            "2026-07-17_14-30_zoom.mic.wav"
        );
    }

    #[test]
    fn имя_для_системной_дорожки() {
        assert_eq!(
            recording_filename(момент(), "zoom", Track::System),
            "2026-07-17_14-30_zoom.system.wav"
        );
    }

    #[test]
    fn ручная_запись_помечается_как_manual() {
        assert_eq!(
            recording_filename(момент(), "manual", Track::Mic),
            "2026-07-17_14-30_manual.mic.wav"
        );
    }

    #[test]
    fn имя_процесса_чистится_от_exe_и_регистра() {
        assert_eq!(
            recording_filename(момент(), "Zoom.exe", Track::Mic),
            "2026-07-17_14-30_zoom.mic.wav"
        );
    }

    #[test]
    fn небезопасные_символы_в_имени_процесса_заменяются() {
        assert_eq!(
            recording_filename(момент(), "My App v2.exe", Track::Mic),
            "2026-07-17_14-30_my-app-v2.mic.wav"
        );
    }
}
```

- [ ] **Step 3: Убедиться, что тесты падают**

Run: `cargo.exe test storage`
Expected: FAIL — `cannot find function 'recording_filename'`

- [ ] **Step 4: Реализовать**

Вставить в начало `src/storage.rs`:

```rust
use chrono::{DateTime, Local};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    Mic,
    System,
}

impl Track {
    fn suffix(self) -> &'static str {
        match self {
            Track::Mic => "mic",
            Track::System => "system",
        }
    }
}

pub const SAMPLE_RATE: u32 = 16_000;

/// `2026-07-17_14-30_zoom.mic.wav`
pub fn recording_filename(started: DateTime<Local>, source: &str, track: Track) -> String {
    format!(
        "{}_{}.{}.wav",
        started.format("%Y-%m-%d_%H-%M"),
        sanitize_source(source),
        track.suffix()
    )
}

/// `Zoom.exe` → `zoom`, `My App v2.exe` → `my-app-v2`
fn sanitize_source(source: &str) -> String {
    let stem = source.strip_suffix(".exe").unwrap_or(source);
    let cleaned: String = stem
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    // схлопнуть повторы дефисов и обрезать по краям
    let mut out = String::with_capacity(cleaned.len());
    for c in cleaned.chars() {
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    out.trim_matches('-').to_string()
}

/// Инкрементальная запись 16 кГц моно 16 бит WAV.
pub struct WavSink {
    writer: hound::WavWriter<std::io::BufWriter<std::fs::File>>,
    path: PathBuf,
}

impl WavSink {
    pub fn create(dir: &Path, filename: &str) -> Result<Self, hound::Error> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(filename);
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::create(&path, spec)?;
        Ok(Self { writer, path })
    }

    pub fn write(&mut self, samples: &[i16]) -> Result<(), hound::Error> {
        for &s in samples {
            self.writer.write_sample(s)?;
        }
        Ok(())
    }

    /// Дописывает WAV-заголовок с реальной длиной. Без этого файл битый.
    pub fn finalize(self) -> Result<PathBuf, hound::Error> {
        self.writer.finalize()?;
        Ok(self.path)
    }
}
```

- [ ] **Step 5: Убедиться, что тесты проходят**

Run: `cargo.exe test storage`
Expected: PASS, 5 тестов

- [ ] **Step 6: Коммит**

```bash
git add Cargo.toml Cargo.lock src/storage.rs src/main.rs
git commit -m "feat(storage): именование записей и инкрементальный WAV-райтер"
```

---

### Task 5: Захват аудио — микрофон и системный loopback

**Files:**
- Create: `src/capture/mod.rs`
- Modify: `Cargo.toml` (добавить `cpal`)
- Modify: `src/main.rs` (добавить `mod capture;`)

**Interfaces:**
- Consumes: `storage::SAMPLE_RATE`
- Produces: `enum Source { Mic, SystemLoopback }`, `start_capture(source: Source, sink: Sender<Vec<i16>>) -> Result<cpal::Stream, CaptureError>`, `fn downmix_to_mono_i16(data: &[f32], channels: u16) -> Vec<i16>`

- [ ] **Step 1: Добавить зависимость**

Modify `Cargo.toml`:

```toml
cpal = "0.15"
```

- [ ] **Step 2: Написать падающие тесты для чистой части (сведение в моно)**

Create `src/capture/mod.rs`:

```rust
// реализация ниже

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn моно_проходит_насквозь() {
        let out = downmix_to_mono_i16(&[0.0, 1.0, -1.0], 1);
        assert_eq!(out, vec![0, i16::MAX, i16::MIN + 1]);
    }

    #[test]
    fn стерео_усредняется_в_моно() {
        // два кадра стерео: (1.0, -1.0) → 0.0, (0.5, 0.5) → 0.5
        let out = downmix_to_mono_i16(&[1.0, -1.0, 0.5, 0.5], 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], 0);
        assert!((out[1] as f32 - 0.5 * i16::MAX as f32).abs() < 2.0);
    }

    #[test]
    fn клиппинг_не_переполняется() {
        // cpal может отдать значения за пределами [-1, 1]
        let out = downmix_to_mono_i16(&[5.0, -5.0], 1);
        assert_eq!(out, vec![i16::MAX, i16::MIN + 1]);
    }

    #[test]
    fn пустой_вход_даёт_пустой_выход() {
        assert_eq!(downmix_to_mono_i16(&[], 2), Vec::<i16>::new());
    }
}
```

- [ ] **Step 3: Убедиться, что тесты падают**

Run: `cargo.exe test capture`
Expected: FAIL — `cannot find function 'downmix_to_mono_i16'`

- [ ] **Step 4: Реализовать**

Вставить в начало `src/capture/mod.rs`:

```rust
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::mpsc::Sender;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Mic,
    SystemLoopback,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("устройство не найдено: {0:?}")]
    NoDevice(Source),
    #[error("ошибка cpal: {0}")]
    Build(#[from] cpal::BuildStreamError),
    #[error("ошибка запуска потока: {0}")]
    Play(#[from] cpal::PlayStreamError),
    #[error("ошибка конфигурации: {0}")]
    Config(#[from] cpal::DefaultStreamConfigError),
}

/// Сводит интерливнутые f32-кадры в моно i16 с клиппингом.
/// Вынесено отдельно от cpal — это единственная часть захвата, которую
/// можно проверить без железа.
pub fn downmix_to_mono_i16(data: &[f32], channels: u16) -> Vec<i16> {
    let ch = channels.max(1) as usize;
    data.chunks_exact(ch)
        .map(|frame| {
            let avg = frame.iter().sum::<f32>() / ch as f32;
            let clamped = avg.clamp(-1.0, 1.0);
            (clamped * i16::MAX as f32) as i16
        })
        .collect()
}

pub fn start_capture(source: Source, sink: Sender<Vec<i16>>) -> Result<cpal::Stream, CaptureError> {
    let host = cpal::default_host();
    let device = match source {
        Source::Mic => host.default_input_device(),
        // WASAPI loopback: cpal отдаёт output-устройство как источник ввода.
        Source::SystemLoopback => host.default_output_device(),
    }
    .ok_or(CaptureError::NoDevice(source))?;

    let config = match source {
        Source::Mic => device.default_input_config()?,
        Source::SystemLoopback => device.default_output_config()?,
    };
    let channels = config.channels();

    let stream = device.build_input_stream(
        &config.into(),
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            let mono = downmix_to_mono_i16(data, channels);
            // Приёмник мог отвалиться (запись остановлена) — это не ошибка.
            let _ = sink.send(mono);
        },
        |err| eprintln!("ошибка потока захвата: {err}"),
        None,
    )?;
    stream.play()?;
    Ok(stream)
}
```

- [ ] **Step 5: Убедиться, что тесты проходят**

Run: `cargo.exe test capture`
Expected: PASS, 4 теста

- [ ] **Step 6: Проверить на живом железе**

Временно заменить `main()` на захват 5 секунд с микрофона в файл и убедиться, что WAV открывается и в нём слышен голос. Затем то же для `Source::SystemLoopback` при играющей музыке.

> **Ожидаемая проблема.** `cpal` на Windows может не отдавать loopback через `default_output_device()` напрямую — поддержка WASAPI loopback в `cpal` менялась. Если не заработает, свериться с [cpal changelog](https://github.com/RustAudio/cpal/releases) и примером `beep`/`record_wav`; при необходимости взять устройство из `host.output_devices()` явно. **Не выдумывать API — читать доки версии 0.15.**
>
> Второй момент: частота устройства почти наверняка **не 16 кГц** (обычно 44.1/48 кГц). Ресемплинг в 16 кГц в MVP не реализован — если частоты не совпадут, добавить крейт `rubato` и ресемплить перед `sink.send`. Это известный пробел плана, вскроется здесь.

- [ ] **Step 7: Коммит**

```bash
git add Cargo.toml Cargo.lock src/capture/ src/main.rs
git commit -m "feat(capture): захват микрофона и системного loopback, сведение в моно"
```

---

### Task 6: Сборка целого без GUI

Первая точка, где всё работает end-to-end. Подтверждение — с консоли, не тостом: GUI пока нет, а логику проверить надо уже сейчас.

**Files:**
- Modify: `src/main.rs` (полностью переписать)
- Create: `src/app.rs`

**Interfaces:**
- Consumes: `detector::{MeetingDetector, WindowsDetector, MicSession}`, `session::{SessionMachine, Event, Action, Trigger}`, `ringbuf::RingBuffer`, `storage::{WavSink, Track, recording_filename, SAMPLE_RATE}`, `capture::{start_capture, Source}`
- Produces: `App::new(recordings_dir: PathBuf) -> Self`, `App::tick(&mut self) -> Result<()>` — один шаг цикла

- [ ] **Step 1: Написать оркестратор**

Create `src/app.rs`:

```rust
use crate::capture::{start_capture, Source};
use crate::detector::{MeetingDetector, MicSession};
use crate::ringbuf::RingBuffer;
use crate::session::{Action, Event, SessionMachine};
use crate::storage::{recording_filename, Track, WavSink, SAMPLE_RATE};
use chrono::Local;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

const RING_SECONDS: usize = 30;
const RING_CAPACITY: usize = SAMPLE_RATE as usize * RING_SECONDS;

/// Живут только пока идёт детект или запись. Drop останавливает захват,
/// поэтому индикатор микрофона в Windows гаснет сразу после Idle.
struct Streams {
    _mic: cpal::Stream,
    _sys: cpal::Stream,
    rx_mic: Receiver<Vec<i16>>,
    rx_sys: Receiver<Vec<i16>>,
}

pub struct App {
    machine: SessionMachine,
    dir: PathBuf,
    ring_mic: RingBuffer,
    ring_sys: RingBuffer,
    sink_mic: Option<WavSink>,
    sink_sys: Option<WavSink>,
    streams: Option<Streams>,
    current_source: String,
    started: chrono::DateTime<Local>,
}

impl App {
    /// Микрофон здесь НЕ открывается. Потоки поднимаются только по детекту
    /// или по ручному старту — см. open_streams().
    pub fn new(dir: PathBuf) -> Self {
        Self {
            machine: SessionMachine::new(),
            dir,
            ring_mic: RingBuffer::new(RING_CAPACITY),
            ring_sys: RingBuffer::new(RING_CAPACITY),
            sink_mic: None,
            sink_sys: None,
            streams: None,
            current_source: "manual".into(),
            started: Local::now(),
        }
    }

    fn open_streams(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.streams.is_some() {
            return Ok(());
        }
        let (tx_mic, rx_mic) = channel();
        let (tx_sys, rx_sys) = channel();
        let _mic = start_capture(Source::Mic, tx_mic)?;
        let _sys = start_capture(Source::SystemLoopback, tx_sys)?;
        self.streams = Some(Streams { _mic, _sys, rx_mic, rx_sys });
        Ok(())
    }

    /// Drop у cpal::Stream останавливает захват — этого достаточно.
    fn close_streams(&mut self) {
        self.streams = None;
    }

    /// Забрать всё, что накопилось в каналах захвата.
    fn drain_channels(&mut self) -> (Vec<i16>, Vec<i16>) {
        match &self.streams {
            Some(s) => (
                s.rx_mic.try_iter().flatten().collect(),
                s.rx_sys.try_iter().flatten().collect(),
            ),
            None => (Vec::new(), Vec::new()),
        }
    }

    pub fn on_event(&mut self, e: Event, source: Option<&MicSession>) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(s) = source {
            self.current_source = s.process_name.clone();
        }
        let action = self.machine.handle(e);
        self.apply(action)
    }

    fn apply(&mut self, action: Action) -> Result<(), Box<dyn std::error::Error>> {
        match action {
            Action::StartRingBuffer => {
                self.started = Local::now();
                // Микрофон берётся ИМЕННО ЗДЕСЬ — на детекте, до вопроса.
                // Индикатор мика в Windows загорается в этот момент.
                self.open_streams()?;
            }
            Action::DiscardRing => {
                self.ring_mic.drain_to_vec();
                self.ring_sys.drain_to_vec();
                self.close_streams(); // отказ — отпускаем микрофон немедленно
            }
            Action::StartFileWrite => {
                self.started = Local::now();
                self.current_source = "manual".into();
                self.open_streams()?;
                self.open_sinks()?;
            }
            Action::FlushRingToFile => {
                self.open_sinks()?;
                let mic = self.ring_mic.drain_to_vec();
                let sys = self.ring_sys.drain_to_vec();
                if let Some(w) = self.sink_mic.as_mut() { w.write(&mic)?; }
                if let Some(w) = self.sink_sys.as_mut() { w.write(&sys)?; }
            }
            Action::CloseFile => {
                let result = self.close_sinks();
                self.close_streams();
                // FinalizeDone обязан уйти в машину ДАЖЕ при ошибке закрытия.
                // Иначе она навсегда останется в Finalizing, откуда единственный
                // выход — это событие, и приложение молча перестанет записывать
                // что-либо вообще. В Task 7 (аудио-цикл в фоновом потоке под Tauri)
                // это будет выглядеть как живой GUI, который ничего не пишет.
                self.machine.handle(Event::FinalizeDone);
                result?;
            }
            Action::None => {}
        }
        Ok(())
    }

    fn open_sinks(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let src = self.current_source.clone();
        self.sink_mic = Some(WavSink::create(&self.dir, &recording_filename(self.started, &src, Track::Mic))?);
        self.sink_sys = Some(WavSink::create(&self.dir, &recording_filename(self.started, &src, Track::System))?);
        Ok(())
    }

    /// Дописывает хвост и финализирует обе дорожки.
    ///
    /// Ошибку дописывания запоминаем, но НЕ выходим по `?`: финализировать
    /// файл надо в любом случае. hound пишет длину данных в заголовок только
    /// на finalize(); без него на диске останется WAV, который существует,
    /// весит сотни мегабайт, открывается плеером — и играет тишину.
    /// Отказ здесь выглядит как успех, поэтому дорожка финализируется даже
    /// тогда, когда запись хвоста в неё провалилась.
    fn close_sinks(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Хвост, натёкший между последним pump_audio и стопом.
        let (mic, sys) = self.drain_channels();
        let mut first_err: Option<Box<dyn std::error::Error>> = None;

        if let Some(w) = self.sink_mic.as_mut() {
            if let Err(e) = w.write(&mic) { first_err.get_or_insert(e.into()); }
        }
        if let Some(w) = self.sink_sys.as_mut() {
            if let Err(e) = w.write(&sys) { first_err.get_or_insert(e.into()); }
        }
        for sink in [self.sink_mic.take(), self.sink_sys.take()].into_iter().flatten() {
            match sink.finalize() {
                Ok(p) => println!("записано: {}", p.display()),
                Err(e) => { first_err.get_or_insert(e.into()); }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Прокачать накопленные семплы туда, куда велит текущее состояние.
    pub fn pump_audio(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        use crate::session::State;
        let (mic, sys) = self.drain_channels();
        match self.machine.state() {
            State::Armed => {
                self.ring_mic.push_slice(&mic);
                self.ring_sys.push_slice(&sys);
            }
            State::Recording(_) => {
                if let Some(w) = self.sink_mic.as_mut() { w.write(&mic)?; }
                if let Some(w) = self.sink_sys.as_mut() { w.write(&sys)?; }
            }
            _ => {}
        }
        Ok(())
    }
}
```

- [ ] **Step 2: Добавить хелпер состояния в `App`**

Добавить в `impl App` в `src/app.rs` (нужен `main.rs` в следующем шаге):

```rust
    pub fn state_is_armed(&self) -> bool {
        matches!(self.machine.state(), crate::session::State::Armed)
    }
```

- [ ] **Step 3: Переписать `main.rs` под консольный цикл**

Replace `src/main.rs`:

```rust
mod app;
mod capture;
mod detector;
mod ringbuf;
mod session;
mod storage;

use app::App;
use detector::{MeetingDetector, MicSession, WindowsDetector, POLL_INTERVAL};
use session::Event;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::{thread, time::Duration, time::Instant};

/// Ввод с консоли в отдельном потоке, чтобы не блокировать аудио-цикл.
fn spawn_stdin() -> Receiver<String> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines().map_while(Result::ok) {
            if tx.send(line.trim().to_lowercase()).is_err() {
                break;
            }
        }
    });
    rx
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = PathBuf::from(r"C:\Users\<username>\Recordings");
    let det = WindowsDetector::new()?;
    let mut app = App::new(dir);
    let input = spawn_stdin();

    println!("Готов. Команды: s — старт вручную, x — стоп, y/n — ответ на предложение.");
    let mut was_active = false;
    let mut asked = false;
    let mut active: Option<MicSession> = None;
    // Первый опрос — сразу, без ожидания интервала.
    let mut last_poll = Instant::now() - POLL_INTERVAL;

    loop {
        // Детектор опрашивается раз в POLL_INTERVAL (2 с) — это глобальное
        // ограничение. Сам цикл крутится в 10 раз чаще, но только ради
        // отзывчивости ввода и прокачки аудио, а не ради детекта.
        if last_poll.elapsed() >= POLL_INTERVAL {
            last_poll = Instant::now();
            active = det.poll().unwrap_or_default().into_iter().next();
            let is_active = active.is_some();

            if is_active && !was_active {
                app.on_event(Event::SessionAppeared, active.as_ref())?;
                if app.state_is_armed() {
                    println!("Похоже, встреча ({}). Записать? y/n", active.as_ref().unwrap().process_name);
                    asked = true;
                }
            } else if !is_active && was_active {
                app.on_event(Event::SessionGone, None)?;
                asked = false;
            }
            was_active = is_active;
        }

        for cmd in input.try_iter() {
            // Любая из четырёх команд снимает висящий вопрос: иначе `s` в Armed
            // оставил бы asked=true, и следующее `n` прилетело бы уже в
            // Recording, где UserDeclined проглатывается — человек сказал
            // «не записывать», а запись продолжила бы литься на диск.
            let e = match cmd.as_str() {
                "y" if asked => Some(Event::UserConfirmed),
                "n" if asked => Some(Event::UserDeclined),
                "s" => Some(Event::ManualStart),
                "x" => Some(Event::ManualStop),
                _ => None,
            };
            if let Some(e) = e {
                asked = false;
                app.on_event(e, active.as_ref())?;
            }
        }

        app.pump_audio()?;
        std::io::stdout().flush().ok();
        thread::sleep(Duration::from_millis(200));
    }
}
```

- [ ] **Step 4: Собрать и прогнать все тесты**

Run: `cargo.exe test && cargo.exe build`
Expected: все тесты Task 2–5 проходят, бинарь собирается

- [ ] **Step 5: Проверить end-to-end вручную**

Run: `cargo.exe run`

Три сценария:
1. **Авто:** зайти в звонок → появляется вопрос → `y` → поговорить → выйти из звонка → в `C:\Users\<username>\Recordings` две дорожки, **и в начале слышно то, что было до ответа `y`** (проверка кольцевого буфера).
2. **Отказ:** зайти в звонок → `n` → выйти. В каталоге **не появилось ничего**.
3. **Ручной:** без всякого звонка → `s` → поговорить → `x` → две дорожки с префиксом `manual`.

- [ ] **Step 6: Коммит**

```bash
git add src/
git commit -m "feat(app): сборка end-to-end через консоль, без GUI"
```

---

### Task 7: Tauri-оболочка — трей, тост, окно, хоткей

**Files:**
- Create: `src-tauri/` (через `cargo create-tauri-app`)
- Modify: структура проекта — текущие `src/*.rs` переезжают в `src-tauri/src/`
- Create: `src/index.html`, `src/main.js` (фронтенд со списком записей)

**Interfaces:**
- Consumes: всё из Task 6
- Produces: GUI

- [ ] **Step 1: Инициализировать Tauri поверх существующего кода**

Run: `cd /mnt/c/Users/<username>/Projects/meeting-recorder && npm.cmd create tauri-app@latest -- --template vanilla`

Затем перенести модули из `src/*.rs` в `src-tauri/src/` и переключить `main.rs` на Tauri-точку входа. Аудио-цикл из Task 6 переезжает в фоновый поток, поднимаемый в `tauri::Builder::setup`.

- [ ] **Step 2: Добавить плагины трея и хоткея**

Modify `src-tauri/Cargo.toml`:

```toml
tauri = { version = "2", features = ["tray-icon"] }
tauri-plugin-global-shortcut = "2"
tauri-plugin-notification = "2"
```

- [ ] **Step 3: Пробросить события из аудио-цикла в UI**

Фоновый поток шлёт `app_handle.emit("state-changed", state)`; фронтенд подписывается и красит иконку трея. Тост-предложение — через `tauri-plugin-notification` с кнопками «Записать» / «Нет», ответ уходит обратно в цикл через канал.

- [ ] **Step 4: Глобальный хоткей**

Зарегистрировать `Ctrl+Shift+R` на toggle записи (шлёт `Event::ManualStart` / `Event::ManualStop` в зависимости от состояния).

- [ ] **Step 5: Окно со списком записей**

Читает каталог `C:\Users\<username>\Recordings`, группирует по префиксу `YYYY-MM-DD_HH-MM_source`, показывает пары дорожек, даёт кнопку «открыть папку».

- [ ] **Step 6: Проверить**

Run: `npm.cmd run tauri dev`

Проверить все три сценария из Task 6, но через GUI: тост появляется, кнопки работают, хоткей стартует/останавливает, список пополняется, иконка трея отражает состояние.

- [ ] **Step 7: Коммит**

```bash
git add -A
git commit -m "feat(ui): Tauri-оболочка — трей, тост-предложение, хоткей, список записей"
```

---

## Известные пробелы плана

Названы явно, чтобы не выглядели сюрпризом:

1. **Ресемплинг не спроектирован.** Устройства почти наверняка отдают 44.1/48 кГц, а формат требует 16 кГц. Вскроется в Task 5 Step 6; лечится крейтом `rubato`. Если это окажется большой работой — выделить в отдельную задачу между Task 5 и Task 6.
2. **Точные сигнатуры `windows-rs` 0.58 не проверены компиляцией** — код детектора в Task 1 написан по устройству API. Task 1 Step 5 это и вскроет.
3. **Loopback в `cpal` 0.15 может потребовать другого способа получить устройство** — отмечено в Task 5 Step 6.
4. **`cpal::Stream` не `Send`** на WASAPI — его нельзя перекинуть между потоками. В Task 6 это неважно (всё в одном цикле), но в Task 7 аудио-цикл уезжает в фоновый поток, и `App` целиком должен жить **на том же потоке**, где создаются потоки захвата. Общаться с UI — только сообщениями через канал, не шарингом `App`. Если это проигнорировать, Task 7 упрётся в ошибку компиляции, которая выглядит как проблема Tauri, а на самом деле идёт отсюда.
5. **Task 7 менее детализирован, чем остальные** — сознательно: его форма зависит от того, что вскроется в Task 1–6. Это осознанный долг плана, а не недосмотр: перед выполнением Task 7 стоит перечитать и дописать его, опираясь на то, что выяснится в Task 1 и Task 5.
