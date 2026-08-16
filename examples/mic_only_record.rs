//! Проверка режима одного микрофона на настоящем железе и настоящих файлах.
//!
//! Существует потому, что этот режим невозможно проверить ни юнит-тестом, ни
//! руками через GUI. Юнит-тесты в `app.rs` гоняют его на фейковом захвате и
//! фейковой фабрике файлов — они доказывают логику (`has_system` спрашивается,
//! вторая дорожка не создаётся), но не доказывают, что настоящий
//! `MacAudio::new_mic_only` вообще отдаёт сэмплы и что `hound` дописывает
//! заголовок. А через GUI режим включается только отказом в системном
//! разрешении, которое `tccutil` на ad-hoc подписи не возвращает обратно.
//!
//! Здесь всё настоящее: реальный микрофон через cpal, реальный `WavSink`,
//! реальный каталог. Проверяется ровно то, ради чего режим сделан, — что на
//! диске оказывается ОДИН файл с ненулевым звуком, а не пара и не ничего.
//!
//! Запуск: `cargo run --example mic_only_record`
//! Пишет во временный каталог и убирает за собой.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("Пример только для macOS: режим одного микрофона — следствие того, что");
    eprintln!("на macOS захват системного звука спрашивают у человека. На Windows");
    eprintln!("loopback разрешения не требует, и отказывать в нём некому.");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use meeting_recorder::app::{App, MacAudio};
    use meeting_recorder::capture::DeviceChoice;
    use meeting_recorder::session::Event;
    use std::time::{Duration, Instant};

    // Свой каталог, а не `~/Recordings`: пример не имеет права подмешивать
    // тестовый файл к настоящим записям — его потом не отличить от встречи.
    let dir = std::env::temp_dir().join(format!("mr-mic-only-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    println!("каталог: {}", dir.display());

    let mut app = App::new_with_audio(
        dir.clone(),
        Box::new(MacAudio::new_mic_only(DeviceChoice::Default)),
    );

    println!("старт записи (5 с) — скажите что-нибудь в микрофон");
    app.on_event(Event::ManualStart, None)?;

    // Тот же шаг, что у настоящего цикла (`audio::run`, `src/main.rs`): режим
    // проверяется в тех же условиях, в каких работает.
    let начало = Instant::now();
    while начало.elapsed() < Duration::from_secs(5) {
        app.pump_audio()?;
        std::thread::sleep(Duration::from_millis(200));
    }

    app.on_event(Event::ManualStop, None)?;
    app.on_event(Event::FinalizeDone, None)?;

    // ---- что получилось --------------------------------------------------
    let mut файлы: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(Result::ok)
        .flat_map(|e| {
            let путь = e.path();
            // Запись ложится в месячную подпапку — обходим оба уровня.
            if путь.is_dir() {
                std::fs::read_dir(&путь)
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .collect()
            } else {
                vec![путь]
            }
        })
        .filter(|p| p.extension().is_some_and(|e| e == "wav"))
        .collect();
    файлы.sort();

    println!("\nфайлов на диске: {}", файлы.len());
    for f in &файлы {
        let размер = std::fs::metadata(f)?.len();
        println!("  {} — {размер} байт", f.file_name().unwrap().to_string_lossy());
    }

    let итог = проверить(&файлы);
    let _ = std::fs::remove_dir_all(&dir);
    итог
}

/// Отдельной функцией, чтобы каталог убирался в любом случае — и на успехе, и
/// на провале. Оставленный мусор в `/tmp` пережил бы разбор причины провала.
#[cfg(target_os = "macos")]
fn проверить(файлы: &[std::path::PathBuf]) -> Result<(), Box<dyn std::error::Error>> {
    if файлы.len() != 1 {
        return Err(format!(
            "ожидался ровно ОДИН файл (только микрофон), а их {}: {:?}",
            файлы.len(),
            файлы
        )
        .into());
    }
    let имя = файлы[0].file_name().unwrap().to_string_lossy().to_string();
    if !имя.ends_with(".mic.wav") {
        return Err(format!("единственный файл обязан быть дорожкой микрофона, а это {имя}").into());
    }

    // Размер, а не только существование: `hound` дописывает длину данных в
    // заголовок лишь на `finalize()`. Файл без него открывается, весит 44
    // байта и играет тишину — то есть отказ выглядел бы как успех.
    let размер = std::fs::metadata(&файлы[0])?.len();
    if размер <= 44 {
        return Err(format!("{имя} весит {размер} байт — это пустой заголовок, звука в нём нет").into());
    }

    println!("\nOK: один файл {имя}, {размер} байт, пары к нему нет.");
    Ok(())
}
