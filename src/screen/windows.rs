//! Запись экрана на Windows: Windows Graphics Capture (крейт `windows-capture`)
//! и его кодировщик Media Foundation в mp4.
//!
//! Только «весь экран». Окно WGC писать умеет, но кодировщик держит размер
//! кадра с первого кадра и при росте окна обрезает картинку, а проверить
//! это на живой Windows было негде. Режим «окно» поэтому на Windows скрыт
//! ([`support`]), а не обещан вслепую.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::encoder::{
    AudioSettingsBuilder, ContainerSettingsBuilder, VideoEncoder, VideoSettingsBuilder, VideoSettingsSubType,
};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings, MinimumUpdateIntervalSettings,
    SecondaryWindowSettings, Settings,
};

use super::{bitrate, DisplayInfo, Kind, ScreenError, Showing, Sources, Support, Target, FPS};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub fn support() -> Support {
    Support {
        window: false,
        screen: true,
        reason: None,
    }
}

/// Только основной монитор, без миниатюр: выбора окна и монитора на Windows
/// нет (см. докблок модуля). Имя пустое — интерфейс подпишет «основной».
pub fn list_sources(_thumbs: bool) -> Result<Sources, ScreenError> {
    Ok(Sources {
        windows: Vec::new(),
        displays: vec![DisplayInfo {
            id: 0,
            name: String::new(),
            main: true,
            thumb: None,
        }],
        meeting_app: None,
    })
}

fn primary_showing() -> Showing {
    Showing {
        kind: Kind::Screen,
        app: None,
        title: None,
        display: None,
    }
}

struct Flags {
    path: PathBuf,
    width: u32,
    height: u32,
    /// Сюда колбэк кладёт причину, если кодировщик отказал посреди записи.
    failed: Arc<Mutex<Option<String>>>,
}

struct Handler {
    encoder: Option<VideoEncoder>,
    failed: Arc<Mutex<Option<String>>>,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Flags;
    type Error = BoxError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let f = ctx.flags;
        let encoder = VideoEncoder::new(
            VideoSettingsBuilder::new(f.width, f.height)
                // H.264, а не HEVC по умолчанию крейта: HEVC на Windows
                // открывается только с платным расширением из Store.
                .sub_type(VideoSettingsSubType::H264)
                .bitrate(bitrate(f.width, f.height))
                .frame_rate(FPS),
            AudioSettingsBuilder::default().disabled(true),
            ContainerSettingsBuilder::default(),
            &f.path,
        )?;
        Ok(Self {
            encoder: Some(encoder),
            failed: f.failed,
        })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, _ctl: InternalCaptureControl) -> Result<(), Self::Error> {
        if let Some(enc) = self.encoder.as_mut() {
            if let Err(e) = enc.send_frame(frame) {
                // Дальше кадры слать некуда: кодировщик сломан. Звук от этого
                // не зависит — запись встречи идёт своим потоком.
                log::error!("запись экрана: кодировщик отказал: {e}");
                if let Ok(mut g) = self.failed.lock() {
                    g.get_or_insert(e.to_string());
                }
                self.encoder = None;
            }
        }
        Ok(())
    }
}

pub struct Active {
    control: CaptureControl<Handler, BoxError>,
    failed: Arc<Mutex<Option<String>>>,
    reported: bool,
}

impl Active {
    /// Любой «монитор» — основной: выбора монитора на Windows нет.
    pub fn start(target: &Target, path: &Path) -> Result<(Self, Showing), ScreenError> {
        if !matches!(target, Target::Display { .. }) {
            return Err(ScreenError::Unsupported("на Windows пишется только весь экран".into()));
        }
        let monitor = Monitor::primary().map_err(|e| ScreenError::Failed(e.to_string()))?;
        // Кодировщик не масштабирует, а обрезает (`build_padded_surface`),
        // поэтому размер — ровно размер монитора, чётный.
        let width = monitor.width().map_err(|e| ScreenError::Failed(e.to_string()))? & !1;
        let height = monitor.height().map_err(|e| ScreenError::Failed(e.to_string()))? & !1;
        let failed = Arc::new(Mutex::new(None));
        let settings = Settings::new(
            monitor,
            CursorCaptureSettings::WithCursor,
            // Default, а не WithoutBorder: убрать жёлтую рамку Windows 10
            // разрешает не всегда, и отказ здесь сорвал бы саму запись.
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            Flags {
                path: path.to_path_buf(),
                width,
                height,
                failed: failed.clone(),
            },
        );
        let control = Handler::start_free_threaded(settings).map_err(|e| ScreenError::Failed(e.to_string()))?;
        log::info!("запись экрана: {} ({width}×{height})", path.display());
        let active = Self {
            control,
            failed,
            reported: false,
        };
        Ok((active, primary_showing()))
    }

    /// Монитор → монитор — ничего не меняется (пишется всегда основной).
    /// Окно на Windows не пишется.
    pub fn switch(&mut self, target: &Target) -> Result<Showing, ScreenError> {
        match target {
            Target::Display { .. } => Ok(primary_showing()),
            _ => Err(ScreenError::Unsupported("на Windows пишется только весь экран".into())),
        }
    }

    pub fn failure(&mut self) -> Option<String> {
        if self.reported {
            return None;
        }
        if let Some(e) = self.failed.lock().ok().and_then(|g| g.clone()) {
            self.reported = true;
            return Some(e);
        }
        if self.control.is_finished() {
            self.reported = true;
            return Some("захват экрана остановился сам".into());
        }
        None
    }

    pub fn stop(self) -> Result<(), ScreenError> {
        let callback = self.control.callback();
        // Сначала поток захвата: после него кадров больше не будет, и
        // кодировщик можно закрывать без гонки с `on_frame_arrived`.
        let stopped = self.control.stop().map_err(|e| ScreenError::Failed(e.to_string()));
        let encoder = callback.lock().encoder.take();
        if let Some(enc) = encoder {
            enc.finish().map_err(|e| ScreenError::Failed(e.to_string()))?;
        }
        stopped
    }
}
