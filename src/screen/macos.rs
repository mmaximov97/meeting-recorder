//! Запись экрана на macOS: ScreenCaptureKit, `SCStream` + `SCRecordingOutput`.
//!
//! `SCRecordingOutput` (macOS 15+) сам кодирует H.264 и пишет mp4 — ни
//! AVAssetWriter, ни своих буферов кадров здесь нет. Цена — порог версии:
//! на 14.x запись экрана не предлагается вовсе (см. [`support`]), а звук
//! пишется как раньше.
//!
//! Все вызовы ScreenCaptureKit асинхронные, с completion handler на чужой
//! очереди. Наш поток ждёт ответ через канал с таймаутом: подвисший
//! ScreenCaptureKit не должен вешать рабочий поток записи экрана навсегда.
//!
//! Классы ScreenCaptureKit ищутся рантаймом по имени (objc2 без
//! `unstable-static-class`), а сам фреймворк линкуется слабо (`build.rs`): на
//! старой системе процесс доживает до проверки версии, а не падает в dyld.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadMarker};
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSCompositingOperation, NSDeviceRGBColorSpace, NSGraphicsContext,
    NSImageCompressionFactor, NSRunningApplication, NSScreen,
};
use objc2_core_graphics::CGImage;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_foundation::{
    NSArray, NSData, NSDataBase64EncodingOptions, NSDictionary, NSError, NSNumber, NSPoint, NSRect, NSSize, NSString, NSURL,
};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCRecordingOutput, SCRecordingOutputConfiguration,
    SCRecordingOutputDelegate, SCRunningApplication, SCScreenshotManager, SCShareableContent, SCStream,
    SCStreamConfiguration, SCStreamErrorCode, SCStreamOutput, SCStreamOutputType, SCWindow,
};

use super::{
    classify_failure, display_at, frame_watch, FrameWatch, is_listable, pick_app, pick_display, pick_window, target_size, thumb_size, DisplayInfo, Kind,
    RawWindow, Rect, ScreenApp, ScreenError, Showing, Sources, Support, Target, WindowInfo, FPS,
};
use crate::capture::macos::parse_major_minor;

/// `SCRecordingOutput` появился в macOS 15.0.
const MIN_MACOS: (u32, u32) = (15, 0);

/// Сколько ждать ответа ScreenCaptureKit на запрос контента и старт/стоп.
const SCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Сколько ждать, пока `SCRecordingOutput` допишет и закроет mp4 после стопа.
const FINISH_TIMEOUT: Duration = Duration::from_secs(10);

/// Сколько меню ждёт миниатюры. Не успевшие приходят пустыми: серый
/// плейсхолдер лучше, чем меню, которое открывается секунды.
const THUMBS_TIMEOUT: Duration = Duration::from_millis(900);

/// Сколько ждать имена мониторов с главного потока. Не дождались — имя
/// заменяет разрешение («2560×1440»).
const NAMES_TIMEOUT: Duration = Duration::from_millis(400);

/// Качество JPEG миниатюры: 240 px шириной выходит 8–20 КБ.
const THUMB_QUALITY: f64 = 0.7;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGMainDisplayID() -> u32;
}

fn version_reason() -> Option<String> {
    const NEED: &str = "нужна macOS 15 или новее";
    match sysinfo::System::os_version().as_deref().and_then(parse_major_minor) {
        Some(v) if v >= MIN_MACOS => None,
        Some((major, minor)) => Some(format!("{NEED} (у вас {major}.{minor})")),
        None => Some(format!("{NEED} (версию определить не удалось)")),
    }
}

pub fn support() -> Support {
    match version_reason() {
        None => Support {
            window: true,
            screen: true,
            reason: None,
        },
        Some(r) => Support {
            window: false,
            screen: false,
            reason: Some(r),
        },
    }
}

pub fn has_permission() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

pub fn request_permission() -> bool {
    unsafe { CGRequestScreenCaptureAccess() }
}

