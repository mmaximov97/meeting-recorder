//! Трей: иконка-индикатор и меню.
//!
//! Иконки рисуются кодом, а не лежат файлами: это три залитых кружка, и держать
//! ради них PNG-и в репозитории — лишняя связь (файл переименовали → иконка
//! молча пропала). `Image::new_owned` принимает сырую RGBA, чего для кружка
//! более чем достаточно.

use crate::audio::{Ctl, UiState};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;
use std::time::Duration;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

/// Текущее состояние для иконки. Глобаль, а не поле в `State`, потому что читать
/// её надо из потока-мигалки, а писать — из аудио-потока; заводить ради двух
/// байт `Arc<Mutex<_>>` и протаскивать его через оба места дороже, чем польза.
/// Трей в приложении один, экземпляр приложения — тоже.
static ICON_STATE: AtomicU8 = AtomicU8::new(0);
/// Фаза мигания в Armed: true — кружок горит, false — притушен.
static BLINK_ON: AtomicBool = AtomicBool::new(true);
/// Пункт «Начать/Остановить запись»: его текст меняется под состояние.
///
/// Хранится здесь, потому что достать его из трея обратно нечем — у
/// `TrayIcon` есть `set_menu`, но нет `menu()` (проверено компилятором на
/// tauri 2.11). Единственный способ дотянуться до пункта после сборки меню —
/// не терять ссылку на него.
static TOGGLE_ITEM: OnceLock<MenuItem<Wry>> = OnceLock::new();

const IDLE: u8 = 0;
const ARMED: u8 = 1;
const RECORDING: u8 = 2;

/// Как часто мигает иконка в Armed.
const BLINK: Duration = Duration::from_millis(600);

const SIZE: u32 = 32;

/// Залитый кружок SIZE×SIZE в RGBA.
///
/// Сглаживания нет намеренно: на 32×32 в трее его не видно, а лишний код —
/// видно.
fn dot(r: u8, g: u8, b: u8, a: u8) -> Image<'static> {
    let mut buf = vec![0u8; (SIZE * SIZE * 4) as usize];
    let c = (SIZE as f32 - 1.0) / 2.0;
    let radius = c - 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (dx, dy) = (x as f32 - c, y as f32 - c);
            if dx * dx + dy * dy <= radius * radius {
                let i = ((y * SIZE + x) * 4) as usize;
                buf[i] = r;
                buf[i + 1] = g;
                buf[i + 2] = b;
                buf[i + 3] = a;
            }
        }
    }
    Image::new_owned(buf, SIZE, SIZE)
}

fn icon_for(state: u8, blink_on: bool) -> Image<'static> {
    match state {
        // Пишем — красный, всегда горит.
        RECORDING => dot(220, 50, 50, 255),
        // Вопрос висит — жёлтый, мигает: состояние требует ответа.
        ARMED if blink_on => dot(230, 170, 30, 255),
        ARMED => dot(230, 170, 30, 70),
        // Idle — серый: мы ничего не слушаем, микрофон отпущен.
        _ => dot(130, 130, 130, 255),
    }
}

/// Перерисовать иконку и текст пункта меню под текущее состояние.
///
/// Зовётся только с главного потока (см. `set_state`): GUI-объекты Windows
/// принадлежат потоку, который их создал.
fn repaint(app: &AppHandle) {
    let state = ICON_STATE.load(Ordering::Relaxed);
    let Some(tray) = app.tray_by_id("main") else {
        return;
    };
    let _ = tray.set_icon(Some(icon_for(state, BLINK_ON.load(Ordering::Relaxed))));
    let _ = tray.set_tooltip(Some(match state {
        RECORDING => "Идёт запись",
        ARMED => "Похоже, встреча — записать?",
        _ => "Ожидание встречи",
    }));

    if let Some(item) = TOGGLE_ITEM.get() {
        let _ = item.set_text(match state {
            RECORDING => "Остановить запись",
            ARMED => "Записать эту встречу",
            _ => "Начать запись",
        });
    }
}

/// Сообщить трею новое состояние. Можно звать откуда угодно — перерисовка сама
/// уедет на главный поток.
pub fn set_state(app: &AppHandle, st: UiState) {
    let v = match st {
        UiState::Idle => IDLE,
        UiState::Armed => ARMED,
        UiState::Recording => RECORDING,
    };
    ICON_STATE.store(v, Ordering::Relaxed);
    // Каждый заход в Armed начинается с горящей фазы, иначе первый кадр мог бы
    // оказаться притушенным и выглядеть как «ничего не произошло».
    BLINK_ON.store(true, Ordering::Relaxed);
    let h = app.clone();
    let _ = app.run_on_main_thread(move || repaint(&h));
}

/// Поток-мигалка. Работает только в Armed: в остальных состояниях иконка
/// статична, и будить главный поток незачем.
fn spawn_blinker(app: &AppHandle) {
    let h = app.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(BLINK);
        if ICON_STATE.load(Ordering::Relaxed) != ARMED {
            continue;
        }
        BLINK_ON.fetch_xor(true, Ordering::Relaxed);
        let h2 = h.clone();
        let _ = h.run_on_main_thread(move || repaint(&h2));
    });
}

/// Выход из трея.
///
/// Идёт в аудио-поток, а не в `app.exit()`: там `ManualStop` финализирует
/// запись, и только после этого процесс уходит. Прямой выход отсюда оставил бы
/// WAV с нулевой длиной в заголовке — файл существует, открывается и играет
/// тишину.
///
/// Если аудио-поток мёртв (канал закрыт), финализировать уже нечего и некому —
/// выходим сами, иначе «Выход» перестал бы работать вовсе.
fn request_quit(app: &AppHandle, tx: &Sender<Ctl>) {
    if tx.send(Ctl::Shutdown).is_err() {
        app.exit(0);
    }
}

/// Собрать трей. `tx` — тот же канал в аудио-поток, что и у остальных команд:
/// трей ничего не решает сам.
pub fn build(app: &AppHandle, tx: Sender<Ctl>) -> tauri::Result<()> {
    let toggle = MenuItem::with_id(app, "toggle", "Начать запись", true, None::<&str>)?;
    let folder = MenuItem::with_id(app, "folder", "Открыть папку записей", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&toggle, &folder, &quit])?;
    let _ = TOGGLE_ITEM.set(toggle.clone());

    TrayIconBuilder::with_id("main")
        .icon(icon_for(IDLE, true))
        .tooltip("Ожидание встречи")
        .menu(&menu)
        // Меню — по правой кнопке (привычка Windows), левая открывает окно.
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "toggle" => {
                let _ = tx.send(Ctl::Toggle);
            }
            "folder" => {
                if let Err(e) = crate::open_folder() {
                    eprintln!("не удалось открыть папку: {e}");
                }
            }
            "quit" => request_quit(app, &tx),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.unminimize();
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
        })
        .build(app)?;

    spawn_blinker(app);
    Ok(())
}
