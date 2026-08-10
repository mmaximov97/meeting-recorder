//! Захват аудио: микрофон (владелец) и системный loopback (собеседники).
//!
//! Дорожки принципиально раздельные — это даёт бесплатную разметку говорящих,
//! whisper сам по себе не диаризует.
//!
//! Модуль отвечает за приведение железа к формату, который ждёт `storage`:
//! **16 кГц, моно, i16**. Устройство почти никогда не отдаёт этот формат
//! напрямую (обычно 48 кГц / f32 / стерео), поэтому здесь живут две вещи,
//! которых не было в исходном плане: `Resampler` и диспетчеризация по
//! реальному `SampleFormat` устройства.
//!
//! # Почему нельзя просто попросить у устройства 16 кГц
//!
//! Напрашивается очевидное упрощение: не ресемплить самим, а запросить
//! `StreamConfig { sample_rate: 16_000, .. }` и отдать работу WASAPI. Для
//! **воспроизведения** так и работает, для **записи** — нет. cpal инициализирует
//! captureпоток без флага `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` и считает формат
//! пригодным только если `IsFormatSupported` вернул `S_OK`
//! (`src/host/wasapi/device.rs`):
//!
//! ```text
//! // Output streams use AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM so Initialize accepts any
//! // format regardless of what IsFormatSupported returns. Capture streams do not;
//! // only native formats will work.
//! ```
//!
//! В shared-режиме родной формат записи — это mix format устройства, то есть
//! запрос 16 кГц упрётся в `ErrorKind::UnsupportedConfig`. Значит, ресемплинг —
//! наша обязанность, а не опция.
//!
//! Ошибиться здесь особенно неприятно: если объявить в WAV-заголовке 16 кГц, а
//! писать туда сэмплы с 48 кГц, файл откроется и будет играть — просто втрое
//! быстрее. Whisper выдаст на таком правдоподобный мусор, а не ошибку, то есть
//! отказ замаскируется под успех. Поэтому `Resampler` покрыт тестом на реальную
//! частоту синуса, а не только на длину выхода.

use std::f64::consts::PI;
use std::sync::mpsc::Sender;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};

use crate::storage::SAMPLE_RATE;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::{build_loopback_capture, start_silence};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Mic,
    SystemLoopback,
}

/// Какой микрофон брать. `Default` — тот, что выбран в системе.
///
/// Идентификатор, а не имя. `DeviceTrait::id()` на Windows отдаёт
/// `IMMDevice::GetId()` — эндпоинт-идентификатор WASAPI, стабильный между
/// перезапусками и переименованиями; докблок `DeviceId` прямо предписывает
/// персистить его через `Display`/`FromStr`. Имя же меняется в настройках
/// системы и не уникально, а промах по нему выглядит как тихий откат на
/// системный дефолт — тот самый отказ, который эта фича и устраняет.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DeviceChoice {
    #[default]
    Default,
    Id(String),
}

/// Устройство записи для выпадашки: чем искать и что показать.
///
/// Имя здесь — исключительно для глаз. Матчинг по нему не идёт нигде, иначе
/// хрупкость вернулась бы через чёрный ход.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDevice {
    pub id: String,
    pub name: String,
}