fn ready() -> Result<(), ScreenError> {
    if let Some(r) = version_reason() {
        return Err(ScreenError::Unsupported(r));
    }
    if !has_permission() {
        return Err(ScreenError::Permission);
    }
    Ok(())
}

fn error_text(e: &NSError) -> String {
    e.localizedDescription().to_string()
}

/// Отказ TCC приходит из ScreenCaptureKit кодом `UserDeclined`, а не
/// отдельным видом ошибки — превращаем его в понятное «нет разрешения».
fn map_error(e: &NSError) -> ScreenError {
    if e.code() == SCStreamErrorCode::UserDeclined.0 {
        ScreenError::Permission
    } else {
        ScreenError::Failed(error_text(e))
    }
}

/// Обёртка ради канала: completion handler зовётся на чужой очереди, а
/// `Retained<SCShareableContent>` не `Send`. Объект неизменяемый — снимок
/// того, что было на экране в момент запроса, — и читается дальше только
/// одним потоком, тем, что его ждал.
struct Content(Retained<SCShareableContent>);
unsafe impl Send for Content {}

/// Все окна, а не только видимые: окно встречи на соседнем рабочем столе
/// пишется так же, и в меню выбора оно должно быть.
fn shareable_content() -> Result<Retained<SCShareableContent>, ScreenError> {
    let (tx, rx) = mpsc::sync_channel::<Result<Content, ScreenError>>(1);
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        let r = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(Content(c)),
            None => Err(match unsafe { err.as_ref() } {
                Some(e) => map_error(e),
                None => ScreenError::Failed("ScreenCaptureKit не отдал список окон".into()),
            }),
        };
        let _ = tx.try_send(r);
    });
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, false, &block,
        )
    };
    match rx.recv_timeout(SCK_TIMEOUT) {
        Ok(r) => r.map(|c| c.0),
        Err(_) => Err(ScreenError::Failed("ScreenCaptureKit не ответил".into())),
    }
}

fn my_pid() -> i32 {
    std::process::id() as i32
}

/// Окно, которое считается окном встречи: обычный слой, на экране, не крошка.
fn is_real_window(w: &SCWindow) -> bool {
    let f = unsafe { w.frame() };
    let shown = unsafe { w.windowLayer() == 0 && w.isOnScreen() };
    shown && f.size.width >= 200.0 && f.size.height >= 120.0
}

fn bundle_id(app: &SCRunningApplication) -> String {
    unsafe { app.bundleIdentifier() }.to_string()
}

