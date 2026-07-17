use super::{DetectError, MeetingDetector, MicSession};
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

            for d in 0..devices.GetCount()? {
                let device = devices.Item(d)?;
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
