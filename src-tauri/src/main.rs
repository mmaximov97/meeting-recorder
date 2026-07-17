// В релизе консольного окна за GUI быть не должно; в отладке оно нужно —
// eprintln! из аудио-потока это единственный способ увидеть, что там происходит.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod tray;

use audio::Ctl;
use meeting_recorder::session::Event;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;
use tauri::WindowEvent;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

/// Тот же каталог, что у консольного бинаря (`src/main.rs`). Хардкод переехал
/// сюда как есть: разъедься эти два пути, GUI перестал бы показывать записи,
/// сделанные консолью, — а консоль остаётся инструментом отладки того же ядра.
fn recordings_dir() -> PathBuf {
    PathBuf::from(r"C:\Users\<username>\Recordings")
}

/// Канал в аудио-поток. `Mutex` — потому что `tauri::State` шарится между
/// потоками, а `Sender` не `Sync`.
struct Cmd(Mutex<Sender<Ctl>>);

impl Cmd {
    fn send(&self, c: Ctl) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .send(c)
            .map_err(|_| "аудио-поток не отвечает".to_string())
    }
}

/// Одна запись: пара дорожек под общим именем.
#[derive(Serialize)]
struct Recording {
    /// `2026-07-17_14-30_zoom` — общая основа обеих дорожек.
    name: String,
    mic: bool,
    system: bool,
    /// Суммарный размер дорожек в байтах.
    size: u64,
}

#[tauri::command]
fn send_event(name: &str, state: tauri::State<Cmd>) -> Result<(), String> {
    let ev = match name {
        "confirm" => Event::UserConfirmed,
        "decline" => Event::UserDeclined,
        "start" => Event::ManualStart,
        "stop" => Event::ManualStop,
        // Toggle — не событие ядра: что делать, знает только машина (см. Ctl).
        "toggle" => return state.send(Ctl::Toggle),
        other => return Err(format!("неизвестное событие: {other}")),
    };
    state.send(Ctl::Event(ev))
}

/// Список записей, сгруппированный по основе имени.
///
/// Имя дорожки — `{основа}.{mic|system}.wav`, где основа это
/// `YYYY-MM-DD_HH-MM_источник` с необязательным `_N` у повторов в ту же минуту
/// (см. `app::free_name_pair`). Группируем срезанием суффикса дорожки: пара
/// склеивается обратно ровно тем же правилом, которым её разложили.
///
/// `BTreeMap` даёт сортировку по имени, то есть по дате — основа начинается с
/// `YYYY-MM-DD_HH-MM`, так что лексикографический порядок и есть хронологический.
/// Наверх список отдаётся перевёрнутым: свежее сверху.
#[tauri::command]
fn list_recordings() -> Result<Vec<Recording>, String> {
    let dir = recordings_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        // Каталога нет — записей просто ещё не было. Это не ошибка.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("не удалось прочитать {}: {e}", dir.display())),
    };

    let mut found: BTreeMap<String, Recording> = BTreeMap::new();
    for entry in entries.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        let (base, is_mic) = match (file.strip_suffix(".mic.wav"), file.strip_suffix(".system.wav"))
        {
            (Some(b), _) => (b.to_string(), true),
            (_, Some(b)) => (b.to_string(), false),
            // Не наша дорожка — чужой файл в каталоге, не наше дело.
            _ => continue,
        };
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let rec = found.entry(base.clone()).or_insert(Recording {
            name: base,
            mic: false,
            system: false,
            size: 0,
        });
        if is_mic {
            rec.mic = true;
        } else {
            rec.system = true;
        }
        rec.size += size;
    }

    Ok(found.into_values().rev().collect())
}

/// Открыть каталог записей в проводнике.
///
/// Через `explorer.exe` напрямую, без `tauri-plugin-opener`: плагин ради одной
/// строчки тянул бы за собой ещё и права в capabilities.
///
/// Код возврата не проверяется намеренно: `explorer.exe` возвращает 1 даже когда
/// окно успешно открылось. Проверять здесь нечего — либо папка открылась, либо
/// пользователь это увидит сам.
#[tauri::command]
fn open_folder() -> Result<(), String> {
    let dir = recordings_dir();
    // Иначе explorer откроет «Документы» вместо пустого несуществующего пути.
    std::fs::create_dir_all(&dir).map_err(|e| format!("не удалось создать {}: {e}", dir.display()))?;
    std::process::Command::new("explorer.exe")
        .arg(&dir)
        .spawn()
        .map_err(|e| format!("не удалось открыть проводник: {e}"))?;
    Ok(())
}

fn main() {
    let (tx, rx) = channel::<Ctl>();
    let tray_tx = tx.clone();
    let hotkey_tx = tx.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(Cmd(Mutex::new(tx)))
        .invoke_handler(tauri::generate_handler![
            send_event,
            list_recordings,
            open_folder
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            tray::build(&handle, tray_tx)?;

            // Ctrl+Shift+R — toggle. Что именно делать, решает аудио-поток по
            // состоянию машины: хоткей обязан работать и с закрытым окном, а
            // спрашивать состояние у webview, которого может не быть на экране,
            // — способ однажды не остановить запись.
            let shortcut = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyR);
            app.global_shortcut().on_shortcut(shortcut, move |_app, _sc, event| {
                // Только на нажатие: без этого один хоткей даёт две команды
                // (нажатие + отпускание), то есть старт и мгновенный стоп.
                if event.state() == ShortcutState::Pressed {
                    let _ = hotkey_tx.send(Ctl::Toggle);
                }
            })?;

            // Аудио-поток. Всё !Send рождается ВНУТРИ него.
            std::thread::spawn(move || audio::run(handle, rx, recordings_dir()));
            Ok(())
        })
        .on_window_event(|window, event| {
            // Крестик прячет окно, а не выходит: это трей-приложение, детект
            // обязан продолжать работать. Выход — только через меню трея, где он
            // проходит через финализацию записи.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("не удалось запустить приложение");
}
