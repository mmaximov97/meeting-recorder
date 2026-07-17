use chrono::{DateTime, Local};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    Mic,
    System,
}

impl Track {
    fn suffix(self) -> &'static str {
        match self {
            Track::Mic => "mic",
            Track::System => "system",
        }
    }
}

pub const SAMPLE_RATE: u32 = 16_000;

/// `2026-07-17_14-30_zoom.mic.wav`
pub fn recording_filename(started: DateTime<Local>, source: &str, track: Track) -> String {
    format!(
        "{}_{}.{}.wav",
        started.format("%Y-%m-%d_%H-%M"),
        sanitize_source(source),
        track.suffix()
    )
}

/// Максимум для очищенного имени источника в имени файла.
///
/// Полный файл — `{дата}_{время}_{источник}.{дорожка}.wav`. Дата+время+разделитель
/// занимают 17 символов (`2026-07-17_14-30_`), самый длинный суффикс дорожки с
/// расширением — `.system.wav` (11 символов). Итого служебная часть — 28 символов.
/// Предел компонента пути на NTFS — 255 символов (UTF-16 code units), так что запас
/// огромный в любом случае; 100 символов — не попытка впритык влезть в лимит NTFS, а
/// разумная граница для имени процесса как такового (реальные имена процессов даже с
/// длинными кириллическими названиями вроде «Яндекс.Телемост» на порядок короче), при
/// этом с большим запасом на конкатенацию нескольких слов через дефис.
const MAX_SOURCE_LEN: usize = 100;

