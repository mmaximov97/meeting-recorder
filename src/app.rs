//! Оркестратор: связывает детектор, машину состояний, кольцо и запись на диск.
//!
//! Здесь нет ни ввода, ни вывода в консоль сверх логов — консольный цикл живёт
//! в `main.rs`, а в Task 7 его заменит GUI. `App` про это знать не должен:
//! наружу торчат только `on_event` (событие → действие) и `pump_audio`
//! (перелить накопленное аудио туда, куда велит состояние).
//!
//! # Почему захват и дорожки спрятаны за трейтами
//!
//! [`AudioIo`] и [`SinkFactory`] существуют ровно ради тестов. Инварианты, на
//! которых держится вся задача — «`FinalizeDone` уходит в машину даже при
//! ошибке закрытия», «финализируются обе дорожки, даже если запись хвоста в
//! первую провалилась», «микрофон отпускается на отказе» — все до одного
//! проявляются ТОЛЬКО на пути ошибки. Живой `cpal` и живой `hound` по команде
//! не падают, поэтому без подставного бэкенда эти ветки не выполняются в
//! тестах ни разу: код верен, покрытие нулевое, и любая «причёсывающая»
//! правка остаётся зелёной. Фейки в тестах умеют падать в нужной точке —
//! этого достаточно, чтобы каждый инвариант ловил свою мутацию.

#[cfg(target_os = "windows")]
use crate::capture::{build_loopback_capture, start_silence};
use crate::capture::{build_mic_capture, DeviceChoice};
use crate::detector::MicSession;
use crate::ringbuf::RingBuffer;
use crate::session::{Action, Event, SessionMachine, State};
use crate::storage::{month_dir, recording_filename, Track, WavSink, SAMPLE_RATE};
use chrono::{DateTime, Local};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::time::Instant;

const RING_SECONDS: usize = 30;
const RING_CAPACITY: usize = SAMPLE_RATE as usize * RING_SECONDS;

/// Сколько суффиксов перебрать, прежде чем сдаться (см. `free_name_pair`).
/// Тысяча записей в одну минуту — это уже не коллизия, а сломанные часы или
/// зацикленный вызывающий; честная ошибка лучше бесконечного цикла.
const MAX_SEQ: u32 = 1000;

type Res<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Одна дорожка, открытая на запись.
///
/// `finalize` забирает `Box<Self>`, а не `self`: дорожка обязана потребляться
/// (после финализации писать некуда), но трейт должен остаться объектно
/// безопасным — `App` держит `Option<Box<dyn Sink>>`.
pub trait Sink {
    fn write(&mut self, samples: &[i16]) -> Res;
    /// Дописывает WAV-заголовок с реальной длиной. Без него файл играет тишину.
    fn finalize(self: Box<Self>) -> Res<PathBuf>;
}

/// Открывает дорожки записи. Отдельно от [`Sink`], потому что имя файла
/// подбирается на каждую запись заново (см. `free_name_pair`).
pub trait SinkFactory {
    fn create(&self, dir: &Path, filename: &str) -> Res<Box<dyn Sink>>;
}

/// Что оркестратору нужно от захвата: взять микрофон, отпустить микрофон,
/// забрать накопленное.
///
/// `open`/`close` — это ровно те две точки, где в Windows загорается и гаснет
/// индикатор микрофона, поэтому они и вынесены в трейт: privacy-инварианты
/// («мик берётся на детекте, а не в `App::new`», «отказ отпускает мик
/// немедленно») проверяются через `is_open`.
pub trait AudioIo {
    fn open(&mut self) -> Res;
    fn close(&mut self);
    /// Держим ли мы сейчас микрофон, то есть горит ли индикатор в Windows.
    ///
    /// В рабочем коде не зовётся: `App` знает про захват из собственного
    /// состояния, а Task 7 будет спрашивать про индикатор у `state()`.
    /// Существует ради тестов privacy-инвариантов — «мик берётся на детекте,
    /// а не в `App::new`» и «отказ отпускает мик немедленно» иначе не
    /// сформулировать вовсе: наблюдать за индикатором изнутри теста больше
    /// нечем.
    #[cfg_attr(not(test), allow(dead_code))]
    fn is_open(&self) -> bool;
    /// Всё, что накопилось с прошлого раза: `(mic, system)`.
    fn drain(&mut self) -> (Vec<i16>, Vec<i16>);

    /// Сменить микрофон. Применяется к следующему `open()`: менять устройство
    /// под уже идущей записью значило бы порвать дорожку посередине.
    fn set_mic_device(&mut self, _choice: DeviceChoice) {}

    /// `Some(имя)` — при последнем `open()` просили это устройство, не нашли и
    /// взяли системный дефолт.
    fn fell_back_from(&self) -> Option<String> {
        None
    }

    /// Есть ли вообще вторая дорожка. `false` — системный звук захватить нечем,
    /// и запись обязана состоять из одного микрофона.
    ///
    /// Существует только ради macOS, где захват системного звука спрашивают у
    /// человека и он вправе отказать (см. `MacAudio::new_mic_only`). На Windows
    /// loopback-эндпоинт разрешения не требует, поэтому дефолт — `true`.
    ///
    /// Спрашивается в `open_sinks`, а не при записи: пустой `sys` из `drain`
    /// сам по себе неотличим от тишины, и без этого вопроса на диск ложился бы
    /// второй WAV, который открывается, играет тишину и выглядит как поломка
    /// записи, а не как отсутствие разрешения. Дорожки, которой нет, не должно
    /// быть и в файлах.
    fn has_system(&self) -> bool {
        true
    }
}

impl Sink for WavSink {
    fn write(&mut self, samples: &[i16]) -> Res {
        WavSink::write(self, samples)?;
        Ok(())
    }

    fn finalize(self: Box<Self>) -> Res<PathBuf> {
        Ok(WavSink::finalize(*self)?)
    }
}

struct WavSinks;

impl SinkFactory for WavSinks {
    fn create(&self, dir: &Path, filename: &str) -> Res<Box<dyn Sink>> {
        Ok(Box::new(WavSink::create(dir, filename)?))
    }
}

/// Живут только пока идёт детект или запись. Drop останавливает захват,
/// поэтому индикатор микрофона в Windows гаснет сразу после Idle.
///
/// Windows-only вместе с `CpalAudio`, которая единственная её и строит:
/// у `MacAudio` системная дорожка не в потоке cpal, а в общем на весь процесс
/// `SystemTap`, и держать под неё поле здесь нечего.
#[cfg(target_os = "windows")]
struct Streams {
    /// Тихий render-поток: не даёт эндпоинту простаивать, иначе WASAPI loopback
    /// не отдаёт пакеты и дорожка `system` начинается не с открытия потока, а с
    /// момента, когда в системе впервые что-то заиграло (см. `start_silence`).
    ///
    /// Лежит здесь, а не рядом с `App`, ровно ради требования «живёт столько же,
    /// сколько захват»: раз он в той же структуре, что `_mic`/`_sys`, то один и
    /// тот же `open` его поднимает, а один и тот же Drop — гасит. Забыть погасить
    /// его отдельно невозможно, потому что отдельного гашения не существует.
    ///
    /// `Option`, потому что тишина — это средство выравнивания, а не условие
    /// записи (см. `open`).
    _silence: Option<cpal::Stream>,
    _mic: cpal::Stream,
    _sys: cpal::Stream,
    rx_mic: Receiver<Vec<i16>>,
    rx_sys: Receiver<Vec<i16>>,
}

/// Реальный захват через cpal.
#[cfg(target_os = "windows")]
struct CpalAudio {
    streams: Option<Streams>,
    /// Какое устройство просить на следующем `open()`.
    mic: DeviceChoice,
    /// `Some(имя)` — на последнем `open()` просили не дефолт, не нашли и
    /// взяли системный дефолт.
    fell_back: Option<String>,
    /// `MR_DEBUG_TIMING=1` — замер стоимости открытия потоков и задержки
    /// первого чанка по каждой дорожке. Живого звонка отладчиком не поймать,
    /// а расхождение старта mic и loopback видно только на числах: это
    /// доказательная база для отдельной задачи про смещение дорожек.
    timing: bool,
    /// Начало `open()` — общая точка отсчёта для обеих дорожек.
    opened_at: Option<Instant>,
    logged_mic: bool,
    logged_sys: bool,
}

#[cfg(target_os = "windows")]
impl CpalAudio {
    fn new(mic: DeviceChoice) -> Self {
        Self {
            streams: None,
            mic,
            fell_back: None,
            timing: std::env::var_os("MR_DEBUG_TIMING").is_some(),
            opened_at: None,
            logged_mic: false,
            logged_sys: false,
        }
    }
}

#[cfg(target_os = "windows")]
impl AudioIo for CpalAudio {
    fn open(&mut self) -> Res {
        if self.streams.is_some() {
            return Ok(());
        }
        let (tx_mic, rx_mic) = channel();
        let (tx_sys, rx_sys) = channel();

        let t0 = Instant::now();
        // Тишина поднимается ПЕРВОЙ и намеренно: к моменту, когда откроется
        // loopback, эндпоинт уже обязан не простаивать. Открой мы её последней —
        // между стартом loopback и первым пакетом осталась бы дыра ровно той
        // длины, которую эта задача и убирает.
        //
        // Отказ тихого потока НЕ роняет запись: выравнивание — средство, а
        // встреча — цель, и «дорожки разъехались» несравнимо дешевле, чем «записи
        // нет вообще». Молчанием это не становится: причина уходит в stderr, а
        // сам сценарий почти невозможен — устройство и конфиг здесь ровно те же,
        // что у loopback ниже, а output-поток вдобавок терпимее к формату
        // (AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM), так что упасть в одиночку ему
        // практически негде: почти всегда следом упадёт и `build_mic_capture`, уже с `?`.
        let _silence = match start_silence() {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!(
                    "не удалось поднять тихий render-поток: {e}\n\
                     запись продолжится, но дорожка system может начаться позже mic \
                     (loopback молчит, пока эндпоинт простаивает) — выравнивание не гарантировано"
                );
                None
            }
        };
        let t1 = Instant::now();