/// Индекс выбранного устройства в списке идентификаторов.
///
/// Сравнение строго по равенству: эндпоинт-идентификаторы разделяют префикс
/// контейнера, и нестрогий матчинг выбрал бы соседнее устройство.
fn pick(ids: &[String], choice: &DeviceChoice) -> Option<usize> {
    match choice {
        DeviceChoice::Default => None,
        DeviceChoice::Id(want) => ids.iter().position(|id| id == want),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("устройство не найдено: {0:?}")]
    NoDevice(Source),
    #[error("ошибка cpal: {0}")]
    Cpal(#[from] cpal::Error),
    #[error("устройство отдаёт неподдерживаемый формат сэмплов: {0}")]
    UnsupportedSampleFormat(SampleFormat),
}

/// Клиппинг и масштабирование одного f32-сэмпла в i16.
///
/// cpal может отдать значения за пределами [-1, 1], поэтому clamp обязателен:
/// без него `as i16` насыщается молча и по-разному на разных платформах.
/// Масштаб — `i16::MAX`, поэтому -1.0 даёт `i16::MIN + 1`, а не `i16::MIN`.
fn f32_to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

/// Сводит интерливнутые кадры любого формата в моно f32.
///
/// Обобщено по `T`, потому что устройство не обязано отдавать f32 — реальный
/// формат берётся из `SupportedStreamConfig::sample_format()` в рантайме.
pub fn downmix_to_mono_f32<T>(data: &[T], channels: u16) -> Vec<f32>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let ch = channels.max(1) as usize;
    data.chunks_exact(ch)
        .map(|frame| {
            let sum: f32 = frame.iter().map(|&s| f32::from_sample(s)).sum();
            sum / ch as f32
        })
        .collect()
}

/// Сводит интерливнутые f32-кадры в моно i16 с клиппингом.
/// Вынесено отдельно от cpal — это единственная часть захвата, которую
/// можно проверить без железа.
pub fn downmix_to_mono_i16(data: &[f32], channels: u16) -> Vec<i16> {
    downmix_to_mono_f32(data, channels)
        .into_iter()
        .map(f32_to_i16)
        .collect()
}

/// Сколько отводов КИХ-фильтра приходится на одну полифазную ветвь.
///
/// Это цена одного выходного сэмпла: 128 умножений-сложений. При 16 кГц на
/// выходе — около 2 млн операций в секунду, то есть доли процента ядра. Дёшево
/// настолько, что ресемплинг спокойно живёт прямо в аудио-колбэке.
const TAPS_PER_PHASE: usize = 128;

