//! Захват системного звука на Windows: WASAPI loopback + тихий render-поток,
//! который не даёт loopback-эндпоинту простаивать. Специфика этого модуля —
//! WASAPI-only, у macOS другой механизм (`capture::macos`).

use std::sync::mpsc::Sender;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample};

use super::{build_stream_for_format, CaptureError, PendingCapture, Source};

/// Открывает системный loopback, **не запуская** поток.
///
/// WASAPI включает loopback прозрачно: cpal видит `data_flow == eRender` и сам
/// добавляет `AUDCLNT_STREAMFLAGS_LOOPBACK` при инициализации входного потока.
/// Отдельного «loopback-устройства» перебирать не нужно — проверено по
/// исходникам cpal 0.18.1 (`src/host/wasapi/device.rs`).
///
/// Устройство всегда системное по умолчанию: собеседников слушают через тот же
/// выход, на который идёт звук. Родной формат читается из output-конфига — у
/// render-устройства список input-конфигов пуст по построению.
pub fn build_loopback_capture(sink: Sender<Vec<i16>>) -> Result<PendingCapture, CaptureError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(CaptureError::NoDevice(Source::SystemLoopback))?;
    let supported = device.default_output_config()?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    build_stream_for_format(&device, config, sample_format, sink)
}

/// Открывает тихий render-поток на том же устройстве, с которого снимается loopback.
///
/// # Зачем приложению играть тишину
///
/// WASAPI loopback не отдаёт пакеты, пока render-эндпоинт простаивает. Дорожка
/// `system` начинается не тогда, когда мы открыли поток, а тогда, когда в системе
/// **впервые что-то заиграло**. Замерено, а не выведено рассуждением: при полной
/// тишине `system` — пустой файл (44 байта, только заголовок), при звуке,
/// игравшем заранее, расхождение с `mic` падает до 0.03 с. Между этими краями
/// смещение гуляло от 0.03 до 5.04 с — то есть определялось не нами, а тем,
/// слушал ли кто-то музыку.
///
/// Смещение **невосстановимо из файлов**: в WAV нет меток времени, только
/// сэмплы. Если `mic` длиной 300 с, а `system` — 250 с, то отличить «собеседники
/// заговорили на 50-й секунде» от «эндпоинт проснулся на 50-й» нельзя ничем и
/// никогда — информация уничтожается в момент записи, и никакой последующий
/// мердж её не вернёт. Реальный сценарий, где это ломает всё: владелец зашёл в
/// звонок и говорит первым — пока собеседники молчат, дорожка `system` не
/// начинается вообще.
///
/// Поэтому эндпоинт не «будят один раз», а **не дают ему уснуть**: пока идёт
/// захват, мы сами держим на нём поток и льём в него нули. Loopback отдаёт
/// пакеты с первой секунды, обе дорожки начинаются вместе, и выравнивание
/// получается **по построению** — одинаковый индекс сэмпла означает один и тот
/// же момент времени. Это не поправка к смещению, а его отсутствие: поправлять
/// было бы нечего, потому что чинить в файлах уже нечего.
///
/// # Почему это не слышно и не пишется в дорожку
///
/// В поток идёт `Sample::EQUILIBRIUM`, то есть настоящая тишина, а не тихий
/// сигнал. Микшер складывает нули с тем, что играют остальные, — пользователь не
/// слышит ничего, и в loopback наш вклад тоже ровно нулевой, так что artefact'ом
/// в `system` он не станет. Прогрев детектором тишины эту задачу не решал бы: он
/// перечисляет `eCapture`, а loopback висит на `eRender` — другая коллекция
/// устройств (проверено: ожидание 6 с перед стартом не меняло ничего, `system`
/// оставался пустым).
///
/// Устройство берётся тем же вызовом, что и источник loopback в [`start_capture`]
/// (`default_output_device`): будить надо ровно тот эндпоинт, с которого снимаем,
/// иначе поток разбудит одно устройство, а loopback будет ждать другое.
pub fn start_silence() -> Result<cpal::Stream, CaptureError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(CaptureError::NoDevice(Source::SystemLoopback))?;
    let supported = device.default_output_config()?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    build_silence_for_format(&device, config, sample_format)
}

/// Заполняет буфер вывода настоящей тишиной.
///
/// `EQUILIBRIUM`, а не литеральный `0`: нулевой уровень зависит от формата. У
/// `f32`/`i16`/`i32` это действительно 0, но у **`u8` тишина — это 128**. Залив
/// в u8-поток нули, мы получили бы не тишину, а постоянное смещение на полшкалы,
/// то есть щелчок при старте и DC-составляющую всё время записи — ровно то, чего
/// эта функция обязана избежать. Вынесено отдельно от cpal, потому что это
/// единственная часть тихого потока, которую можно проверить без железа.
fn fill_silence<T: SizedSample>(data: &mut [T]) {
    data.fill(T::EQUILIBRIUM);
}

/// Диспетчер по формату устройства — та же причина, что и у
/// [`build_stream_for_format`]: `build_output_stream::<T>` паникует, если `T`
/// разошёлся с реальным форматом устройства.
fn build_silence_for_format(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    sample_format: SampleFormat,
) -> Result<cpal::Stream, CaptureError> {
    match sample_format {
        SampleFormat::F32 => build_silence::<f32>(device, config),
        SampleFormat::I16 => build_silence::<i16>(device, config),
        SampleFormat::I32 => build_silence::<i32>(device, config),
        SampleFormat::U8 => build_silence::<u8>(device, config),
        other => Err(CaptureError::UnsupportedSampleFormat(other)),
    }
}

fn build_silence<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
) -> Result<cpal::Stream, CaptureError>
where
    T: SizedSample,
{
    let stream = device.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| fill_silence(data),
        |err| eprintln!("ошибка тихого render-потока: {err}"),
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn тишина_затирает_буфер_целиком() {
        let mut buf = [0.7f32, -0.3, 0.9, 0.1];
        fill_silence(&mut buf);
        assert_eq!(buf, [0.0; 4]);
    }

    #[test]
    fn тишина_для_знаковых_форматов_это_ноль() {
        let mut i = [7i16, -9, 32_000];
        fill_silence(&mut i);
        assert_eq!(i, [0; 3]);

        let mut j = [7i32, -9, 2_000_000];
        fill_silence(&mut j);
        assert_eq!(j, [0; 3]);
    }

    #[test]
    fn тишина_для_u8_это_середина_шкалы_а_не_ноль() {
        let mut u = [0u8, 255, 3];
        fill_silence(&mut u);
        assert_eq!(
            u,
            [128; 3],
            "нули в u8-потоке — это не тишина, а постоянное смещение на полшкалы"
        );
    }

    #[test]
    fn пустой_буфер_вывода_не_паникует() {
        let mut buf: [f32; 0] = [];
        fill_silence(&mut buf);
        assert!(buf.is_empty());
    }
}