/// Приложения с видимыми окнами — то, из чего выбирает «окно встречи».
fn meeting_apps(content: &SCShareableContent) -> Vec<ScreenApp> {
    let me = my_pid();
    let mut apps: Vec<ScreenApp> = Vec::new();
    for w in unsafe { content.windows() }.iter() {
        if !is_real_window(&w) {
            continue;
        }
        let Some(owner) = (unsafe { w.owningApplication() }) else { continue };
        if unsafe { owner.processID() } == me {
            continue;
        }
        let id = bundle_id(&owner);
        if id.is_empty() {
            continue;
        }
        let title = unsafe { w.title() }.map(|t| t.to_string()).unwrap_or_default();
        match apps.iter_mut().find(|a| a.id == id) {
            Some(a) => a.titles.push(title),
            None => apps.push(ScreenApp {
                id,
                name: unsafe { owner.applicationName() }.to_string(),
                titles: vec![title],
            }),
        }
    }
    // Звонилки наверх: их и будут выбирать.
    apps.sort_by(|a, b| {
        super::is_meeting_app(&b.id)
            .cmp(&super::is_meeting_app(&a.id))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    apps
}

pub fn list_apps() -> Result<Vec<ScreenApp>, ScreenError> {
    ready()?;
    let content = shareable_content()?;
    Ok(meeting_apps(&content))
}

fn main_display_id() -> u32 {
    unsafe { CGMainDisplayID() }
}

fn display_rects(content: &SCShareableContent) -> Vec<(u32, Rect)> {
    unsafe { content.displays() }
        .iter()
        .map(|d| {
            let r = unsafe { d.frame() };
            (unsafe { d.displayID() }, (r.origin.x, r.origin.y, r.size.width, r.size.height))
        })
        .collect()
}

/// Окна для меню выбора — с самими `SCWindow`, чтобы по выбранному сразу
/// собрать фильтр. Звонилки наверх, дальше по имени приложения.
fn listed_windows(content: &SCShareableContent) -> Vec<(Retained<SCWindow>, WindowInfo)> {
    let rects = display_rects(content);
    let main = main_display_id();
    let me = my_pid();
    let mut out = Vec::new();
    for w in unsafe { content.windows() }.iter() {
        let Some(owner) = (unsafe { w.owningApplication() }) else { continue };
        let f = unsafe { w.frame() };
        let title = unsafe { w.title() }.map(|t| t.to_string()).unwrap_or_default();
        let raw = RawWindow {
            layer: unsafe { w.windowLayer() } as i64,
            width: f.size.width,
            height: f.size.height,
            on_screen: unsafe { w.isOnScreen() },
            own: unsafe { owner.processID() } == me,
            app_id: bundle_id(&owner),
            title,
        };
        if !is_listable(&raw) {
            continue;
        }
        let (cx, cy) = (f.origin.x + f.size.width / 2.0, f.origin.y + f.size.height / 2.0);
        let info = WindowInfo {
            id: unsafe { w.windowID() },
            app_id: raw.app_id,
            app_name: unsafe { owner.applicationName() }.to_string(),
            title: raw.title,
            display_id: display_at(&rects, cx, cy).unwrap_or(main),
            width: f.size.width,
            height: f.size.height,
            thumb: None,
            icon: None,
        };
        let info = WindowInfo {
            icon: app_icon(unsafe { owner.processID() }, &info.app_id),
            ..info
        };
        out.push((w, info));
    }
    out.sort_by(|(_, a), (_, b)| {
        super::is_meeting_app(&b.app_id)
            .cmp(&super::is_meeting_app(&a.app_id))
            .then_with(|| a.app_name.to_lowercase().cmp(&b.app_name.to_lowercase()))
    });
    out
}

/// Имена мониторов по `CGDirectDisplayID`. `NSScreen` живёт на главном
/// потоке, а зовут нас из фонового — поэтому через главную очередь и с
/// таймаутом: занятый главный поток не должен держать меню.
fn display_names() -> HashMap<u32, String> {
    fn read(mtm: MainThreadMarker) -> HashMap<u32, String> {
        let key = NSString::from_str("NSScreenNumber");
        NSScreen::screens(mtm)
            .iter()
            .filter_map(|s| {
                let n = s.deviceDescription().objectForKey(&key)?.downcast::<NSNumber>().ok()?;
                Some((n.unsignedIntValue(), s.localizedName().to_string()))
            })
            .collect()
    }
    if let Some(mtm) = MainThreadMarker::new() {
        return read(mtm);
    }
    let (tx, rx) = mpsc::sync_channel(1);
    DispatchQueue::main().exec_async(move || {
        if let Some(mtm) = MainThreadMarker::new() {
            let _ = tx.try_send(read(mtm));
        }
    });
    rx.recv_timeout(NAMES_TIMEOUT).unwrap_or_default()
}

/// Мониторы для меню выбора, основной первым.
fn listed_displays(content: &SCShareableContent) -> Vec<(Retained<SCDisplay>, DisplayInfo)> {
    let names = display_names();
    let main = main_display_id();
    let mut out: Vec<_> = unsafe { content.displays() }
        .iter()
        .map(|d| {
            let id = unsafe { d.displayID() };
            let name = names
                .get(&id)
                .cloned()
                .unwrap_or_else(|| unsafe { format!("{}×{}", d.width(), d.height()) });
            let info = DisplayInfo {
                id,
                name,
                main: id == main,
                thumb: None,
            };
            (d, info)
        })
        .collect();
    out.sort_by_key(|(_, i)| !i.main);
    out
}

/// Свои приложения (пульт, всплывашка вопроса) — чтобы убрать их с записи
/// экрана.
fn own_apps(content: &SCShareableContent) -> Retained<NSArray<SCRunningApplication>> {
    let me = my_pid();
    let own: Vec<_> = unsafe { content.applications() }
        .iter()
        .filter(|a| unsafe { a.processID() } == me)
        .collect();
    NSArray::from_retained_slice(&own)
}

fn display_filter(content: &SCShareableContent, display: &SCDisplay, hide_self: bool) -> Retained<SCContentFilter> {
    let no_windows = NSArray::<SCWindow>::new();
    unsafe {
        if hide_self {
            SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                SCContentFilter::alloc(),
                display,
                &own_apps(content),
                &no_windows,
            )
        } else {
            SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), display, &no_windows)
        }
    }
}