/// Полифазный рациональный ресемплер L/M.
///
/// Классическая схема «интерполяция в L раз → ФНЧ → дециммация в M раз», но
/// нули после интерполяции никогда не материализуются: для каждого выходного
/// сэмпла берётся только своя фаза фильтра. Поэтому стоимость не зависит от L,
/// и 44.1 кГц (L=160, M=441) обходится ровно так же дёшево, как 48 кГц (L=1, M=3).
///
/// Наивная дециммация «брать каждый третий сэмпл» тут не годится: без ФНЧ всё,
/// что выше 8 кГц, завернётся (алиасинг) обратно в речевой диапазон. Например,
/// 18 кГц свернулись бы в 2 кГц — прямо поверх голоса. Тест
/// `частота_выше_найквиста_подавляется` сторожит именно это.
pub struct Resampler {
    /// Коэффициент интерполяции L.
    up: usize,
    /// Коэффициент дециммации M.
    down: usize,
    taps_per_phase: usize,
    /// Прототип ФНЧ длиной `taps_per_phase * up`.
    taps: Vec<f32>,
    /// Входные сэмплы: хвост истории от прошлого вызова + новые.
    buf: Vec<f32>,
    /// Индекс в `buf` входного сэмпла для следующего выхода.
    /// Может указывать за конец `buf` — это значит «ждём ещё входа».
    base: usize,
    /// Текущая фаза фильтра, 0..up.
    phase: usize,
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// Окно Блэкмана + идеальный ФНЧ (sinc), нормированные на единичное усиление
/// по постоянному току. `cutoff` — доля частоты дискретизации прототипа (0..0.5).
///
/// Считаем в f64: при L=160 прототип длиной 20480 отводов, и накопленная
/// ошибка синуса в f32 заметно портит стопбанд.
fn design_lowpass(num_taps: usize, cutoff: f64) -> Vec<f32> {
    let center = (num_taps - 1) as f64 / 2.0;
    let mut h: Vec<f64> = (0..num_taps)
        .map(|i| {
            let x = i as f64 - center;
            let sinc = if x.abs() < 1e-12 {
                2.0 * cutoff
            } else {
                (2.0 * PI * cutoff * x).sin() / (PI * x)
            };
            let n = i as f64 / (num_taps - 1) as f64;
            let window = 0.42 - 0.5 * (2.0 * PI * n).cos() + 0.08 * (4.0 * PI * n).cos();
            sinc * window
        })
        .collect();
    let sum: f64 = h.iter().sum();
    if sum.abs() > 1e-12 {
        for v in &mut h {
            *v /= sum;
        }
    }
    h.into_iter().map(|v| v as f32).collect()
}

impl Resampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        // Равные частоты (или бессмысленный ноль) — не строим фильтр вообще.
        if from_rate == 0 || to_rate == 0 || from_rate == to_rate {
            return Self {
                up: 1,
                down: 1,
                taps_per_phase: 0,
                taps: Vec::new(),
                buf: Vec::new(),
                base: 0,
                phase: 0,
            };
        }
        let g = gcd(from_rate as usize, to_rate as usize);
        let up = to_rate as usize / g;
        let down = from_rate as usize / g;
        // Срез — минимум из двух Найквистов, пересчитанный в частоту прототипа
        // (она работает на up * from_rate). Для 48→16 это 8 кГц, для 44.1→16 —
        // тоже 8 кГц: 160*44100 * (0.5/441) = 8000.
        let cutoff = 0.5 / up.max(down) as f64;
        let taps = design_lowpass(TAPS_PER_PHASE * up, cutoff);
        Self {
            up,
            down,
            taps_per_phase: TAPS_PER_PHASE,
            taps,
            // Предзаполняем историю тишиной: первые выходные сэмплы — это
            // прогрев фильтра, задержка порядка 64 сэмплов входа (~1.3 мс).
            buf: vec![0.0; TAPS_PER_PHASE - 1],
            base: TAPS_PER_PHASE - 1,
            phase: 0,
        }
    }

    fn is_passthrough(&self) -> bool {
        self.taps.is_empty()
    }

    /// Частота на выходе для заданной входной. Нужна для проверок и логов.
    pub fn output_rate(&self, from_rate: u32) -> u32 {
        (from_rate as u64 * self.up as u64 / self.down as u64) as u32
    }

    /// Пропускает моно-сэмплы через ресемплер. Длина выхода плавает от вызова
    /// к вызову — фаза сохраняется между вызовами, поэтому склейка чанков
    /// не даёт щелчков.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if self.is_passthrough() {
            return input.to_vec();
        }
        self.buf.extend_from_slice(input);

        let tpp = self.taps_per_phase;
        // Оценка сверху, чтобы не растить Vec по одному сэмплу в колбэке.
        let mut out = Vec::with_capacity(input.len() * self.up / self.down + 1);
        while self.base < self.buf.len() {
            let mut acc = 0.0f32;
            for t in 0..tpp {
                acc += self.taps[self.phase + t * self.up] * self.buf[self.base - t];
            }
            // Компенсация вставки нулей при интерполяции: в каждой фазе лежит
            // примерно 1/up от суммы прототипа.
            out.push(acc * self.up as f32);

            let next = self.phase + self.down;
            self.base += next / self.up;
            self.phase = next % self.up;
        }

        // Оставляем ровно ту историю, которая ещё понадобится: сэмплы от
        // `base - (tpp - 1)` и дальше. `base` мог перескочить конец буфера —
        // тогда часть нужного окна ещё не пришла, и это нормально.
        let keep_from = self.base.saturating_sub(tpp - 1).min(self.buf.len());
        self.buf.drain(..keep_from);
        self.base -= keep_from;
        out
    }

    /// То же, что `process`, но сразу в формат хранения.
    pub fn process_to_i16(&mut self, input: &[f32]) -> Vec<i16> {
        self.process(input).into_iter().map(f32_to_i16).collect()
    }
}

