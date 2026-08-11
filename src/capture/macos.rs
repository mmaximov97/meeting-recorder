//! Захват системного звука на macOS: Core Audio Process Tap, агрегированное
//! устройство поверх него и разбор входного `AudioBufferList`.
//!
//! Файл делится надвое. Сверху — ЧИСТАЯ часть тракта ([`locate_tap`],
//! [`interleave_streams`], [`accumulate_stream`]): её можно проверить без железа
//! и без разрешения TCC, и она покрыта тестами. Снизу — [`SystemTap`]: вызовы
//! Core Audio, IOProc и его колбэк, то есть ровно то, что тестом не берётся.
//! Граница проведена так же и по той же причине, по которой отдельно живёт
//! `downmix_to_mono_f32`.
//!
//! # Третья ловушка рецепта
//!
//! Дизайн-док описывает две ловушки процесс-тапа (тап нельзя делать главным
//! саб-девайсом; `isExclusive` нельзя трогать руками). Прогон спайка
//! `examples/mac_tap_spike.rs` на реальной машине вскрыл третью, нигде не
//! описанную: **входной ABL агрегата содержит не только тап.**
//!
//! Главный саб-девайс агрегата — реальное устройство вывода, и если у него есть
//! СВОИ входные потоки, они приезжают в тот же `AudioBufferList` рядом с тапом.
//! На AirPods это выглядит так:
//!
//! ```text
//! [0] mNumberChannels=1, 2048 Б  <- микрофон AirPods
//! [1] mNumberChannels=2, 4096 Б  <- собственно тап (48 кГц, f32, переплетённое стерео)
//! ```
//!
//! Код, считающий ABL целиком тапом, подмешивает микрофон в «системный звук».
//! На встроенных динамиках (входных потоков нет) он при этом работает — то есть
//! ошибка невоспроизводима на половине машин и молча портит запись на другой.
//!
//! # Почему смещение вычисляется, а не берётся константой
//!
//! Сопоставления «саб-тап → индекс буфера» в Core Audio **нет**, и это следует
//! из заголовка. `AudioHardware.h`, секция AudioSubTap Properties:
//!
//! > The AudioSubTap class is a subclass of AudioObject class... However,
//! > AudioSubTap objects do not implement an IO path of their own and as such do
//! > not implement any AudioDevice properties associated with the IO path.
//! > **They also don't have any streams.**
//!
//! То есть спросить у саб-тапа его поток или канал нельзя в принципе, и
//! `kAudioAggregateDevicePropertySubTapList` смещения не даёт.
//!
//! Единственная гарантия порядка, которая в заголовке есть, касается только
//! саб-девайсов (`kAudioAggregateDevicePropertyFullSubDeviceList`):
//!
//! > The order of the items in the array is significant and is used to determine
//! > the order of the streams of the AudioAggregateDevice.
//!
//! Где относительно них встают ТАПЫ — заголовок не говорит. Поэтому порядок
//! «сначала вход саб-девайса, потом тап» здесь считается наблюдением, а не
//! законом: [`locate_tap`] проверяет обе гипотезы и отказывается угадывать,
//! если раскладка допускает оба прочтения.

/// Где во входном `AudioBufferList` лежат буферы тапа: `[start, start + len)`.
///
/// Смещение выводится из двух НЕЗАВИСИМО запрошенных величин:
/// - сколько буферов и каналов вносит своим входом само устройство вывода
///   (`kAudioDevicePropertyStreamConfiguration` в области
///   `kAudioObjectPropertyScopeInput`);
/// - сколько каналов у тапа (`mChannelsPerFrame` из `kAudioTapPropertyFormat`).
///
/// Проверяются обе величины сразу, а не одна сумма каналов: раскладка
/// `[1кан][1кан][2кан]` при входе «2 канала» по одним каналам читается двояко
/// (`[1,1 | 2]` и наоборот), а с числом буферов однозначна. Этот случай поймал
/// тест `usb_вход_двумя_буферами_различим_по_числу_буферов`, а не рассуждение.
///
/// Любая раскладка, которую нельзя разрешить однозначно, возвращает `Err` с
/// конкретной причиной. Молча угадывать нельзя: цена ошибки — запись чужого
/// микрофона вместо системного звука, причём звучащая правдоподобно.
pub fn locate_tap(
    channels: &[u32],
    device_in_buffers: usize,
    device_in_channels: u32,
    tap_channels: u32,
) -> Result<(usize, usize), &'static str> {
    let total: u32 = channels.iter().sum();
    if total != device_in_channels + tap_channels {
        return Err("сумма каналов ABL != каналы входа саб-девайса + каналы тапа");
    }
    if device_in_buffers > channels.len() {
        return Err("во входном ABL буферов меньше, чем вносит вход саб-девайса");
    }
    let tap_buffers = channels.len() - device_in_buffers;
    let sum = |part: &[u32]| -> u32 { part.iter().sum() };

    let forward = sum(&channels[..device_in_buffers]) == device_in_channels
        && sum(&channels[device_in_buffers..]) == tap_channels;
    let reverse = sum(&channels[..tap_buffers]) == tap_channels
        && sum(&channels[tap_buffers..]) == device_in_channels;

    // Вход саб-девайса пуст (встроенные динамики): тап — весь ABL, и оба
    // прочтения вырождаются в одно. Это тривиальный случай, а не двусмысленность.
    if device_in_buffers == 0 {
        return Ok((0, channels.len()));
    }
    match (forward, reverse) {
        (true, true) => Err("раскладка симметрична: вход саб-девайса и тап неразличимы"),
        (true, false) => Ok((device_in_buffers, tap_buffers)),
        (false, true) => Err("порядок обратный ожидаемому: тап идёт ПЕРЕД входом саб-девайса"),
        (false, false) => Err("раскладка ABL не сходится ни с прямым, ни с обратным порядком"),
    }
}

/// Сшивает несколько потоков в один переплетённый кадр на `total_channels`.
///
/// Каждый элемент `streams` — это `(число каналов потока, его сэмплы)`, уже
/// переплетённые ВНУТРИ потока. Нужно потому, что `mNumberBuffers > 1` **не**
/// означает «непереплетённый»: это несколько ПОТОКОВ, и каждый из них сам может
/// нести несколько переплетённых каналов.
///
/// Ровно на этом различии сломался спайк в первой версии: он читал `frames`
/// подряд идущих f32 из каждого буфера как сэмплы ОДНОГО канала, то есть глотал
/// L,R,L,R стерео-тапа как последовательное моно. Временная база схлопывалась
/// вдвое — 440 Гц уезжали на ~220, 1318.5 Гц на ~659, — что и показала
/// спектральная проверка записи. Различитель — `mNumberChannels` буфера, а не
/// число буферов.
///
/// Длина берётся МИНИМАЛЬНАЯ по потокам: буферы в одном ABL не обязаны быть
/// одной длины, и длина, снятая с одного, стала бы перечитом за конец другого.
pub fn interleave_streams(streams: &[(u32, &[f32])], total_channels: u32) -> Vec<f32> {
    let total = total_channels.max(1) as usize;
    let frames = streams
        .iter()
        .map(|(ch, data)| data.len() / (*ch).max(1) as usize)
        .min()
        .unwrap_or(0);
    if frames == 0 {
        return Vec::new();
    }
    let mut out = vec![0.0f32; frames * total];
    let mut base = 0usize;
    for (ch, data) in streams {
        let ch = (*ch).max(1) as usize;
        // Потоков суммарно шире, чем заявлено каналов, быть не должно; если
        // всё же так — обрываемся, а не пишем за край.
        if base + ch > total {
            break;
        }
        for f in 0..frames {
            for c in 0..ch {
                out[f * total + base + c] = data[f * ch + c];
            }
        }
        base += ch;
    }
    out
}

