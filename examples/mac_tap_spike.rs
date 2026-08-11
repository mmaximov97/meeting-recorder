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
        kAudioDevicePropertyDeviceUID, kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
        kAudioSubDeviceUIDKey, kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey,
        kAudioTapPropertyFormat, AudioDeviceCreateIOProcIDWithBlock, AudioDeviceDestroyIOProcID,
        AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
        AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
        AudioHardwareDestroyProcessTap, AudioObjectGetPropertyData, AudioObjectID,
        AudioObjectPropertyAddress, CATapDescription,
    };
    use objc2_core_audio_types::{
        kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, AudioBufferList,
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

    /// Состояние за одним мьютексом: ресемплер и накопитель.
    ///
    /// Ресемплер именно ОДИН на весь поток, а не новый на пакет: он stateful —
    /// хранит фазу и историю фильтра между вызовами (см. докблок `Resampler`),
    /// поэтому пересоздание на каждый пакет дало бы щелчок и прогрев на каждом шве.
    struct State {
        resampler: Resampler,
        samples: Vec<i16>,
        packets: u64,
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
        let tap_channels = asbd.mChannelsPerFrame as u16;
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
        let state = Arc::new(Mutex::new(State {
            resampler: Resampler::new(tap_rate, SAMPLE_RATE),
            samples: Vec::new(),
            packets: 0,
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

                let mono = if count == 1 {
                    let b = &buffers[0];
                    if b.mData.is_null() {
                        return;
                    }
                    let samples = unsafe {
                        std::slice::from_raw_parts(
                            b.mData as *const f32,
                            b.mDataByteSize as usize / 4,
                        )
                    };
                    downmix_to_mono_f32(samples, b.mNumberChannels.max(1) as u16)
                } else {
                    // Непереплетённый вариант: по буферу на канал. Переплетаем в
                    // скрэтч, чтобы свести тем же downmix_to_mono_f32, а не своим.
                    let frames = buffers[0].mDataByteSize as usize / 4;
                    let mut inter = vec![0.0f32; frames * count];
                    for (ch, b) in buffers.iter().enumerate() {
                        if b.mData.is_null() {
                            continue;
                        }
                        let src =
                            unsafe { std::slice::from_raw_parts(b.mData as *const f32, frames) };
                        for (i, &s) in src.iter().enumerate() {
                            inter[i * count + ch] = s;
                        }
                    }
                    downmix_to_mono_f32(&inter, count as u16)
                };

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
        println!("Играй что-нибудь на {RECORD_SECS} секунд — Spotify, YouTube, что угодно...");
        check(
            unsafe { AudioDeviceStart(agg_id, io_proc_id) },
            "AudioDeviceStart",
        );
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
                "ТИШИНА. Сверься с двумя ловушками: тап не должен быть главным \
                 саб-девайсом агрегата, и isExclusive нельзя трогать руками."
            );
        }

        let mut sink = WavSink::create(Path::new("."), "mac_tap_spike.wav").expect("создать WAV");
        sink.write(&st.samples).expect("записать сэмплы");
        let path = sink.finalize().expect("закрыть WAV");
        println!(
            "Записано в {} — прослушай и подтверди, что там системный звук",
            path.display()
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
