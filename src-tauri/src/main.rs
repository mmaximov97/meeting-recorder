// В релизе консольного окна за GUI быть не должно; в отладке оно нужно —
// eprintln! из аудио-потока это единственный способ увидеть, что там происходит.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod config;
mod status;
mod tray;

use audio::Ctl;
use config::Config;
use meeting_recorder::session::Event;
use serde::Serialize;
use status::{Snapshot, Status};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;
use tauri::{AppHandle, WindowEvent};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

/// Тот же каталог, что у консольного бинаря (`src/main.rs`). Хардкод переехал
/// сюда как есть: разъедься эти два пути, GUI перестал бы показывать записи,
/// сделанные консолью, — а консоль остаётся инструментом отладки того же ядра.
fn recordings_dir() -> PathBuf {
    PathBuf::from(r"C:\Users\Cypher\Recordings")
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
#[derive(Serialize, PartialEq, Eq, Debug)]
struct Recording {
    /// `2026-07-17_14-30_zoom` — общая основа обеих дорожек.
    name: String,
    mic: bool,
    system: bool,
    /// Суммарный размер дорожек в байтах.
    size: u64,
}

#[tauri::command]
fn send_event(name: &str, state: tauri::State<Cmd>, app: AppHandle) -> Result<(), String> {
    let ctl = match name {
        "confirm" => Ctl::Event(Event::UserConfirmed),
        "decline" => Ctl::Event(Event::UserDeclined),
        "start" => Ctl::Event(Event::ManualStart),
        "stop" => Ctl::Event(Event::ManualStop),
        // Toggle — не событие ядра: что делать, знает только машина (см. Ctl).
        "toggle" => Ctl::Toggle,
        other => return Err(format!("неизвестное событие: {other}")),
    };
    // Ошибку увидит и нажавший (она вернётся в webview), но одного этого мало:
    // кнопка в окне — не единственный вход, а причина у всех входов общая.
    // Поэтому провал send здесь — такой же фатальный случай, как в трее и на
    // хоткее, и объявляется он одинаково.
    state
        .send(ctl)
        .inspect_err(|_| status::fatal(&app, status::DEAD.to_string()))
}

/// Текущее состояние целиком — для тех, кто опоздал на события.
///
/// Без этой команды webview узнаёт состояние только из `emit`, а `emit` уходит
/// лишь на изменении и только уже подписанным. Значит, страница, загрузившаяся
/// после старта аудио-потока (то есть всегда) или перезагруженная посреди
/// записи, осталась бы с захардкоженным «Ожидание встречи» из `index.html` —
/// вплоть до следующей смены состояния. См. `status` целиком.
#[tauri::command]
fn get_state(status: tauri::State<Status>) -> Snapshot {
    status.snapshot()
}

/// Склеить дорожки в записи по основе имени.
///
/// Имя дорожки — `{основа}.{mic|system}.wav`, где основа это
/// `YYYY-MM-DD_HH-MM_источник` с необязательным `_N` у повторов в ту же минуту
/// (см. `app::free_name_pair`). Группируем срезанием суффикса дорожки: пара
/// склеивается обратно ровно тем же правилом, которым её разложили.
///
/// `BTreeMap` даёт сортировку по имени, то есть по дате — основа начинается с
/// `YYYY-MM-DD_HH-MM`, так что лексикографический порядок и есть хронологический.
/// Наверх список отдаётся перевёрнутым: свежее сверху.
///
/// Отделено от обхода каталога намеренно: правило склейки — это единственное
/// здесь, что можно сломать незаметно (отсутствие дорожки в паре UI показывает
/// предупреждением, и ошибка в группировке выглядела бы как испорченная запись).
/// Проверять его через `read_dir` значило бы держать в тесте настоящие файлы
/// ради логики, которой файлы не нужны.
fn group_recordings(files: impl IntoIterator<Item = (String, u64)>) -> Vec<Recording> {
    let mut found: BTreeMap<String, Recording> = BTreeMap::new();
    for (file, size) in files {
        let (base, is_mic) = match (file.strip_suffix(".mic.wav"), file.strip_suffix(".system.wav"))
        {
            (Some(b), _) => (b.to_string(), true),
            (_, Some(b)) => (b.to_string(), false),
            // Не наша дорожка — чужой файл в каталоге, не наше дело.
            _ => continue,
        };
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

    found.into_values().rev().collect()
}

/// Список записей. Обход каталога — здесь, склейка — в [`group_recordings`].
#[tauri::command]
fn list_recordings() -> Result<Vec<Recording>, String> {
    let dir = recordings_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        // Каталога нет — записей просто ещё не было. Это не ошибка.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("не удалось прочитать {}: {e}", dir.display())),
    };

    Ok(group_recordings(entries.flatten().map(|entry| {
        (
            entry.file_name().to_string_lossy().into_owned(),
            entry.metadata().map(|m| m.len()).unwrap_or(0),
        )
    })))
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

/// Доступные микрофоны для выпадашки: идентификатор и что показать.
///
/// `InputDevice` уже `Serialize`? Нет — он в ядре, где serde не подключён.
/// Поэтому здесь своя DTO: тащить serde в ядро ради одной структуры значило бы
/// расширить его зависимости под нужду GUI.
#[derive(serde::Serialize)]
struct MicDevice {
    id: String,
    name: String,
}

#[tauri::command]
fn list_mic_devices() -> Result<Vec<MicDevice>, String> {
    Ok(meeting_recorder::capture::list_input_devices()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|d| MicDevice {
            id: d.id,
            name: d.name,
        })
        .collect())
}

#[tauri::command]
fn get_config(app: AppHandle) -> Config {
    Config::load(&app)
}

/// `id: None` — вернуться на системный дефолт.
///
/// Имя приходит вместе с идентификатором и сохраняется рядом: когда устройства
/// не окажется в системе, показать пользователю будет нечего, кроме него.
#[tauri::command]
fn set_mic_device(
    id: Option<String>,
    name: Option<String>,
    state: tauri::State<Cmd>,
    app: AppHandle,
) -> Result<(), String> {
    let cfg = Config {
        mic_device_id: id,
        mic_device_name: name,
    };
    cfg.save(&app)?;
    state
        .send(Ctl::SetMicDevice(cfg.choice()))
        .inspect_err(|_| status::fatal(&app, status::DEAD.to_string()))
}

fn main() {
    let (tx, rx) = channel::<Ctl>();
    let tray_tx = tx.clone();
    let hotkey_tx = tx.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(Cmd(Mutex::new(tx)))
        // Заводится до setup(): аудио-поток пишет сюда с первой же строки, а
        // фатальная ошибка там случается раньше, чем webview успеет подписаться.
        .manage(Status::default())
        .invoke_handler(tauri::generate_handler![
            send_event,
            get_state,
            list_recordings,
            open_folder,
            list_mic_devices,
            get_config,
            set_mic_device
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            tray::build(&handle, tray_tx)?;

            // Ctrl+Shift+R — toggle. Что именно делать, решает аудио-поток по
            // состоянию машины: хоткей обязан работать и с закрытым окном, а
            // спрашивать состояние у webview, которого может не быть на экране,
            // — способ однажды не остановить запись.
            let shortcut = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyR);
            app.global_shortcut().on_shortcut(shortcut, move |app, _sc, event| {
                // Только на нажатие: без этого один хоткей даёт две команды
                // (нажатие + отпускание), то есть старт и мгновенный стоп.
                if event.state() != ShortcutState::Pressed {
                    return;
                }
                // Хоткей — самый молчаливый из входов: нажатие вслепую, без
                // окна и без меню. Проглотить здесь ошибку значит оставить
                // пользователя уверенным, что запись идёт.
                if hotkey_tx.send(Ctl::Toggle).is_err() {
                    status::fatal(app, status::DEAD.to_string());
                }
            })?;

            // Аудио-поток. Всё !Send рождается ВНУТРИ него.
            let mic = Config::load(&handle).choice();
            std::thread::spawn(move || audio::run(handle, rx, recordings_dir(), mic));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(name: &str, mic: bool, system: bool, size: u64) -> Recording {
        Recording {
            name: name.to_string(),
            mic,
            system,
            size,
        }
    }

    fn group(files: &[(&str, u64)]) -> Vec<Recording> {
        group_recordings(files.iter().map(|(n, s)| (n.to_string(), *s)))
    }

    #[test]
    fn пара_дорожек_склеивается_в_одну_запись() {
        assert_eq!(
            group(&[
                ("2026-07-17_14-45_zoom.mic.wav", 100),
                ("2026-07-17_14-45_zoom.system.wav", 20),
            ]),
            vec![rec("2026-07-17_14-45_zoom", true, true, 120)],
            "размер записи — сумма дорожек, имя — общая основа"
        );
    }

    /// Отсутствие дорожки — не косметика: пара mic+system и есть запись, и UI
    /// показывает неполную пару предупреждением.
    #[test]
    fn одинокая_дорожка_видна_как_неполная() {
        assert_eq!(
            group(&[("2026-07-17_14-45_zoom.mic.wav", 100)]),
            vec![rec("2026-07-17_14-45_zoom", true, false, 100)]
        );
        assert_eq!(
            group(&[("2026-07-17_14-45_zoom.system.wav", 100)]),
            vec![rec("2026-07-17_14-45_zoom", false, true, 100)]
        );
    }

    #[test]
    fn чужие_файлы_в_каталоге_не_наше_дело() {
        assert_eq!(
            group(&[
                ("заметки.txt", 10),
                ("2026-07-17_14-45_zoom.wav", 10),
                ("mic.wav", 10),
                (".mic.wav.bak", 10),
                ("2026-07-17_14-45_zoom.mic.wav", 100),
            ]),
            vec![rec("2026-07-17_14-45_zoom", true, false, 100)]
        );
    }

    /// Основа начинается с `YYYY-MM-DD_HH-MM`, поэтому лексикографический
    /// порядок BTreeMap и есть хронологический, а `.rev()` даёт «свежее сверху».
    #[test]
    fn свежее_сверху_независимо_от_порядка_обхода() {
        let list = group(&[
            ("2026-07-17_09-00_meet.mic.wav", 1),
            ("2026-07-18_10-00_zoom.mic.wav", 1),
            ("2026-07-16_23-59_teams.mic.wav", 1),
        ]);
        let names: Vec<&str> = list.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "2026-07-18_10-00_zoom",
                "2026-07-17_09-00_meet",
                "2026-07-16_23-59_teams"
            ]
        );
    }

    /// `_N` у повтора в ту же минуту — часть основы (см. `app::with_seq`),
    /// значит это отдельная запись, а не вторая дорожка первой.
    #[test]
    fn повтор_в_ту_же_минуту_это_отдельная_запись() {
        let list = group(&[
            ("2026-07-17_14-45_zoom.mic.wav", 1),
            ("2026-07-17_14-45_zoom.system.wav", 1),
            ("2026-07-17_14-45_zoom_2.mic.wav", 5),
            ("2026-07-17_14-45_zoom_2.system.wav", 5),
        ]);
        assert_eq!(
            list,
            vec![
                rec("2026-07-17_14-45_zoom_2", true, true, 10),
                rec("2026-07-17_14-45_zoom", true, true, 2),
            ]
        );
    }

    #[test]
    fn пустой_каталог_это_пустой_список_а_не_ошибка() {
        assert_eq!(group(&[]), vec![]);
    }
}