/// Сколько ПОЛНЫХ кадров несёт поток из `len` сэмплов при `channels` каналах.
///
/// Отдельной функцией, потому что делить приходится в трёх местах, и ошибка
/// «делить на ноль каналов» стоила бы паники в аудио-колбэке.
pub fn frames_in(len: usize, channels: u32) -> usize {
    len / channels.max(1) as usize
}

/// Прибавляет к моно-аккумулятору сумму каналов одного потока.
///
/// Число кадров задаёт `acc`: сколько в нём элементов, столько кадров и
/// обрабатывается (но не больше, чем реально лежит в `data`). Так несколько
/// потоков разной длины складываются по общему минимуму, не вылезая за конец
/// ни одного из них.
///
/// Пара `accumulate_stream` + [`scale_mono`] заменяет связку
/// [`interleave_streams`] + `downmix_to_mono_f32` там, где важна цена:
/// в IOProc-колбэке. Результат тот же (тест
/// `сумма_потоков_совпадает_с_переплетением_и_даунмиксом`), но промежуточный
/// переплетённый кадр не материализуется вовсе — а значит, на пакет не
/// приходится ни одной аллокации: `acc` переиспользуется между вызовами.
/// `interleave_streams` при этом остаётся: она описывает раскладку явно и
/// служит эталоном для этой оптимизации.
pub fn accumulate_stream(acc: &mut [f32], data: &[f32], channels: u32) {
    let ch = channels.max(1) as usize;
    let frames = acc.len().min(frames_in(data.len(), channels));
    for (f, slot) in acc.iter_mut().enumerate().take(frames) {
        let base = f * ch;
        let mut sum = 0.0f32;
        for c in 0..ch {
            sum += data[base + c];
        }
        *slot += sum;
    }
}

/// Превращает сумму каналов в среднее — вторая половина даунмикса.
///
/// Делится на ОБЪЯВЛЕННОЕ число каналов кадра, а не на число сложенных потоков:
/// иначе тап, у которого часть каналов приехала отдельным потоком, звучал бы
/// громче тапа, отдавшего то же самое одним переплетённым потоком.
pub fn scale_mono(acc: &mut [f32], total_channels: u32) {
    let k = 1.0 / total_channels.max(1) as f32;
    for v in acc {
        *v *= k;
    }
}

// ===========================================================================
// Сам тап: вызовы Core Audio, IOProc и его колбэк.
// ===========================================================================

use crate::capture::{CaptureError, Resampler};
use crate::storage::SAMPLE_RATE;
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::AllocAnyThread;
use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
    kAudioAggregateDeviceMainSubDeviceKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceSubDeviceListKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyStreamConfiguration, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput, kAudioObjectSystemObject, kAudioSubDeviceUIDKey,
    kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey, kAudioTapPropertyFormat,
    AudioDeviceCreateIOProcIDWithBlock, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
    AudioHardwareDestroyProcessTap, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectID, AudioObjectPropertyAddress, CATapDescription,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatLinearPCM, AudioBuffer, AudioBufferList,
    AudioStreamBasicDescription, AudioTimeStamp,
};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSUUID};
use std::ffi::CStr;
use std::ptr::{null, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Размер одного сэмпла тапа в байтах. Константа, а не поле: формат проверяется
/// на 32-битный float в [`SystemTap::start`], и всё, что не он, до колбэка не
/// доходит вовсе.
const SAMPLE_BYTES: usize = std::mem::size_of::<f32>();

/// Не чаще одного сообщения об отказах колбэка за этот интервал.
///
/// IOProc зовётся сотни раз в секунду; сломанная раскладка ABL без ограничения
/// залила бы stderr целиком и утопила бы в себе всё остальное.
const DIAG_INTERVAL: Duration = Duration::from_secs(5);

/// Тип блока IOProc — ровно `AudioDeviceIOBlock`, только во владеющей форме.
type IoProcBlock = RcBlock<
    dyn Fn(
        NonNull<AudioTimeStamp>,
        NonNull<AudioBufferList>,
        NonNull<AudioTimeStamp>,
        NonNull<AudioBufferList>,
        NonNull<AudioTimeStamp>,
    ),
>;

/// Счётчики отказов аудио-колбэка.
///
/// Живут отдельно от состояния колбэка намеренно: читает их [`SystemTap::drain`]
/// с другого потока, и лезть за ними под тот же мьютекс значило бы раз в тик
/// блокировать аудио-поток ради диагностики.
#[derive(Default)]
struct Diag {
    /// Пакеты, в которых тап не удалось опознать во входном ABL.
    skipped: AtomicU64,
    /// Пакеты, в которых буфер тапа приехал с `mData == NULL`. По
    /// `AudioHardware.h` так выглядит НЕиспользуемый поток: «тап есть, данных
    /// нет». Отдельный счётчик, потому что причина у этого своя.
    null_data: AtomicU64,
    /// Последняя причина отказа `locate_tap`.
    ///
    /// `try_lock`, а не `lock`: колбэк не имеет права ждать здесь никого. Не
    /// успел взять — счётчик всё равно вырос, а причина приедет со следующим
    /// отказом; их сотни в секунду.
    reason: Mutex<Option<&'static str>>,
}

impl Diag {
    fn note_locate_failure(&self, why: &'static str) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut slot) = self.reason.try_lock() {
            *slot = Some(why);
        }
    }

    fn note_null_data(&self) {
        self.null_data.fetch_add(1, Ordering::Relaxed);
    }
}

