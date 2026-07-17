//! Оркестратор: связывает детектор, машину состояний, кольцо и запись на диск.
//!
//! Здесь нет ни ввода, ни вывода в консоль сверх логов — консольный цикл живёт
//! в `main.rs`, а в Task 7 его заменит GUI. `App` про это знать не должен:
//! наружу торчат только `on_event` (событие → действие) и `pump_audio`
//! (перелить накопленное аудио туда, куда велит состояние).

use crate::capture::{start_capture, Source};
use crate::detector::MicSession;
use crate::ringbuf::RingBuffer;
use crate::session::{Action, Event, SessionMachine, State};
use crate::storage::{recording_filename, Track, WavSink, SAMPLE_RATE};
use chrono::{DateTime, Local};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

const RING_SECONDS: usize = 30;
const RING_CAPACITY: usize = SAMPLE_RATE as usize * RING_SECONDS;

/// Сколько суффиксов перебрать, прежде чем сдаться (см. `free_name_pair`).
/// Тысяча записей в одну минуту — это уже не коллизия, а сломанные часы или
/// зацикленный вызывающий; честная ошибка лучше бесконечного цикла.
const MAX_SEQ: u32 = 1000;

type Res<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Живут только пока идёт детект или запись. Drop останавливает захват,
/// поэтому индикатор микрофона в Windows гаснет сразу после Idle.
struct Streams {
    _mic: cpal::Stream,
    _sys: cpal::Stream,
    rx_mic: Receiver<Vec<i16>>,
    rx_sys: Receiver<Vec<i16>>,
}

pub struct App {
    machine: SessionMachine,
    dir: PathBuf,
    ring_mic: RingBuffer,
    ring_sys: RingBuffer,
    sink_mic: Option<WavSink>,
    sink_sys: Option<WavSink>,
    streams: Option<Streams>,
    current_source: String,
    started: DateTime<Local>,
}

impl App {
    /// Микрофон здесь НЕ открывается. Потоки поднимаются только по детекту
    /// или по ручному старту — см. open_streams().
    pub fn new(dir: PathBuf) -> Self {
        Self {
            machine: SessionMachine::new(),
            dir,
            ring_mic: RingBuffer::new(RING_CAPACITY),
            ring_sys: RingBuffer::new(RING_CAPACITY),
            sink_mic: None,
            sink_sys: None,
            streams: None,
            current_source: "manual".into(),
            started: Local::now(),
        }
    }

    pub fn state(&self) -> State {
        self.machine.state()
    }

    pub fn state_is_armed(&self) -> bool {
        matches!(self.machine.state(), State::Armed)
    }

    fn open_streams(&mut self) -> Res {
        if self.streams.is_some() {
            return Ok(());
        }
        let (tx_mic, rx_mic) = channel();
        let (tx_sys, rx_sys) = channel();
        let _mic = start_capture(Source::Mic, tx_mic)?;
        let _sys = start_capture(Source::SystemLoopback, tx_sys)?;
        self.streams = Some(Streams {
            _mic,
            _sys,
            rx_mic,
            rx_sys,
        });
        Ok(())
    }

    /// Drop у cpal::Stream останавливает захват — этого достаточно.
    fn close_streams(&mut self) {
        self.streams = None;
    }

    /// Забрать всё, что накопилось в каналах захвата.
    fn drain_channels(&mut self) -> (Vec<i16>, Vec<i16>) {
        match &self.streams {
            Some(s) => (
                s.rx_mic.try_iter().flatten().collect(),
                s.rx_sys.try_iter().flatten().collect(),
            ),
            None => (Vec::new(), Vec::new()),
        }
    }

    pub fn on_event(&mut self, e: Event, source: Option<&MicSession>) -> Res {
        if let Some(s) = source {
            self.current_source = s.process_name.clone();
        }
        let action = self.machine.handle(e);
        self.apply(action)
    }