        // Сначала ОТКРЫВАЕМ оба потока, не запуская ни одного. Открытие стоит
        // дорого и непредсказуемо (loopback на спящем BT-эндпоинте — до 795 мс
        // против 20 мс на проснувшемся), и вся эта разница ушла бы прямо в
        // расхождение дорожек, стартуй мы их по очереди «открыл-запустил».
        let (mic_pending, fell_back) = build_mic_capture(&self.mic, tx_mic)?;
        self.fell_back = fell_back;
        let t2 = Instant::now();
        let sys_pending = build_loopback_capture(tx_sys)?;
        let t3 = Instant::now();

        // ...и только теперь запускаем — двумя вызовами подряд, между которыми
        // не делается ничего. Отсюда и берётся остаточная Δ: это уже не
        // стоимость открытия, а только промежуток между двумя `Start()`.
        let _mic = mic_pending.play()?;
        let t4 = Instant::now();
        let _sys = sys_pending.play()?;
        let t5 = Instant::now();

        if self.timing {
            eprintln!(
                "[timing] start_silence           = {:?}{}",
                t1.duration_since(t0),
                if _silence.is_some() {
                    ""
                } else {
                    " (НЕ ПОДНЯЛСЯ)"
                }
            );
            eprintln!(
                "[timing] build(mic)              = {:?}",
                t2.duration_since(t1)
            );
            eprintln!(
                "[timing] build(loopback)         = {:?}",
                t3.duration_since(t2)
            );
            eprintln!(
                "[timing] play(mic)               = {:?}",
                t4.duration_since(t3)
            );
            eprintln!(
                "[timing] play(loopback)          = {:?}",
                t5.duration_since(t4)
            );
            // Остаточное смещение дорожек — промежуток между двумя стартами,
            // то есть ровно стоимость play(mic). Стоимость открытия сюда уже
            // не входит: она вся уплачена выше, до первого Start().
            eprintln!(
                "[timing] ожидаемая Δ дорожек     = {:?}",
                t4.duration_since(t3)
            );
            eprintln!(
                "[timing] open_streams всего      = {:?}",
                t5.duration_since(t0)
            );
        }
        self.opened_at = Some(t0);
        self.logged_mic = false;
        self.logged_sys = false;

        self.streams = Some(Streams {
            _silence,
            _mic,
            _sys,
            rx_mic,
            rx_sys,
        });
        Ok(())
    }

    /// Drop у cpal::Stream останавливает захват — этого достаточно.
    /// Тихий render-поток лежит в той же структуре и умирает тем же Drop'ом:
    /// отдельно его гасить не надо и, что важнее, невозможно забыть.
    fn close(&mut self) {
        self.streams = None;
        self.opened_at = None;
    }

    fn is_open(&self) -> bool {
        self.streams.is_some()
    }

    fn drain(&mut self) -> (Vec<i16>, Vec<i16>) {
        let (mic, sys) = match &self.streams {
            Some(s) => (
                s.rx_mic.try_iter().flatten().collect::<Vec<i16>>(),
                s.rx_sys.try_iter().flatten().collect::<Vec<i16>>(),
            ),
            None => (Vec::new(), Vec::new()),
        };
        if self.timing {
            // Замер грубый: drain зовётся из цикла раз в 200 мс, так что
            // «первый чанк» округлён вверх до тика. Для смещения масштаба
            // ~1 с этого хватает, для микросекундных выводов — нет.
            if let Some(t0) = self.opened_at {
                if !self.logged_mic && !mic.is_empty() {
                    eprintln!("[timing] первый чанк mic      = +{:?}", t0.elapsed());
                    self.logged_mic = true;
                }
                if !self.logged_sys && !sys.is_empty() {
                    eprintln!("[timing] первый чанк loopback = +{:?}", t0.elapsed());
                    self.logged_sys = true;
                }
            }
        }
        (mic, sys)
    }

    fn set_mic_device(&mut self, choice: DeviceChoice) {
        self.mic = choice;
    }

    fn fell_back_from(&self) -> Option<String> {
        self.fell_back.clone()
    }
}

/// Реальный захват на macOS: микрофон через cpal, система — через процесс-тап.
///
/// Структурно параллельна [`CpalAudio`], но с одним принципиальным отличием:
/// системная дорожка НЕ открывается в `open()` и не гаснет в `close()`. Тап
/// поднят снаружи, до `App`, и течёт всё время работы приложения — иначе
/// сигналу активности звука, которым на macOS достраивается детект, не на чем
/// было бы работать до первого детекта (см. докблок `capture::SystemTap`).
///
/// `pub` — потому что `MacAudio::new` зовётся из `src-tauri/src/audio.rs`,
/// другого крейта. Поля при этом остаются приватными: наружу торчит только
/// конструктор.
#[cfg(target_os = "macos")]
pub struct MacAudio {
    mic: Option<cpal::Stream>,
    mic_rx: Option<Receiver<Vec<i16>>>,
    mic_choice: DeviceChoice,
    fell_back: Option<String>,
    /// Живёт всё время процесса — сконструирован снаружи и передан сюда,
    /// а не создаётся в `open()`/`close()`.
    ///
    /// `None` — человек отказал в разрешении на захват системного звука
    /// (см. `new_mic_only`). Приложение при этом продолжает работать, но пишет
    /// одну дорожку и теряет автодетект: на macOS звонок отличается от просто
    /// открытого Zoom ровно тем, что система при нём звучит, а без тапа этот
    /// сигнал не с чего взять.
    system_tap: Option<std::rc::Rc<std::cell::RefCell<crate::capture::SystemTap>>>,
    /// `MR_DEBUG_TIMING=1` — то же, что у `CpalAudio`, и по той же причине:
    /// живого звонка отладчиком не поймать, а расхождение старта дорожек видно
    /// только на числах.
    ///
    /// Мерить здесь есть что, хотя системная дорожка и не открывается: вопрос
    /// «сколько микрофон догоняет уже идущий тап» — это ровно то, что ручная
    /// проверка на macOS и должна увидеть. Переменная обязана работать на обеих
    /// платформах: инструкция к проверке одна, и молчащий на macOS
    /// `MR_DEBUG_TIMING` человек прочитал бы как отказ сборки, а не как «эта
    /// платформа ничего не печатает».
    timing: bool,
    /// Начало `open()` — точка отсчёта для дорожки микрофона.
    opened_at: Option<Instant>,
    logged_mic: bool,
    logged_sys: bool,
}

#[cfg(target_os = "macos")]
impl MacAudio {
    /// `system_tap` — общий на весь процесс, конструируется и передаётся
    /// вызывающим (`audio::run()`), а не здесь: это ресурс уровня процесса,
    /// а не уровня одной записи.
    pub fn new(
        mic: DeviceChoice,
        system_tap: std::rc::Rc<std::cell::RefCell<crate::capture::SystemTap>>,
    ) -> Self {
        Self::with_tap(mic, Some(system_tap))
    }

    /// Без системной дорожки: тап не поднялся, потому что человек отказал в
    /// разрешении на захват звука.
    ///
    /// Отдельный конструктор, а не `new(mic, None)`, чтобы вызов читался как
    /// решение («пишем только микрофон»), а не как забытый аргумент. Цена этого
    /// режима не в одной дорожке, а в автодетекте — он перестаёт срабатывать
    /// совсем, и сказать об этом человеку обязан тот, кто сюда попал
    /// (`audio::run`), потому что отсюда до UI не дотянуться.
    pub fn new_mic_only(mic: DeviceChoice) -> Self {
        Self::with_tap(mic, None)
    }

    fn with_tap(
        mic: DeviceChoice,
        system_tap: Option<std::rc::Rc<std::cell::RefCell<crate::capture::SystemTap>>>,
    ) -> Self {
        Self {
            mic: None,
            mic_rx: None,
            mic_choice: mic,
            fell_back: None,
            system_tap,
            timing: std::env::var_os("MR_DEBUG_TIMING").is_some(),
            opened_at: None,
            logged_mic: false,
            logged_sys: false,
        }
    }
}

#[cfg(target_os = "macos")]
impl AudioIo for MacAudio {
    /// Открывает ТОЛЬКО микрофон: системная дорожка течёт из `system_tap`
    /// независимо от этого вызова.
    ///
    /// Разделения «открыть» и «запустить» здесь достаточно в вырожденном виде:
    /// поток ровно один, выравнивать его не с чем — тап уже идёт, и его время
    /// отсчитывается от старта приложения, а не от `open()`.
    fn open(&mut self) -> Res {
        if self.mic.is_some() {
            return Ok(());
        }
        let (tx, rx) = channel();
        let t0 = Instant::now();
        let (pending, fell_back) = build_mic_capture(&self.mic_choice, tx)?;
        self.fell_back = fell_back;
        let t1 = Instant::now();
        self.mic = Some(pending.play()?);
        let t2 = Instant::now();
        if self.timing {
            eprintln!("[timing] build(mic)              = {:?}", t1 - t0);
            eprintln!("[timing] play(mic)               = {:?}", t2 - t1);
            eprintln!(
                "[timing] системная дорожка      = уже идёт (тап поднят при старте \
                 приложения, открывать нечего)"
            );
        }
        self.opened_at = Some(t0);
        self.logged_mic = false;
        self.logged_sys = false;
        self.mic_rx = Some(rx);
        Ok(())
    }

    fn close(&mut self) {
        self.mic = None;
        self.mic_rx = None;
        self.opened_at = None;
    }

    /// Про микрофон — как и требует докблок трейта («горит ли индикатор»).
    /// Системная дорожка сюда не входит: privacy-индикатора у тапа нет, и
    /// этим методом она не гейтится.
    fn is_open(&self) -> bool {
        self.mic.is_some()
    }