/// Состояние, которое живёт между вызовами колбэка.
///
/// Ресемплер именно ОДИН на весь поток, а не новый на пакет: он stateful —
/// хранит фазу и историю фильтра между вызовами, поэтому пересоздание давало бы
/// щелчок и прогрев на каждом шве.
///
/// `ch_counts` и `mono` — переиспользуемые буферы. Аллокация в аудио-колбэке
/// это поход в malloc из потока реального времени, то есть блокировка на чужом
/// замке; после прогрева ни один из этих двух не растёт.
///
/// Начальные ёмкости — под замеренный случай (2 буфера, 512–2048 кадров) с
/// запасом. Промахнуться не страшно: `Vec` вырастет один раз в первых пакетах
/// и дальше будет молчать, — но и занижать их незачем, память тут копеечная.
struct CallbackState {
    resampler: Resampler,
    ch_counts: Vec<u32>,
    mono: Vec<f32>,
}

/// Объекты Core Audio, которые мы создали и обязаны уничтожить.
///
/// Отдельным типом — ради двух вещей сразу. Во-первых, откат на полпути:
/// если агрегат не создался, тап обязан быть уничтожен, а не остаться висеть
/// до конца процесса, и любой `?` в [`SystemTap::start`] делает это сам через
/// Drop. Во-вторых, разбор в конце жизни: [`SystemTap`] живёт весь процесс, и
/// порядок его сноса — не деталь, а требование (шаг 7 рецепта).
struct Owned {
    /// 0 — не создан (`kAudioObjectUnknown`).
    tap: AudioObjectID,
    agg: AudioObjectID,
    io_proc: AudioDeviceIOProcID,
    running: bool,
}

impl Drop for Owned {
    /// Порядок обратный созданию и обязателен: остановить IO, снять IOProc,
    /// снести агрегат, снести тап. Уничтожить агрегат под живым IOProc — значит
    /// оставить HAL с висящим блоком.
    fn drop(&mut self) {
        if self.running {
            log_status(
                unsafe { AudioDeviceStop(self.agg, self.io_proc) },
                "AudioDeviceStop",
            );
        }
        if self.io_proc.is_some() {
            log_status(
                unsafe { AudioDeviceDestroyIOProcID(self.agg, self.io_proc) },
                "AudioDeviceDestroyIOProcID",
            );
        }
        if self.agg != 0 {
            log_status(
                unsafe { AudioHardwareDestroyAggregateDevice(self.agg) },
                "AudioHardwareDestroyAggregateDevice",
            );
        }
        if self.tap != 0 {
            log_status(
                unsafe { AudioHardwareDestroyProcessTap(self.tap) },
                "AudioHardwareDestroyProcessTap",
            );
        }
    }
}

/// Системная дорожка macOS: процесс-тап + приватный агрегат + IOProc на нём.
///
/// # Почему это ресурс уровня ПРОЦЕССА, а не записи
///
/// В отличие от Windows-дорожки, которая открывается на `Action::StartRingBuffer`
/// и гаснет вместе с записью, тап поднимается один раз на старте приложения и
/// живёт до выхода. Причина в детекте: на macOS «известный процесс» сам по себе
/// не значит «идёт звонок» (Zoom и Teams держатся открытыми и без него), и
/// вторым сигналом служит уровень системного звука. Считать его не из чего, пока
/// тап не открыт, — а открывать тап по детекту значит требовать детекта до
/// открытия тапа. Отсюда и время жизни.
///
/// Privacy-индикатора у тапа нет (он не микрофон), поэтому «висит всё время» не
/// означает «горит лампочка»: разрешение спрашивается один раз, при первом
/// создании тапа.
pub struct SystemTap {
    /// Объявлено ПЕРВЫМ: поля дропаются в порядке объявления, и разбор обязан
    /// пройти раньше, чем умрут блок и очередь, на которые ссылается HAL.
    ///
    /// С подчёркиванием, как `_block` и `_queue`, и по той же причине: поле
    /// существует ради Drop, читать его никто не должен.
    _owned: Owned,
    rx: Receiver<Vec<i16>>,
    diag: Arc<Diag>,
    /// Блок и очередь HAL ретейнит сам (`AudioHardware.h`: блок копируется, а
    /// очередь удерживается до `AudioDeviceDestroyIOProcID`), но держим и мы:
    /// владение здесь стоит два указателя и снимает вопрос «а точно ли ретейнит»
    /// с того места, где ответ на него — use-after-free в аудио-потоке.
    _block: IoProcBlock,
    _queue: DispatchRetained<DispatchQueue>,
    /// Сколько отказов уже показано человеку — чтобы не повторять одно и то же.
    reported_skipped: u64,
    reported_null: u64,
    last_report: Option<Instant>,
}