    fn apply(&mut self, action: Action) -> Res {
        match action {
            Action::StartRingBuffer => {
                self.started = Local::now();
                // Микрофон берётся ИМЕННО ЗДЕСЬ — на детекте, до вопроса.
                // Индикатор мика в Windows загорается в этот момент.
                self.open_streams()?;
            }
            Action::DiscardRing => {
                self.ring_mic.drain_to_vec();
                self.ring_sys.drain_to_vec();
                self.close_streams(); // отказ — отпускаем микрофон немедленно
            }
            Action::StartFileWrite => {
                self.started = Local::now();
                self.current_source = "manual".into();
                self.open_streams()?;
                self.open_sinks()?;
            }
            Action::FlushRingToFile => {
                self.open_sinks()?;
                let mic = self.ring_mic.drain_to_vec();
                let sys = self.ring_sys.drain_to_vec();
                if let Some(w) = self.sink_mic.as_mut() {
                    w.write(&mic)?;
                }
                if let Some(w) = self.sink_sys.as_mut() {
                    w.write(&sys)?;
                }
            }
            Action::CloseFile => {
                let result = self.close_sinks();
                self.close_streams();
                // FinalizeDone обязан уйти в машину ДАЖЕ при ошибке закрытия.
                // Иначе она навсегда останется в Finalizing, откуда единственный
                // выход — это событие, и приложение молча перестанет записывать
                // что-либо вообще. В Task 7 (аудио-цикл в фоновом потоке под Tauri)
                // это будет выглядеть как живой GUI, который ничего не пишет.
                self.machine.handle(Event::FinalizeDone);
                result?;
            }
            Action::None => {}
        }
        Ok(())
    }

    /// Открывает обе дорожки под одним, гарантированно свободным именем.
    fn open_sinks(&mut self) -> Res {
        let src = self.current_source.clone();
        let (mic, sys) = free_name_pair(&self.dir, self.started, &src)?;
        // Порядок важен: если вторая дорожка не открылась, первую надо
        // закрыть, иначе на диске останется осиротевший mic-файл, а
        // sink_mic — висеть в Some до следующего open_sinks.
        let sink_mic = WavSink::create(&self.dir, &mic)?;
        match WavSink::create(&self.dir, &sys) {
            Ok(sink_sys) => {
                self.sink_mic = Some(sink_mic);
                self.sink_sys = Some(sink_sys);
                Ok(())
            }
            Err(e) => {
                let _ = sink_mic.finalize();
                let _ = std::fs::remove_file(self.dir.join(&mic));
                Err(e.into())
            }
        }
    }