    fn drain(&mut self) -> (Vec<i16>, Vec<i16>) {
        let mic: Vec<i16> = self
            .mic_rx
            .as_ref()
            .map(|rx| rx.try_iter().flatten().collect())
            .unwrap_or_default();
        // Дренируется ВСЕГДА, даже когда микрофон закрыт: иначе накопленное в
        // канале тапа росло бы без предела всё время простоя, а уровень
        // системного звука (второй сигнал детекта на macOS) считался бы по
        // тому, что играло минуты назад.
        //
        // Без тапа — пустой вектор, то есть ровно то же, что «тап есть, но
        // сейчас тихо». Разница между этими случаями видна не здесь, а в
        // `has_system`: тишина пишется в файл, отсутствие дорожки — нет.
        let sys = match self.system_tap.as_ref() {
            Some(t) => t.borrow_mut().drain(),
            None => Vec::new(),
        };
        if self.timing {
            // Замер грубый: drain зовётся из цикла раз в 200 мс, так что
            // «первый чанк» округлён вверх до тика. Отсчёт — от open(), то есть
            // от взятия микрофона; для системной дорожки это ответ на вопрос
            // «сколько её уже было к моменту, когда включился микрофон», а не
            // «сколько она поднималась».
            if let Some(t0) = self.opened_at {
                if !self.logged_mic && !mic.is_empty() {
                    eprintln!("[timing] первый чанк mic      = +{:?}", t0.elapsed());
                    self.logged_mic = true;
                }
                if !self.logged_sys && !sys.is_empty() {
                    eprintln!("[timing] первый чанк system   = +{:?}", t0.elapsed());
                    self.logged_sys = true;
                }
            }
        }
        (mic, sys)
    }

    fn set_mic_device(&mut self, choice: DeviceChoice) {
        self.mic_choice = choice;
    }

    fn fell_back_from(&self) -> Option<String> {
        self.fell_back.clone()
    }

    fn has_system(&self) -> bool {
        self.system_tap.is_some()
    }
}

/// Пиковый уровень по обеим дорожкам, 0..1.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Levels {
    pub mic: f32,
    pub system: f32,
}

/// Во сколько раз уровень падает за тик без сигнала. При тике 200 мс полоска
/// опускается примерно за полсекунды — глазу видно движение, но не мерцание.
const LEVEL_DECAY: f32 = 0.7;

fn peak(samples: &[i16]) -> f32 {
    samples
        .iter()
        .map(|s| (*s as f32 / i16::MAX as f32).abs())
        .fold(0.0, f32::max)
        // Масштаб — `i16::MAX`, а не `i16::MIN.abs()`, поэтому `i16::MIN` даёт
        // 1.0000305 — за пределами документированного диапазона `Levels`
        // (0..1). Раньше зажим стоял только в UI; контракт ядра обязан
        // держаться сам, а не полагаться на то, что его подстрахует другой слой.
        .min(1.0)
}

pub struct App {
    machine: SessionMachine,
    /// Корень записей. Конкретная папка считается из `started` — см. `month_dir`.
    root: PathBuf,
    ring_mic: RingBuffer,
    ring_sys: RingBuffer,
    sink_mic: Option<Box<dyn Sink>>,
    sink_sys: Option<Box<dyn Sink>>,
    audio: Box<dyn AudioIo>,
    sinks: Box<dyn SinkFactory>,
    current_source: String,
    started: DateTime<Local>,
    /// Включена ли явная проверка микрофона. Не состояние машины: она про
    /// запись, а это про «дай послушать».
    monitor: bool,
    /// Устройство сменили, пока шла запись, — она продолжает писаться прежним.
    ///
    /// Флаг нужен потому, что иначе смена микрофона во время записи выглядит
    /// выполненной: выпадашка показывает новое устройство, а дорожка пишется
    /// старым до конца встречи. Ровно этот молчаливый разрыв между «что
    /// показано» и «что происходит» однажды стоил сорока шести минут записи
    /// не тем микрофоном.
    mic_change_deferred: bool,
    level_mic: f32,
    level_sys: f32,
}

impl App {
    /// Микрофон здесь НЕ открывается. Потоки поднимаются только по детекту,
    /// ручному старту или явной проверке — см. `Action::StartRingBuffer`.
    #[cfg(target_os = "windows")]
    pub fn new(root: PathBuf, mic: DeviceChoice) -> Self {
        Self::with_backends(root, Box::new(CpalAudio::new(mic)), Box::new(WavSinks))
    }

    /// Для платформ, где `AudioIo` собирается снаружи.
    ///
    /// На macOS это `MacAudio` с общим на весь процесс `SystemTap`: одним
    /// `DeviceChoice`, который принимает `App::new`, такой захват не
    /// описывается — тап поднимается раньше `App` и живёт дольше любой записи.
    ///
    /// `SinkFactory` при этом всегда `WavSinks`, как и в `App::new`:
    /// варьируется только `AudioIo`. Подставной `SinkFactory` остаётся делом
    /// тестов и `with_backends`.
    pub fn new_with_audio(root: PathBuf, audio: Box<dyn AudioIo>) -> Self {
        Self::with_backends(root, audio, Box::new(WavSinks))
    }

    /// Сменить микрофон. Вступает в силу со следующего открытия потоков.
    ///
    /// Если в этот момент идёт запись (машина не в `Idle`), выбор запоминается,
    /// но текущая дорожка продолжает писаться прежним устройством — менять его
    /// на ходу значило бы порвать её посередине. Факт отложенности выставляется
    /// флагом: сказать об этом обязан интерфейс, иначе смена выглядит
    /// применённой, а не отложенной.
    pub fn set_mic_device(&mut self, choice: DeviceChoice) {
        self.audio.set_mic_device(choice);
        self.mic_change_deferred = !matches!(self.machine.state(), State::Idle);
    }

    /// `true` — выбранное устройство ждёт следующей записи, текущая идёт на
    /// прежнем.
    pub fn mic_change_deferred(&self) -> bool {
        self.mic_change_deferred
    }

    /// `Some(имя)` — писали не тем микрофоном, о котором просили.
    pub fn device_warning(&self) -> Option<String> {
        self.audio.fell_back_from()
    }

    pub fn levels(&self) -> Levels {
        Levels {
            mic: self.level_mic,
            system: self.level_sys,
        }
    }

    pub fn is_monitoring(&self) -> bool {
        self.monitor
    }

    /// Явная проверка микрофона: открыть потоки, ничего не записывая.
    ///
    /// Инвариант «микрофон открыт только когда мы слушаем» сохраняется по
    /// смыслу: слушаем именно потому, что попросили. Индикатор Windows при этом
    /// честно горит.
    ///
    /// Выключение гасит потоки ТОЛЬКО в `Idle`. Если за время проверки пришёл
    /// детект, потоки уже принадлежат записи — закрыть их здесь значило бы
    /// оборвать встречу тем, что пользователь выключил проверку.
    pub fn set_monitor(&mut self, on: bool) -> Res {
        if !matches!(self.machine.state(), State::Idle) {
            self.monitor = on;
            return Ok(());
        }
        // `self.monitor` встаёт ТОЛЬКО после успешного `open()`. Отказ `open()`
        // (устройство занято или исчезло) не имеет права оставить включённым
        // флаг при закрытых потоках: иначе в том же тике в окно уйдёт
        // `{mic: 0, system: 0, monitoring: true}` — кнопка «Остановить
        // проверку», полоски на нуле, неотличимо от «микрофон не слышит»,
        // ровно то, от чего обещает защищать докблок `drain_ctl`. Ложное
        // состояние держалось бы до следующего клика или автовыключения через
        // 60 секунд. Для ветки выключения порядок не важен — `close()` не падает.
        if on {
            self.audio.open()?;
            self.monitor = true;
        } else {
            self.audio.close();
            self.level_mic = 0.0;
            self.level_sys = 0.0;
            self.monitor = false;
        }
        Ok(())
    }

    fn with_backends(root: PathBuf, audio: Box<dyn AudioIo>, sinks: Box<dyn SinkFactory>) -> Self {
        Self {
            machine: SessionMachine::new(),
            root,
            ring_mic: RingBuffer::new(RING_CAPACITY),
            ring_sys: RingBuffer::new(RING_CAPACITY),
            sink_mic: None,
            sink_sys: None,
            audio,
            sinks,
            current_source: "manual".into(),
            started: Local::now(),
            monitor: false,
            mic_change_deferred: false,
            level_mic: 0.0,
            level_sys: 0.0,
        }
    }

    pub fn state(&self) -> State {
        self.machine.state()
    }

    pub fn state_is_armed(&self) -> bool {
        matches!(self.machine.state(), State::Armed)
    }

    /// Забрать всё, что накопилось в каналах захвата.
    fn drain_channels(&mut self) -> (Vec<i16>, Vec<i16>) {
        self.audio.drain()
    }

    /// Выбросить всё, что уже лежит в каналах захвата, — «запись начинается
    /// отсюда».
    ///
    /// # Зачем
    ///
    /// `pump_audio` дренирует каналы РАНЬШЕ, чем смотрит на состояние машины, и
    /// пишет всё, что вынул, если состояние — `Recording`. На тике, где случился
    /// переход `Idle → Recording`, «всё, что вынул» — это ещё и звук, пришедший
    /// ДО нажатия: до тика проходит до 200 мс, и на macOS системный тап течёт
    /// непрерывно, поэтому эти 200 мс там всегда есть и всегда непустые.
    ///
    /// Ловится это только на живой машине: ручная проверка Task 4 дала
    /// системную дорожку на 5119 сэмплов (0.32 с) длиннее микрофонной при
    /// одновременном закрытии обеих. Часть той разницы — вот эта.
    ///
    /// Две беды сразу, и вторая хуже первой:
    ///
    /// 1. **Расхождение дорожек.** Раздельные дорожки существуют, чтобы их можно
    ///    было сопоставлять; лишние 200 мс в начале одной из них это ломают.
    /// 2. **Звук до согласия.** В файл попадает то, что звучало до того, как
    ///    человек решил записывать. Для системной дорожки это нарушение
    ///    обещания дизайн-дока; для микрофонной (когда до старта была включена
    ///    проверка микрофона, и канал уже полон) — это ещё и запись голоса,
    ///    которую никто не просил.
    ///
    /// # Почему только на ручном старте
    ///
    /// Это `Action::StartFileWrite`, то есть `Idle → Recording` — единственный
    /// путь, где запись начинается «с этого мгновения». Путь автодетекта идёт
    /// через `Armed`, где предзапись — сознательная фича: кольцо копит 30 с
    /// именно затем, чтобы в файл попало то, что было ДО ответа. Там
    /// выбрасывать нечего и незачем, и `Action::FlushRingToFile` эта правка не
    /// трогает вовсе.
    fn discard_pending_audio(&mut self) {
        let _ = self.drain_channels();
    }

