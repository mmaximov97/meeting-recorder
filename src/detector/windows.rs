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