    /// Дописывает хвост и финализирует обе дорожки.
    ///
    /// Ошибку дописывания запоминаем, но НЕ выходим по `?`: финализировать
    /// файл надо в любом случае. hound пишет длину данных в заголовок только
    /// на finalize(); без него на диске останется WAV, который существует,
    /// весит сотни мегабайт, открывается плеером — и играет тишину.
    /// Отказ здесь выглядит как успех, поэтому дорожка финализируется даже
    /// тогда, когда запись хвоста в неё провалилась.
    fn close_sinks(&mut self) -> Res {
        // Хвост, натёкший между последним pump_audio и стопом.
        let (mic, sys) = self.drain_channels();
        let mut first_err: Option<Box<dyn std::error::Error>> = None;

        if let Some(w) = self.sink_mic.as_mut() {
            if let Err(e) = w.write(&mic) {
                first_err.get_or_insert(e.into());
            }
        }
        if let Some(w) = self.sink_sys.as_mut() {
            if let Err(e) = w.write(&sys) {
                first_err.get_or_insert(e.into());
            }
        }
        for sink in [self.sink_mic.take(), self.sink_sys.take()]
            .into_iter()
            .flatten()
        {
            match sink.finalize() {
                Ok(p) => println!("записано: {}", p.display()),
                Err(e) => {
                    first_err.get_or_insert(e.into());
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Прокачать накопленные семплы туда, куда велит текущее состояние.
    pub fn pump_audio(&mut self) -> Res {
        let (mic, sys) = self.drain_channels();
        match self.machine.state() {
            State::Armed => {
                self.ring_mic.push_slice(&mic);
                self.ring_sys.push_slice(&sys);
            }
            State::Recording(_) => {
                if let Some(w) = self.sink_mic.as_mut() {
                    w.write(&mic)?;
                }
                if let Some(w) = self.sink_sys.as_mut() {
                    w.write(&sys)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Вставляет порядковый номер в имя записи: `..._zoom.mic.wav` → `..._zoom_2.mic.wav`.
///
/// Опирается ровно на один инвариант `recording_filename`: первая точка в имени
/// отделяет основу от `{дорожка}.wav`. Он выполняется по построению — источник
/// проходит через `sanitize_source`, которая заменяет всё неалфавитно-цифровое
/// на `-`, а дата/время состоят из цифр, `-` и `_`. Формат даты здесь не
/// дублируется: имя берётся у `storage` и правится, а не собирается заново.
///
/// Разделитель — `_`, а не `-`, именно потому, что `sanitize_source` не может
/// его выпустить: `zoom_2` однозначно читается как «источник zoom, запись №2»,
/// тогда как `zoom-2` конфликтовал бы с процессом, который сам зовётся «Zoom 2».
fn with_seq(base: &str, n: u32) -> String {
    match base.split_once('.') {
        Some((stem, rest)) => format!("{stem}_{n}.{rest}"),
        None => format!("{base}_{n}"),
    }
}

/// Подбирает имена обеих дорожек так, чтобы ни одно не затёрло существующий файл.
///
/// Проблема: имя содержит время с точностью до минуты, а `hound` открывает файл
/// через `File::create`, который truncate'ит молча. Две записи в одну минуту
/// (вышел из звонка и сразу зашёл обратно; `x` и тут же `s`) затирали друг друга
/// без единого сообщения — тихая потеря записи.
///
/// Решение — суффикс-счётчик, общий для обеих дорожек. Именно общий: mic и system
/// одной записи обязаны лежать под одним именем, иначе их не сопоставить, поэтому
/// номер принимается только когда СВОБОДНЫ ОБА кандидата. Отсюда же и место фикса —
/// здесь, где обе дорожки открываются вместе, а не внутри `recording_filename`,
/// которая про вторую дорожку ничего не знает.
///
/// Первая запись в минуту сохраняет имя ровно по спеке (`..._zoom.mic.wav`) —
/// суффикс появляется только начиная со второй, так что обычный случай не меняется.
///
/// Проверка «файла нет» и его создание не атомарны, но это не гонка на практике:
/// приложение однопоточно по части открытия файлов и рассчитано на один экземпляр.
/// Два одновременно запущенных экземпляра дрались бы за микрофон куда заметнее,
/// чем за имя файла.
fn free_name_pair(dir: &Path, started: DateTime<Local>, source: &str) -> Res<(String, String)> {
    let mic = recording_filename(started, source, Track::Mic);
    let sys = recording_filename(started, source, Track::System);
    if !dir.join(&mic).exists() && !dir.join(&sys).exists() {
        return Ok((mic, sys));
    }
    for n in 2..=MAX_SEQ {
        let (m, s) = (with_seq(&mic, n), with_seq(&sys, n));
        if !dir.join(&m).exists() && !dir.join(&s).exists() {
            return Ok((m, s));
        }
    }
    Err(Box::new(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "не удалось подобрать свободное имя для записи «{mic}» в {}: \
             занято больше {MAX_SEQ} вариантов",
            dir.display()
        ),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn момент() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 7, 17, 14, 30, 0).unwrap()
    }

    /// Уникальный временный каталог, удаляется в Drop. Тот же приём, что в
    /// `storage::tests`: без внешних зависимостей и без мусора в `%TEMP%`.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path =
                std::env::temp_dir().join(format!("meeting-recorder-app-{tag}-{pid}-{nanos}-{n}"));
            std::fs::create_dir_all(&path).expect("создать временный каталог");
            Self(path)
        }
    }

    impl std::ops::Deref for ScratchDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn коснуться(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"").expect("создать файл-заглушку");
    }

    #[test]
    fn суффикс_встаёт_перед_дорожкой_а_не_в_конец_имени() {
        assert_eq!(
            with_seq("2026-07-17_14-30_zoom.mic.wav", 2),
            "2026-07-17_14-30_zoom_2.mic.wav"
        );
        assert_eq!(
            with_seq("2026-07-17_14-30_zoom.system.wav", 3),
            "2026-07-17_14-30_zoom_3.system.wav"
        );
    }

    /// Имя без точек — случая нет, но деградировать надо предсказуемо,
    /// а не паникой на unwrap.
    #[test]
    fn суффикс_для_имени_без_точки_просто_дописывается() {
        assert_eq!(with_seq("noext", 2), "noext_2");
    }

    #[test]
    fn чистый_каталог_даёт_имена_ровно_по_спеке_без_суффикса() {
        let dir = ScratchDir::new("clean");
        let (mic, sys) = free_name_pair(&dir, момент(), "zoom").expect("подбор имён");
        assert_eq!(mic, "2026-07-17_14-30_zoom.mic.wav");
        assert_eq!(sys, "2026-07-17_14-30_zoom.system.wav");
    }

    /// Гвоздь задачи: вторая запись в ту же минуту не имеет права затереть первую.
    #[test]
    fn вторая_запись_в_ту_же_минуту_получает_суффикс_а_не_затирает_первую() {
        let dir = ScratchDir::new("collision");
        коснуться(&dir, "2026-07-17_14-30_zoom.mic.wav");
        коснуться(&dir, "2026-07-17_14-30_zoom.system.wav");

        let (mic, sys) = free_name_pair(&dir, момент(), "zoom").expect("подбор имён");
        assert_eq!(mic, "2026-07-17_14-30_zoom_2.mic.wav");
        assert_eq!(sys, "2026-07-17_14-30_zoom_2.system.wav");
        assert!(dir.join("2026-07-17_14-30_zoom.mic.wav").exists());
    }

    #[test]
    fn третья_запись_в_ту_же_минуту_считает_дальше() {
        let dir = ScratchDir::new("third");
        for name in [
            "2026-07-17_14-30_zoom.mic.wav",
            "2026-07-17_14-30_zoom.system.wav",
            "2026-07-17_14-30_zoom_2.mic.wav",
            "2026-07-17_14-30_zoom_2.system.wav",
        ] {
            коснуться(&dir, name);
        }
        let (mic, sys) = free_name_pair(&dir, момент(), "zoom").expect("подбор имён");
        assert_eq!(mic, "2026-07-17_14-30_zoom_3.mic.wav");
        assert_eq!(sys, "2026-07-17_14-30_zoom_3.system.wav");
    }

    /// Суффикс обязан быть общим: если занята хотя бы одна дорожка, номер
    /// пропускают ОБЕ. Иначе mic уехал бы в `_2`, а system остался бы без
    /// суффикса — и пара разъехалась бы по именам.
    #[test]
    fn занятость_одной_дорожки_сдвигает_обе() {
        let dir = ScratchDir::new("half");
        коснуться(&dir, "2026-07-17_14-30_zoom.system.wav");

        let (mic, sys) = free_name_pair(&dir, момент(), "zoom").expect("подбор имён");
        assert_eq!(mic, "2026-07-17_14-30_zoom_2.mic.wav");
        assert_eq!(sys, "2026-07-17_14-30_zoom_2.system.wav");
    }

    #[test]
    fn разные_источники_в_одну_минуту_не_конфликтуют() {
        let dir = ScratchDir::new("sources");
        коснуться(&dir, "2026-07-17_14-30_zoom.mic.wav");
        коснуться(&dir, "2026-07-17_14-30_zoom.system.wav");

        let (mic, _) = free_name_pair(&dir, момент(), "manual").expect("подбор имён");
        assert_eq!(mic, "2026-07-17_14-30_manual.mic.wav");
    }

    /// Каталога ещё нет (первый запуск) — это не ошибка: WavSink::create
    /// сделает create_dir_all, а свободно тут заведомо всё.
    #[test]
    fn несуществующий_каталог_не_ломает_подбор() {
        let dir = ScratchDir::new("missing");
        let sub = dir.join("nested");
        let (mic, _) = free_name_pair(&sub, момент(), "zoom").expect("подбор имён");
        assert_eq!(mic, "2026-07-17_14-30_zoom.mic.wav");
    }

    /// Privacy-инвариант: App::new не трогает микрофон. Тест держит это
    /// свойство честным — соблазн открыть потоки один раз в конструкторе и
    /// не гасить их реален (проще код, нет задержки на старте записи), но
    /// тогда индикатор мика в Windows горел бы всё время работы приложения.
    #[test]
    fn новый_app_не_открывает_потоки_захвата() {
        let app = App::new(PathBuf::from("."));
        assert!(
            app.streams.is_none(),
            "App::new не имеет права открывать микрофон — он берётся на детекте"
        );
        assert_eq!(app.state(), State::Idle);
    }

    /// pump_audio в Idle не должен ничего требовать от каналов и файлов —
    /// это самый частый вызов в цикле (5 раз в секунду).
    #[test]
    fn pump_audio_в_idle_ничего_не_делает() {
        let mut app = App::new(PathBuf::from("."));
        app.pump_audio().expect("pump в Idle обязан быть no-op");
        assert_eq!(app.state(), State::Idle);
    }
}