impl SystemTap {
    /// Поднимает тап, агрегат и IOProc. Зовётся ОДИН раз за процесс.
    ///
    /// При первом в жизни вызове macOS показывает системный диалог разрешения
    /// на захват звука; решение «липкое» и откатывается только через `tccutil`.
    ///
    /// Любой отказ по дороге уносит за собой уже созданное: за это отвечает
    /// [`Owned`], а не последовательность ручных чисток на каждом `?`.
    pub fn start() -> Result<Self, CaptureError> {
        // --- 1. описание тапа: весь микс, никого не исключаем ---------------
        //
        // `isExclusive` руками НЕ трогаем: это флаг направления (включать
        // перечисленные процессы или исключать их), который конструктор уже
        // выставил правильно. Ручная правка инвертирует смысл и даёт тишину.
        let no_processes: Retained<NSArray<NSNumber>> = NSArray::new();
        let tap_desc = unsafe {
            CATapDescription::initStereoGlobalTapButExcludeProcesses(
                CATapDescription::alloc(),
                &no_processes,
            )
        };
        let tap_uuid = NSUUID::new();
        unsafe {
            tap_desc.setName(&NSString::from_str("meeting-recorder system tap"));
            // UUID выставляем явно, а не полагаемся на конструктор: его
            // строковая форма — это то, чем тап адресуется в списке `taps`
            // агрегата ниже.
            tap_desc.setUUID(&tap_uuid);
        }

        // --- 2. создать тап (здесь всплывает диалог TCC) --------------------
        let mut tap_id: AudioObjectID = 0;
        check(
            unsafe { AudioHardwareCreateProcessTap(Some(&tap_desc), &mut tap_id) },
            "AudioHardwareCreateProcessTap",
        )?;
        // С этой строки за уничтожение отвечает `owned`: любой `?` ниже уронит
        // его Drop и уберёт всё, что успело создаться.
        let mut owned = Owned {
            tap: tap_id,
            agg: 0,
            io_proc: None,
            running: false,
        };

        // --- 3. реальный формат тапа ----------------------------------------
        //
        // Спрашиваем, а не предполагаем: на этой машине замерено 48 кГц / 2 кан.
        // / 32 бита float, но и частота (в ресемплер), и число каналов (в разбор
        // ABL и в даунмикс) обязаны браться из ответа устройства.
        let asbd = tap_format(tap_id)?;
        let tap_rate = asbd.mSampleRate as u32;
        let tap_channels = asbd.mChannelsPerFrame;
        if tap_rate == 0 || tap_channels == 0 {
            return Err(CaptureError::Tap(format!(
                "тап отдал бессмысленный формат: {tap_rate} Гц, {tap_channels} кан."
            )));
        }
        // Читать буферы как f32 мы имеем право только если это и есть f32.
        // Целочисленный тап в природе не наблюдался, но принять его молча
        // значило бы записать шум вместо звука — а звучит это правдоподобно.
        if asbd.mFormatID != kAudioFormatLinearPCM
            || asbd.mFormatFlags & kAudioFormatFlagIsFloat == 0
            || asbd.mBitsPerChannel != 32
        {
            return Err(CaptureError::Tap(format!(
                "поддерживается только 32-битный float LPCM; тап отдал \
                 formatID=0x{:08x}, {} бит/кан., flags=0x{:08x}",
                asbd.mFormatID, asbd.mBitsPerChannel, asbd.mFormatFlags
            )));
        }

        // --- 4. устройство вывода и его собственный вход --------------------
        //
        // Сколько ВХОДНЫХ каналов вносит само устройство вывода — спрашиваем ДО
        // создания агрегата: после устройство уже в него зачислено, и вопрос
        // «не изменился ли его собственный вход от участия в агрегате» пришлось
        // бы проверять на железе. Здесь он просто не возникает.
        let output_device = default_output_device()?;
        let output_uid = device_uid(output_device)?;
        let (device_in_buffers, device_in_channels) = device_input_streams(output_device)?;

        // --- 5. словарь агрегированного устройства --------------------------
        //
        // Ловушка: главным саб-девайсом обязано быть РЕАЛЬНОЕ устройство вывода.
        // Тап главным саб-девайсом быть не может — агрегат соберётся, но отдаст
        // тишину. Тап идёт отдельным списком `taps`.
        let agg_uid = NSUUID::new().UUIDString();
        let yes = NSNumber::new_bool(true);
        let no = NSNumber::new_bool(false);

        let k_sub_device_uid = ns(kAudioSubDeviceUIDKey);
        let sub_device_values: [&AnyObject; 1] = [&output_uid];
        let sub_device: Retained<NSDictionary<NSString, AnyObject>> =
            NSDictionary::from_slices(&[&*k_sub_device_uid], &sub_device_values);

        let k_sub_tap_uid = ns(kAudioSubTapUIDKey);
        let k_sub_tap_drift = ns(kAudioSubTapDriftCompensationKey);
        let tap_uuid_string = tap_uuid.UUIDString();
        let sub_tap_values: [&AnyObject; 2] = [&tap_uuid_string, &yes];
        let sub_tap: Retained<NSDictionary<NSString, AnyObject>> =
            NSDictionary::from_slices(&[&*k_sub_tap_uid, &*k_sub_tap_drift], &sub_tap_values);

        let sub_devices = NSArray::from_retained_slice(&[sub_device]);
        let sub_taps = NSArray::from_retained_slice(&[sub_tap]);

        let agg_name = NSString::from_str("meeting-recorder system aggregate");
        let keys = [
            ns(kAudioAggregateDeviceNameKey),
            ns(kAudioAggregateDeviceUIDKey),
            ns(kAudioAggregateDeviceMainSubDeviceKey),
            ns(kAudioAggregateDeviceIsPrivateKey),
            ns(kAudioAggregateDeviceIsStackedKey),
            ns(kAudioAggregateDeviceTapAutoStartKey),
            ns(kAudioAggregateDeviceSubDeviceListKey),
            ns(kAudioAggregateDeviceTapListKey),
        ];
        let values: [&AnyObject; 8] = [
            &agg_name,
            &agg_uid,
            &output_uid,
            &yes,
            &no,
            &yes,
            &sub_devices,
            &sub_taps,
        ];
        let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
        let agg_dict: Retained<NSDictionary<NSString, AnyObject>> =
            NSDictionary::from_slices(&key_refs, &values);

        // --- 6. создать агрегированное устройство ---------------------------
        //
        // NSDictionary и CFDictionary — toll-free bridged, поэтому указатель на
        // первый законно читается как второй.
        let cf_dict: &CFDictionary =
            unsafe { &*(Retained::as_ptr(&agg_dict) as *const CFDictionary) };
        let mut agg_id: AudioObjectID = 0;
        check(
            unsafe { AudioHardwareCreateAggregateDevice(cf_dict, NonNull::from(&mut agg_id)) },
            "AudioHardwareCreateAggregateDevice",
        )?;
        owned.agg = agg_id;

        // --- 7. IOProc прямо на агрегате ------------------------------------
        //
        // Не через AVAudioEngine: он не ретаргетится на произвольный HAL-девайс —
        // `kAudioOutputUnitProperty_CurrentDevice` вернёт noErr, но движок молча
        // продолжит читать системный дефолтный ВХОД (микрофон), а не тап.
        let (tx, rx) = channel::<Vec<i16>>();
        let diag = Arc::new(Diag::default());
        let state = Mutex::new(CallbackState {
            resampler: Resampler::new(tap_rate, SAMPLE_RATE),
            ch_counts: Vec::with_capacity(16),
            mono: Vec::with_capacity(8192),
        });
        let block: IoProcBlock = io_proc_block(
            state,
            Arc::clone(&diag),
            tx,
            TapLayout {
                device_in_buffers,
                device_in_channels,
                tap_channels,
            },
        );

        // Очередь обязана быть не-nil. Последовательная (attr = None) — от этого
        // зависит корректность колбэка: два одновременных вызова разъехались бы
        // на состоянии ресемплера.
        let queue = DispatchQueue::new("com.meeting-recorder.system-tap", None);
        let mut io_proc_id: AudioDeviceIOProcID = None;
        check(
            unsafe {
                AudioDeviceCreateIOProcIDWithBlock(
                    NonNull::from(&mut io_proc_id),
                    agg_id,
                    Some(&queue),
                    RcBlock::as_ptr(&block),
                )
            },
            "AudioDeviceCreateIOProcIDWithBlock",
        )?;
        owned.io_proc = io_proc_id;

        // `AudioDeviceIOProcID` — это `Option<fn>`, и `noErr` сам по себе не
        // обещает, что внутри `Some`. Пустой ID в `AudioDeviceStart` означает
        // «запусти IO-цикл устройства вообще без IOProc»: колбэк не вызовется
        // ни разу, ни один счётчик в `Diag` не шевельнётся, и системная дорожка
        // будет молчать вечно — без единой строчки в логе. Ровно тот класс
        // молчаливого отказа, ради которого этот модуль и разбирает всё
        // остальное так подробно.
        let Some(proc_id) = io_proc_id else {
            return Err(CaptureError::Tap(
                "AudioDeviceCreateIOProcIDWithBlock вернул noErr, но не отдал IOProcID".into(),
            ));
        };

        // --- 8. поехали ------------------------------------------------------
        check(
            unsafe { AudioDeviceStart(agg_id, Some(proc_id)) },
            "AudioDeviceStart",
        )?;
        owned.running = true;

        Ok(Self {
            _owned: owned,
            rx,
            diag,
            _block: block,
            _queue: queue,
            reported_skipped: 0,
            reported_null: 0,
            last_report: None,
        })
    }