fn window_filter(window: &SCWindow) -> Retained<SCContentFilter> {
    unsafe { SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), window) }
}

/// Сторона иконки приложения в пикселях: 32 точки на Retina.
const ICON_PX: f64 = 64.0;

/// Иконка приложения как PNG data URL. Кэш по bundle id на весь процесс:
/// иконки не меняются, а меню открывают много раз за встречу.
fn app_icon(pid: i32, bundle: &str) -> Option<String> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().ok().and_then(|c| c.get(bundle).cloned()) {
        return hit;
    }
    let icon = render_icon(pid);
    if let Ok(mut c) = cache.lock() {
        c.insert(bundle.to_string(), icon.clone());
    }
    icon
}

/// `NSRunningApplication.icon` → PNG 64×64. Рисуется в свой битмап, а не
/// берётся готовое представление: у иконки их много (16…1024), и нужного
/// размера среди них может не быть.
fn render_icon(pid: i32) -> Option<String> {
    let image = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?.icon()?;
    let side = ICON_PX as isize;
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            side,
            side,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            0,
            0,
        )
    }?;
    let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&ctx));
    image.drawInRect_fromRect_operation_fraction(
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(ICON_PX, ICON_PX)),
        NSRect::ZERO,
        NSCompositingOperation::SourceOver,
        1.0,
    );
    NSGraphicsContext::restoreGraphicsState_class();
    let props = NSDictionary::<NSString, AnyObject>::new();
    let data = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) }?;
    Some(data_url("image/png", &data))
}

fn data_url(mime: &str, data: &NSData) -> String {
    let b64 = data.base64EncodedStringWithOptions(NSDataBase64EncodingOptions::empty());
    format!("data:{mime};base64,{b64}")
}

/// CGImage → `data:image/jpeg;base64,…`.
fn jpeg_data_url(image: &CGImage) -> Option<String> {
    let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), image);
    let quality = NSNumber::new_f64(THUMB_QUALITY);
    let key: &NSString = unsafe { NSImageCompressionFactor };
    let props = NSDictionary::<NSString, AnyObject>::from_slices(&[key], &[quality.as_ref()]);
    let data = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::JPEG, &props) }?;
    Some(data_url("image/jpeg", &data))
}

