//! Сравнение громкости дорожек готовой записи.
//!
//! Существует потому, что выбор устройства не доказывает, что устройство слышит
//! владельца. Отказ здесь маскируется под успех: файл есть, весит сколько
//! положено, открывается и играет — просто голоса в нём почти нет, и обнаружится
//! это только на транскрипте, через день.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// Пол уровня. Всё, что тише, считается этим значением.
///
/// Пол, а не `-inf`, чтобы арифметика сравнения оставалась обычной: с
/// бесконечностями пришлось бы отдельно разбирать случай «мик молчал совсем»,
/// который как раз и есть самый важный.
pub const FLOOR_DBFS: f32 = -120.0;

/// Насколько mic должна быть тише system, чтобы это считалось проблемой.
///
/// 20 дБ выбраны между нормальной разницей близких уровней (до ~10 дБ) и
/// реальным отказом: у записи 30.07 расхождение по RMS было 36 дБ. По пикам оно
/// составляло всего 16 дБ — поэтому здесь именно RMS, пиковый порог этот случай
/// не поймал бы.
pub const THRESHOLD_DB: f32 = 20.0;

/// Сколько сэмплов в одном окне выборки.
const WINDOW: usize = 4096;

/// RMS файла в dBFS по разреженной выборке.
///
/// Читается `windows` окон по [`WINDOW`] сэмплов, равномерно по файлу: для
/// 38-минутной записи это ~1.6 МБ вместо 73 МБ. Файл короче суммарной выборки
/// читается целиком — выборка вырождается в полный проход, а не в ошибку.
pub fn sampled_rms_dbfs(path: &Path, windows: usize) -> Result<f32, hound::Error> {
    let mut reader = hound::WavReader::open(path)?;
    let total = reader.len() as usize;
    if total == 0 {
        return Ok(FLOOR_DBFS);
    }

    let mut sum_sq = 0f64;
    let mut counted = 0usize;
    let mut accumulate = |reader: &mut hound::WavReader<std::io::BufReader<std::fs::File>>,
                          take: usize,
                          sum_sq: &mut f64,
                          counted: &mut usize|
     -> Result<(), hound::Error> {
        for s in reader.samples::<i16>().take(take) {
            let v = s? as f64;
            *sum_sq += v * v;
            *counted += 1;
        }
        Ok(())
    };

    if windows == 0 || total <= windows * WINDOW {
        accumulate(&mut reader, total, &mut sum_sq, &mut counted)?;
    } else {
        let stride = total / windows;
        for i in 0..windows {
            reader.seek((i * stride) as u32)?;
            accumulate(&mut reader, WINDOW, &mut sum_sq, &mut counted)?;
        }
    }

    if counted == 0 {
        return Ok(FLOOR_DBFS);
    }
    let rms = (sum_sq / counted as f64).sqrt() / i16::MAX as f64;
    if rms <= 0.0 {
        return Ok(FLOOR_DBFS);
    }
    Ok((20.0 * rms.log10()).max(FLOOR_DBFS as f64) as f32)
}

/// Насколько mic тише system, если это тянет на проблему.
///
/// `None` — либо разница в пределах нормы, либо mic громче. Обратный перекос не
/// помечается: система тише микрофона — это просто тихий собеседник, а не
/// сломанная запись.
pub fn imbalance(mic_dbfs: f32, sys_dbfs: f32) -> Option<f32> {
    let diff = sys_dbfs - mic_dbfs;
    (diff > THRESHOLD_DB).then_some(diff)
}

/// Кеш посчитанных уровней.
///
/// Ключ включает размер и время модификации, поэтому заменённый или дописанный
/// файл пересчитывается сам. Без кеша список, обновляющийся на каждый фокус
/// окна, перечитывал бы диск заново.
#[derive(Default)]
pub struct Cache(Mutex<HashMap<(PathBuf, u64, SystemTime), f32>>);