    /// Всё, что накопилось с прошлого вызова: 16 кГц, моно, i16.
    ///
    /// Сведение, ресемплинг и упаковка в формат хранения происходят в колбэке,
    /// как и у `build_mic_capture`; здесь остаётся только выгрести канал —
    /// ровно тем же `try_iter().flatten().collect()`, каким `CpalAudio::drain`
    /// выгребает свои два.
    pub fn drain(&mut self) -> Vec<i16> {
        self.report_failures();
        self.rx.try_iter().flatten().collect()
    }

    /// Отказы аудио-колбэка — в stderr, но не чаще раза в [`DIAG_INTERVAL`].
    ///
    /// Спайк на этом месте падал ассертом, и для спайка это было верно. Здесь
    /// падать нельзя дважды: тап живёт весь процесс, и «раскладку ABL не
    /// разобрали» не должно уносить приложение вместе с идущей записью, а
    /// паниковать в самом колбэке — значит разворачивать стек сквозь C-кадры
    /// HAL и dispatch. Но и молчать нельзя: молчаливый отказ выглядит как
    /// исправно записанная тишина, то есть неотличим от «никто не говорил».
    ///
    /// Зовётся из `drain`, то есть на потоке приложения, а не в аудио-пути.
    fn report_failures(&mut self) {
        let skipped = self.diag.skipped.load(Ordering::Relaxed);
        let null_data = self.diag.null_data.load(Ordering::Relaxed);
        if skipped == self.reported_skipped && null_data == self.reported_null {
            return;
        }
        if self.last_report.is_some_and(|t| t.elapsed() < DIAG_INTERVAL) {
            return;
        }
        if skipped > self.reported_skipped {
            let why = self
                .diag
                .reason
                .lock()
                .ok()
                .and_then(|g| *g)
                .unwrap_or("причина не записана");
            eprintln!(
                "системный тап: тап не опознан во входном ABL ({why}); \
                 пропущено пакетов: {skipped}. Системная дорожка в это время \
                 не пишется — это не тишина в звуке, а отсутствие данных."
            );
        }
        if null_data > self.reported_null {
            eprintln!(
                "системный тап: в {null_data} пакет(ах) у тапа mData == NULL — \
                 HAL отдаёт поток как НЕиспользуемый. Данных в нём нет вовсе."
            );
        }
        self.reported_skipped = skipped;
        self.reported_null = null_data;
        self.last_report = Some(Instant::now());
    }
}

/// Что колбэку нужно знать про раскладку, чтобы найти в ABL именно тап.
///
/// Снимается один раз на старте: устройство вывода в агрегате не меняется, пока
/// жив сам агрегат.
#[derive(Clone, Copy)]
struct TapLayout {
    device_in_buffers: u32,
    device_in_channels: u32,
    tap_channels: u32,
}

/// Собственно колбэк IOProc.
///
/// Вынесен из [`SystemTap::start`] отдельной функцией только ради читаемости
/// самого рецепта: там девять шагов подряд, и тридцать строк работы с сырыми
/// указателями посередине превращают их в стену.
///
/// # Что здесь запрещено
///
/// Паниковать (разворот стека сквозь C-кадры HAL и dispatch) и ждать на замках,
/// которые держит кто-то ещё. Отсюда `try_lock` в `Diag` и
/// `let ... else { return }` вместо `unwrap` на каждом шагу.
///
/// # Про аллокации — честно
///
/// Их здесь ТРИ на пакет, и все три приходят из общего для всего захвата
/// тракта, а не из этого файла:
///
/// 1. `Resampler::process` заводит выходной `Vec<f32>` (`Vec::with_capacity`
///    в `capture/mod.rs`);
/// 2. `process_to_i16` собирает из него второй, уже `Vec<i16>` — специализация
///    `collect` на месте здесь не срабатывает, типы разной ширины;
/// 3. `Sender::send` заводит под результат узел канала.
///
/// Ровно столько же аллоцируют колбэки `build_mic_capture` и
/// `build_loopback_capture` на Windows: это общий паттерн захвата, а не местная
/// небрежность. Убрать их можно только двумя способами — переписав `Resampler`
/// на запись в переданный буфер (общий с Windows код, не зона этой задачи) и
/// сменив транспорт на предвыделенное кольцо (разойтись с остальным захватом).
/// Ни то, ни другое здесь не делалось.
///
/// Важно, чего в этом списке НЕТ: аллокаций сверх тракта. В спайке их было три
/// собственных на пакет.
///
/// Счётчики каналов и моно-аккумулятор переиспользуются между вызовами (см.
/// [`CallbackState`]), а промежуточный переплетённый кадр не строится вовсе.
/// После прогрева ни один из этих буферов не растёт.
///
/// Мьютекс вокруг `CallbackState` при этом честный `lock`, и это не
/// противоречие: за ним никто, кроме самого колбэка, не ходит — ни `drain`, ни
/// Drop, — а очередь последовательная, так что второго претендента на него не
/// существует. Захват такого мьютекса — одна неоспоренная атомарная операция.
fn io_proc_block(
    state: Mutex<CallbackState>,
    diag: Arc<Diag>,
    tx: Sender<Vec<i16>>,
    layout: TapLayout,
) -> IoProcBlock {
    RcBlock::new(
        move |_now: NonNull<AudioTimeStamp>,
              input: NonNull<AudioBufferList>,
              _in_time: NonNull<AudioTimeStamp>,
              _output: NonNull<AudioBufferList>,
              _out_time: NonNull<AudioTimeStamp>| {
            // Число буферов лежит В ПРЕДЕЛАХ объявленного размера структуры, его
            // можно читать полем. А вот сами буферы — нет, см. `buffer_at`.
            let count =
                unsafe { std::ptr::addr_of!((*input.as_ptr()).mNumberBuffers).read() } as usize;
            if count == 0 {
                return;
            }
            // Отравленный мьютекс здесь недостижим (паниковать в этом колбэке
            // нечему), но `unwrap` всё равно был бы паникой в аудио-потоке.
            let Ok(mut guard) = state.lock() else {
                return;
            };
            let st = &mut *guard;

            // Опознаём тап на КАЖДОМ пакете, а не только на первом: раскладка ABL
            // не обязана быть постоянной, а стоит это десяток целочисленных
            // операций.
            st.ch_counts.clear();
            for i in 0..count {
                st.ch_counts
                    .push(unsafe { buffer_at(input, i) }.mNumberChannels);
            }
            let located = locate_tap(
                &st.ch_counts,
                layout.device_in_buffers as usize,
                layout.device_in_channels,
                layout.tap_channels,
            );
            let (start, len) = match located {
                Ok(slice) => slice,
                Err(why) => {
                    diag.note_locate_failure(why);
                    return;
                }
            };

            // Кадров берём МИНИМУМ по буферам тапа: буферы одного ABL не обязаны
            // быть одной длины, и длина, снятая с одного, стала бы перечитом за
            // конец другого.
            let mut frames = usize::MAX;
            for i in start..start + len {
                let b = unsafe { buffer_at(input, i) };
                if b.mData.is_null() {
                    diag.note_null_data();
                    return;
                }
                frames = frames.min(frames_in(
                    b.mDataByteSize as usize / SAMPLE_BYTES,
                    b.mNumberChannels,
                ));
            }
            if frames == 0 {
                return;
            }

            // Сводим ТОЛЬКО буферы тапа. Чужие входы (микрофон AirPods в соседнем
            // буфере того же ABL) в системную дорожку не попадают.
            st.mono.clear();
            st.mono.resize(frames, 0.0);
            for i in start..start + len {
                let b = unsafe { buffer_at(input, i) };
                // SAFETY: `mData` не NULL (проверено выше), в нём `mDataByteSize`
                // байт живых данных HAL, и формат подтверждён 32-битным float на
                // старте.
                let data = unsafe {
                    std::slice::from_raw_parts(
                        b.mData as *const f32,
                        b.mDataByteSize as usize / SAMPLE_BYTES,
                    )
                };
                accumulate_stream(&mut st.mono, data, b.mNumberChannels);
            }
            scale_mono(&mut st.mono, layout.tap_channels);

            let out = st.resampler.process_to_i16(&st.mono);
            if out.is_empty() {
                return;
            }
            // Приёмник мог отвалиться — это не ошибка (и в норме не бывает:
            // `rx` живёт столько же, сколько сам тап).
            let _ = tx.send(out);
        },
    )
}

