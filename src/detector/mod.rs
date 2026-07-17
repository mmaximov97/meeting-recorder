pub mod windows;

use std::time::Duration;

pub use self::windows::WindowsDetector;

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

/// Интервал опроса детектора — единственный источник правды для всех вызывающих.
///
/// Поллинг, а не события: перечисление каждый раз видит все сессии, включая
/// созданные до старта приложения.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Опрашивается раз в [`POLL_INTERVAL`].
pub trait MeetingDetector {
    fn poll(&self) -> Result<Vec<MicSession>, DetectError>;
}