    /// Событие → переход машины → применение действия.
    ///
    /// # Почему ошибка применения сбрасывает машину
    ///
    /// Машина переходит ДО того, как действие применено (иначе не узнать, какое
    /// действие применять), поэтому провалившееся действие оставляет состояние
    /// и реальный мир в разных точках. Самый дорогой случай: `FlushRingToFile`
    /// не смог открыть файлы — машина уже в `Recording(Auto)`, `sink_*` пусты,
    /// потоки открыты. Дальше `pump_audio` через `if let Some(w)` молча ничего
    /// не пишет и возвращает `Ok`, а `close_sinks` с двумя `None` возвращает
    /// `Ok` без единого файла: **отказ выглядит как успех**. В Task 6 это
    /// маскировал `?` в `main` (процесс просто падал), но в Task 7 GUI ловит
    /// ошибку и живёт дальше — получился бы живой интерфейс с горящим
    /// индикатором мика, который не пишет ничего.
    ///
    /// Выбран сброс в `Idle`, а не откат состояния назад, по двум причинам.
    /// Во-первых, `Idle` — единственная точка, где у КАЖДОГО поля есть
    /// известное значение (нет синков, нет потоков, кольцо пусто), поэтому
    /// «мир сошёлся с машиной» здесь проверяется, а не выводится рассуждением;
    /// откат же в `Armed` пришлось бы дополнять разбором того, что именно
    /// действие успело сделать до падения (открыть потоки? слить кольцо?).
    /// Во-вторых, `Idle` честнее по смыслу: запись не начата и микрофон
    /// отпущен — ровно это и произошло. Откат в `Armed` оставил бы горящий
    /// индикатор и крутящееся кольцо после того, как запись уже провалилась.
    ///
    /// Ошибка при этом не глотается — она уходит наверх, к тому, кто умеет о
    /// ней сказать (в Task 7 — GUI).
    pub fn on_event(&mut self, e: Event, source: Option<&MicSession>) -> Res {
        if let Some(s) = source {
            self.current_source = s.process_name.clone();
        }
        let action = self.machine.handle(e);
        match self.apply(action) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.reset_to_idle();
                Err(err)
            }
        }
    }

    /// Свести машину и мир в одну точку после провалившегося действия.
    ///
    /// Машина пересоздаётся, а не «переводится» в `Idle`: легального события
    /// «у меня всё сломалось» в `enum Event` нет, а `session.rs` — готовый
    /// модуль, трогать его нельзя. `SessionMachine::new()` — это и есть Idle.
    ///
    /// `close_sinks` зовётся best-effort: его собственная ошибка отбрасывается,
    /// потому что наверх уже уходит первая, настоящая причина. Но позвать его
    /// надо обязательно — если синки успели открыться и в них что-то попало,
    /// это «что-то» лучше дописать и финализировать, чем бросить недописанным.
    fn reset_to_idle(&mut self) {
        let _ = self.close_sinks();
        self.audio.close();
        self.monitor = false;
        self.mic_change_deferred = false;
        self.ring_mic.drain_to_vec();
        self.ring_sys.drain_to_vec();
        self.machine = SessionMachine::new();
    }

    fn apply(&mut self, action: Action) -> Res {
        match action {
            Action::StartRingBuffer => {
                self.started = Local::now();
                // Микрофон берётся ИМЕННО ЗДЕСЬ — на детекте, до вопроса.
                // Индикатор мика в Windows загорается в этот момент.
                self.audio.open()?;
            }
            Action::DiscardRing => {
                self.ring_mic.drain_to_vec();
                self.ring_sys.drain_to_vec();
                self.audio.close(); // отказ — отпускаем микрофон немедленно
                self.monitor = false;
                // Записи, которая шла бы на прежнем устройстве, больше нет.
                self.mic_change_deferred = false;
            }
            Action::StartFileWrite => {
                self.started = Local::now();
                self.current_source = "manual".into();
                self.audio.open()?;
                // ...и сразу выбрасываем всё, что уже лежит в каналах: это звук
                // ДО нажатия «записать». См. докблок `discard_pending_audio` —
                // без этой строки первый же `pump_audio` дописал бы его в начало
                // файла.
                //
                // Именно ЗДЕСЬ, между `open()` и `open_sinks()`, а не после:
                // так выброшено ровно предзаписное, а всё, что натечёт за время
                // создания файлов (миллисекунды), уже относится к записи и
                // сохраняется.
                self.discard_pending_audio();
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
                self.audio.close();
                self.monitor = false;
                // Следующая запись возьмёт уже новое устройство — откладывать
                // больше нечего.
                self.mic_change_deferred = false;
                // FinalizeDone обязан уйти в машину ДАЖЕ при ошибке закрытия.
                // Иначе она навсегда останется в Finalizing, откуда единственный
                // выход — это событие, и приложение молча перестанет записывать
                // что-либо вообще. В Task 7 (аудио-цикл в фоновом потоке под Tauri)
                // это будет выглядеть как живой GUI, который ничего не пишет.
                //
                // `reset_to_idle` в on_event сегодня подстраховал бы и этот
                // случай, но полагаться на страховку тут нельзя: докблок
                // `State::Finalizing` требует присылать FinalizeDone ВСЕГДА,
                // включая провал закрытия, — это контракт машины, а не
                // внутреннее дело `apply`. Уважать его дешевле здесь, чем
                // чинить снаружи пересозданием машины.
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
        let dir = month_dir(&self.root, self.started);
        let (mic, sys) = free_name_pair(&dir, self.started, &src)?;
        // Порядок важен: если вторая дорожка не открылась, первую надо закрыть,
        // иначе на диске останется осиротевший mic-файл.
        let sink_mic = self.sinks.create(&dir, &mic)?;
        // Дорожки, которую нечем наполнить, на диске быть не должно — см.
        // докблок `AudioIo::has_system`. Имя `sys` при этом всё равно выбрано
        // выше и не пропадает: `free_name_pair` считает пару занятой, если
        // занято ЛЮБОЕ из двух имён, так что следующая запись — хоть с
        // разрешением, хоть без — на это имя уже не сядет.
        if !self.audio.has_system() {
            self.sink_mic = Some(sink_mic);
            self.sink_sys = None;
            return Ok(());
        }
        match self.sinks.create(&dir, &sys) {
            Ok(sink_sys) => {
                self.sink_mic = Some(sink_mic);
                self.sink_sys = Some(sink_sys);
                Ok(())
            }
            Err(e) => {
                let _ = sink_mic.finalize();
                let _ = std::fs::remove_file(dir.join(&mic));
                Err(e)
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
                first_err.get_or_insert(e);
            }
        }
        if let Some(w) = self.sink_sys.as_mut() {
            if let Err(e) = w.write(&sys) {
                first_err.get_or_insert(e);
            }
        }
        for sink in [self.sink_mic.take(), self.sink_sys.take()]
            .into_iter()
            .flatten()
        {
            match sink.finalize() {
                Ok(p) => println!("записано: {}", p.display()),
                Err(e) => {
                    first_err.get_or_insert(e);
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
        // Уровень считается всегда, когда что-то течёт: в записи он даровой
        // (данные и так проходят здесь), в проверке — единственный смысл.
        // Затухание, а не мгновенный ноль: иначе полоска мигала бы на паузах
        // между словами.
        self.level_mic = peak(&mic).max(self.level_mic * LEVEL_DECAY);
        self.level_sys = peak(&sys).max(self.level_sys * LEVEL_DECAY);
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
            // Idle и Finalizing: сэмплы дренированы и отброшены. В проверке
            // это и требуется — послушать, ничего не сохранив.
            _ => {}
        }
        Ok(())
    }
}

/// Первая mic-сессия, которая не наша собственная.
///
/// Мы сами держим микрофон всё время, пока живы Armed и Recording (его берут на
/// детекте, до вопроса), и детектор честно возвращает нас в списке — он сообщает
/// факт «процесс держит мик», а не мнение о том, звонок это или нет. Отфильтровать
/// себя — обязанность потребителя, иначе собственный захват маскирует уход
/// настоящей сессии, и `SessionGone` не приходит НИКОГДА:
///
///  * в `Armed` — вопрос висит вечно, кольцо крутится, индикатор мика горит,
///    хотя встреча давно кончилась (сломан инвариант «сессия исчезла до ответа →
///    кольцо выброшено, микрофон отпущен»);
///  * в `Recording(Auto)` — запись не останавливается по концу звонка вообще,
///    только руками (сломано правило «Recording{Auto} — стоп по SessionGone»).
///
/// Ни детектор, ни захват поодиночке этой ошибки иметь не могут — она живёт ровно
/// в шве между ними, то есть появляется впервые там, где оба модуля впервые
/// оказались в одном процессе.
fn first_foreign_session(sessions: Vec<MicSession>, me: u32) -> Option<MicSession> {
    sessions.into_iter().find(|s| s.pid != me)
}

/// Решение по одному опросу детектора: кто теперь активная сессия и какое
/// событие из этого следует.
///
/// Живёт здесь, а не в `main.rs`, потому что потребителей ядра теперь двое —
/// консоль и Tauri-оболочка, — и фильтр своего pid обязателен обоим одинаково.
/// Оставь его в бинаре, и GUI пришлось бы написать эту логику заново; ошибись
/// он в ней — приложение детектило бы само себя, `SessionGone` не приходил бы
/// никогда, а тесты консоли остались бы зелёными.
///
/// Вынесено из цикла намеренно. Фикс само-детекта — это ДВЕ вещи: сама
/// `first_foreign_session` и то, что её результат заведён в `was_active`,
/// откуда и берутся `SessionAppeared`/`SessionGone`. Хелпер был покрыт
/// тестами, проводка — нет, а бага жила именно в проводке. Пока эта строка
/// стояла в `main`, её можно было откатить на `.next()`, и все тесты
/// остались бы зелёными.
///
/// `was_active` передаётся, а не хранится: функция чистая — тот же вход даёт
/// тот же выход, и оба перехода (появление/уход) проверяются без цикла,
/// детектора и WASAPI.
pub fn poll_to_event(
    was_active: bool,
    sessions: Vec<MicSession>,
    me: u32,
) -> (Option<MicSession>, Option<Event>) {
    let active = first_foreign_session(sessions, me);
    let event = match (active.is_some(), was_active) {
        (true, false) => Some(Event::SessionAppeared),
        (false, true) => Some(Event::SessionGone),
        // Состояние не изменилось: повторный детект той же встречи — шум,
        // а повторное «сессий по-прежнему нет» — тем более.
        _ => None,
    };
    (active, event)
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
    use crate::session::Trigger;
    use chrono::TimeZone;
    use std::cell::RefCell;
    use std::rc::Rc;

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

    fn сессия(pid: u32, name: &str) -> MicSession {
        MicSession {
            pid,
            process_name: name.into(),
        }
    }

    // ---- фильтр своего pid --------------------------------------------------

    /// Регресс на реальную багу, найденную при сборке целого: пока мы держим
    /// микрофон (Armed/Recording), детектор возвращает нас самих, и без фильтра
    /// уход настоящей сессии становится невидимым — `SessionGone` не приходит.
    #[test]
    fn собственная_сессия_не_считается_встречей() {
        let sessions = vec![сессия(42, "meeting-recorder.exe")];
        assert_eq!(first_foreign_session(sessions, 42), None);
    }

    #[test]
    fn чужая_сессия_находится_даже_если_наша_идёт_первой() {
        let sessions = vec![сессия(42, "meeting-recorder.exe"), сессия(7, "Zoom.exe")];
        assert_eq!(
            first_foreign_session(sessions, 42),
            Some(сессия(7, "Zoom.exe"))
        );
    }

    #[test]
    fn пустой_список_даёт_none() {
        assert_eq!(first_foreign_session(Vec::new(), 42), None);
    }

    #[test]
    fn единственная_чужая_сессия_возвращается_как_есть() {
        let sessions = vec![сессия(7, "Zoom.exe")];
        assert_eq!(
            first_foreign_session(sessions, 42),
            Some(сессия(7, "Zoom.exe"))
        );
    }

    // ---- проводка фикса: poll_to_event ------------------------------------
    //
    // Тесты выше проверяют хелпер, эти — то, что его результат действительно
    // заведён в решение о событии. Мутация «вернуть `.next()` вместо фильтра»
    // (в любом из двух мест — в самой `first_foreign_session` или в вызове из
    // `poll_to_event`) валит именно эту группу.

    /// Гвоздь всей баги. Мы в Armed/Recording, то есть сами держим микрофон и
    /// сами же попадаем в список детектора. Zoom вышел из звонка — остались
    /// только мы. Это обязано читаться как «сессий нет» → `SessionGone`.
    ///
    /// Без фильтра своего pid `active` был бы `Some(мы)`, `is_active` осталось
    /// бы `true` при `was_active == true`, и события не случилось бы ВООБЩЕ:
    /// в Armed вопрос висел бы вечно с горящим микрофоном, в Recording(Auto)
    /// запись не остановилась бы по концу звонка никогда.
    #[test]
    fn уход_zoom_даёт_session_gone_даже_пока_мы_держим_микрофон() {
        let (active, event) = poll_to_event(true, vec![сессия(42, "meeting-recorder.exe")], 42);
        assert_eq!(active, None, "наш собственный захват — не встреча");
        assert_eq!(
            event,
            Some(Event::SessionGone),
            "уход настоящей сессии обязан быть виден, даже когда мик держим мы"
        );
    }

    /// Обратная сторона того же шва: собственный захват не имеет права
    /// выглядеть началом встречи — иначе приложение предложило бы записать
    /// само себя, а на `y` ушло бы в самоподдерживающийся детект.
    #[test]
    fn собственный_захват_не_поднимает_session_appeared() {
        let (active, event) = poll_to_event(false, vec![сессия(42, "meeting-recorder.exe")], 42);
        assert_eq!(active, None);
        assert_eq!(event, None, "мы сами себе не встреча");
    }

    /// Чужая сессия рядом с нашей — всё ещё встреча: фильтр обязан убирать
    /// ровно нас, а не всё подряд.
    #[test]
    fn zoom_рядом_с_нашей_сессией_остаётся_активной_встречей() {
        let (active, event) = poll_to_event(
            true,
            vec![сессия(42, "meeting-recorder.exe"), сессия(7, "Zoom.exe")],
            42,
        );
        assert_eq!(active, Some(сессия(7, "Zoom.exe")));
        assert_eq!(event, None, "встреча уже шла — повторного события не нужно");
    }

    #[test]
    fn появление_zoom_из_тишины_даёт_session_appeared() {
        let (active, event) = poll_to_event(false, vec![сессия(7, "Zoom.exe")], 42);
        assert_eq!(active, Some(сессия(7, "Zoom.exe")));
        assert_eq!(event, Some(Event::SessionAppeared));
    }

    #[test]
    fn исчезновение_последней_сессии_даёт_session_gone() {
        let (active, event) = poll_to_event(true, Vec::new(), 42);
        assert_eq!(active, None);
        assert_eq!(event, Some(Event::SessionGone));
    }

    /// Тишина в Idle — самый частый опрос, событий быть не должно.
    #[test]
    fn пустой_опрос_без_активной_сессии_не_даёт_события() {
        let (active, event) = poll_to_event(false, Vec::new(), 42);
        assert_eq!(active, None);
        assert_eq!(event, None);
    }

    // ---- подставной бэкенд -------------------------------------------------
    //
    // Всё, что ниже, существует ради веток ошибок. Живой cpal и живой hound
    // по команде не падают, поэтому инварианты «финализируем обе дорожки даже
    // при ошибке записи», «FinalizeDone уходит даже при провале закрытия» и
    // «микрофон отпущен» без фейков не выполняются в тестах ни разу.

    /// Общий журнал вызовов: он же способ увидеть, что именно App сделал с
    /// дорожками и микрофоном. Rc/RefCell, а не каналы — App однопоточен,
    /// а `cpal::Stream` и так `!Send`.
    type Журнал = Rc<RefCell<Vec<String>>>;

    fn журнал() -> Журнал {
        Rc::new(RefCell::new(Vec::new()))
    }

    fn ошибка(текст: &str) -> Box<dyn std::error::Error> {
        Box::new(std::io::Error::other(текст.to_string()))
    }

    /// Что именно должно сломаться в подставных дорожках.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Поломка {
        Нет,
        /// `create` не открывает дорожку вовсе — сценарий «диск полон».
        НеОткрывается,
        /// `write` падает только у mic — так проверяется, что беда с одной
        /// дорожкой не уносит финализацию второй.
        ХвостMicНеПишется,
        /// `finalize` падает у обеих.
        НеФинализируется,
    }

    struct ФейкSink {
        дорожка: &'static str,
        журнал: Журнал,
        падать_на_write: bool,
        падать_на_finalize: bool,
    }

    impl Sink for ФейкSink {
        fn write(&mut self, samples: &[i16]) -> Res {
            if self.падать_на_write {
                self.журнал
                    .borrow_mut()
                    .push(format!("write-err:{}", self.дорожка));
                return Err(ошибка("запись сэмплов провалилась"));
            }
            self.журнал
                .borrow_mut()
                .push(format!("write:{}:{}", self.дорожка, samples.len()));
            Ok(())
        }

        fn finalize(self: Box<Self>) -> Res<PathBuf> {
            // Пишем в журнал ДО проверки поломки: нам важен сам факт попытки
            // финализации, а не её успех.
            self.журнал
                .borrow_mut()
                .push(format!("finalize:{}", self.дорожка));
            if self.падать_на_finalize {
                return Err(ошибка("финализация провалилась"));
            }
            Ok(PathBuf::from(format!("{}.wav", self.дорожка)))
        }
    }

    struct ФейкSinks {
        журнал: Журнал,
        поломка: Поломка,
    }

    impl SinkFactory for ФейкSinks {
        fn create(&self, _dir: &Path, filename: &str) -> Res<Box<dyn Sink>> {
            // Дорожку опознаём по имени — App открывает их одним и тем же
            // вызовом, и другого способа их различить у фабрики нет.
            let mic = filename.contains(".mic.");
            let дорожка = if mic { "mic" } else { "system" };
            if self.поломка == Поломка::НеОткрывается {
                self.журнал
                    .borrow_mut()
                    .push(format!("create-err:{дорожка}"));
                return Err(ошибка("не удалось открыть дорожку"));
            }
            self.журнал.borrow_mut().push(format!("create:{дорожка}"));
            Ok(Box::new(ФейкSink {
                дорожка,
                журнал: self.журнал.clone(),
                падать_на_write: self.поломка
                    == Поломка::ХвостMicНеПишется
                    && mic,
                падать_на_finalize: self.поломка
                    == Поломка::НеФинализируется,
            }))
        }
    }

    struct ФейкAudio {
        журнал: Журнал,
        открыт: bool,
        /// Что отдавать по каждому вызову drain, по порядку.
        очередь: Vec<(Vec<i16>, Vec<i16>)>,
        /// Куда фейк кладёт последний выбор устройства — тест смотрит сюда.
        выбор: Rc<RefCell<Option<DeviceChoice>>>,
        /// Что вернуть из `fell_back_from`: `Some` — притворяемся, что просили
        /// это устройство и не нашли.
        подмена: Option<String>,
        /// `false` — притворяемся `MacAudio` без тапа: человек отказал в
        /// разрешении на захват системного звука.
        есть_система: bool,
    }

    impl AudioIo for ФейкAudio {
        fn open(&mut self) -> Res {
            self.открыт = true;
            self.журнал.borrow_mut().push("audio:open".into());
            Ok(())
        }

        fn close(&mut self) {
            if self.открыт {
                self.журнал.borrow_mut().push("audio:close".into());
            }
            self.открыт = false;
        }

        fn is_open(&self) -> bool {
            self.открыт
        }

        fn drain(&mut self) -> (Vec<i16>, Vec<i16>) {
            // Закрытый захват не отдаёт ничего — как и настоящий.
            if !self.открыт || self.очередь.is_empty() {
                return (Vec::new(), Vec::new());
            }
            self.очередь.remove(0)
        }

        fn set_mic_device(&mut self, choice: DeviceChoice) {
            *self.выбор.borrow_mut() = Some(choice);
        }

        fn fell_back_from(&self) -> Option<String> {
            self.подмена.clone()
        }

        fn has_system(&self) -> bool {
            self.есть_система
        }
    }

    /// Захват, который всегда отказывает на `open()` — «устройство занято
    /// другим приложением». Используется и в тестах на Armed (провал детекта),
    /// и в тестах на `set_monitor` (провал проверки): оба инварианта ловятся
    /// одним и тем же отказом, второго фейка заводить незачем.
    struct МикЗанят;
    impl AudioIo for МикЗанят {
        fn open(&mut self) -> Res {
            Err(ошибка("микрофон занят другим приложением"))
        }
        fn close(&mut self) {}
        fn is_open(&self) -> bool {
            false
        }
        fn drain(&mut self) -> (Vec<i16>, Vec<i16>) {
            (Vec::new(), Vec::new())
        }
    }

    /// App на подставном бэкенде. `dir` — заведомо несуществующий путь: до
    /// файловой системы эти тесты не доходят, всё ловится фейками.
    fn стенд(
        журнал: &Журнал, поломка: Поломка, звук: Vec<(Vec<i16>, Vec<i16>)>
    ) -> App {
        стенд_с_системой(журнал, поломка, звук, true)
    }

    /// `есть_система: false` — захват без системной дорожки, то есть `MacAudio`
    /// после отказа в разрешении на захват звука.
    fn стенд_с_системой(
        журнал: &Журнал,
        поломка: Поломка,
        звук: Vec<(Vec<i16>, Vec<i16>)>,
        есть_система: bool,
    ) -> App {
        App::with_backends(
            PathBuf::from("."),
            Box::new(ФейкAudio {
                журнал: журнал.clone(),
                открыт: false,
                очередь: звук,
                выбор: Rc::new(RefCell::new(None)),
                подмена: None,
                есть_система,
            }),
            Box::new(ФейкSinks {
                журнал: журнал.clone(),
                поломка,
            }),
        )
    }

    /// Стенд, у которого захват можно расспросить про устройство: возвращает
    /// App и общую ссылку на то, что фейку сказали выбрать.
    fn стенд_с_устройством(
        журнал: &Журнал,
        подмена: Option<&str>,
    ) -> (App, Rc<RefCell<Option<DeviceChoice>>>) {
        let выбор = Rc::new(RefCell::new(None));
        let app = App::with_backends(
            PathBuf::from("."),
            Box::new(ФейкAudio {
                журнал: журнал.clone(),
                открыт: false,
                очередь: Vec::new(),
                выбор: выбор.clone(),
                подмена: подмена.map(str::to_string),
                есть_система: true,
            }),
            Box::new(ФейкSinks {
                журнал: журнал.clone(),
                поломка: Поломка::Нет,
            }),
        );
        (app, выбор)
    }

    fn записано(журнал: &Журнал) -> Vec<String> {
        журнал.borrow().clone()
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
        let ж = журнал();
        let app = стенд(&ж, Поломка::Нет, Vec::new());
        assert!(
            !app.audio.is_open(),
            "App::new не имеет права открывать микрофон — он берётся на детекте"
        );
        assert_eq!(app.state(), State::Idle);
        assert!(
            записано(&ж).is_empty(),
            "конструктор не должен трогать ни захват, ни диск"
        );
    }

    /// В Idle накопленное аудио обязано быть выброшено, а не осесть в кольце
    /// и не уехать на диск.
    ///
    /// Прежний вариант этого теста звал `pump_audio` на пустом `App` и
    /// проверял `state() == Idle` после функции, которая машину не трогает
    /// вовсе: при `streams: None` дренаж пуст по построению, так что тест
    /// падал бы только на панике. Здесь захват открыт и данные есть — то
    /// есть проверяется настоящее решение `pump_audio`, а не тавтология.
    #[test]
    fn pump_audio_в_idle_выбрасывает_аудио_а_не_копит_его() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::Нет, vec![(vec![1, 2, 3], vec![4, 5, 6])]);
        app.audio.open().expect("открыть подставной захват");

        app.pump_audio().expect("pump в Idle обязан быть безобиден");

        assert_eq!(app.state(), State::Idle);
        assert!(
            app.ring_mic.is_empty() && app.ring_sys.is_empty(),
            "в Idle кольцо не набирается: это состояние «мы не слушаем»"
        );
        assert!(
            записано(&ж).iter().all(|з| !з.starts_with("write:")),
            "в Idle на диск не уходит ничего, журнал: {:?}",
            записано(&ж)
        );
    }

    /// Кольцо набирается только в Armed — обратная сторона предыдущего теста,
    /// иначе «ничего не копим» проходило бы и у сломанного pump_audio.
    #[test]
    fn pump_audio_в_armed_набирает_кольцо() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::Нет, vec![(vec![1, 2, 3], vec![4, 5, 6])]);
        app.on_event(Event::SessionAppeared, None).expect("детект");
        assert_eq!(app.state(), State::Armed);

        app.pump_audio().expect("pump в Armed");

        assert_eq!(app.ring_mic.len(), 3, "кольцо обязано набираться в Armed");
        assert_eq!(app.ring_sys.len(), 3);
        assert!(
            записано(&ж).iter().all(|з| !з.starts_with("create:")),
            "до подтверждения на диск не создаётся ничего"
        );
    }

    // ---- Захват без системной дорожки (отказ в разрешении на macOS) --------

    /// Отказ в разрешении на захват системного звука не имеет права породить
    /// вторую дорожку.
    ///
    /// Без этого на диск легла бы пара файлов, из которых второй открывается,
    /// весит заголовок и играет тишину. Человек, у которого запись «наполовину
    /// пустая», читает это как поломку записи, а не как отсутствие разрешения,
    /// — то есть чинит не то. Дорожки, которой нет, не должно быть и в файлах.
    #[test]
    fn без_системного_звука_создаётся_только_дорожка_микрофона() {
        let ж = журнал();
        let mut app = стенд_с_системой(&ж, Поломка::Нет, Vec::new(), false);
        app.on_event(Event::ManualStart, None)
            .expect("ручной старт");
        assert_eq!(app.state(), State::Recording(Trigger::Manual));

        let создано: Vec<_> = записано(&ж)
            .into_iter()
            .filter(|з| з.starts_with("create:"))
            .collect();
        assert_eq!(
            создано,
            vec!["create:mic".to_string()],
            "без тапа системная дорожка не создаётся вовсе, журнал: {:?}",
            записано(&ж)
        );
    }

    /// Обратная сторона предыдущего теста. Без неё «создана одна дорожка»
    /// проходило бы и у кода, который перестал создавать вторую ВСЕГДА, — то
    /// есть тихо сломал бы запись системного звука на обеих платформах.
    #[test]
    fn с_системным_звуком_создаются_обе_дорожки() {
        let ж = журнал();
        let mut app = стенд_с_системой(&ж, Поломка::Нет, Vec::new(), true);
        app.on_event(Event::ManualStart, None)
            .expect("ручной старт");

        let создано: Vec<_> = записано(&ж)
            .into_iter()
            .filter(|з| з.starts_with("create:"))
            .collect();
        assert_eq!(
            создано,
            vec!["create:mic".to_string(), "create:system".to_string()],
            "журнал: {:?}",
            записано(&ж)
        );
    }

    /// Запись без второй дорожки обязана идти и закрываться штатно, а не
    /// падать на отсутствующем `sink_sys`.
    ///
    /// Проверяется именно `write` в дорожку микрофона: `pump_audio` пропускает
    /// обе через `if let Some(w)`, поэтому «ничего не упало» само по себе
    /// доказывало бы и то, что не пишется НИЧЕГО.
    ///
    /// Порций в очереди две, а не одна, по той же причине, что и в тестах на
    /// кольцо: первую забирает `discard_pending_audio` между `open()` и
    /// `open_sinks()`.
    #[test]
    fn без_системного_звука_запись_микрофона_идёт_и_закрывается() {
        let ж = журнал();
        let mut app = стенд_с_системой(
            &ж,
            Поломка::Нет,
            vec![(vec![9, 9], Vec::new()), (vec![1, 2, 3], Vec::new())],
            false,
        );
        app.on_event(Event::ManualStart, None)
            .expect("ручной старт");
        app.pump_audio().expect("прокачка без системной дорожки");
        app.on_event(Event::ManualStop, None).expect("стоп");

        let ж = записано(&ж);
        assert!(
            ж.contains(&"write:mic:3".to_string()),
            "микрофон обязан писаться, и именно этими сэмплами, журнал: {ж:?}"
        );
        assert!(
            ж.iter().all(|з| !з.starts_with("write:system")
                && !з.starts_with("create:system")
                && !з.starts_with("finalize:system")),
            "системной дорожки нет — ни создания, ни записи, ни финализации, \
             журнал: {ж:?}"
        );
        assert!(
            ж.contains(&"finalize:mic".to_string()),
            "дорожка микрофона обязана быть финализирована, журнал: {ж:?}"
        );
    }

    // ---- Important 2 ревью: машина и мир не расходятся при ошибке ----------

    /// Провал открытия файлов не имеет права оставить машину в `Recording`.
    ///
    /// Без сброса состояние было бы `Recording(Auto)` при `sink_* = None`:
    /// `pump_audio` через `if let Some(w)` молча вернул бы `Ok`, ничего не
    /// записав, и `close_sinks` с двумя `None` — тоже `Ok`, без единого файла.
    /// Отказ выглядел бы как успех, а в Task 7 — как живой GUI с горящим
    /// индикатором мика, который ничего не пишет.
    #[test]
    fn провал_открытия_файла_не_оставляет_машину_в_записи() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::НеОткрывается, Vec::new());
        app.on_event(Event::SessionAppeared, None).expect("детект");
        assert_eq!(app.state(), State::Armed);
        assert!(app.audio.is_open(), "на детекте микрофон берётся");

        let err = app
            .on_event(Event::UserConfirmed, None)
            .expect_err("открытие дорожек обязано упасть");
        assert!(
            err.to_string().contains("не удалось открыть дорожку"),
            "наверх обязана уйти первая, настоящая причина, а не подмена: {err}"
        );

        assert_eq!(
            app.state(),
            State::Idle,
            "машина не имеет права остаться в Recording, когда файлов нет"
        );
        assert!(
            !app.audio.is_open(),
            "запись не начата — микрофон обязан быть отпущен, индикатор погашен"
        );
        assert!(
            app.sink_mic.is_none() && app.sink_sys.is_none(),
            "синков нет — и машина обязана говорить о мире то же самое"
        );
    }

    /// Та же ошибка, но с точки зрения последствий: после провала `pump_audio`
    /// не должен изображать запись. Это и есть та «тихая» половина баги —
    /// сама по себе она возвращает Ok и потому незаметна.
    #[test]
    fn после_провала_открытия_файла_pump_audio_ничего_не_изображает() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::НеОткрывается, vec![(vec![1, 2], vec![3, 4])]);
        app.on_event(Event::SessionAppeared, None).expect("детект");
        app.on_event(Event::UserConfirmed, None)
            .expect_err("открытие дорожек обязано упасть");

        app.pump_audio().expect("pump после сброса безобиден");

        assert_eq!(app.state(), State::Idle);
        assert!(
            записано(&ж).iter().all(|з| !з.starts_with("write:")),
            "писать некуда, и делать вид, что пишем, нельзя: {:?}",
            записано(&ж)
        );
    }

    /// Провал взятия микрофона на детекте — тот же инвариант с другого конца:
    /// машина не имеет права уйти в Armed, если захват не открылся.
    #[test]
    fn провал_взятия_микрофона_не_оставляет_машину_в_armed() {
        let ж = журнал();
        let mut app = App::with_backends(
            PathBuf::from("."),
            Box::new(МикЗанят),
            Box::new(ФейкSinks {
                журнал: ж.clone(),
                поломка: Поломка::Нет,
            }),
        );

        app.on_event(Event::SessionAppeared, None)
            .expect_err("взятие микрофона обязано упасть");

        assert_eq!(
            app.state(),
            State::Idle,
            "Armed означает «мы слушаем»; если микрофон не взят, это ложь"
        );
    }

    // ---- Important 3 ревью: инварианты закрытия ---------------------------

    /// `FinalizeDone` обязан уйти в машину даже когда закрытие файла упало.
    ///
    /// Зовём `apply` напрямую, а не через `on_event`: `on_event` ловит ошибку
    /// и сбрасывает машину в Idle своей страховкой (`reset_to_idle`), поэтому
    /// сквозь него контракт самого `apply` не виден — тест был бы зелёным в
    /// обе стороны. Проверяется именно `apply`: он обязан соблюдать контракт
    /// `State::Finalizing` («FinalizeDone присылают ВСЕГДА, включая провал
    /// закрытия») своими силами, а не в расчёте на страховку снаружи.
    ///
    /// Без этого машина навсегда осталась бы в `Finalizing`: легальный выход
    /// оттуда ровно один, и приложение молча перестало бы писать что-либо.
    #[test]
    fn finalize_done_уходит_в_машину_даже_если_закрытие_упало() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::НеФинализируется, Vec::new());
        app.on_event(Event::ManualStart, None)
            .expect("ручной старт");
        assert_eq!(app.state(), State::Recording(Trigger::Manual));

        let action = app.machine.handle(Event::ManualStop);
        assert_eq!(action, Action::CloseFile);
        assert_eq!(app.state(), State::Finalizing);

        let err = app.apply(action).expect_err("финализация обязана упасть");
        assert!(err.to_string().contains("финализация провалилась"));
        assert_eq!(
            app.state(),
            State::Idle,
            "машина застряла в Finalizing: выход оттуда только по FinalizeDone, \
             и прислать его обязаны даже при ошибке закрытия"
        );
    }

    /// Обе дорожки финализируются, даже если запись хвоста в первую упала.
    ///
    /// hound пишет длину данных в заголовок только на `finalize()`. Без него
    /// на диске остаётся WAV, который существует, весит сколько надо,
    /// открывается плеером — и играет тишину. Ошибка в mic не имеет права
    /// утащить за собой system.
    #[test]
    fn обе_дорожки_финализируются_даже_если_хвост_в_первую_не_записался() {
        let ж = журнал();
        let mut app = стенд(
            &ж,
            Поломка::ХвостMicНеПишется,
            vec![(vec![1, 2, 3], vec![4, 5, 6])],
        );
        app.on_event(Event::ManualStart, None)
            .expect("ручной старт");

        app.on_event(Event::ManualStop, None)
            .expect_err("запись хвоста в mic обязана упасть");

        let ж = записано(&ж);
        assert!(
            ж.contains(&"write-err:mic".to_string()),
            "тест бессмысленен, если хвост в mic не падал: {ж:?}"
        );
        assert!(
            ж.contains(&"finalize:mic".to_string()),
            "дорожка mic обязана быть финализирована даже после ошибки записи \
             хвоста — иначе заголовок остаётся с нулевой длиной и WAV играет \
             тишину: {ж:?}"
        );
        assert!(
            ж.contains(&"finalize:system".to_string()),
            "ошибка в mic не имеет права утащить за собой финализацию system: {ж:?}"
        );
    }

    /// Хвост дописывается во вторую дорожку, даже если первая упала: беда с
    /// mic не должна стоить system её последних сэмплов.
    ///
    /// Порций в очереди две, а не одна: первую забирает `discard_pending_audio`
    /// на старте записи (звук до нажатия), хвостом становится вторая.
    /// Проверяемый инвариант от этого не изменился — изменилось только то,
    /// какая порция играет роль хвоста.
    #[test]
    fn хвост_во_вторую_дорожку_пишется_даже_если_первая_упала() {
        let ж = журнал();
        let mut app = стенд(
            &ж,
            Поломка::ХвостMicНеПишется,
            vec![(vec![9, 9], vec![9, 9]), (vec![1, 2, 3], vec![4, 5, 6])],
        );
        app.on_event(Event::ManualStart, None)
            .expect("ручной старт");
        app.on_event(Event::ManualStop, None)
            .expect_err("запись хвоста в mic обязана упасть");

        assert!(
            записано(&ж).contains(&"write:system:3".to_string()),
            "хвост system обязан быть дописан: {:?}",
            записано(&ж)
        );
    }

    /// Отказ отпускает микрофон немедленно и не оставляет на диске ничего.
    #[test]
    fn отказ_отпускает_микрофон_и_не_пишет_на_диск() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::Нет, vec![(vec![1, 2, 3], vec![4, 5, 6])]);
        app.on_event(Event::SessionAppeared, None).expect("детект");
        app.pump_audio().expect("набрать кольцо");
        assert!(app.audio.is_open(), "на детекте микрофон берётся");
        assert!(!app.ring_mic.is_empty(), "кольцо набралось");

        app.on_event(Event::UserDeclined, None).expect("отказ");

        assert_eq!(app.state(), State::Idle);
        assert!(
            !app.audio.is_open(),
            "отказ обязан отпустить микрофон немедленно — индикатор гаснет"
        );
        assert!(
            app.ring_mic.is_empty() && app.ring_sys.is_empty(),
            "кольцо обязано быть выброшено"
        );
        assert!(
            записано(&ж).iter().all(|з| !з.starts_with("create:")),
            "на диск не должно попасть ничего: {:?}",
            записано(&ж)
        );
    }

    /// Закрытие файла тоже отпускает микрофон: после стопа приложение не
    /// слушает, и индикатор обязан погаснуть.
    #[test]
    fn закрытие_файла_отпускает_микрофон() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::Нет, Vec::new());
        app.on_event(Event::ManualStart, None)
            .expect("ручной старт");
        assert!(app.audio.is_open());

        app.on_event(Event::ManualStop, None).expect("стоп");

        assert_eq!(app.state(), State::Idle);
        assert!(
            !app.audio.is_open(),
            "после закрытия файла микрофон обязан быть отпущен"
        );
        let ж = записано(&ж);
        assert!(
            ж.contains(&"finalize:mic".to_string()) && ж.contains(&"finalize:system".to_string()),
            "обе дорожки обязаны быть финализированы на нормальном пути: {ж:?}"
        );
    }

    /// Сессия исчезла до ответа — кольцо выброшено, микрофон отпущен, на диск
    /// не попало ничего. Тот же инвариант, что и у отказа, но по событию от
    /// детектора: именно этот путь ломался само-детектом (см. `main.rs`).
    #[test]
    fn уход_сессии_до_ответа_выбрасывает_кольцо_и_отпускает_микрофон() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::Нет, vec![(vec![1, 2, 3], vec![4, 5, 6])]);
        app.on_event(Event::SessionAppeared, None).expect("детект");
        app.pump_audio().expect("набрать кольцо");

        app.on_event(Event::SessionGone, None).expect("сессия ушла");

        assert_eq!(app.state(), State::Idle);
        assert!(!app.audio.is_open(), "микрофон обязан быть отпущен");
        assert!(app.ring_mic.is_empty() && app.ring_sys.is_empty());
        assert!(
            записано(&ж).iter().all(|з| !з.starts_with("create:")),
            "на диск не должно попасть ничего: {:?}",
            записано(&ж)
        );
    }

    /// Подтверждение сливает кольцо в файл — то, ради чего кольцо и заведено:
    /// в записи слышно то, что было до ответа `y`.
    #[test]
    fn подтверждение_сливает_кольцо_в_файл() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::Нет, vec![(vec![1, 2, 3], vec![4, 5, 6])]);
        app.on_event(Event::SessionAppeared, None).expect("детект");
        app.pump_audio().expect("набрать кольцо");

        app.on_event(Event::UserConfirmed, None)
            .expect("подтверждение");

        assert_eq!(app.state(), State::Recording(Trigger::Auto));
        let ж = записано(&ж);
        assert!(
            ж.contains(&"write:mic:3".to_string()) && ж.contains(&"write:system:3".to_string()),
            "кольцо обязано уйти в начало файла: {ж:?}"
        );
    }

    // ---- звук до нажатия «записать» ---------------------------------------
    //
    // Регресс на дефект, найденный ручной проверкой Task 4 на живом железе:
    // системная дорожка вышла на 5119 сэмплов (0.32 с) длиннее микрофонной при
    // одновременном закрытии обеих. `pump_audio` дренирует каналы ДО разбора
    // состояния, поэтому на тике перехода в `Recording` в файл уезжало всё, что
    // натекло с прошлого тика, — то есть звук ДО нажатия.
    //
    // Обе проверки падают на коде без `discard_pending_audio`.

    /// Первая порция — «до нажатия», вторая — уже запись. В файл обязана уйти
    /// только вторая.
    #[test]
    fn ручной_старт_не_пишет_звук_накопленный_до_нажатия() {
        let ж = журнал();
        let mut app = стенд(
            &ж,
            Поломка::Нет,
            vec![
                (vec![1, 2, 3], vec![4, 5, 6, 7]),
                (vec![8, 9], vec![10, 11]),
            ],
        );

        app.on_event(Event::ManualStart, None)
            .expect("старт записи");
        app.pump_audio().expect("прокачка");

        let ж = записано(&ж);
        assert!(
            ж.contains(&"write:system:2".to_string()),
            "запись после нажатия обязана попасть в файл: {ж:?}"
        );
        assert!(
            ж.contains(&"write:mic:2".to_string()),
            "запись после нажатия обязана попасть в файл: {ж:?}"
        );
        assert!(
            !ж.contains(&"write:system:4".to_string()),
            "звук ДО нажатия уехал в системную дорожку: {ж:?}"
        );
        assert!(
            !ж.contains(&"write:mic:3".to_string()),
            "звук ДО нажатия уехал в микрофонную дорожку: {ж:?}"
        );
    }

    /// Худший случай той же баги, и он про приватность, а не про выравнивание.
    ///
    /// Если до старта была включена проверка микрофона, то микрофон уже открыт и
    /// его канал уже полон — то есть в файл уезжал бы кусок ГОЛОСА, записанный
    /// до того, как человек нажал «записать». На macOS то же верно для системной
    /// дорожки всегда: тап течёт с запуска приложения независимо от проверки.
    #[test]
    fn старт_после_проверки_микрофона_не_пишет_голос_до_нажатия() {
        let ж = журнал();
        let mut app = стенд(
            &ж,
            Поломка::Нет,
            vec![
                (vec![100, 101, 102, 103, 104], vec![200, 201]),
                (vec![7], vec![8]),
            ],
        );

        app.set_monitor(true).expect("включить проверку");
        app.on_event(Event::ManualStart, None)
            .expect("старт записи");
        app.pump_audio().expect("прокачка");

        let ж = записано(&ж);
        assert!(
            !ж.contains(&"write:mic:5".to_string()),
            "голос, звучавший во время проверки до нажатия, уехал в файл: {ж:?}"
        );
        assert!(
            ж.contains(&"write:mic:1".to_string()) && ж.contains(&"write:system:1".to_string()),
            "после нажатия запись обязана идти как обычно: {ж:?}"
        );
    }

    /// Обратная сторона: путь автодетекта эта правка не трогает. Кольцо —
    /// сознательная предзапись, и `FlushRingToFile` обязан отдать в файл ровно
    /// то, что в нём накопилось.
    ///
    /// Дублирует `подтверждение_сливает_кольцо_в_файл` не по лени, а по адресу:
    /// тот тест сторожит саму фичу кольца, этот — то, что `discard_pending_audio`
    /// на неё не распространился. Сломай кто-нибудь границу (позови сброс из
    /// `Action::StartRingBuffer`) — упадёт именно он, и текст скажет почему.
    #[test]
    fn автодетект_сохраняет_предзапись_кольца() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::Нет, vec![(vec![1, 2, 3], vec![4, 5, 6])]);

        app.on_event(Event::SessionAppeared, None).expect("детект");
        app.pump_audio().expect("набрать кольцо");
        app.on_event(Event::UserConfirmed, None)
            .expect("подтверждение");

        let ж = записано(&ж);
        assert!(
            ж.contains(&"write:mic:3".to_string()) && ж.contains(&"write:system:3".to_string()),
            "предзапись кольца — фича, а не баг: она обязана уйти в файл целиком: {ж:?}"
        );
    }

    // ---- выбор устройства -------------------------------------------------

    #[test]
    fn выбор_устройства_доезжает_до_захвата() {
        let ж = журнал();
        let (mut app, выбор) = стенд_с_устройством(&ж, None);
        app.set_mic_device(DeviceChoice::Id("{0.0.1.00000000}.{guid}".into()));
        assert_eq!(
            *выбор.borrow(),
            Some(DeviceChoice::Id("{0.0.1.00000000}.{guid}".into())),
            "App не хранит выбор сам — он обязан уехать в AudioIo"
        );
    }

    /// В `Idle` смена устройства применяется к ближайшей записи, откладывать
    /// нечего — флаг обязан остаться снятым, иначе окно соврёт наоборот.
    #[test]
    fn смена_устройства_в_покое_ничего_не_откладывает() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::Нет, Vec::new());
        app.set_mic_device(DeviceChoice::Id("{0.0.1.00000000}.{guid}".into()));
        assert!(!app.mic_change_deferred());
    }

    /// Главный случай: под идущей записью устройство не меняется, и это должно
    /// быть видно. Молчание здесь однажды стоило 46 минут записи не тем
    /// микрофоном — выпадашка показывала новое устройство, дорожка писалась
    /// прежним.
    #[test]
    fn смена_устройства_под_записью_откладывается_и_это_видно() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::Нет, Vec::new());
        app.on_event(Event::ManualStart, None)
            .expect("старт записи");
        app.set_mic_device(DeviceChoice::Id("{0.0.1.00000000}.{guid}".into()));
        assert!(
            app.mic_change_deferred(),
            "запись идёт на прежнем устройстве — интерфейс обязан это сказать"
        );
    }

    /// Заметка живёт ровно столько, сколько идёт запись, которая новое
    /// устройство не использует: следующая возьмёт уже его.
    #[test]
    fn конец_записи_снимает_отложенность() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::Нет, Vec::new());
        app.on_event(Event::ManualStart, None)
            .expect("старт записи");
        app.set_mic_device(DeviceChoice::Id("{0.0.1.00000000}.{guid}".into()));
        assert!(app.mic_change_deferred(), "предусловие теста");
        app.on_event(Event::ManualStop, None).expect("стоп записи");
        assert!(!app.mic_change_deferred());
    }

    #[test]
    fn подмена_устройства_видна_снаружи() {
        let ж = журнал();
        let (app, _) = стенд_с_устройством(&ж, Some("Headset (Boss Bose)"));
        assert_eq!(app.device_warning().as_deref(), Some("Headset (Boss Bose)"));
    }

    #[test]
    fn без_подмены_предупреждения_нет() {
        let ж = журнал();
        let (app, _) = стенд_с_устройством(&ж, None);
        assert_eq!(app.device_warning(), None);
    }

    // ---- проверка микрофона и уровень --------------------------------------

    /// Инвариант микрофона: в Idle потоки открыты, только если явно попросили
    /// их послушать.
    #[test]
    fn проверка_открывает_и_закрывает_микрофон_в_idle() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::Нет, Vec::new());
        assert!(!app.audio.is_open(), "в Idle микрофон отпущен");
        app.set_monitor(true).expect("включение проверки");
        assert!(app.audio.is_open());
        app.set_monitor(false).expect("выключение проверки");
        assert!(!app.audio.is_open());
    }

    /// БЛОКЕР ревью: провал `open()` (устройство занято, исчезло) не имеет
    /// права оставить `is_monitoring() == true` при закрытых потоках. Без
    /// фикса в тот же тик в окно ушло бы `{mic: 0, system: 0, monitoring:
    /// true}` — кнопка «Остановить проверку», полоски на нуле, неотличимо от
    /// «микрофон не слышит», ровно то, от чего обещает защищать `drain_ctl`.
    #[test]
    fn провал_включения_проверки_не_оставляет_флаг_включённым() {
        let mut app = App::with_backends(
            PathBuf::from("."),
            Box::new(МикЗанят),
            Box::new(ФейкSinks {
                журнал: журнал(),
                поломка: Поломка::Нет,
            }),
        );
        assert!(!app.is_monitoring(), "до включения проверка выключена");

        app.set_monitor(true)
            .expect_err("открытие потоков обязано упасть");

        assert!(
            !app.is_monitoring(),
            "флаг проверки не имеет права остаться включённым, когда потоки \
             так и не открылись — иначе окно покажет «идёт проверка» с \
             полосками на нуле, неотличимо от молчащего микрофона"
        );
    }

    /// Если за время проверки пришёл детект, потоки уже принадлежат записи.
    /// Погасить их по выключению проверки значило бы оборвать встречу.
    #[test]
    fn выключение_проверки_не_гасит_идущую_запись() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::Нет, Vec::new());
        app.set_monitor(true).expect("включение проверки");
        app.on_event(Event::SessionAppeared, Some(&сессия(42, "zoom.exe")))
            .expect("детект");
        app.set_monitor(false).expect("выключение проверки");
        assert!(
            app.audio.is_open(),
            "машина в Armed — микрофон обязан остаться открытым"
        );
    }

    #[test]
    fn уровень_растёт_от_громких_сэмплов_и_затухает() {
        let ж = журнал();
        let mut app = стенд(
            &ж,
            Поломка::Нет,
            vec![(vec![i16::MAX / 2], Vec::new()), (Vec::new(), Vec::new())],
        );
        assert_eq!(app.levels().mic, 0.0, "до прокачки уровня нет");
        // Захват отдаёт очередь только открытым — иначе drain вернёт пустоту.
        app.set_monitor(true).expect("включение проверки");
        app.pump_audio().expect("прокачка");
        let первый = app.levels().mic;
        assert!(первый > 0.4, "уровень: {первый}");
        app.pump_audio().expect("прокачка на тишине");
        assert!(
            app.levels().mic < первый,
            "без сигнала уровень обязан затухать"
        );
    }

    /// Дешёвая находка ревью: `i16::MIN / i16::MAX` даёт 1.0000305, за
    /// пределами документированного диапазона `Levels` (0..1). Зажим обязан
    /// стоять в самом `peak`, а не полагаться на то, что его подстрахует UI.
    #[test]
    fn peak_зажимается_к_единице_на_i16_min() {
        assert_eq!(
            peak(&[i16::MIN]),
            1.0,
            "i16::MIN/i16::MAX без зажима даёт 1.0000305"
        );
    }

    #[test]
    fn проверка_не_пишет_ни_в_кольцо_ни_в_файл() {
        let ж = журнал();
        let mut app = стенд(&ж, Поломка::Нет, vec![(vec![100; 1000], vec![100; 1000])]);
        app.set_monitor(true).expect("включение проверки");
        app.pump_audio().expect("прокачка");
        assert_eq!(
            app.ring_mic.len(),
            0,
            "в Idle кольцо не крутится, даже когда микрофон открыт"
        );
        assert!(
            !записано(&ж).iter().any(|s| s.starts_with("create:")),
            "проверка не открывает файлов: {:?}",
            записано(&ж)
        );
    }
}
