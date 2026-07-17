use super::{DetectError, MeetingDetector, MicSession};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};
use windows::core::Interface;
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::Media::Audio::{
    eCapture, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};

/// Не чаще раза в это окно печатаем в stderr сообщение о пропущенном
/// устройстве/сессии. `poll()` зовётся раз в [`super::POLL_INTERVAL`] (2с)
/// бесконечно — без троттлинга стабильно отваливающееся устройство залило бы
/// stderr потоком одинаковых строк. Троттлинг глобальный (не per-device):
/// нам важно не молчать совсем, а не вести точный учёт того, какое именно
/// устройство сейчас шумит — это всё равно видно в тексте самого сообщения.
const SKIP_LOG_THROTTLE_SECS: u64 = 30;
static LAST_SKIP_LOG_SECS: AtomicU64 = AtomicU64::new(0);

/// Логирует отказ отдельного устройства/сессии в stderr с троттлингом (см.
/// [`SKIP_LOG_THROTTLE_SECS`]). Не паникует и не глотает молча — просто не
/// каждый вызов долетает до печати.
fn log_skip(context: &str, err: &windows::core::Error) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = LAST_SKIP_LOG_SECS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < SKIP_LOG_THROTTLE_SECS {
        return;
    }
    // CAS, а не просто store: если два потока (в теории — см. докблок про
    // !Send, на практике это один поток) одновременно проходят проверку
    // выше, напечатать должен только один.
    if LAST_SKIP_LOG_SECS
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        eprintln!("[meeting-recorder] пропускаю {context}: {err}");
    }
}

/// Детектор mic-сессий поверх WASAPI.
///
/// # Тип `!Send` и `!Sync` — это контракт, а не недоработка
///
/// Внутри лежит `IMMDeviceEnumerator`, а он обёрнут вокруг `IUnknown(NonNull<c_void>)`,
/// из-за чего `WindowsDetector` автоматически становится `!Send + !Sync`. Так и
/// должно быть: `CoInitializeEx` инициализирует COM-апартамент *потока*, и
/// полученный в нём COM-указатель действителен только в этом апартаменте.
///
/// Практическое следствие: экземпляр **нельзя перекидывать между потоками** — его
/// надо конструировать через [`WindowsDetector::new`] внутри того самого потока,
/// который будет звать [`poll`]. Оборачивание в `Arc`/`Mutex` не помогает: проблема
/// не в гонке за данные, а в привязке к апартаменту. И тем более сюда **нельзя
/// дописывать `unsafe impl Send`** — это не перенесёт апартамент, а лишь спрячет
/// контракт и даст UB.
///
/// Когда аудио-цикл уедет в фоновый поток под Tauri (Task 7), правильная форма —
/// создать детектор *внутри* `std::thread::spawn(...)`, а наружу отдавать уже
/// готовые [`MicSession`] через канал. Если вместо этого попытаться передать в
/// поток сам детектор, компилятор выдаст E0277 — «`NonNull<c_void>` cannot be sent
/// between threads safely», с цепочкой `IUnknown` -> `IMMDeviceEnumerator` ->
/// `WindowsDetector`. Ошибка выглядит загадочно (про `NonNull` мы ничего не писали),
/// но это ожидаемое поведение, а не баг сборки.
///
/// [`poll`]: MeetingDetector::poll
pub struct WindowsDetector {
    enumerator: IMMDeviceEnumerator,
}

impl WindowsDetector {
    /// Конструировать строго в том потоке, который будет опрашивать детектор
    /// (см. заметку про `!Send` в доке типа).
    pub fn new() -> Result<Self, DetectError> {
        unsafe {
            // Инициализируем COM-апартамент этого потока. HRESULT разбираем явно:
            // глушить здесь всё подряд нельзя — при E_OUTOFMEMORY/E_INVALIDARG COM
            // не поднялся, и CoCreateInstance ниже всё равно упадёт, только уже с
            // менее внятной причиной.
            match CoInitializeEx(None, COINIT_MULTITHREADED) {
                // Апартамент подняли мы.
                S_OK => {}
                // COM на этом потоке уже был инициализирован в том же режиме —
                // просто увеличился счётчик ссылок.
                S_FALSE => {}
                // Поток уже вошёл в STA, а мы просим MTA. Сменить режим уже
                // существующего апартамента нельзя, но апартамент есть и работать
                // в нём можно — продолжаем. Под Tauri это реальный сценарий:
                // UI-поток инициализирует COM как STA.
                RPC_E_CHANGED_MODE => {}
                hr => return Err(DetectError::Com(hr.into())),
            }
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            Ok(Self { enumerator })
        }
    }
}