/// Открытый, но ещё не запущенный поток захвата.
///
/// # Зачем разделены «открыть» и «запустить»
///
/// Дорожка начинает идти не с `build_mic_capture`/`build_loopback_capture`, а
/// с [`PendingCapture::play`]:
/// в cpal `build_input_stream` только инициализирует WASAPI-клиент, а
/// `IAudioClient::Start()` вызывается исключительно по команде `PlayStream`
/// (проверено по исходникам cpal 0.18.1, `src/host/wasapi/stream.rs` — во всём
/// бэкенде ровно один вызов `.Start()`, и он в обработчике этой команды).
///
/// Это и есть рычаг выравнивания. Открытие потока стоит непредсказуемо дорого и
/// по-разному: на замерах этой машины `build` микрофона — стабильные ~65 мс, а
/// loopback — от 20 мс на проснувшемся эндпоинте до **795 мс** на спящем
/// Bluetooth-устройстве, которое как раз в этот момент поднимает линк. Если
/// делать «открыл-запустил, открыл-запустил», то вся эта разница попадает прямо
/// в расхождение дорожек: у кого открытие дороже, тот и стартует позже. Именно
/// так и получилось при первом замере — Δ = 0.80 с на холодном старте, при
/// 0.02 с на тёплом. Смещение при этом определялось не нами, а тем, спало ли
/// устройство.
///
/// Разделив фазы, мы платим все дорогие открытия ДО того, как пошло время хоть
/// одной дорожки, и стартуем оба клиента подряд. Остаток — это уже не стоимость
/// открытия, а только промежуток между двумя `play()`, и он не зависит ни от
/// устройства, ни от того, холодный старт или тёплый. Ровно этого и требует
/// «выравнивание по построению»: одинаковый индекс сэмпла — один момент времени.
///
/// Тип отдельный, а не «не забудь позвать play» в комментарии: `play` забирает
/// `self`, поэтому запустить поток дважды или потерять его нельзя, а забыть
/// запустить — значит получить не собранный код, а не тихо пустую дорожку.
pub struct PendingCapture(cpal::Stream);

impl PendingCapture {
    /// Запускает захват. С этого момента идёт время дорожки.
    ///
    /// Возвращённый `Stream` нужно держать живым: его `Drop` останавливает запись.
    pub fn play(self) -> Result<cpal::Stream, CaptureError> {
        self.0.play()?;
        Ok(self.0)
    }
}

/// Идентификатор устройства строкой. `None` — дескриптор не читается.
///
/// `DeviceId` персистится через `Display`, поэтому строка — это и есть его
/// каноническая форма, а не наше изобретение.
fn device_id(d: &cpal::Device) -> Option<String> {
    d.id().ok().map(|id| id.to_string())
}

/// Устройства записи — для выпадашки в UI.
///
/// Устройство, у которого не читается идентификатор ИЛИ имя, пропускается:
/// выбрать его всё равно нельзя, а показать в списке — значит предложить то,
/// что не запомнится.
pub fn list_input_devices() -> Result<Vec<InputDevice>, CaptureError> {
    let host = cpal::default_host();
    Ok(host
        .input_devices()?
        .filter_map(|d| {
            let id = device_id(&d)?;
            let name = d.description().ok()?.name().to_string();
            Some(InputDevice { id, name })
        })
        .collect())
}

/// Устройство записи плюс признак того, что взяли не то, о чём просили.
pub struct Resolved {
    pub device: cpal::Device,
    /// `Some(идентификатор)` — просили это, не нашли, взяли системный дефолт.
    pub fell_back_from: Option<String>,
}

/// Находит устройство по выбору, откатываясь на системный дефолт.
///
/// Фолбэк, а не ошибка: цена несимметрична. Испорченная дорожка чинится вторым
/// дублем или усилением, потерянная встреча не чинится ничем. Но молчать о
/// подмене нельзя — за этим и нужен `fell_back_from`.
pub fn resolve_input(choice: &DeviceChoice) -> Result<Resolved, CaptureError> {
    let host = cpal::default_host();
    if let DeviceChoice::Id(want) = choice {
        let devices: Vec<cpal::Device> = host.input_devices()?.collect();
        let ids: Vec<String> = devices
            .iter()
            .map(|d| device_id(d).unwrap_or_default())
            .collect();
        if let Some(i) = pick(&ids, choice) {
            return Ok(Resolved {
                device: devices.into_iter().nth(i).expect("индекс из pick валиден"),
                fell_back_from: None,
            });
        }
        let device = host
            .default_input_device()
            .ok_or(CaptureError::NoDevice(Source::Mic))?;
        return Ok(Resolved {
            device,
            fell_back_from: Some(want.clone()),
        });
    }
    let device = host
        .default_input_device()
        .ok_or(CaptureError::NoDevice(Source::Mic))?;
    Ok(Resolved {
        device,
        fell_back_from: None,
    })
}

