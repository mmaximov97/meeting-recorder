//! Спайк: рецепт Core Audio Process Tap (macOS 14.4+) на минимальном коде.
//!
//! Цель — де-рискнуть самый неопределённый кусок порта ДО того, как он обрастёт
//! обвязкой (`capture::macos`, каналы, `AudioIo`). Здесь только последовательность
//! системных вызовов; запись WAV и конвертация формата переиспользуют то, что уже
//! есть в ядре (`storage::WavSink`, `capture::downmix_to_mono_f32`,
//! `capture::Resampler`), а не пишутся заново.
//!
//! Запуск: `cargo run --example mac_tap_spike` — при ПЕРВОМ запуске macOS покажет
//! системный диалог разрешения на захват аудио. Решение «липкое»: отказ придётся
//! откатывать через `tccutil`.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("этот пример только для macOS");
}

#[cfg(target_os = "macos")]
fn main() {
    imp::run();
}

#[cfg(target_os = "macos")]
mod imp {
    use block2::RcBlock;
    use dispatch2::DispatchQueue;
    use meeting_recorder::capture::{downmix_to_mono_f32, Resampler};
    use meeting_recorder::storage::{WavSink, SAMPLE_RATE};
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::AllocAnyThread;
    use objc2_core_audio::{
        kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
        kAudioAggregateDeviceMainSubDeviceKey, kAudioAggregateDeviceNameKey,
        kAudioAggregateDeviceSubDeviceListKey, kAudioAggregateDeviceTapAutoStartKey,
        kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey,
        kAudioDevicePropertyDeviceUID, kAudioDevicePropertyStreamConfiguration,
        kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyElementMain,
        kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput, kAudioObjectSystemObject,
        kAudioSubDeviceUIDKey, kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey,
        kAudioTapPropertyFormat, AudioDeviceCreateIOProcIDWithBlock, AudioDeviceDestroyIOProcID,
        AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
        AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
        AudioHardwareDestroyProcessTap, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
        AudioObjectID, AudioObjectPropertyAddress, CATapDescription,
    };
    use objc2_core_audio_types::{
        kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, AudioBuffer, AudioBufferList,
        AudioStreamBasicDescription, AudioTimeStamp,
    };
    use objc2_core_foundation::CFDictionary;
    use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSUUID};
    use std::ffi::CStr;
    use std::path::Path;
    use std::ptr::{null, NonNull};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Сколько секунд системного звука пишем.
    const RECORD_SECS: u64 = 5;

    /// Фора человеку на «переключиться в плеер и нажать play» до старта записи.
    const GRACE_SECS: u64 = 3;

    /// Состояние за одним мьютексом: ресемплер и накопитель.
    ///
    /// Ресемплер именно ОДИН на весь поток, а не новый на пакет: он stateful —
    /// хранит фазу и историю фильтра между вызовами (см. докблок `Resampler`),
    /// поэтому пересоздание на каждый пакет дало бы щелчок и прогрев на каждом шве.
    struct State {
        resampler: Resampler,
        samples: Vec<i16>,
        packets: u64,
        /// Раскладка ВХОДНОГО ABL, снятая на первом пакете, — для диагностики.
        abl_buffers: u32,
        abl_channels: u32,
        /// Срез буферов ABL, опознанный как тап: `[start .. start+len)`.
        tap_start: usize,
        tap_len: usize,
        /// Первая причина, по которой опознать тап не удалось. Паниковать прямо
        /// в колбэке нельзя (разворот стека сквозь C-кадры HAL и dispatch), так
        /// что причина копится здесь, а падает `run()` уже на своём потоке.
        locate_error: Option<&'static str>,
        skipped: u64,
    }

    /// ТРЕТЬЯ ловушка рецепта, не описанная в дизайн-доке (первые две — тап как
    /// главный саб-девайс и `isExclusive`). Найдена прогоном на AirPods.
    ///
    /// Входной ABL агрегата содержит НЕ ТОЛЬКО тап. Главный саб-девайс — реальное
    /// устройство вывода, и если у него есть собственные ВХОДНЫЕ потоки (у AirPods
    /// это микрофон, у USB-интерфейса — его входы), они приезжают в тот же ABL:
    ///
    /// ```text
    /// [0] mNumberChannels=1, 2048 Б  <- микрофон AirPods
    /// [1] mNumberChannels=2, 4096 Б  <- собственно тап (48 кГц, f32, переплетённое стерео)
    /// ```
    ///
    /// Смещение тапа НЕ хардкодится. Оно выводится: у устройства вывода
    /// спрашивается, сколько входных каналов оно вносит
    /// (`kAudioDevicePropertyStreamConfiguration` во входной области), у тапа
    /// известно `mChannelsPerFrame`, и граница между ними проводится по
    /// накопленной сумме каналов. Порядок «сначала саб-девайс, потом тап» —
    /// наблюдение, а не гарантия SDK, поэтому обратный порядок и неоднозначные
    /// раскладки распознаются отдельно и валят прогон с внятным текстом.
    ///
    /// Опорный факт из заголовка (`AudioHardware.h`, `FullSubDeviceList`):
    /// «The order of the items in the array is significant and is used to
    /// determine the order of the streams of the AudioAggregateDevice» — то есть
    /// порядок потоков саб-девайсов задан их порядком в композиции. Про то, где
    /// относительно них встают тапы, заголовок не говорит ничего; отсюда сверка.
    ///
    /// Возвращает `[start, len)` в буферах ABL.
    fn locate_tap(
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
        // Вход устройства известен ДВУМЯ величинами — сколько буферов и сколько
        // каналов. Проверять обе строго сильнее, чем одну накопленную сумму:
        // раскладка [1кан][1кан][2кан] при входе «2 кан.» по одним каналам
        // читается двояко, а с числом буферов — однозначно.
        let tap_buffers = channels.len() - device_in_buffers;
        let sum = |part: &[u32]| -> u32 { part.iter().sum() };

        let forward = sum(&channels[..device_in_buffers]) == device_in_channels
            && sum(&channels[device_in_buffers..]) == tap_channels;
        let reverse = sum(&channels[..tap_buffers]) == tap_channels
            && sum(&channels[tap_buffers..]) == device_in_channels;

        // Вход саб-девайса пуст (встроенные динамики): тап — весь ABL, и оба
        // прочтения вырождаются в одно. Это не неоднозначность, а тривиальный случай.
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

    /// Сводит буферы ТАПА (и только их) в моно f32, уважая собственное
    /// `mNumberChannels` каждого буфера.
    ///
    /// Вторая половина того же бага: `mNumberBuffers > 1` НЕ означает
    /// «непереплетённый». Это несколько ПОТОКОВ, каждый из которых сам может быть
    /// переплетённым. Тап отдаёт один поток с двумя переплетёнными каналами, и
    /// старый код читал его L,R,L,R как последовательные моно-сэмплы — отсюда
    /// алиасинг, убивший 1318.5 Гц в спектральной проверке. Правильный
    /// различитель — `mNumberChannels` буфера, а не число буферов.
    ///
    /// # Safety
    /// `bufs` должны указывать на живые буферы HAL с корректным `mDataByteSize`.
    unsafe fn tap_to_mono(bufs: &[AudioBuffer], tap_channels: u32) -> Vec<f32> {
        if bufs.iter().any(|b| b.mData.is_null()) {
            return Vec::new();
        }
        // Один поток — общий случай для тапа: переплетённое стерео. Копии не надо.
        if bufs.len() == 1 {
            let b = &bufs[0];
            let ch = b.mNumberChannels.max(1);
            let n = b.mDataByteSize as usize / 4;
            let samples = unsafe { std::slice::from_raw_parts(b.mData as *const f32, n) };
            return downmix_to_mono_f32(samples, ch as u16);
        }
        // Несколько потоков тапа: каждый со своим числом каналов. Переплетаем в
        // скрэтч на tap_channels каналов и сводим тем же downmix_to_mono_f32.
        // `frames` — минимум по потокам: буферы не обязаны быть одной длины, и
        // длина, снятая с одного, стала бы перечитом за конец другого.
        let frames = bufs
            .iter()
            .map(|b| b.mDataByteSize as usize / 4 / b.mNumberChannels.max(1) as usize)
            .min()
            .unwrap_or(0);
        let total = tap_channels.max(1) as usize;
        if frames == 0 {
            return Vec::new();
        }
        let mut inter = vec![0.0f32; frames * total];
        let mut base = 0usize;
        for b in bufs {
            let ch = b.mNumberChannels.max(1) as usize;
            if base + ch > total {
                break;
            }
            let src = unsafe { std::slice::from_raw_parts(b.mData as *const f32, frames * ch) };
            for f in 0..frames {
                for c in 0..ch {
                    inter[f * total + base + c] = src[f * ch + c];
                }
            }
            base += ch;
        }
        downmix_to_mono_f32(&inter, total as u16)
    }

    pub fn run() {
        // --- 1. описание тапа: весь микс, никого не исключаем -----------------
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
            tap_desc.setName(&NSString::from_str("meeting-recorder spike tap"));
            // UUID выставляем явно, а не полагаемся на конструктор: его строковая
            // форма — это то, чем тап адресуется в списке `taps` агрегата (шаг 4).
            tap_desc.setUUID(&tap_uuid);
        }

        // --- 2. создать тап (здесь всплывает TCC-диалог) ----------------------
        let mut tap_id: AudioObjectID = 0;
        check(
            unsafe { AudioHardwareCreateProcessTap(Some(&tap_desc), &mut tap_id) },
            "AudioHardwareCreateProcessTap",
        );

        // Реальный формат тапа — задокументированный факт для Task 4.
        let asbd = tap_format(tap_id);
        let tap_rate = asbd.mSampleRate as u32;
        let tap_channels = asbd.mChannelsPerFrame;
        let non_interleaved = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
        println!(
            "формат тапа: {} Гц, {} кан., {} бит/кан., {}, flags=0x{:08x}",
            tap_rate,
            tap_channels,
            asbd.mBitsPerChannel,
            if non_interleaved {
                "непереплетённый"
            } else {
                "переплетённый"
            },
            asbd.mFormatFlags,
        );
        assert!(tap_rate > 0, "тап отдал нулевую частоту дискретизации");
        assert!(
            asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0 && asbd.mBitsPerChannel == 32,
            "спайк умеет читать только 32-битный float; тап отдал {} бит, flags=0x{:08x}",
            asbd.mBitsPerChannel,
            asbd.mFormatFlags
        );

        // --- 3. UID текущего дефолтного устройства вывода ---------------------
        let output_device = default_output_device();
        let output_uid = device_uid(output_device);

        // --- 4. словарь агрегированного устройства ----------------------------
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
        let agg_name = NSString::from_str("meeting-recorder spike aggregate");
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

        // --- 5. создать агрегированное устройство ------------------------------
        //
        // NSDictionary и CFDictionary — toll-free bridged, поэтому указатель на
        // первый законно читается как второй.
        let cf_dict: &CFDictionary =
            unsafe { &*(Retained::as_ptr(&agg_dict) as *const CFDictionary) };
        let mut agg_id: AudioObjectID = 0;
        check(
            unsafe { AudioHardwareCreateAggregateDevice(cf_dict, NonNull::from(&mut agg_id)) },
            "AudioHardwareCreateAggregateDevice",
        );

        // --- 6. IOProc прямо на агрегате ---------------------------------------
        //
        // Не через AVAudioEngine: он не ретаргетится на произвольный HAL-девайс —
        // `kAudioOutputUnitProperty_CurrentDevice` вернёт noErr, но движок молча
        // продолжит читать системный дефолтный ВХОД (микрофон), а не тап.
        // Сколько ВХОДНЫХ каналов вносит в агрегат само устройство вывода. Это и
        // есть та величина, из которой выводится смещение тапа в ABL, — вместо
        // «пропустить первый буфер» или «тап последний», то есть вместо замены
        // одного непроверенного допущения другим.
        let (device_in_buffers, device_in_channels) = device_input_streams(output_device);
        println!(
            "вход устройства вывода вносит в агрегат: {} буфер(ов), {} кан.",
            device_in_buffers, device_in_channels
        );

        let state = Arc::new(Mutex::new(State {
            resampler: Resampler::new(tap_rate, SAMPLE_RATE),
            samples: Vec::new(),
            packets: 0,
            abl_buffers: 0,
            abl_channels: 0,
            tap_start: 0,
            tap_len: 0,
            locate_error: None,
            skipped: 0,
        }));
        let cb_state = Arc::clone(&state);

        let block = RcBlock::new(
            move |_now: NonNull<AudioTimeStamp>,
                  input: NonNull<AudioBufferList>,
                  _in_time: NonNull<AudioTimeStamp>,
                  _output: NonNull<AudioBufferList>,
                  _out_time: NonNull<AudioTimeStamp>| {
                let abl = unsafe { input.as_ref() };
                let count = abl.mNumberBuffers as usize;
                if count == 0 {
                    return;
                }
                // `mBuffers` объявлен как массив из одного элемента, реальная длина
                // лежит в `mNumberBuffers` — классический C-хвост переменной длины.
                let buffers = unsafe { std::slice::from_raw_parts(abl.mBuffers.as_ptr(), count) };

                let mut guard = cb_state.lock().expect("мьютекс отравлен");
                let st = &mut *guard;
                st.packets += 1;

                // Опознаём тап на КАЖДОМ пакете, а не только на первом: раскладка
                // ABL не обязана быть постоянной, а стоит это десяток целочисленных
                // операций. Ошибка не паникует здесь, а копится в состоянии.
                let ch_counts: Vec<u32> = buffers.iter().map(|b| b.mNumberChannels).collect();
                let located = locate_tap(
                    &ch_counts,
                    device_in_buffers as usize,
                    device_in_channels,
                    tap_channels,
                );

                // Раскладку печатаем ровно один раз: без этого открытый вопрос
                // «а тап ли вообще лежит в буфере 0» уходит в Task 4 непроверенным.
                // Печать в аудио-колбэке — грех, но однократный.
                if st.packets == 1 {
                    st.abl_buffers = abl.mNumberBuffers;
                    st.abl_channels = ch_counts.iter().sum();
                    let layout: Vec<String> = buffers
                        .iter()
                        .enumerate()
                        .map(|(i, b)| format!("[{i}] {}кан./{}Б", b.mNumberChannels, b.mDataByteSize))
                        .collect();
                    println!(
                        "раскладка входного ABL: {} буфер(ов) {{{}}}, итого {} кан.",
                        st.abl_buffers,
                        layout.join(", "),
                        st.abl_channels,
                    );
                    if let Ok((start, len)) = located {
                        st.tap_start = start;
                        st.tap_len = len;
                    }
                    match located {
                        Ok((start, len)) => println!(
                            "тап опознан как буфер(ы) [{}..{}) — вход устройства вывода вносит \
                             {} кан. в {} буфер(ах), тап вносит {} кан.; граница проведена по \
                             накопленной сумме каналов, не по фиксированному смещению",
                            start,
                            start + len,
                            device_in_channels,
                            device_in_buffers,
                            tap_channels,
                        ),
                        Err(why) => eprintln!("тап в ABL НЕ опознан: {why}"),
                    }
                }

                let (start, len) = match located {
                    Ok(slice) => slice,
                    Err(why) => {
                        st.locate_error.get_or_insert(why);
                        st.skipped += 1;
                        return;
                    }
                };

                // Сводим ТОЛЬКО буферы тапа. Чужие входы не попадают в микс.
                let mono = unsafe { tap_to_mono(&buffers[start..start + len], tap_channels) };
                if mono.is_empty() {
                    return;
                }
                let out = st.resampler.process_to_i16(&mono);
                st.samples.extend_from_slice(&out);
            },
        );

        // Очередь обязана быть не-nil.
        let queue = DispatchQueue::new("com.meeting-recorder.spike-tap", None);
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
        );

        // --- 7. старт / стоп / разбор в обратном порядке ------------------------
        //
        // Отсчёт до старта обязателен. Без него окно записи начинает тикать
        // раньше, чем человек успел переключиться на плеер и нажать play: он
        // не успевает, файл выходит пустым, и спайк печатает подсказку про две
        // ловушки Core Audio, которые на самом деле реализованы верно. Это
        // ровно тот ложноотрицательный результат, ради исключения которого
        // спайк и существует — и он стоил бы одноразового TCC-решения.
        println!(
            "Включи что-нибудь — Spotify, YouTube, что угодно. \
             Запись начнётся через {GRACE_SECS} с и продлится {RECORD_SECS} с."
        );
        for remaining in (1..=GRACE_SECS).rev() {
            println!("  {remaining}...");
            std::thread::sleep(Duration::from_secs(1));
        }
        check(
            unsafe { AudioDeviceStart(agg_id, io_proc_id) },
            "AudioDeviceStart",
        );
        println!("ПИШУ {RECORD_SECS} с — пусть играет.");
        std::thread::sleep(Duration::from_secs(RECORD_SECS));
        check(
            unsafe { AudioDeviceStop(agg_id, io_proc_id) },
            "AudioDeviceStop",
        );
        check(
            unsafe { AudioDeviceDestroyIOProcID(agg_id, io_proc_id) },
            "AudioDeviceDestroyIOProcID",
        );
        check(
            unsafe { AudioHardwareDestroyAggregateDevice(agg_id) },
            "AudioHardwareDestroyAggregateDevice",
        );
        check(
            unsafe { AudioHardwareDestroyProcessTap(tap_id) },
            "AudioHardwareDestroyProcessTap",
        );
        drop(block);

        // --- запись через существующий WavSink ---------------------------------
        //
        // WavSink жёстко пишет заголовок «16 кГц, моно, 16 бит» и под вход НЕ
        // подстраивается. Поэтому сэмплы уже сведены и ресемплированы в колбэке —
        // иначе заголовок соврал бы о частоте и файл звучал бы втрое медленнее.
        let st = state.lock().expect("мьютекс отравлен");
        let peak = st
            .samples
            .iter()
            .map(|s| s.unsigned_abs())
            .max()
            .unwrap_or(0);
        println!(
            "пакетов: {}, сэмплов 16 кГц моно: {} (~{:.2} с), пик: {peak}",
            st.packets,
            st.samples.len(),
            st.samples.len() as f64 / SAMPLE_RATE as f64,
        );
        if peak == 0 {
            eprintln!(
                "ТИШИНА. По порядку, от самого частого к самому редкому:\n\
                 1) ...или ничего не играло — проверь, что звук действительно шёл \
                 эти {RECORD_SECS} с (успел ли ты нажать play за {GRACE_SECS} с отсчёта, \
                 не был ли звук выключен или выведен на другое устройство);\n\
                 2) тап не должен быть главным саб-девайсом агрегата;\n\
                 3) isExclusive нельзя трогать руками."
            );
        }

        let mut sink = WavSink::create(Path::new("."), "mac_tap_spike.wav").expect("создать WAV");
        sink.write(&st.samples).expect("записать сэмплы");
        let path = sink.finalize().expect("закрыть WAV");
        println!(
            "Записано в {} — прослушай и подтверди, что там системный звук",
            path.display()
        );

        // Вердикт по раскладке — ПОСЛЕ записи WAV и разбора устройств, но до
        // выхода с нулевым кодом. Порядок именно такой: файл и вся диагностика
        // уже на диске, разбирать проблему есть по чему, а прогон при этом
        // честно завершается ошибкой и галочку поставить не даёт.
        //
        // Сама сверка сделана здесь, а не ассертом внутри колбэка, сознательно:
        // паника из блока IOProc разворачивала бы стек сквозь C-кадры HAL и
        // dispatch (UB), причём именно в том сценарии, который мы диагностируем.
        // Тут это обычная паника на обычном потоке.
        assert!(
            st.packets > 0,
            "колбэк IOProc не вызвался ни разу — записывать нечего, рецепт не подтверждён"
        );
        assert!(
            st.locate_error.is_none(),
            "тап не удалось опознать во входном ABL: {}. В ABL было {} буфер(ов) на {} кан.; \
             вход устройства вывода вносит {} кан., у тапа {} кан. Пропущено пакетов: {}. \
             Записанное в WAV неполно или пусто — галочку по этому прогону ставить нельзя. \
             Task 4 обязан разбирать этот случай, а не полагать, что тап занимает весь ABL.",
            st.locate_error.unwrap_or(""),
            st.abl_buffers,
            st.abl_channels,
            device_in_channels,
            tap_channels,
            st.skipped,
        );
        assert!(
            st.skipped == 0,
            "{} пакет(ов) из {} пропущено — раскладка ABL менялась по ходу прогона",
            st.skipped,
            st.packets,
        );
        println!(
            "раскладка подтверждена: тап = буфер(ы) [{}..{}) из {}",
            st.tap_start,
            st.tap_start + st.tap_len,
            st.abl_buffers,
        );
    }

    // --- мелкие помощники ------------------------------------------------------

    /// Адрес свойства в глобальной области, главный элемент — форма, в которой
    /// читаются все свойства этого спайка.
    fn addr(selector: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    fn default_output_device() -> AudioObjectID {
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
        );
        device
    }

    /// UID устройства. CFStringRef приходит с +1 (правило Copy), а CFString и
    /// NSString toll-free bridged — поэтому владение сразу забирает `Retained`.
    fn device_uid(device: AudioObjectID) -> Retained<NSString> {
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
        );
        unsafe { Retained::from_raw(uid) }.expect("устройство вернуло пустой UID")
    }

    /// Сколько буферов и каналов устройство вносит СВОИМ входом.
    ///
    /// `kAudioDevicePropertyStreamConfiguration` во входной области возвращает
    /// `AudioBufferList` переменной длины — размер сначала спрашивается отдельно.
    /// У устройства без входов (встроенные динамики) свойства может не быть
    /// вовсе: это не ошибка, а честный ноль.
    fn device_input_streams(device: AudioObjectID) -> (u32, u32) {
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
            return (0, 0);
        }
        // Через Vec<u64>, а не Vec<u8>: в AudioBufferList есть указатель, и читать
        // структуру с невыровненного адреса — UB. u64 даёт нужные 8 байт выравнивания.
        let mut raw = vec![0u64; size as usize / 8 + 1];
        check(
            unsafe {
                AudioObjectGetPropertyData(
                    device,
                    NonNull::from(&mut a),
                    0,
                    null(),
                    NonNull::from(&mut size),
                    NonNull::new(raw.as_mut_ptr().cast()).expect("Vec дал нулевой указатель"),
                )
            },
            "kAudioDevicePropertyStreamConfiguration (вход устройства вывода)",
        );
        let abl = unsafe { &*(raw.as_ptr() as *const AudioBufferList) };
        let count = abl.mNumberBuffers as usize;
        if count == 0 {
            return (0, 0);
        }
        let bufs = unsafe { std::slice::from_raw_parts(abl.mBuffers.as_ptr(), count) };
        (count as u32, bufs.iter().map(|b| b.mNumberChannels).sum())
    }

    fn tap_format(tap: AudioObjectID) -> AudioStreamBasicDescription {
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
        );
        asbd
    }

    /// Ключи словаря агрегата объявлены в SDK как C-строки — переводим в NSString.
    fn ns(key: &CStr) -> Retained<NSString> {
        NSString::from_str(key.to_str().expect("ключ Core Audio не UTF-8"))
    }

    /// Панику на ошибке спайк себе позволяет: и приватный агрегат, и процесс-тап
    /// принадлежат создавшему процессу и уничтожаются системой при его выходе,
    /// так что аварийное завершение ничего не оставляет в системе.
    fn check(status: i32, what: &str) {
        assert!(
            status == 0,
            "{what} упал: OSStatus {status} ({})",
            fourcc(status)
        );
    }

    /// OSStatus у Core Audio почти всегда четырёхсимвольный код вроде 'nope'.
    fn fourcc(code: i32) -> String {
        let bytes = (code as u32).to_be_bytes();
        if bytes.iter().all(|b| (0x20..=0x7e).contains(b)) {
            format!("'{}'", String::from_utf8_lossy(&bytes))
        } else {
            "не 4CC".to_string()
        }
    }
}