/// `Zoom.exe` → `zoom`, `My App v2.exe` → `my-app-v2`,
/// `Яндекс.Телемост.exe` → `яндекс-телемост`. Юникодные буквы/цифры сохраняются —
/// имена процессов на кириллице (например, у «Яндекс.Телемост») реальны и не должны
/// схлопываться в пустоту. Если после очистки ничего не осталось (пустая строка,
/// строка из одних разделителей, `.exe` без имени) — возвращается `unknown`, чтобы
/// источник в имени файла не терялся молча.
fn sanitize_source(source: &str) -> String {
    let stem = source.strip_suffix(".exe").unwrap_or(source);
    let cleaned: String = stem
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    // схлопнуть повторы дефисов и обрезать по краям
    let mut out = String::with_capacity(cleaned.len());
    for c in cleaned.chars() {
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    let out = out.trim_matches('-');
    // обрезаем по границе символа, а не байта — источник может быть кириллицей
    let truncated: String = out.chars().take(MAX_SOURCE_LEN).collect();
    let truncated = truncated.trim_end_matches('-');
    if truncated.is_empty() {
        "unknown".to_string()
    } else {
        truncated.to_string()
    }
}

/// Инкрементальная запись 16 кГц моно 16 бит WAV.
pub struct WavSink {
    writer: hound::WavWriter<std::io::BufWriter<std::fs::File>>,
    path: PathBuf,
}

impl WavSink {
    pub fn create(dir: &Path, filename: &str) -> Result<Self, hound::Error> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(filename);
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::create(&path, spec)?;
        Ok(Self { writer, path })
    }

    pub fn write(&mut self, samples: &[i16]) -> Result<(), hound::Error> {
        for &s in samples {
            self.writer.write_sample(s)?;
        }
        Ok(())
    }

    /// Дописывает WAV-заголовок с реальной длиной. Без этого файл битый.
    pub fn finalize(self) -> Result<PathBuf, hound::Error> {
        self.writer.finalize()?;
        Ok(self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn момент() -> chrono::DateTime<chrono::Local> {
        chrono::Local.with_ymd_and_hms(2026, 7, 17, 14, 30, 0).unwrap()
    }

    #[test]
    fn имя_для_дорожки_микрофона() {
        assert_eq!(
            recording_filename(момент(), "zoom", Track::Mic),
            "2026-07-17_14-30_zoom.mic.wav"
        );
    }

    #[test]
    fn имя_для_системной_дорожки() {
        assert_eq!(
            recording_filename(момент(), "zoom", Track::System),
            "2026-07-17_14-30_zoom.system.wav"
        );
    }

    #[test]
    fn ручная_запись_помечается_как_manual() {
        assert_eq!(
            recording_filename(момент(), "manual", Track::Mic),
            "2026-07-17_14-30_manual.mic.wav"
        );
    }

    #[test]
    fn имя_процесса_чистится_от_exe_и_регистра() {
        assert_eq!(
            recording_filename(момент(), "Zoom.exe", Track::Mic),
            "2026-07-17_14-30_zoom.mic.wav"
        );
    }

    #[test]
    fn небезопасные_символы_в_имени_процесса_заменяются() {
        assert_eq!(
            recording_filename(момент(), "My App v2.exe", Track::Mic),
            "2026-07-17_14-30_my-app-v2.mic.wav"
        );
    }

    #[test]
    fn кириллическое_имя_процесса_не_схлопывается_в_пустоту() {
        assert_eq!(
            recording_filename(момент(), "Яндекс.Телемост.exe", Track::Mic),
            "2026-07-17_14-30_яндекс-телемост.mic.wav"
        );
    }

    #[test]
    fn один_только_exe_даёт_unknown() {
        assert_eq!(
            recording_filename(момент(), ".exe", Track::Mic),
            "2026-07-17_14-30_unknown.mic.wav"
        );
    }

    #[test]
    fn источник_из_одних_дефисов_даёт_unknown() {
        assert_eq!(
            recording_filename(момент(), "---", Track::Mic),
            "2026-07-17_14-30_unknown.mic.wav"
        );
    }

    #[test]
    fn пустой_источник_даёт_unknown() {
        assert_eq!(
            recording_filename(момент(), "", Track::Mic),
            "2026-07-17_14-30_unknown.mic.wav"
        );
    }

    #[test]
    fn длинный_источник_обрезается_по_лимиту_без_висящего_дефиса() {
        // 99 'a' + разделитель, который попадает ровно на границу обрезки —
        // после take(MAX_SOURCE_LEN) висящий дефис должен быть срезан отдельно.
        let long_source = format!("{}.{}", "a".repeat(99), "b".repeat(50));
        let result = sanitize_source(&long_source);
        assert_eq!(result, "a".repeat(99));
        assert!(result.len() <= MAX_SOURCE_LEN);
    }

    #[test]
    fn источник_ровно_на_лимите_не_обрезается() {
        let source = "a".repeat(MAX_SOURCE_LEN);
        assert_eq!(sanitize_source(&source), source);
    }

    /// Уникальный временный каталог без внешних зависимостей (без `tempfile`/`rand`):
    /// счётчик + pid процесса + наносекунды с эпохи. Каталог удаляется в Drop, даже
    /// если тест упал — не оставляем мусор в `%TEMP%`.
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
            let path = std::env::temp_dir().join(format!(
                "meeting-recorder-test-{tag}-{pid}-{nanos}-{n}"
            ));
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

    #[test]
    fn wav_sink_round_trip_читает_записанные_сэмплы() {
        let dir = ScratchDir::new("round-trip");
        let samples: Vec<i16> = vec![0, 1, -1, i16::MAX, i16::MIN, 12345, -12345];

        let mut sink = WavSink::create(&dir, "test.wav").expect("создание WavSink");
        sink.write(&samples).expect("запись сэмплов");
        let path = sink.finalize().expect("финализация WavSink");

        let mut reader = hound::WavReader::open(&path).expect("открыть WAV для чтения");
        let spec = reader.spec();
        assert_eq!(spec.sample_rate, SAMPLE_RATE);
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, hound::SampleFormat::Int);

        let read_samples: Vec<i16> = reader
            .samples::<i16>()
            .collect::<Result<_, _>>()
            .expect("прочитать сэмплы");
        assert_eq!(read_samples, samples);
    }

    #[test]
    fn wav_sink_несколько_write_дают_непрерывный_поток_сэмплов() {
        let dir = ScratchDir::new("chunked-writes");
        let chunk1: Vec<i16> = (0..100).collect();
        let chunk2: Vec<i16> = (100..250).collect();
        let chunk3: Vec<i16> = vec![-1, -2, -3];

        let mut sink = WavSink::create(&dir, "chunked.wav").expect("создание WavSink");
        sink.write(&chunk1).expect("запись чанка 1");
        sink.write(&chunk2).expect("запись чанка 2");
        sink.write(&chunk3).expect("запись чанка 3");
        let path = sink.finalize().expect("финализация WavSink");

        let mut reader = hound::WavReader::open(&path).expect("открыть WAV для чтения");
        let read_samples: Vec<i16> = reader
            .samples::<i16>()
            .collect::<Result<_, _>>()
            .expect("прочитать сэмплы");

        let mut expected = Vec::new();
        expected.extend_from_slice(&chunk1);
        expected.extend_from_slice(&chunk2);
        expected.extend_from_slice(&chunk3);
        assert_eq!(read_samples, expected);
    }

    #[test]
    fn wav_sink_дропнутый_без_finalize_всё_равно_читается() {
        // hound::WavWriter::drop() сам вызывает update_header(), если finalize()
        // не был вызван явно — подстраховка на случай раннего return без явного
        // finalize(). Тест фиксирует это поведение текущей версии hound: если
        // зависимость обновится и подстраховка пропадёт, тест упадёт.
        //
        // Важно: это НЕ защищает от жёсткого краша процесса (SIGKILL/паника с
        // abort/отключение питания) — там Drop вообще не выполняется, и в
        // заголовке останется нулевая длина данных, хотя сами сэмплы на диске
        // будут. Это и есть реальный необнаружаемый юнит-тестом риск,
        // описанный в ревью: файл существует, весит норм, но играет тишину.
        let dir = ScratchDir::new("drop-without-finalize");
        let samples: Vec<i16> = vec![10, -10, 20, -20];
        let path = dir.join("dropped.wav");
        {
            let mut sink = WavSink::create(&dir, "dropped.wav").expect("создание WavSink");
            sink.write(&samples).expect("запись сэмплов");
            // sink роняется в конце блока без явного finalize()
        }

        let mut reader = hound::WavReader::open(&path)
            .expect("файл должен читаться как валидный WAV даже без явного finalize()");
        let read_samples: Vec<i16> = reader
            .samples::<i16>()
            .collect::<Result<_, _>>()
            .expect("прочитать сэмплы");
        assert_eq!(read_samples, samples);
    }
}