/// Снять миниатюры разом и ждать их не дольше [`THUMBS_TIMEOUT`] на всех.
/// `shots` — фильтр и размер источника в точках; ответ — по тому же порядку.
fn snapshots(shots: Vec<(Retained<SCContentFilter>, f64, f64)>) -> Vec<Option<String>> {
    let n = shots.len();
    let (tx, rx) = mpsc::channel::<(usize, Option<String>)>();
    // Фильтры, конфиги и блоки живут до конца ожидания.
    let mut keep = Vec::with_capacity(n);
    for (i, (filter, w, h)) in shots.into_iter().enumerate() {
        let (tw, th) = thumb_size(w, h);
        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            config.setWidth(tw as usize);
            config.setHeight(th as usize);
            config.setShowsCursor(false);
            config.setScalesToFit(true);
        }
        let tx = tx.clone();
        let block = RcBlock::new(move |image: *mut CGImage, _err: *mut NSError| {
            // JPEG кодируется здесь, на очереди ScreenCaptureKit: так снимки
            // жмутся параллельно, а не по очереди в ждущем потоке.
            let url = unsafe { image.as_ref() }.and_then(jpeg_data_url);
            let _ = tx.send((i, url));
        });
        unsafe {
            SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(&filter, &config, Some(&block))
        };
        keep.push((filter, config, block));
    }
    drop(tx);
    let mut out = vec![None; n];
    let deadline = Instant::now() + THUMBS_TIMEOUT;
    for _ in 0..n {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((i, url)) => out[i] = url,
            Err(_) => break,
        }
    }
    out
}

pub fn list_sources(thumbs: bool) -> Result<Sources, ScreenError> {
    ready()?;
    let content = shareable_content()?;
    let windows = listed_windows(&content);
    let displays = listed_displays(&content);
    let apps = meeting_apps(&content);
    let meeting_app = pick_app(&apps, None, None).map(|i| apps[i].name.clone());

    let mut pics = if thumbs {
        let shots = windows
            .iter()
            .map(|(w, i)| (window_filter(w), i.width, i.height))
            .chain(displays.iter().map(|(d, _)| {
                let (w, h) = unsafe { (d.width() as f64, d.height() as f64) };
                (display_filter(&content, d, true), w, h)
            }))
            .collect();
        snapshots(shots).into_iter()
    } else {
        Vec::new().into_iter()
    };
    let windows = windows
        .into_iter()
        .map(|(_, mut i)| {
            i.thumb = pics.next().flatten();
            i
        })
        .collect();
    let displays = displays
        .into_iter()
        .map(|(_, mut i)| {
            i.thumb = pics.next().flatten();
            i
        })
        .collect();
    Ok(Sources {
        windows,
        displays,
        meeting_app,
    })
}

/// Экран, на котором лежит центр окна. Нет такого — главный.
fn display_for(content: &SCShareableContent, window: &SCWindow) -> Option<Retained<SCDisplay>> {
    let displays = unsafe { content.displays() };
    let f = unsafe { window.frame() };
    let (cx, cy) = (f.origin.x + f.size.width / 2.0, f.origin.y + f.size.height / 2.0);
    let id = display_at(&display_rects(content), cx, cy).unwrap_or_else(main_display_id);
    displays
        .iter()
        .find(|d| unsafe { d.displayID() } == id)
        .or_else(|| displays.iter().next())
}

fn no_display() -> ScreenError {
    ScreenError::Failed("не найден ни один экран".into())
}