impl MeetingDetector for WindowsDetector {
    fn poll(&self) -> Result<Vec<MicSession>, DetectError> {
        let mut out: Vec<MicSession> = Vec::new();
        unsafe {
            // eCapture — устройства ЗАПИСИ. Именно на них висят сессии тех, кто
            // держит микрофон.
            //
            // Перечисляем ВСЕ активные capture-эндпоинты, а не дефолтный, потому
            // что дефолтный пропускает реальные звонки:
            //  * роли eConsole и eCommunications в Windows раздельные, и Zoom/Teams
            //    с настройкой «Same as System» идут в communications-эндпоинт —
            //    если дефолты разъехались, дефолтный console вернёт пусто;
            //  * если в звонилке явно выбран не-дефолтный мик, сессия висит на нём
            //    и через дефолтный эндпоинт не видна вообще.
            //
            // Пустая коллекция (мика нет вовсе — например, выдернули гарнитуру) —
            // не ошибка, а отсутствие сессий: цикл просто не сделает ни итерации,
            // и наружу уйдёт пустой Vec.
            let devices = self
                .enumerator
                .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;

            // `?` наружу из poll() оставлен только на самом EnumAudioEndpoints
            // (и на GetCount() коллекции сразу над ним — это ещё часть «вижу
            // ли я устройства вообще», а не отказ конкретного устройства).
            // Дальше — отказ ОДНОГО устройства или ОДНОЙ сессии не должен
            // хоронить результаты остальных: устройства опрашиваются по
            // очереди, и, например, гарнитуру могут выдернуть между
            // EnumAudioEndpoints и Activate (AUDCLNT_E_DEVICE_INVALIDATED).
            // Частичный результат лучше отсутствующего — если один мик
            // отвалился, а на другом идёт звонок, мы обязаны его увидеть.
            for d in 0..devices.GetCount()? {
                let device = match devices.Item(d) {
                    Ok(device) => device,
                    Err(e) => {
                        log_skip(&format!("устройство #{d} (Item)"), &e);
                        continue;
                    }
                };
                let manager: IAudioSessionManager2 = match device.Activate(CLSCTX_ALL, None) {
                    Ok(manager) => manager,
                    Err(e) => {
                        log_skip(&format!("устройство #{d} (Activate)"), &e);
                        continue;
                    }
                };
                let sessions = match manager.GetSessionEnumerator() {
                    Ok(sessions) => sessions,
                    Err(e) => {
                        log_skip(&format!("устройство #{d} (GetSessionEnumerator)"), &e);
                        continue;
                    }
                };
                let count = match sessions.GetCount() {
                    Ok(count) => count,
                    Err(e) => {
                        log_skip(&format!("устройство #{d} (GetCount сессий)"), &e);
                        continue;
                    }
                };

                for i in 0..count {
                    let ctl = match sessions.GetSession(i) {
                        Ok(ctl) => ctl,
                        Err(e) => {
                            log_skip(&format!("сессия #{i} устройства #{d} (GetSession)"), &e);
                            continue;
                        }
                    };
                    let state = match ctl.GetState() {
                        Ok(state) => state,
                        Err(e) => {
                            // Сессия могла умереть между GetSession и GetState —
                            // тоже отказ ОДНОЙ сессии, а не всего опроса.
                            log_skip(&format!("сессия #{i} устройства #{d} (GetState)"), &e);
                            continue;
                        }
                    };
                    if state != AudioSessionStateActive {
                        continue;
                    }
                    let ctl2: IAudioSessionControl2 = match ctl.cast() {
                        Ok(ctl2) => ctl2,
                        Err(e) => {
                            log_skip(&format!("сессия #{i} устройства #{d} (cast)"), &e);
                            continue;
                        }
                    };
                    let pid = match ctl2.GetProcessId() {
                        Ok(pid) => pid,
                        Err(e) => {
                            log_skip(&format!("сессия #{i} устройства #{d} (GetProcessId)"), &e);
                            continue;
                        }
                    };
                    if pid == 0 {
                        continue; // системная сессия, не процесс
                    }
                    // Раз мы обходим все устройства, один процесс может дать
                    // несколько сессий (на разных эндпоинтах или несколько на
                    // одном). Наружу интересен факт «процесс держит мик», поэтому
                    // схлопываем по pid. Проверка до push'а, а не dedup после:
                    // так на дубли не тратится дорогой process_name_for.
                    if out.iter().any(|s| s.pid == pid) {
                        continue;
                    }
                    out.push(MicSession {
                        pid,
                        process_name: process_name_for(pid),
                    });
                }
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
