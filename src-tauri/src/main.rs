// В релизе консольного окна за GUI быть не должно; в отладке оно нужно —
// eprintln! из аудио-потока это единственный способ увидеть, что там происходит.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod config;
mod imbalance;
mod rename;
mod status;
mod tray;

use audio::Ctl;
use config::Config;
use imbalance::Cache;
use meeting_recorder::session::Event;
use serde::Serialize;
use status::{Snapshot, Status};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;
use tauri::{AppHandle, WindowEvent};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

/// Корень записей. Тот же, что у консольного бинаря (`src/main.rs`): разъедься
/// эти два пути, GUI перестал бы показывать записи, сделанные консолью, — а
/// консоль остаётся инструментом отладки того же ядра.
///
/// Конкретная запись ложится в месячную подпапку, см. `storage::month_dir`.
#[cfg(target_os = "windows")]
fn recordings_root() -> PathBuf {
    PathBuf::from(r"C:\Users\<username>\Recordings")
}

#[cfg(target_os = "macos")]
fn recordings_root() -> PathBuf {
    let home = std::env::var("HOME").expect("$HOME обязан быть установлен");
    PathBuf::from(home).join("Recordings")
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
///
/// `Eq` из производных убран: появилось поле `f32`, на котором он не выводится.
/// `assert_eq!` в тестах работает и на одном `PartialEq`.
#[derive(Serialize, PartialEq, Debug)]
struct Recording {
    /// `2026-07-17_14-30_zoom` — общая основа обеих дорожек.
    name: String,
    /// Месячная папка или `None` для корня (записи до перехода на папки).
    folder: Option<String>,
    mic: bool,
    system: bool,
    /// Суммарный размер дорожек в байтах.
    size: u64,
    /// Насколько mic-дорожка тише system, в дБ. Заполняется в `list_recordings`
    /// после группировки — считать это здесь значило бы тащить в чистую
    /// функцию чтение файлов.
    #[serde(skip_serializing_if = "Option::is_none")]
    imbalance_db: Option<f32>,
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

/// Склеить дорожки в записи по основе имени И папке.
///
/// Имя дорожки — `{основа}.{mic|system}.wav`, где основа это
/// `YYYY-MM-DD_HH-MM_источник` с необязательным `_N` у повторов в ту же минуту
/// (см. `app::free_name_pair`). Группируем срезанием суффикса дорожки: пара
/// склеивается обратно ровно тем же правилом, которым её разложили.
///
/// **Ключ — пара `(base, folder)`, а не одна `base`.** Прежняя версия считала
/// «дата в основе однозначно задаёт месячную папку, поэтому одна запись не
/// может лежать в двух папках сразу» — эта посылка ложна в двух достижимых
/// сценариях:
///
/// 1. Прерванная миграция: `scripts/migrate-to-month-folders.sh` переносит
///    ПОФАЙЛОВО (`for entry in *`, один `mv` на файл) и на конфликте выходит с
///    кодом 1, не откатывая уже перенесённые члены тройки. После такого
///    прогона `.mic.wav` может остаться в корне, а `.system.wav` — уже уехать
///    в `2026-07/`.
/// 2. Коллизия имён между корнем и месячной папкой: `free_name_pair`
///    (`src/app.rs`) проверяет занятость имени только внутри целевой месячной
///    папки — список файлов корня в неё не попадает. Пока миграция не
///    прогнана, новая запись в ту же минуту с тем же источником, что и старая
///    корневая запись, получит имя БЕЗ суффикса `_2` и совпадёт с ней по основе.
///
/// Ключ только по `base` в обоих случаях схлопнул бы две половинки в одну
/// «полную» запись, чья `folder` бралась бы от первого встреченного файла:
/// «Переименовать» переименовало бы только эту половину, вторая дорожка (и,
/// возможно, `.transcript`) осталась бы под старым именем молча — `rename_
/// recording` вернул бы `Ok`, ничего не сообщив о разрыве. Ключ `(base,
/// folder)` вместо этого честно показывает такую ситуацию как ДВЕ неполные
/// записи — ровно то, чем она и является на диске.
///
/// `BTreeMap` по-прежнему сортирует в первую очередь по `base` — оно первый
/// компонент кортежа, `folder` работает только тайбрейком при совпадении
/// основы. Основа начинается с `YYYY-MM-DD_HH-MM`, так что лексикографический
/// порядок и есть хронологический. Наверх список отдаётся перевёрнутым:
/// свежее сверху.
///
/// Отделено от обхода каталога намеренно: правило склейки — это единственное
/// здесь, что можно сломать незаметно (отсутствие дорожки в паре UI показывает
/// предупреждением, и ошибка в группировке выглядела бы как испорченная запись).
/// Проверять его через `read_dir` значило бы держать в тесте настоящие файлы
/// ради логики, которой файлы не нужны.
fn group_recordings(
    files: impl IntoIterator<Item = (Option<String>, String, u64)>,
) -> Vec<Recording> {
    let mut found: BTreeMap<(String, Option<String>), Recording> = BTreeMap::new();
    for (folder, file, size) in files {
        let (base, is_mic) = match (file.strip_suffix(".mic.wav"), file.strip_suffix(".system.wav"))
        {
            (Some(b), _) => (b.to_string(), true),
            (_, Some(b)) => (b.to_string(), false),
            // Не наша дорожка — чужой файл в каталоге, не наше дело.
            _ => continue,
        };
        let key = (base.clone(), folder.clone());
        let rec = found.entry(key).or_insert(Recording {
            name: base,
            folder,
            mic: false,
            system: false,
            size: 0,
            imbalance_db: None,
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

/// Похоже ли имя папки на месячную (`2026-07`).
fn is_month_folder(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 7
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..].iter().all(u8::is_ascii_digit)
}

/// Файлы корня плюс файлы месячных подпапок. Глубина ровно два уровня:
/// предсказуемо и не засасывает чужое дерево, если рядом окажется постороннее.
fn collect_files(root: &Path) -> Result<Vec<(Option<String>, String, u64)>, String> {
    fn read(dir: &Path, folder: Option<&str>, out: &mut Vec<(Option<String>, String, u64)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                out.push((
                    folder.map(str::to_string),
                    e.file_name().to_string_lossy().into_owned(),
                    e.metadata().map(|m| m.len()).unwrap_or(0),
                ));
            }
        }
    }

    let mut out = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        // Каталога нет — записей просто ещё не было. Это не ошибка.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("не удалось прочитать {}: {e}", root.display())),
    };
    let mut months: Vec<PathBuf> = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        match e.file_type() {
            Ok(t) if t.is_dir() && is_month_folder(&name) => months.push(e.path()),
            Ok(t) if t.is_file() => out.push((
                None,
                name,
                e.metadata().map(|m| m.len()).unwrap_or(0),
            )),
            _ => {}
        }
    }
    for m in months {
        let folder = m.file_name().map(|n| n.to_string_lossy().into_owned());
        read(&m, folder.as_deref(), &mut out);
    }
    Ok(out)
}

/// Список записей для окна: обход каталога, склейка дорожек в пары и разметка
/// дисбаланса громкости.
///
/// Три шага одной команды, а не три отдельных, и это не одно и то же с точки
/// зрения того, где что тестируется. Обход (`collect_files`) и склейка
/// (`group_recordings`) разделены сознательно — см. их докблоки: это
/// единственная логика здесь, которую можно сломать незаметно, и она чистая,
/// без файлового ввода-вывода, значит проверяется без диска. Разметка
/// дисбаланса, наоборот, СОБРАНА прямо тут, а не вынесена рядом: она читает
/// содержимое файлов через `Cache::rms`, и утаскивать чтение файлов в чистую
/// функцию значило бы отнять у неё главное свойство — тестируемость без диска.
///
/// Пометка считается только для полных пар (`r.mic && r.system`): одинокая
/// дорожка уже помечена как неполная в UI, второе предупреждение поверх неё
/// ничего не добавит, а чтение файла стоит времени зря.
#[tauri::command]
fn list_recordings(cache: tauri::State<Cache>) -> Result<Vec<Recording>, String> {
    let root = recordings_root();
    let mut list = group_recordings(collect_files(&root)?);
    for r in &mut list {
        // Пометка имеет смысл только для полной пары: одинокая дорожка уже
        // помечена как неполная, и второе предупреждение о ней ничего не добавит.
        if !(r.mic && r.system) {
            continue;
        }
        let dir = match &r.folder {
            Some(f) => root.join(f),
            None => root.clone(),
        };
        let mic = cache.rms(&dir.join(format!("{}.mic.wav", r.name)));
        let sys = cache.rms(&dir.join(format!("{}.system.wav", r.name)));
        if let (Some(m), Some(s)) = (mic, sys) {
            r.imbalance_db = imbalance::imbalance(m, s);
        }
    }
    Ok(list)
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
    let dir = recordings_root();
    // Иначе explorer откроет «Документы» вместо пустого несуществующего пути.
    std::fs::create_dir_all(&dir).map_err(|e| format!("не удалось создать {}: {e}", dir.display()))?;
    #[cfg(target_os = "windows")]
    let mut cmd = std::process::Command::new("explorer.exe");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    cmd.arg(&dir);
    cmd.spawn().map_err(|e| format!("не удалось открыть Finder/проводник: {e}"))?;
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

/// Переименовать запись. `folder` — месячная папка или `None` для корня.
#[tauri::command]
fn rename_recording(
    folder: Option<String>,
    base: String,
    new_tail: String,
) -> Result<String, String> {
    let dir = match folder {
        Some(f) => recordings_root().join(f),
        None => recordings_root(),
    };
    rename::rename_recording(&dir, &base, &new_tail)
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

/// Включить/выключить проверку микрофона.
#[tauri::command]
fn set_monitor(on: bool, state: tauri::State<Cmd>, app: AppHandle) -> Result<(), String> {
    state
        .send(Ctl::Monitor(on))
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
        .manage(Cache::default())
        .invoke_handler(tauri::generate_handler![
            send_event,
            get_state,
            list_recordings,
            open_folder,
            list_mic_devices,
            get_config,
            set_mic_device,
            set_monitor,
            rename_recording
        ])
        .setup(move |app| {
            // Приложение строки меню, а не Dock: окно стартует скрытым, крестик
            // его прячет, а не выходит, — иконка в Dock, за которой нет окна и
            // по клику на которую ничего не происходит (обработчика Reopen у
            // нас нет), только вводила бы в заблуждение.
            //
            // Парная половина решения — `LSUIElement` в `src-tauri/Info.plist`.
            // Нужны обе, и вот почему ни одной по отдельности не хватает:
            // `LSUIElement` убирает Dock на момент запуска, но tao на
            // `applicationDidFinishLaunching` безусловно зовёт
            // `setActivationPolicy` своим значением, а его дефолт — `Regular`
            // (tao 0.35.3, `app_state.rs`: `launched` → `apply_activation_policy`),
            // и иконка вернулась бы. Эта строка задаёт tao нужное значение ДО
            // старта цикла событий, но сама по себе успела бы дать Dock'у
            // мигнуть.
            //
            // На показ окна из `status::fatal` это не влияет: `set_focus()` в
            // tao — это `makeKeyAndOrderFront` + `activateIgnoringOtherApps`,
            // то есть явная активация, которую accessory-приложению как раз и
            // положено делать самому.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

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
            std::thread::spawn(move || audio::run(handle, rx, recordings_root(), mic));
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

    fn rec(name: &str, folder: Option<&str>, mic: bool, system: bool, size: u64) -> Recording {
        Recording {
            name: name.to_string(),
            folder: folder.map(str::to_string),
            mic,
            system,
            size,
            imbalance_db: None,
        }
    }

    fn group(files: &[(Option<&str>, &str, u64)]) -> Vec<Recording> {
        group_recordings(
            files
                .iter()
                .map(|(f, n, s)| (f.map(str::to_string), n.to_string(), *s)),
        )
    }

    #[test]
    fn пара_дорожек_склеивается_в_одну_запись() {
        assert_eq!(
            group(&[
                (None, "2026-07-17_14-45_zoom.mic.wav", 100),
                (None, "2026-07-17_14-45_zoom.system.wav", 20),
            ]),
            vec![rec("2026-07-17_14-45_zoom", None, true, true, 120)],
            "размер записи — сумма дорожек, имя — общая основа"
        );
    }

    /// Отсутствие дорожки — не косметика: пара mic+system и есть запись, и UI
    /// показывает неполную пару предупреждением.
    #[test]
    fn одинокая_дорожка_видна_как_неполная() {
        assert_eq!(
            group(&[(None, "2026-07-17_14-45_zoom.mic.wav", 100)]),
            vec![rec("2026-07-17_14-45_zoom", None, true, false, 100)]
        );
        assert_eq!(
            group(&[(None, "2026-07-17_14-45_zoom.system.wav", 100)]),
            vec![rec("2026-07-17_14-45_zoom", None, false, true, 100)]
        );
    }

    #[test]
    fn чужие_файлы_в_каталоге_не_наше_дело() {
        assert_eq!(
            group(&[
                (None, "заметки.txt", 10),
                (None, "2026-07-17_14-45_zoom.wav", 10),
                (None, "mic.wav", 10),
                (None, ".mic.wav.bak", 10),
                (None, "2026-07-17_14-45_zoom.mic.wav", 100),
            ]),
            vec![rec("2026-07-17_14-45_zoom", None, true, false, 100)]
        );
    }

    /// Основа начинается с `YYYY-MM-DD_HH-MM`, поэтому лексикографический
    /// порядок BTreeMap и есть хронологический, а `.rev()` даёт «свежее сверху».
    #[test]
    fn свежее_сверху_независимо_от_порядка_обхода() {
        let list = group(&[
            (None, "2026-07-17_09-00_meet.mic.wav", 1),
            (None, "2026-07-18_10-00_zoom.mic.wav", 1),
            (None, "2026-07-16_23-59_teams.mic.wav", 1),
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
            (None, "2026-07-17_14-45_zoom.mic.wav", 1),
            (None, "2026-07-17_14-45_zoom.system.wav", 1),
            (None, "2026-07-17_14-45_zoom_2.mic.wav", 5),
            (None, "2026-07-17_14-45_zoom_2.system.wav", 5),
        ]);
        assert_eq!(
            list,
            vec![
                rec("2026-07-17_14-45_zoom_2", None, true, true, 10),
                rec("2026-07-17_14-45_zoom", None, true, true, 2),
            ]
        );
    }

    #[test]
    fn пустой_каталог_это_пустой_список_а_не_ошибка() {
        assert_eq!(group(&[]), vec![]);
    }

    #[test]
    fn записи_из_подпапки_и_из_корня_живут_в_одном_списке() {
        let list = group(&[
            (Some("2026-07"), "2026-07-30_13-03_chrome.mic.wav", 10),
            (Some("2026-07"), "2026-07-30_13-03_chrome.system.wav", 10),
            (None, "2026-06-01_10-00_zoom.mic.wav", 5),
        ]);
        assert_eq!(
            list,
            vec![
                rec("2026-07-30_13-03_chrome", Some("2026-07"), true, true, 20),
                rec("2026-06-01_10-00_zoom", None, true, false, 5),
            ],
            "порядок хронологический независимо от папки"
        );
    }

    #[test]
    fn папка_записи_запоминается() {
        let list = group(&[(Some("2026-07"), "2026-07-30_13-03_chrome.mic.wav", 1)]);
        assert_eq!(list[0].folder.as_deref(), Some("2026-07"));
    }

    /// БЛОКЕР ревью: одна и та же основа в двух разных папках — сценарий
    /// прерванной миграции (`.mic.wav` не успел переехать, `.system.wav` уже в
    /// `2026-07/`) или коллизии имён между корнем и месячной папкой
    /// (`free_name_pair` не видит файлы корня). Раньше ключом группировки была
    /// только основа, и обе половинки схлопывались в одну «полную» запись, чья
    /// `folder` бралась от первого встреченного файла, — «Переименовать»
    /// переименовало бы только эту половину, вторая дорожка молча осталась бы
    /// под старым именем. Ключ `(base, folder)` обязан показать это честно:
    /// ДВЕ неполные записи, каждая под своей папкой, а не одна целая.
    #[test]
    fn одна_основа_в_двух_папках_даёт_две_неполные_записи_а_не_одну_целую() {
        let list = group(&[
            (Some("2026-07"), "2026-07-30_13-03_chrome.mic.wav", 10),
            (None, "2026-07-30_13-03_chrome.system.wav", 20),
        ]);
        assert_eq!(
            list,
            vec![
                rec("2026-07-30_13-03_chrome", Some("2026-07"), true, false, 10),
                rec("2026-07-30_13-03_chrome", None, false, true, 20),
            ],
            "половинки одной основы из разных папок не имеют права слиться в одну запись"
        );
    }

    #[test]
    fn месячной_папкой_считается_только_yyyy_mm() {
        assert!(is_month_folder("2026-07"));
        assert!(!is_month_folder("2026-7"));
        assert!(!is_month_folder("2026-07-30"));
        assert!(!is_month_folder("архив"));
        assert!(!is_month_folder(""));
    }
}