impl Cache {
    /// `None` — файла нет или он не читается как WAV. Это не ошибка: пометки
    /// просто не будет.
    pub fn rms(&self, path: &Path) -> Option<f32> {
        let meta = std::fs::metadata(path).ok()?;
        let key = (path.to_path_buf(), meta.len(), meta.modified().ok()?);
        if let Ok(g) = self.0.lock() {
            if let Some(v) = g.get(&key) {
                return Some(*v);
            }
        }
        let v = sampled_rms_dbfs(path, 200).ok()?;
        if let Ok(mut g) = self.0.lock() {
            g.insert(key, v);
        }
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ровные_дорожки_не_дают_пометки() {
        assert_eq!(imbalance(-24.0, -24.0), None);
        assert_eq!(imbalance(-30.0, -24.0), None, "6 дБ — обычная разница");
        assert_eq!(imbalance(-43.9, -24.0), None, "19.9 дБ — ещё под порогом");
    }

    #[test]
    fn микрофон_тише_на_порог_даёт_пометку() {
        let diff = imbalance(-60.0, -24.0).expect("36 дБ — это пометка");
        assert!((diff - 36.0).abs() < 0.01, "разница: {diff}");
    }

    /// Ровно порог не считается: сравнение строгое, чтобы граничное значение
    /// не мигало пометкой туда-сюда между пересчётами.
    #[test]
    fn ровно_порог_не_считается() {
        assert_eq!(imbalance(-44.0, -24.0), None);
    }

    /// Обе дорожки в тишине — сравнивать нечего, предупреждать не о чем.
    #[test]
    fn тишина_в_обеих_дорожках_не_даёт_ложной_пометки() {
        assert_eq!(imbalance(FLOOR_DBFS, FLOOR_DBFS), None);
        assert_eq!(imbalance(-118.0, -119.0), None);
    }

    /// Полностью молчащий микрофон при говорящей системе — худший случай,
    /// и он обязан ловиться, а не отбрасываться как «нечего сравнивать».
    #[test]
    fn молчащий_микрофон_при_говорящей_системе_ловится() {
        assert!(imbalance(FLOOR_DBFS, -24.0).is_some());
    }

    /// Микрофон громче системы — не наша проблема: пометка только про «тише».
    #[test]
    fn микрофон_громче_системы_не_помечается() {
        assert_eq!(imbalance(-20.0, FLOOR_DBFS), None);
        assert_eq!(imbalance(-10.0, -60.0), None);
    }

    #[test]
    fn rms_синуса_половинной_амплитуды_около_минус_девяти_дб() {
        let dir = std::env::temp_dir().join(format!("mr-imb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sine.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..16_000 {
            let v = (i as f32 / 16_000.0 * 440.0 * std::f32::consts::TAU).sin() * 0.5;
            w.write_sample((v * i16::MAX as f32) as i16).unwrap();
        }
        w.finalize().unwrap();

        // RMS синуса = амплитуда / sqrt(2) = 0.3536 → 20*log10(0.3536) ≈ -9.03 дБ
        let db = sampled_rms_dbfs(&path, 200).unwrap();
        assert!((db + 9.03).abs() < 0.5, "получили {db} дБ");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn файл_короче_выборки_читается_целиком_а_не_падает() {
        let dir = std::env::temp_dir().join(format!("mr-imb-short-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("short.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..100 {
            w.write_sample(i16::MAX / 2).unwrap();
        }
        w.finalize().unwrap();

        let db = sampled_rms_dbfs(&path, 200).unwrap();
        assert!((db + 6.02).abs() < 0.5, "получили {db} дБ");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn пустой_файл_даёт_пол_а_не_ошибку() {
        let dir = std::env::temp_dir().join(format!("mr-imb-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        hound::WavWriter::create(&path, spec).unwrap().finalize().unwrap();

        assert_eq!(sampled_rms_dbfs(&path, 200).unwrap(), FLOOR_DBFS);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
