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