/// Фильтр под цель и что под неё нашлось.
///
/// «Окно встречи» — все окна приложения на том экране, где его самое большое
/// окно: окно встречи у Zoom не одно (демонстрация, участники), и запись
/// одного `SCWindow` теряла бы остальные.
fn resolve(content: &SCShareableContent, target: &Target) -> Result<(Retained<SCContentFilter>, Showing), ScreenError> {
    match target {
        Target::Meeting { source } => {
            let apps = meeting_apps(content);
            let i = pick_app(&apps, None, source.as_deref()).ok_or(ScreenError::NoMeetingWindow)?;
            let id = &apps[i].id;
            let app = unsafe { content.applications() }
                .iter()
                .find(|a| bundle_id(a) == *id)
                .ok_or(ScreenError::NoMeetingWindow)?;
            let biggest = unsafe { content.windows() }
                .iter()
                .filter(|w| is_real_window(w))
                .filter(|w| unsafe { w.owningApplication() }.is_some_and(|o| bundle_id(&o) == *id))
                .max_by(|a, b| {
                    let (fa, fb) = unsafe { (a.frame(), b.frame()) };
                    (fa.size.width * fa.size.height).total_cmp(&(fb.size.width * fb.size.height))
                })
                .ok_or(ScreenError::NoMeetingWindow)?;
            let display = display_for(content, &biggest).ok_or_else(no_display)?;
            let filter = unsafe {
                SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &NSArray::from_retained_slice(&[app]),
                    &NSArray::<SCWindow>::new(),
                )
            };
            let showing = Showing {
                kind: Kind::Meeting,
                app: Some(apps[i].name.clone()),
                title: None,
                display: None,
            };
            Ok((filter, showing))
        }
        Target::Window { id, app_id, title } => {
            let windows = listed_windows(content);
            let infos: Vec<WindowInfo> = windows.iter().map(|(_, i)| i.clone()).collect();
            let i = pick_window(&infos, *id, app_id.as_deref(), title.as_deref()).ok_or(ScreenError::NoWindow)?;
            let (window, info) = &windows[i];
            let showing = Showing {
                kind: Kind::Window,
                app: Some(info.app_name.clone()),
                title: Some(info.title.clone()).filter(|t| !t.is_empty()),
                display: None,
            };
            Ok((window_filter(window), showing))
        }
        Target::Display { id, name, hide_self } => {
            let displays = listed_displays(content);
            let infos: Vec<DisplayInfo> = displays.iter().map(|(_, i)| i.clone()).collect();
            let i = pick_display(&infos, *id, name.as_deref()).ok_or_else(no_display)?;
            let (display, info) = &displays[i];
            let showing = Showing {
                kind: Kind::Screen,
                app: None,
                title: None,
                display: Some(info.name.clone()),
            };
            Ok((display_filter(content, display, *hide_self), showing))
        }
    }
}

/// Размер кадра записи — по самому большому монитору, а не по цели.
///
/// Так смена источника посреди записи не меняет размер кадра вовсе: другой
/// монитор или окно вписываются в тот же кадр с сохранением пропорций
/// (`scalesToFit`), и `SCRecordingOutput` продолжает тот же mp4. Менять
/// размер кадра в идущем файле (`updateConfiguration`) ScreenCaptureKit не
/// обещает, а проверить это можно только на живой записи.
fn frame_size(content: &SCShareableContent) -> (u32, u32) {
    let biggest = unsafe { content.displays() }
        .iter()
        .map(|d| unsafe { (d.width().max(1), d.height().max(1)) })
        .max_by_key(|(w, h)| w * h)
        .unwrap_or((1920, 1080));
    // SCDisplay меряется в точках; ×2 — пиксели Retina, дальше
    // `target_size` ужимает до 1080p по длинной стороне.
    target_size((biggest.0 * 2) as u32, (biggest.1 * 2) as u32)
}

enum RecEvent {
    Failed(String),
    Finished,
}

struct DelegateIvars {
    tx: Mutex<Sender<RecEvent>>,
}

define_class!(
    // SAFETY: NSObject не требует ничего от подклассов, Drop не реализован.
    #[unsafe(super(NSObject))]
    #[ivars = DelegateIvars]
    struct RecordingDelegate;

    unsafe impl NSObjectProtocol for RecordingDelegate {}

    unsafe impl SCRecordingOutputDelegate for RecordingDelegate {
        #[unsafe(method(recordingOutput:didFailWithError:))]
        fn did_fail(&self, _output: &SCRecordingOutput, error: &NSError) {
            log::error!("запись экрана оборвалась: {}", error_text(error));
            self.send(RecEvent::Failed(error_text(error)));
        }

        #[unsafe(method(recordingOutputDidFinishRecording:))]
        fn did_finish(&self, _output: &SCRecordingOutput) {
            self.send(RecEvent::Finished);
        }
    }
);

impl RecordingDelegate {
    fn new(tx: Sender<RecEvent>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { tx: Mutex::new(tx) });
        unsafe { msg_send![super(this), init] }
    }

    fn send(&self, e: RecEvent) {
        if let Ok(tx) = self.ivars().tx.lock() {
            let _ = tx.send(e);
        }
    }
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMSampleBufferGetImageBuffer(sbuf: &CMSampleBuffer) -> *const std::ffi::c_void;
}