/// Копия `i`-го `AudioBuffer` из списка.
///
/// Смещение считается В БАЙТАХ от начала `AudioBufferList`, а не через
/// `&AudioBufferList` с последующей индексацией `mBuffers`. Причина в том, что
/// `mBuffers` объявлен как `[AudioBuffer; 1]` — это C-хвост переменной длины,
/// и настоящая длина лежит в `mNumberBuffers`. Материализовать ссылку на
/// структуру, а потом читать по ней второй и третий элементы массива из одного
/// — значит выходить за объявленный размер того, на что взята ссылка. Здесь же
/// ссылки не возникает вовсе: указатель, который дал HAL, остаётся сырым, и
/// провенанс у арифметики — его, то есть всего настоящего буфера.
///
/// # Safety
/// `abl` указывает на живой `AudioBufferList` HAL, а `i < mNumberBuffers`.
#[inline]
unsafe fn buffer_at(abl: NonNull<AudioBufferList>, i: usize) -> AudioBuffer {
    let base = unsafe {
        abl.as_ptr()
            .cast::<u8>()
            .add(std::mem::offset_of!(AudioBufferList, mBuffers))
    };
    unsafe { base.cast::<AudioBuffer>().add(i).read() }
}

// --- мелкие помощники -------------------------------------------------------

