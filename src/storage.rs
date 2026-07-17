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

/// `Zoom.exe` → `zoom`, `My App v2.exe` → `my-app-v2`
fn sanitize_source(source: &str) -> String {
    let stem = source.strip_suffix(".exe").unwrap_or(source);
    let cleaned: String = stem
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    // схлопнуть повторы дефисов и обрезать по краям
    let mut out = String::with_capacity(cleaned.len());
    for c in cleaned.chars() {
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    out.trim_matches('-').to_string()
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
}