struct CounterIvars {
    frames: Arc<AtomicU64>,
}

define_class!(
    // Получатель кадров, который только считает их. Сами кадры идут в
    // SCRecordingOutput; без получателя типа `.screen` SCStream на каждый
    // кадр пишет в системный лог «stream output NOT found. Dropping frame».
    //
    // Счёт — для сторожа первого кадра (`super::frame_watch`): окно, которое
    // запрещает захват (`NSWindow.sharingType = none`), не отдаёт ни одного
    // кадра с картинкой, и без сторожа видео терялось молча до самого стопа.
    // Считаются только буферы с картинкой: служебные (idle, blank) её не несут.
    #[unsafe(super(NSObject))]
    #[ivars = CounterIvars]
    struct FrameCounter;

    unsafe impl NSObjectProtocol for FrameCounter {}

    unsafe impl SCStreamOutput for FrameCounter {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, buffer: &CMSampleBuffer, _kind: SCStreamOutputType) {
            if !unsafe { CMSampleBufferGetImageBuffer(buffer) }.is_null() {
                self.ivars().frames.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
);

impl FrameCounter {
    fn new(frames: Arc<AtomicU64>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(CounterIvars { frames });
        unsafe { msg_send![super(this), init] }
    }
}

/// Позвать `start/stopCaptureWithCompletionHandler` и дождаться ответа.
fn call_and_wait(call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>)) -> Result<(), ScreenError> {
    let (tx, rx) = mpsc::sync_channel::<Result<(), ScreenError>>(1);
    let block = RcBlock::new(move |err: *mut NSError| {
        let r = match unsafe { err.as_ref() } {
            None => Ok(()),
            Some(e) => Err(map_error(e)),
        };
        let _ = tx.try_send(r);
    });
    call(&block);
    rx.recv_timeout(SCK_TIMEOUT)
        .unwrap_or_else(|_| Err(ScreenError::Failed("ScreenCaptureKit не ответил".into())))
}

pub struct Active {
    stream: Retained<SCStream>,
    // Держатся живыми до конца записи: SCStream и SCRecordingOutput хранят на
    // них слабые ссылки.
    _output: Retained<SCRecordingOutput>,
    _delegate: Retained<RecordingDelegate>,
    _counter: Retained<FrameCounter>,
    _queue: DispatchRetained<DispatchQueue>,
    events: Receiver<RecEvent>,
    /// Кадры с картинкой с начала записи.
    frames: Arc<AtomicU64>,
    /// Сторож первого кадра: с какого момента и от какого счёта ждём кадр,
    /// и было ли это после смены источника. `None` — кадр уже пришёл.
    watch: Option<(Instant, u64, bool)>,
    /// Фильтр, который сейчас в потоке, и прежний — чтобы вернуть его, если
    /// новое окно не даёт себя записать.
    filter: Retained<SCContentFilter>,
    prev_filter: Option<Retained<SCContentFilter>>,
}

impl Active {
    pub fn start(target: &Target, path: &Path) -> Result<(Self, Showing), ScreenError> {
        ready()?;
        let content = shareable_content()?;
        let (filter, showing) = resolve(&content, target)?;

        let (w, h) = frame_size(&content);
        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            config.setWidth(w as usize);
            config.setHeight(h as usize);
            config.setMinimumFrameInterval(CMTime::new(1, FPS as i32));
            config.setShowsCursor(true);
            config.setCapturesAudio(false);
            // Окно и монитор другого размера вписываются в кадр целиком,
            // с полями, а не обрезаются и не растягиваются.
            config.setScalesToFit(true);
            config.setPreservesAspectRatio(true);
        }

        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None)
        };

        let rec_config = unsafe { SCRecordingOutputConfiguration::new() };
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        unsafe { rec_config.setOutputURL(&url) };

        let (tx, events) = mpsc::channel();
        let delegate = RecordingDelegate::new(tx);
        let output = unsafe {
            SCRecordingOutput::initWithConfiguration_delegate(
                SCRecordingOutput::alloc(),
                &rec_config,
                ProtocolObject::from_ref(&*delegate),
            )
        };

        let frames = Arc::new(AtomicU64::new(0));
        let counter = FrameCounter::new(frames.clone());
        let queue = DispatchQueue::new("meetrec.screen", None);
        unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*counter),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| map_error(&e))?;
            stream.addRecordingOutput_error(&output).map_err(|e| map_error(&e))?;
        }
        call_and_wait(|b| unsafe { stream.startCaptureWithCompletionHandler(Some(b)) })?;
        log::info!("запись экрана: {} ({w}×{h}), {:?}", path.display(), showing);

        let active = Self {
            stream,
            _output: output,
            _delegate: delegate,
            _counter: counter,
            _queue: queue,
            events,
            frames,
            watch: Some((Instant::now(), 0, false)),
            filter,
            prev_filter: None,
        };
        Ok((active, showing))
    }

    fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    pub fn has_frames(&self) -> bool {
        self.frames() > 0
    }

    /// Сторож первого кадра, звать на каждом тике. `Some(после_смены)` —
    /// источник за [`super::FIRST_FRAME_TIMEOUT`] не дал ни одного кадра;
    /// отвечает один раз.
    pub fn blocked(&mut self) -> Option<bool> {
        let (since, base, after_switch) = self.watch?;
        match frame_watch(since.elapsed(), self.frames() - base) {
            FrameWatch::Wait => None,
            FrameWatch::Alive => {
                self.watch = None;
                None
            }
            FrameWatch::Blocked => {
                self.watch = None;
                Some(after_switch)
            }
        }
    }

    /// Вернуть фильтр, который был до последней смены источника.
    pub fn revert(&mut self) -> Result<(), ScreenError> {
        let prev = self.prev_filter.take().ok_or_else(|| ScreenError::Failed("прежнего источника нет".into()))?;
        call_and_wait(|b| unsafe { self.stream.updateContentFilter_completionHandler(&prev, Some(b)) })?;
        self.filter = prev;
        log::info!("запись экрана вернулась к прежнему источнику");
        Ok(())
    }

    /// Новый фильтр в идущий поток (`updateContentFilter`): тот же файл,
    /// тот же размер кадра. Цель не нашлась — поток не тронут.
    pub fn switch(&mut self, target: &Target) -> Result<Showing, ScreenError> {
        let content = shareable_content()?;
        let (filter, showing) = resolve(&content, target)?;
        call_and_wait(|b| unsafe { self.stream.updateContentFilter_completionHandler(&filter, Some(b)) })?;
        log::info!("запись экрана переключена: {:?}", showing);
        // ScreenCaptureKit шлёт кадр сразу после смены фильтра — его и ждём.
        self.watch = Some((Instant::now(), self.frames(), true));
        self.prev_filter = Some(std::mem::replace(&mut self.filter, filter));
        Ok(showing)
    }

    pub fn failure(&mut self) -> Option<String> {
        match self.events.try_recv() {
            Ok(RecEvent::Failed(e)) => Some(e),
            _ => None,
        }
    }

    pub fn stop(self) -> Result<(), ScreenError> {
        call_and_wait(|b| unsafe { self.stream.stopCaptureWithCompletionHandler(Some(b)) })?;
        // Файл закрывается после стопа потока, отдельным событием делегата.
        loop {
            match self.events.recv_timeout(FINISH_TIMEOUT) {
                Ok(RecEvent::Finished) => return Ok(()),
                Ok(RecEvent::Failed(e)) => return Err(classify_failure(e)),
                Err(_) => {
                    return Err(ScreenError::Failed(
                        "видео не дописалось за 10 секунд — файл может не открыться".into(),
                    ))
                }
            }
        }
    }
}