/// Адрес свойства в глобальной области, главный элемент.
fn addr(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn default_output_device() -> Result<AudioObjectID, CaptureError> {
    let mut device: AudioObjectID = 0;
    let mut size = std::mem::size_of::<AudioObjectID>() as u32;
    let mut a = addr(kAudioHardwarePropertyDefaultOutputDevice);
    check(
        unsafe {
            AudioObjectGetPropertyData(
                kAudioObjectSystemObject as AudioObjectID,
                NonNull::from(&mut a),
                0,
                null(),
                NonNull::from(&mut size),
                NonNull::from(&mut device).cast(),
            )
        },
        "kAudioHardwarePropertyDefaultOutputDevice",
    )?;
    Ok(device)
}

/// UID устройства. CFStringRef приходит с +1 (правило Copy), а CFString и
/// NSString toll-free bridged — поэтому владение сразу забирает `Retained`.
fn device_uid(device: AudioObjectID) -> Result<Retained<NSString>, CaptureError> {
    let mut uid: *mut NSString = std::ptr::null_mut();
    let mut size = std::mem::size_of::<*mut NSString>() as u32;
    let mut a = addr(kAudioDevicePropertyDeviceUID);
    check(
        unsafe {
            AudioObjectGetPropertyData(
                device,
                NonNull::from(&mut a),
                0,
                null(),
                NonNull::from(&mut size),
                NonNull::from(&mut uid).cast(),
            )
        },
        "kAudioDevicePropertyDeviceUID",
    )?;
    unsafe { Retained::from_raw(uid) }
        .ok_or_else(|| CaptureError::Tap("устройство вывода вернуло пустой UID".into()))
}

/// Сколько буферов и каналов устройство вносит СВОИМ входом.
///
/// `kAudioDevicePropertyStreamConfiguration` во входной области возвращает
/// `AudioBufferList` переменной длины — размер сначала спрашивается отдельно.
/// У устройства без входов (встроенные динамики) свойства может не быть вовсе:
/// это не ошибка, а честный ноль.
fn device_input_streams(device: AudioObjectID) -> Result<(u32, u32), CaptureError> {
    let mut a = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreamConfiguration,
        mScope: kAudioObjectPropertyScopeInput,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size: u32 = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            device,
            NonNull::from(&mut a),
            0,
            null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 || (size as usize) < std::mem::size_of::<AudioBufferList>() {
        return Ok((0, 0));
    }
    // Через Vec<u64>, а не Vec<u8>: в AudioBufferList есть указатель, и читать
    // структуру с невыровненного адреса — UB. u64 даёт нужные 8 байт
    // выравнивания.
    let mut raw = vec![0u64; size as usize / 8 + 1];
    let Some(ptr) = NonNull::new(raw.as_mut_ptr().cast()) else {
        return Ok((0, 0));
    };
    check(
        unsafe {
            AudioObjectGetPropertyData(
                device,
                NonNull::from(&mut a),
                0,
                null(),
                NonNull::from(&mut size),
                ptr,
            )
        },
        "kAudioDevicePropertyStreamConfiguration (вход устройства вывода)",
    )?;
    // Тот же приём, что в колбэке, и по той же причине: список с C-хвостом
    // читается смещениями по сырому указателю, а не через ссылку на структуру.
    let abl: NonNull<AudioBufferList> = ptr.cast();
    let count = unsafe { std::ptr::addr_of!((*abl.as_ptr()).mNumberBuffers).read() } as usize;
    let channels = (0..count)
        .map(|i| unsafe { buffer_at(abl, i) }.mNumberChannels)
        .sum();
    Ok((count as u32, channels))
}

fn tap_format(tap: AudioObjectID) -> Result<AudioStreamBasicDescription, CaptureError> {
    let mut asbd: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
    let mut a = addr(kAudioTapPropertyFormat);
    check(
        unsafe {
            AudioObjectGetPropertyData(
                tap,
                NonNull::from(&mut a),
                0,
                null(),
                NonNull::from(&mut size),
                NonNull::from(&mut asbd).cast(),
            )
        },
        "kAudioTapPropertyFormat",
    )?;
    Ok(asbd)
}

/// Ключи словаря агрегата объявлены в SDK как C-строки — переводим в NSString.
///
/// `expect`, а не `to_string_lossy`: подстановка U+FFFD дала бы НЕ ТОТ ключ
/// словаря, а неверный ключ в словаре агрегата — это тишина без ошибки (та же
/// ловушка, что с тапом в позиции главного саб-девайса). Условие недостижимо —
/// все восемь ключей приходят ASCII-константами из SDK, — но падение здесь
/// честнее молчаливо испорченного агрегата, и происходит оно на старте, а не в
/// аудио-потоке.
fn ns(key: &CStr) -> Retained<NSString> {
    NSString::from_str(key.to_str().expect("ключ Core Audio не UTF-8"))
}

/// `noErr` или ошибка с именем вызова и кодом.
fn check(status: i32, what: &'static str) -> Result<(), CaptureError> {
    if status == 0 {
        Ok(())
    } else {
        Err(CaptureError::CoreAudio { what, status })
    }
}

/// То же, но для Drop, где вернуть ошибку некому. Молчать нельзя: неснесённый
/// агрегат — это лишнее устройство в системе до конца процесса.
fn log_status(status: i32, what: &str) {
    if status != 0 {
        eprintln!("{what} при разборе системного тапа: {}", status_text(status));
    }
}

/// OSStatus у Core Audio почти всегда четырёхсимвольный код вроде 'nope'.
pub(crate) fn status_text(code: i32) -> String {
    let bytes = (code as u32).to_be_bytes();
    if bytes.iter().all(|b| (0x20..=0x7e).contains(b)) {
        format!("OSStatus {code} ('{}')", String::from_utf8_lossy(&bytes))
    } else {
        format!("OSStatus {code}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- locate_tap: раскладки, снятые с живого железа и смежные ---

    /// Реальный прогон спайка: AirPods как устройство вывода по умолчанию.
    /// `[микрофон 1 кан][тап 2 кан]` — тап НЕ в буфере 0.
    #[test]
    fn airpods_микрофон_перед_тапом() {
        assert_eq!(locate_tap(&[1, 2], 1, 1, 2), Ok((1, 1)));
    }

    /// Встроенные динамики: своих входов нет, тап занимает весь ABL.
    /// Это тот путь, который «работал» и маскировал баг.
    #[test]
    fn устройство_без_входов_отдаёт_весь_abl() {
        assert_eq!(locate_tap(&[2], 0, 0, 2), Ok((0, 1)));
    }

    #[test]
    fn тап_двумя_потоками_без_входов_устройства() {
        assert_eq!(locate_tap(&[1, 1], 0, 0, 2), Ok((0, 2)));
    }

    /// Тот самый случай, где одной суммы каналов не хватает: `[1][1][2]` при
    /// входе «2 канала» читается двояко, и различает только число буферов.
    #[test]
    fn usb_вход_двумя_буферами_различим_по_числу_буферов() {
        assert_eq!(locate_tap(&[1, 1, 2], 2, 2, 2), Ok((2, 1)));
    }

    #[test]
    fn многоканальный_интерфейс_одним_буфером() {
        assert_eq!(locate_tap(&[8, 2], 1, 8, 2), Ok((1, 1)));
    }

    #[test]
    fn тап_двумя_моно_потоками_после_двухканального_входа() {
        assert_eq!(locate_tap(&[2, 1, 1], 1, 2, 2), Ok((1, 2)));
    }

    // --- locate_tap: всё, что обязано падать, а не угадываться ---

    /// Симметрия `[2][2]` при входе «1 буфер, 2 канала»: прямое и обратное
    /// прочтения одинаково состоятельны. Угадывать нельзя.
    #[test]
    fn симметричная_раскладка_отвергается() {
        assert!(locate_tap(&[2, 2], 1, 2, 2)
            .unwrap_err()
            .contains("симметрич"));
    }

    /// `[1][1][1]` при входе «1 буфер, 1 канал» и тапе 2 канала: `[1 | 1,1]` и
    /// `[1,1 | 1]` неразличимы даже по обеим величинам.
    #[test]
    fn три_моно_буфера_неразличимы() {
        assert!(locate_tap(&[1, 1, 1], 1, 1, 2)
            .unwrap_err()
            .contains("симметрич"));
    }

    /// Если тап окажется ПЕРЕД входом устройства, это надо заметить, а не
    /// записать чужой вход вместо системного звука.
    #[test]
    fn обратный_порядок_распознаётся_отдельно() {
        assert!(locate_tap(&[2, 1], 1, 1, 2)
            .unwrap_err()
            .contains("обратный"));
    }

    #[test]
    fn сумма_каналов_не_сходится() {
        assert!(locate_tap(&[5], 1, 1, 2).unwrap_err().contains("сумма"));
    }

    /// Сумма сходится, но границу по буферам провести нельзя.
    #[test]
    fn граница_не_проводится() {
        assert!(locate_tap(&[3], 1, 1, 2)
            .unwrap_err()
            .contains("не сходится"));
    }

    #[test]
    fn границы_нет_при_разнокалиберных_буферах() {
        assert!(locate_tap(&[2, 3], 1, 1, 4).is_err());
    }

    #[test]
    fn буферов_меньше_чем_вносит_вход() {
        assert!(locate_tap(&[2], 3, 1, 1)
            .unwrap_err()
            .contains("буферов меньше"));
    }

    // --- interleave_streams ---

    /// Два моно-потока сшиваются в стерео покадрово, а не встык.
    #[test]
    fn два_моно_потока_дают_переплетённое_стерео() {
        let l = [1.0f32, 3.0, 5.0];
        let r = [2.0f32, 4.0, 6.0];
        let out = interleave_streams(&[(1, &l), (1, &r)], 2);
        assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    }

    /// Уже переплетённый стерео-поток проходит насквозь без перестановок —
    /// именно этот случай отдаёт реальный тап.
    #[test]
    fn переплетённый_поток_не_переставляется() {
        let stereo = [1.0f32, 2.0, 3.0, 4.0];
        let out = interleave_streams(&[(2, &stereo)], 2);
        assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0]);
    }

    /// Потоки разной длины: берётся минимум, лишнее отбрасывается, за край не
    /// читаем. Раньше именно здесь был двукратный перечит.
    #[test]
    fn разная_длина_потоков_обрезается_по_минимуму() {
        let long = [1.0f32, 2.0, 3.0, 4.0];
        let short = [9.0f32, 8.0];
        let out = interleave_streams(&[(1, &long), (1, &short)], 2);
        assert_eq!(out, vec![1.0, 9.0, 2.0, 8.0]);
    }

    /// Стерео-поток плюс моно-поток в трёхканальный кадр.
    #[test]
    fn смешанная_ширина_потоков() {
        let stereo = [1.0f32, 2.0, 3.0, 4.0];
        let mono = [7.0f32, 8.0];
        let out = interleave_streams(&[(2, &stereo), (1, &mono)], 3);
        assert_eq!(out, vec![1.0, 2.0, 7.0, 3.0, 4.0, 8.0]);
    }

    #[test]
    fn пустой_вход_даёт_пустой_выход() {
        assert_eq!(interleave_streams(&[], 2), Vec::<f32>::new());
        let empty: [f32; 0] = [];
        assert_eq!(interleave_streams(&[(1, &empty)], 2), Vec::<f32>::new());
    }

    /// Потоки шире заявленного числа каналов — обрываемся, а не пишем за край.
    #[test]
    fn потоки_шире_заявленного_не_переполняют_кадр() {
        let a = [1.0f32, 2.0];
        let b = [3.0f32, 4.0];
        let out = interleave_streams(&[(1, &a), (1, &b)], 1);
        assert_eq!(out, vec![1.0, 2.0]);
    }

    // --- accumulate_stream / scale_mono ---
    //
    // Это то, чем колбэк IOProc сводит буферы тапа в моно, не аллоцируя.
    // Проверяется двумя способами сразу: на конкретных числах и на совпадении
    // с эталоном (`interleave_streams` + `downmix_to_mono_f32`).

    /// Ровно то, что делает колбэк: обнулить аккумулятор на минимальную длину,
    /// сложить в него все потоки, поделить на общее число каналов.
    fn свести_моно(streams: &[(u32, &[f32])], total_channels: u32) -> Vec<f32> {
        let frames = streams
            .iter()
            .map(|(ch, data)| frames_in(data.len(), *ch))
            .min()
            .unwrap_or(0);
        let mut acc = vec![0.0f32; frames];
        for (ch, data) in streams {
            accumulate_stream(&mut acc, data, *ch);
        }
        scale_mono(&mut acc, total_channels);
        acc
    }

    #[test]
    fn кадры_считаются_по_каналам_а_не_по_сэмплам() {
        assert_eq!(frames_in(8, 2), 4);
        assert_eq!(frames_in(7, 2), 3, "неполный кадр не считается");
        assert_eq!(frames_in(0, 2), 0);
    }

    /// Нулевых каналов в ABL быть не должно, но деление на ноль в аудио-колбэке
    /// было бы паникой — то есть разворотом стека сквозь C-кадры HAL.
    #[test]
    fn ноль_каналов_не_делит_на_ноль() {
        assert_eq!(frames_in(4, 0), 4);
        let mut acc = vec![0.0f32; 2];
        accumulate_stream(&mut acc, &[1.0, 2.0], 0);
        scale_mono(&mut acc, 0);
        assert_eq!(acc, vec![1.0, 2.0]);
    }

    /// Переплетённое стерео — реальный случай этой машины: 48 кГц, 2 кан.,
    /// один буфер. (L+R)/2 покадрово.
    #[test]
    fn переплетённое_стерео_сводится_покадрово() {
        let stereo = [1.0f32, 3.0, -1.0, 1.0];
        assert_eq!(свести_моно(&[(2, &stereo)], 2), vec![2.0, 0.0]);
    }

    /// Два моно-потока (непереплетённый тап) дают то же, что одно переплетённое
    /// стерео из тех же чисел.
    #[test]
    fn два_моно_потока_сводятся_как_одно_стерео() {
        let l = [1.0f32, -1.0];
        let r = [3.0f32, 1.0];
        assert_eq!(свести_моно(&[(1, &l), (1, &r)], 2), vec![2.0, 0.0]);
    }

    /// Потоки разной длины: считаем по минимуму, за конец короткого не лезем.
    #[test]
    fn разная_длина_потоков_режется_по_минимуму() {
        let long = [1.0f32, 2.0, 3.0, 4.0];
        let short = [1.0f32, 2.0];
        assert_eq!(свести_моно(&[(1, &long), (1, &short)], 2), vec![1.0, 2.0]);
    }

    /// Гвоздь оптимизации: пара `accumulate_stream` + `scale_mono` обязана
    /// давать ровно то же, что эталонная связка `interleave_streams` +
    /// `downmix_to_mono_f32`, ради экономии которой она и заведена.
    ///
    /// Раскладки — те, которые может отдать тап: сумма каналов потоков равна
    /// объявленному числу каналов кадра (это гарантирует `locate_tap`, иначе
    /// до сведения дело не доходит вовсе). Сравнение с допуском, а не на
    /// равенство: порядок сложения f32 у двух реализаций разный.
    #[test]
    fn сумма_потоков_совпадает_с_переплетением_и_даунмиксом() {
        let a = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0];
        let b = [-1.0f32, 0.5, 0.25, -0.75];
        let c = [0.1f32, 0.2, 0.3];
        // Псевдоним, а не тип на месте: без него clippy справедливо ругается на
        // `Vec<(Vec<(u32, &[f32])>, u32)>` как на нечитаемый.
        type Раскладка<'a> = (Vec<(u32, &'a [f32])>, u32);
        let случаи: Vec<Раскладка> = vec![
            (vec![(2, &a[..])], 2),
            (vec![(1, &c[..]), (1, &c[..])], 2),
            (vec![(2, &a[..]), (1, &c[..])], 3),
            (vec![(2, &b[..]), (2, &a[..])], 4),
            (vec![(1, &a[..]), (1, &b[..]), (1, &c[..])], 3),
        ];
        for (streams, total) in случаи {
            let эталон = crate::capture::downmix_to_mono_f32(
                &interleave_streams(&streams, total),
                total.max(1) as u16,
            );
            let факт = свести_моно(&streams, total);
            assert_eq!(факт.len(), эталон.len(), "разное число кадров при {total} кан.");
            for (i, (f, e)) in факт.iter().zip(эталон.iter()).enumerate() {
                assert!(
                    (f - e).abs() < 1e-6,
                    "кадр {i} при {total} кан.: {f} против эталонных {e}"
                );
            }
        }
    }

    // --- status_text ---

    #[test]
    fn четырёхсимвольный_код_показывается_текстом() {
        // 'nope' — ровно то, чем Core Audio отвечает на неверный аргумент.
        let code = i32::from_be_bytes(*b"nope");
        assert!(status_text(code).contains("'nope'"));
    }

    #[test]
    fn не_печатный_код_показывается_числом() {
        assert_eq!(status_text(-1), "OSStatus -1");
    }
}