/// Открывает микрофон, **не запуская** поток (см. [`PendingCapture`]).
///
/// Возвращает вместе с потоком признак подмены устройства: сообщить о ней
/// должен тот, кто умеет говорить с пользователем, а не этот модуль.
pub fn build_mic_capture(
    choice: &DeviceChoice,
    sink: Sender<Vec<i16>>,
) -> Result<(PendingCapture, Option<String>), CaptureError> {
    let Resolved {
        device,
        fell_back_from,
    } = resolve_input(choice)?;
    let supported = device.default_input_config()?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let pending = build_stream_for_format(&device, config, sample_format, sink)?;
    Ok((pending, fell_back_from))
}

/// Диспетчер по реальному формату устройства.
///
/// План предполагал, что колбэк всегда получает `&[f32]`. Это неверно:
/// `build_input_stream::<T>` паникует («host supplied incorrect sample type»),
/// если `T` разошёлся с форматом устройства, так что тип обязан выбираться
/// по `sample_format()` в рантайме.
fn build_stream_for_format(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    sample_format: SampleFormat,
    sink: Sender<Vec<i16>>,
) -> Result<PendingCapture, CaptureError> {
    match sample_format {
        SampleFormat::F32 => build_stream::<f32>(device, config, sink),
        SampleFormat::I16 => build_stream::<i16>(device, config, sink),
        SampleFormat::I32 => build_stream::<i32>(device, config, sink),
        SampleFormat::U8 => build_stream::<u8>(device, config, sink),
        other => Err(CaptureError::UnsupportedSampleFormat(other)),
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    sink: Sender<Vec<i16>>,
) -> Result<PendingCapture, CaptureError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels;
    let mut resampler = Resampler::new(config.sample_rate, SAMPLE_RATE);

    let stream = device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let mono = downmix_to_mono_f32(data, channels);
            let out = resampler.process_to_i16(&mono);
            if out.is_empty() {
                return;
            }
            // Приёмник мог отвалиться (запись остановлена) — это не ошибка.
            let _ = sink.send(out);
        },
        |err| eprintln!("ошибка потока захвата: {err}"),
        None,
    )?;
    // Намеренно НЕ play(): запускает вызывающий, оба потока подряд (см. PendingCapture).
    Ok(PendingCapture(stream))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn моно_проходит_насквозь() {
        let out = downmix_to_mono_i16(&[0.0, 1.0, -1.0], 1);
        assert_eq!(out, vec![0, i16::MAX, i16::MIN + 1]);
    }

    #[test]
    fn стерео_усредняется_в_моно() {
        // два кадра стерео: (1.0, -1.0) → 0.0, (0.5, 0.5) → 0.5
        let out = downmix_to_mono_i16(&[1.0, -1.0, 0.5, 0.5], 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], 0);
        assert!((out[1] as f32 - 0.5 * i16::MAX as f32).abs() < 2.0);
    }

    #[test]
    fn клиппинг_не_переполняется() {
        // cpal может отдать значения за пределами [-1, 1]
        let out = downmix_to_mono_i16(&[5.0, -5.0], 1);
        assert_eq!(out, vec![i16::MAX, i16::MIN + 1]);
    }

    #[test]
    fn пустой_вход_даёт_пустой_выход() {
        assert_eq!(downmix_to_mono_i16(&[], 2), Vec::<i16>::new());
    }

    // --- ресемплинг ---

    /// Синус амплитуды 1.0 длиной `secs` секунд на частоте `rate`.
    fn синус(freq: f64, rate: u32, secs: f64) -> Vec<f32> {
        let n = (rate as f64 * secs) as usize;
        (0..n)
            .map(|i| (2.0 * PI * freq * i as f64 / rate as f64).sin() as f32)
            .collect()
    }

    /// Считает переходы через ноль — грубая, но честная оценка частоты:
    /// у синуса их ровно 2 на период.
    fn переходы_через_ноль(x: &[f32]) -> usize {
        x.windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count()
    }

    fn среднеквадратичное(x: &[f32]) -> f32 {
        if x.is_empty() {
            return 0.0;
        }
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn равные_частоты_проходят_насквозь_без_фильтра() {
        let mut r = Resampler::new(16_000, 16_000);
        let вход = vec![0.1, -0.2, 0.3, -0.4];
        assert_eq!(r.process(&вход), вход);
    }

    #[test]
    fn сорок_восемь_к_даёт_втрое_меньше_сэмплов() {
        let mut r = Resampler::new(48_000, 16_000);
        let out = r.process(&синус(440.0, 48_000, 1.0));
        // Ровно 16000 ± прогрев фильтра.
        assert!(
            (out.len() as i64 - 16_000).abs() < 100,
            "ожидали ~16000 сэмплов, получили {}",
            out.len()
        );
    }

    #[test]
    fn сорок_четыре_и_одна_к_даёт_верное_число_сэмплов() {
        let mut r = Resampler::new(44_100, 16_000);
        let out = r.process(&синус(440.0, 44_100, 1.0));
        assert!(
            (out.len() as i64 - 16_000).abs() < 100,
            "ожидали ~16000 сэмплов, получили {}",
            out.len()
        );
    }

    /// Главный тест против «ускоренной плёнки»: если ресемплинга нет или он
    /// врёт, 440 Гц поедут в 1320 Гц, и число переходов через ноль утроится.
    /// Длину выхода такая ошибка не меняет — поэтому проверяем именно частоту.
    #[test]
    fn синус_440_гц_остаётся_440_гц_после_ресемплинга() {
        for from in [48_000u32, 44_100] {
            let mut r = Resampler::new(from, 16_000);
            let out = r.process(&синус(440.0, from, 1.0));
            // Отбрасываем прогрев фильтра с обоих концов.
            let полезное = &out[200..out.len() - 200];
            let ожидаем = 2.0 * 440.0 * (полезное.len() as f64 / 16_000.0);
            let факт = переходы_через_ноль(полезное) as f64;
            assert!(
                (факт - ожидаем).abs() < 8.0,
                "{from} Гц → 16 кГц: ожидали ~{ожидаем:.0} переходов через ноль, \
                 получили {факт} (частота уехала в {:.0} Гц)",
                факт / 2.0 / (полезное.len() as f64 / 16_000.0)
            );
        }
    }

    /// Всё выше 8 кГц обязано умереть в фильтре, а не завернуться в речь.
    /// 18 кГц при наивной дециммации 48→16 свернулись бы в 2 кГц с полной
    /// амплитудой — прямо поверх голоса.
    #[test]
    fn частота_выше_найквиста_подавляется() {
        let mut r = Resampler::new(48_000, 16_000);
        let out = r.process(&синус(18_000.0, 48_000, 0.5));
        let полезное = &out[200..out.len() - 200];
        let rms = среднеквадратичное(полезное);
        assert!(
            rms < 0.02,
            "18 кГц должны быть подавлены, но RMS выхода = {rms:.4} \
             (похоже на алиасинг: фильтр не работает)"
        );
    }

    /// Речь на 1 кГц, наоборот, обязана пройти почти без потерь —
    /// иначе фильтр «подавляет всё», и предыдущий тест ничего не доказывает.
    #[test]
    fn речевая_частота_проходит_без_потерь() {
        let mut r = Resampler::new(48_000, 16_000);
        let out = r.process(&синус(1_000.0, 48_000, 0.5));
        let полезное = &out[200..out.len() - 200];
        let rms = среднеквадратичное(полезное);
        // У синуса амплитуды 1.0 RMS = 0.707.
        assert!(
            (rms - 0.707).abs() < 0.03,
            "1 кГц должен пройти без потерь, RMS = {rms:.4}"
        );
    }

    #[test]
    fn постоянный_сигнал_сохраняет_амплитуду() {
        let mut r = Resampler::new(48_000, 16_000);
        let out = r.process(&vec![0.5f32; 48_000]);
        let полезное = &out[200..out.len() - 200];
        for &v in полезное {
            assert!(
                (v - 0.5).abs() < 1e-3,
                "постоянный сигнал 0.5 не должен меняться, получили {v}"
            );
        }
    }

    /// Поток приходит чанками произвольной длины (WASAPI не обещает
    /// постоянный размер колбэка). Разбивка на чанки не должна ни терять
    /// сэмплы, ни менять результат по сравнению с одним куском.
    #[test]
    fn нарезка_на_чанки_даёт_тот_же_результат_что_и_целый_кусок() {
        let вход = синус(440.0, 48_000, 0.5);

        let mut целиком = Resampler::new(48_000, 16_000);
        let ожидаем = целиком.process(&вход);

        let mut по_чанкам = Resampler::new(48_000, 16_000);
        let mut факт = Vec::new();
        // Нарочно неровные чанки, в том числе пустой: k растёт всегда,
        // поэтому нулевой размер не подвешивает цикл.
        let размеры = [0usize, 1, 480, 17, 4096, 333];
        let mut i = 0;
        let mut k = 0;
        while i < вход.len() {
            let n = размеры[k % размеры.len()].min(вход.len() - i);
            факт.extend(по_чанкам.process(&вход[i..i + n]));
            i += n;
            k += 1;
        }
        assert_eq!(факт.len(), ожидаем.len(), "нарезка потеряла сэмплы");
        for (a, b) in факт.iter().zip(ожидаем.iter()) {
            assert!((a - b).abs() < 1e-6, "нарезка изменила сигнал: {a} vs {b}");
        }
    }

    #[test]
    fn ресемплер_знает_свою_выходную_частоту() {
        assert_eq!(Resampler::new(48_000, 16_000).output_rate(48_000), 16_000);
        assert_eq!(Resampler::new(44_100, 16_000).output_rate(44_100), 16_000);
        assert_eq!(Resampler::new(16_000, 16_000).output_rate(16_000), 16_000);
    }

    /// Ловит регресс компенсации усиления (`acc * self.up` в `process`).
    ///
    /// При полифазной интерполяции с коэффициентом L каждый выходной сэмпл
    /// собирается лишь из 1/L отводов прототипа, поэтому без домножения на L
    /// сигнал тише в L раз. Для 48 кГц L=1 — компенсация no-op, мутация там
    /// не проявляется вовсе. Существующие тесты гоняют только 48 кГц
    /// (`постоянный_сигнал_сохраняет_амплитуду`,
    /// `речевая_частота_проходит_без_потерь`) либо, как
    /// `синус_440_гц_остаётся_440_гц_после_ресемплинга`, меряют переходы через
    /// ноль — к амплитуде нечувствительны по построению. Поэтому здесь именно
    /// 44.1 кГц (L=160) и именно амплитуда.
    ///
    /// Постоянный сигнал — самая жёсткая проверка усиления: `design_lowpass`
    /// аналитически нормирует сумму отводов прототипа к 1, поэтому DC-усиление
    /// обязано быть очень близко к единице независимо от L, и допуск может
    /// быть таким же тесным, как в исходном тесте на 48 кГц (1e-3). Без
    /// `* self.up` на 44.1 кГц выход просел бы в 160 раз (0.5 → ~0.003) —
    /// допуск 1e-3 такую просадку гарантированно ловит, не будучи придирчивым
    /// к обычному шуму квантования float32.
    #[test]
    fn постоянный_сигнал_сохраняет_амплитуду_на_обеих_входных_частотах() {
        for from in [48_000u32, 44_100] {
            let mut r = Resampler::new(from, 16_000);
            let out = r.process(&vec![0.5f32; from as usize]);
            let полезное = &out[200..out.len() - 200];
            for &v in полезное {
                assert!(
                    (v - 0.5).abs() < 1e-3,
                    "{from} Гц → 16 кГц: постоянный сигнал 0.5 не должен меняться, получили {v}"
                );
            }
        }
    }

    /// Дублирует предыдущую проверку на переменном сигнале (речевая частота),
    /// чтобы просадка усиления была поймана и через RMS, а не только через DC.
    /// Допуск 0.03 — тот же, что уже используется в
    /// `речевая_частота_проходит_без_потерь` для 48 кГц: там он откалиброван
    /// под естественную просадку АЧХ фильтра на краю полосы, а не под баг
    /// усиления, поэтому расширять его для 44.1 кГц не требуется — просадка
    /// в 160 раз (RMS ~0.0044 вместо ~0.707) выбивает его на два порядка.
    #[test]
    fn речевая_частота_проходит_без_потерь_на_обеих_входных_частотах() {
        for from in [48_000u32, 44_100] {
            let mut r = Resampler::new(from, 16_000);
            let out = r.process(&синус(1_000.0, from, 0.5));
            let полезное = &out[200..out.len() - 200];
            let rms = среднеквадратичное(полезное);
            assert!(
                (rms - 0.707).abs() < 0.03,
                "{from} Гц → 16 кГц: 1 кГц должен пройти без потерь, RMS = {rms:.4}"
            );
        }
    }

    // --- выбор устройства ---

    fn идентификаторы() -> Vec<String> {
        vec![
            "{0.0.1.00000000}.{a1b2c3d4-0000-0000-0000-000000000001}".to_string(),
            "{0.0.1.00000000}.{a1b2c3d4-0000-0000-0000-000000000002}".to_string(),
            "{0.0.1.00000000}.{a1b2c3d4-0000-0000-0000-000000000003}".to_string(),
        ]
    }

    #[test]
    fn дефолт_не_выбирает_никого_из_списка() {
        assert_eq!(
            pick(&идентификаторы(), &DeviceChoice::Default),
            None,
            "Default означает «спросить систему», а не «взять первое из списка»"
        );
    }

    #[test]
    fn устройство_ищется_точным_совпадением_идентификатора() {
        assert_eq!(
            pick(&идентификаторы(), &DeviceChoice::Id(идентификаторы()[1].clone())),
            Some(1)
        );
    }

    /// Префикс — не совпадение. Эндпоинт-идентификаторы WASAPI различаются
    /// хвостом GUID, и нестрогий матчинг (`starts_with`/`contains`) выбрал бы
    /// первое попавшееся устройство того же контейнера — молча и не то.
    #[test]
    fn префикс_идентификатора_не_считается_совпадением() {
        assert_eq!(
            pick(&идентификаторы(), &DeviceChoice::Id("{0.0.1.00000000}".into())),
            None
        );
    }

    #[test]
    fn отсутствующее_устройство_даёт_none() {
        assert_eq!(
            pick(&идентификаторы(), &DeviceChoice::Id("{0.0.1.00000000}.{нет-такого}".into())),
            None
        );
    }

    /// Пустая строка приходит из `resolve_input`, когда у устройства не
    /// прочитался дескриптор. Она обязана не совпасть ни с чем, а не выбрать
    /// случайного соседа.
    #[test]
    fn пустой_идентификатор_ни_с_чем_не_совпадает() {
        assert_eq!(pick(&идентификаторы(), &DeviceChoice::Id(String::new())), None);
    }

    #[test]
    fn пустой_список_не_паникует() {
        assert_eq!(pick(&[], &DeviceChoice::Id("что-нибудь".into())), None);
        assert_eq!(pick(&[], &DeviceChoice::Default), None);
    }
}
