//! Пометка «ваш голос почти не записался».
//!
//! Существует потому, что выбор устройства не доказывает, что устройство слышит
//! владельца. Отказ здесь маскируется под успех: файл есть, весит сколько
//! положено, открывается и играет — просто голоса в нём почти нет, и обнаружится
//! это только на транскрипте, через день.
//!
//! Вопрос, на который отвечает пометка, — «был ли владелец в записи», а не
//! «громко ли он звучал». Поэтому меряем не громкость, а **сколько секунд речи**
//! на дорожке микрофона, и, если расшифровка уже есть, **сколько слов** в ней у
//! владельца. Пометка горит, только когда речи почти нет совсем.
//!
//! Прежнее правило (p99 громкости окон тише −45 dBFS) срабатывало на тихих, но
//! исправных микрофонах: у Ани речь лежит около −43 dBFS, и на встрече вчетвером,
//! где она говорит четверть времени, громкие 1% окон подбирались к порогу.
//! Доля участия в разговоре на несколько человек — не повод для пометки.
//!
//! Смотрим только дорожку микрофона, без сравнения с собеседниками: разница с
//! системной дорожкой стояла на пороге и мигала от записи к записи.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// Пол уровня. Всё, что тише, считается этим значением.
///
/// Пол, а не `-inf`, чтобы арифметика оставалась обычной: цифровой ноль
/// выключенного микрофона — как раз самый важный случай.
pub const FLOOR_DBFS: f32 = -120.0;

/// Насколько окно должно быть громче собственного шума дорожки, чтобы считаться
/// речью, дБ.
///
/// Шум дорожки — 10-й перцентиль окон: в любой встрече владелец молчит больше
/// десятой доли времени. Стационарный шум (вентилятор, шипение входа) гуляет
/// от окна к окну на 1–2 дБ, речь поднимается над ним на 15–25 дБ (у Ани
/// 08.09: шум −69, речь p90 −44). 10 дБ отделяют одно от другого с запасом.
pub const SPEECH_ABOVE_NOISE_DB: f32 = 10.0;

/// Нижняя граница порога речи, dBFS. Когда в паузах цифровой ноль (шум −120),
/// «шум + 10 дБ» ловил бы дизеринг и щелчки — их речью не считаем.
pub const SPEECH_MIN_DBFS: f32 = -60.0;

/// Верхняя граница порога речи, dBFS. Человек, который говорит почти без пауз
/// (монолог, доклад), поднимает 10-й перцентиль до уровня своей же речи, и
/// «шум + 10 дБ» оказался бы выше его голоса. Всё, что громче −45, — точно не
/// шум входа.
pub const SPEECH_MAX_DBFS: f32 = -45.0;

/// Меньше стольких секунд речи на микрофоне за всю запись — пометка.
///
/// 30 с — это несколько реплик. Кто сказал на созвоне больше, был в записи,
/// даже если говорил мало и тихо.
pub const MIN_SPEECH_SEC: f32 = 30.0;

/// Для коротких записей порог — эта доля длительности: на минутной пробе
/// 30 с речи требовать нельзя.
pub const MIN_SPEECH_SHARE: f32 = 0.1;

/// Меньше стольких слов у владельца в расшифровке — пометка («расшифровка
/// пустая или неразборчивая»). Несколько коротких «да», «угу» и галлюцинаций
/// распознавателя на тишине («Продолжение следует…») в сумме сюда помещаются,
/// настоящее участие в разговоре — нет.
pub const MIN_OWNER_WORDS: usize = 15;

/// Короче этого (с) слова в расшифровке не проверяем: короткая проба
/// законно может обойтись парой слов. Для неё остаётся проверка по звуку.
pub const WORDS_CHECK_FROM_SEC: f32 = 300.0;

/// Длина окна RMS, мс.
const WINDOW_MS: u32 = 100;

/// Сколько окон берём из длинного файла (равномерно по файлу). 2000 окон по
/// 100 мс — это 200 с звука, не нужно читать весь файл (38-минутная запись —
/// 73 МБ). Секунды речи оцениваются как доля речевых окон × длительность;
/// ошибка выборки на часовой записи около ±8 с у порога в 30 с — пометку это
/// не переворачивает. Файл короче читается целиком.
const WINDOWS: u32 = 2000;

/// Что известно о дорожке микрофона.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MicStats {
    /// Длительность дорожки, с.
    pub duration_sec: f32,
    /// Сколько секунд из них — речь (окна выше порога речи).
    pub speech_sec: f32,
    /// Шум дорожки: 10-й перцентиль окон, dBFS. Для отладки и ручного прогона.
    pub noise_dbfs: f32,
}

/// Порог речи для дорожки с таким шумом, dBFS.
pub fn speech_threshold(noise_dbfs: f32) -> f32 {
    (noise_dbfs + SPEECH_ABOVE_NOISE_DB).clamp(SPEECH_MIN_DBFS, SPEECH_MAX_DBFS)
}

/// Сводка по уровням окон, из которых выброшена тишина выключенного микрофона.
///
/// Выключенный микрофон пишет цифровой ноль (см. `App::set_mic_muted`), его
/// окна лежат ровно на полу ([`FLOOR_DBFS`]). Без поправки они шли бы в оценку
/// как «речи нет»: выключил на большую часть встречи — получил «ваш голос почти
/// не записался» за то, что сам и попросил. Поэтому такие окна отбрасываются, а
/// длительность сжимается до включённой доли — порог речи считается от неё.
///
/// `None` — включённых окон не осталось: микрофон был выключен всю запись,
/// судить не о чем. Живой тракт ровного цифрового нуля на 100 мс не даёт, так
/// что шумный, но включённый кусок остаётся в оценке.
fn stats_without_muted(levels: &[f32], duration_sec: f32) -> Option<MicStats> {
    let live: Vec<f32> = levels.iter().copied().filter(|l| *l > FLOOR_DBFS).collect();
    if live.is_empty() {
        return None;
    }
    let live_sec = duration_sec * live.len() as f32 / levels.len() as f32;
    Some(stats_from_levels(&live, live_sec))
}

/// Сводка по уже посчитанным уровням окон.
fn stats_from_levels(levels: &[f32], duration_sec: f32) -> MicStats {
    if levels.is_empty() {
        return MicStats { duration_sec, speech_sec: 0.0, noise_dbfs: FLOOR_DBFS };
    }
    let noise = percentile(levels, 0.10);
    let thr = speech_threshold(noise);
    let speech = levels.iter().filter(|&&l| l > thr).count();
    MicStats {
        duration_sec,
        speech_sec: speech as f32 / levels.len() as f32 * duration_sec,
        noise_dbfs: noise,
    }
}

/// Уровни окон по 100 мс дорожки микрофона и сводка по ним.
///
/// Пустой (0 сэмплов) и нечитаемый файл — ошибка (пометки тогда нет).
pub fn mic_stats(path: &Path) -> Result<MicStats, hound::Error> {
    let (levels, duration_sec) = mic_levels(path)?;
    Ok(stats_from_levels(&levels, duration_sec))
}

/// То же для записи, в которой выключали микрофон: тишина выключения в оценку
/// не идёт (см. [`stats_without_muted`]). `Ok(None)` — микрофон был выключен
/// всю запись.
pub fn mic_stats_without_muted(path: &Path) -> Result<Option<MicStats>, hound::Error> {
    let (levels, duration_sec) = mic_levels(path)?;
    Ok(stats_without_muted(&levels, duration_sec))
}

/// Уровни окон по 100 мс (равномерная выборка у длинного файла) и длительность, с.
fn mic_levels(path: &Path) -> Result<(Vec<f32>, f32), hound::Error> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let channels = spec.channels.max(1) as usize;
    let win = ((spec.sample_rate as u64 * WINDOW_MS as u64 / 1000) as usize).max(1);
    // Окно считаем в сэмплах файла (все каналы подряд), чтобы длина в мс не зависела от каналов.
    let win = win * channels;
    let total = reader.len() as usize;
    if total == 0 {
        // Ни одного сэмпла (пустой файл или нулевая длина в заголовке
        // оборвавшейся записи) — данных нет, это не тишина: пометки не будет.
        return Err(hound::Error::FormatError("no samples"));
    }
    let duration_sec = (total / channels) as f32 / spec.sample_rate.max(1) as f32;
    let full = total / win;
    if full == 0 {
        // Короче одного окна: считаем как одно.
        return Ok((vec![window_dbfs(&mut reader, total)?], duration_sec));
    }

    let mut levels = Vec::with_capacity(full.min(WINDOWS as usize));
    if full <= WINDOWS as usize {
        for _ in 0..full {
            levels.push(window_dbfs(&mut reader, win)?);
        }
    } else {
        let stride = total / WINDOWS as usize;
        for i in 0..WINDOWS as usize {
            // seek у hound — в кадрах (сэмпл на канал), не в сэмплах.
            reader.seek(((i * stride) / channels) as u32)?;
            levels.push(window_dbfs(&mut reader, win)?);
        }
    }
    Ok((levels, duration_sec))
}

/// RMS следующих `take` сэмплов в dBFS (с полом).
fn window_dbfs(
    reader: &mut hound::WavReader<std::io::BufReader<std::fs::File>>,
    take: usize,
) -> Result<f32, hound::Error> {
    let mut sum_sq = 0f64;
    let mut counted = 0usize;
    for s in reader.samples::<i16>().take(take) {
        let v = s? as f64;
        sum_sq += v * v;
        counted += 1;
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

/// Перцентиль `q` (0..1) методом ближайшего ранга. Пустой список — пол.
fn percentile(levels: &[f32], q: f64) -> f32 {
    if levels.is_empty() {
        return FLOOR_DBFS;
    }
    let mut sorted = levels.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let rank = (sorted.len() as f64 * q).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Речи на микрофоне почти нет: меньше [`MIN_SPEECH_SEC`] (а у короткой записи —
/// меньше [`MIN_SPEECH_SHARE`] длительности).
///
/// Сравнение строгое: ровно порог не считается, чтобы граничное значение не
/// мигало между пересчётами.
pub fn too_little_speech(s: &MicStats) -> bool {
    let need = MIN_SPEECH_SEC.min(s.duration_sec * MIN_SPEECH_SHARE);
    s.speech_sec < need
}

/// В расшифровке у владельца почти нет слов. `None` про слова (расшифровки нет
/// или дорожка владельца не расшифровалась) — не повод для пометки.
pub fn too_few_words(owner_words: Option<usize>, duration_sec: f32) -> bool {
    duration_sec >= WORDS_CHECK_FROM_SEC && owner_words.is_some_and(|w| w < MIN_OWNER_WORDS)
}

/// Итоговое правило пометки.
pub fn quiet_mic(s: &MicStats, owner_words: Option<usize>) -> bool {
    too_little_speech(s) || too_few_words(owner_words, s.duration_sec)
}

/// Подпись дорожки владельца в `.md` расшифровки — та же, что пишет
/// `transcribe::merge_markdown` (`Label::Owner`).
const OWNER: &str = "Владелец";
/// Начало хвостового блока дорожки без привязки ко времени (см.
/// `transcribe::tail_block`).
const TAIL_SUFFIX: &str = " (без привязки ко времени";

/// Сколько слов у владельца в `.md` расшифровки.
///
/// `None` — дорожка владельца не расшифровалась (`run_transcription` пишет об
/// этом первой строкой): о речи владельца расшифровка тогда ничего не говорит.
/// «Слово» — кусок между пробелами, в котором есть хоть одна буква или цифра,
/// чтобы «…» и «—» не считались.
pub fn owner_words(md: &str) -> Option<usize> {
    if md.lines().any(|l| l.trim_start().starts_with("_Дорожка владельца не транскрибирована")) {
        return None;
    }
    let words = |t: &str| t.split_whitespace().filter(|w| w.chars().any(char::is_alphanumeric)).count();
    let mut n = 0;
    // Внутри хвостового блока владельца: весь его текст идёт следующими
    // абзацами до хвоста собеседников.
    let mut in_owner_tail = false;
    for block in md.split("\n\n") {
        let b = block.trim();
        if let Some(label) = b.strip_prefix('_').and_then(|r| r.strip_suffix('_')) {
            if label.contains(TAIL_SUFFIX) {
                in_owner_tail = label.starts_with(OWNER);
                continue;
            }
        }
        if let Some(rest) = b.strip_prefix("**[") {
            in_owner_tail = false;
            // `**[mm:ss]** _Метка_ текст`
            let Some((_, after)) = rest.split_once("]** _") else { continue };
            let Some((label, text)) = after.split_once("_ ") else { continue };
            if label.starts_with(OWNER) {
                n += words(text);
            }
            continue;
        }
        if in_owner_tail {
            n += words(b);
        }
    }
    Some(n)
}

/// Ключ кеша: файл, размер, время изменения — заменённый или дописанный файл
/// пересчитывается сам.
type Key = (PathBuf, u64, SystemTime);

fn key(path: &Path) -> Option<Key> {
    let meta = std::fs::metadata(path).ok()?;
    Some((path.to_path_buf(), meta.len(), meta.modified().ok()?))
}

/// Кеш посчитанных сводок микрофона и слов владельца.
///
/// Без кеша список, обновляющийся на каждый фокус окна, перечитывал бы диск
/// заново.
#[derive(Default)]
pub struct Cache {
    /// Ключ — файл и «выключали ли микрофон»: от этого зависит, идут ли
    /// окна цифровой тишины в оценку. `None` — микрофон был выключен всю запись.
    mic: Mutex<HashMap<(Key, bool), Option<MicStats>>>,
    words: Mutex<HashMap<Key, Option<usize>>>,
}

impl Cache {
    /// Ставить ли пометку «ваш голос почти не записался».
    ///
    /// `transcript_md` — `.md` расшифровки, если она есть. Файл микрофона,
    /// которого нет или который не читается как WAV, — не ошибка: пометки
    /// просто не будет. Нечитаемая расшифровка — как будто её нет.
    ///
    /// `mic_was_muted` — в записи выключали микрофон: тишина выключения в
    /// оценку не идёт, а выключенный всю запись микрофон пометки не даёт.
    pub fn quiet_mic(&self, mic: &Path, transcript_md: Option<&Path>, mic_was_muted: bool) -> bool {
        let Some(stats) = self.mic_stats(mic, mic_was_muted) else { return false };
        let words = transcript_md.and_then(|md| self.owner_words(md));
        quiet_mic(&stats, words)
    }

    fn mic_stats(&self, path: &Path, mic_was_muted: bool) -> Option<MicStats> {
        let k = (key(path)?, mic_was_muted);
        if let Some(v) = self.mic.lock().ok().and_then(|g| g.get(&k).copied()) {
            return v;
        }
        let v = if mic_was_muted {
            mic_stats_without_muted(path).ok()?
        } else {
            Some(mic_stats(path).ok()?)
        };
        if let Ok(mut g) = self.mic.lock() {
            g.insert(k, v);
        }
        v
    }

    fn owner_words(&self, md: &Path) -> Option<usize> {
        let k = key(md)?;
        if let Some(v) = self.words.lock().ok().and_then(|g| g.get(&k).copied()) {
            return v;
        }
        let v = owner_words(&std::fs::read_to_string(md).ok()?);
        if let Ok(mut g) = self.words.lock() {
            g.insert(k, v);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 16_000;

    /// Пишет моно-WAV: синус 440 Гц с амплитудой `amp(t_sec)` (0..1) плюс
    /// детерминированный «белый» шум с RMS `noise_db` dBFS (`None` — без шума).
    fn write_wav(tag: &str, seconds: usize, noise_db: Option<f64>, amp: impl Fn(f64) -> f64) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mr-quiet-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mic.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        // Равномерный шум на [-1, 1] имеет RMS 1/√3.
        let noise_amp = noise_db.map_or(0.0, |db| 10f64.powf(db / 20.0) * 3f64.sqrt());
        let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..seconds * RATE as usize {
            let t = i as f64 / RATE as f64;
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let u = (rng >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0;
            let v = (t * 440.0 * std::f64::consts::TAU).sin() * amp(t) + u * noise_amp;
            w.write_sample((v * i16::MAX as f64) as i16).unwrap();
        }
        w.finalize().unwrap();
        (dir, path)
    }

    /// Амплитуда синуса, у которого RMS равен `db` dBFS.
    fn amp_for_rms(db: f64) -> f64 {
        10f64.powf(db / 20.0) * std::f64::consts::SQRT_2
    }

    fn stats(path: &Path) -> MicStats {
        mic_stats(path).unwrap()
    }

    /// Выключенный микрофон, который пишет цифровой ноль.
    #[test]
    fn полная_тишина_даёт_пометку() {
        let (dir, path) = write_wav("silence", 60, None, |_| 0.0);
        let s = stats(&path);
        assert_eq!(s.speech_sec, 0.0);
        assert!(quiet_mic(&s, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Выключенный или чужой микрофон, который пишет живую шумовую полку, как
    /// рекордер Ани: −50 dBFS шума, речи нет. Шум не должен сойти за речь.
    #[test]
    fn один_шум_без_речи_даёт_пометку() {
        let (dir, path) = write_wav("noise", 120, Some(-50.0), |_| 0.0);
        let s = stats(&path);
        assert!(s.speech_sec < 1.0, "{s:?}");
        assert!(quiet_mic(&s, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Сам баг: встреча вчетвером, владелец говорит четверть времени тихим
    /// голосом (−43 dBFS, как у Ани) поверх шума −66. Прежнее правило по p99
    /// громкости ставило бы пометку на чуть более тихом микрофоне; теперь —
    /// 45 с речи из 3 минут, пометки нет.
    #[test]
    fn тихий_голос_четверть_времени_без_пометки() {
        let a = amp_for_rms(-43.0);
        let (dir, path) = write_wav("quarter", 180, Some(-66.0), |t| if (t as u64) % 4 == 0 { a } else { 0.0 });
        let s = stats(&path);
        assert!((s.speech_sec - 45.0).abs() < 3.0, "{s:?}");
        assert!(!quiet_mic(&s, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Совсем тихий голос (−52 dBFS, тише старого порога −45) над шумом −70:
    /// голос в записи есть — пометки нет.
    #[test]
    fn голос_тише_старого_порога_без_пометки() {
        let a = amp_for_rms(-52.0);
        let (dir, path) = write_wav("very-quiet", 120, Some(-70.0), |t| if (t as u64) % 2 == 0 { a } else { 0.0 });
        let s = stats(&path);
        assert!(s.speech_sec > 50.0, "{s:?}");
        assert!(!quiet_mic(&s, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Пара реплик на часовой встрече — 20 с речи: это и есть «почти не
    /// записался».
    #[test]
    fn двадцать_секунд_речи_за_длинную_запись_дают_пометку() {
        let a = amp_for_rms(-30.0);
        let (dir, path) = write_wav("few", 600, Some(-65.0), |t| if (100.0..120.0).contains(&t) { a } else { 0.0 });
        let s = stats(&path);
        assert!((s.speech_sec - 20.0).abs() < 5.0, "{s:?}");
        assert!(quiet_mic(&s, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Монолог почти без пауз: 10-й перцентиль — уже сам голос, но порог речи
    /// не выше [`SPEECH_MAX_DBFS`], и голос считается.
    #[test]
    fn монолог_без_пауз_без_пометки() {
        let a = amp_for_rms(-25.0);
        let (dir, path) = write_wav("monologue", 60, Some(-70.0), |_| a);
        let s = stats(&path);
        assert!(s.speech_sec > 55.0, "{s:?}");
        assert!(!quiet_mic(&s, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn у_короткой_записи_порог_по_доле() {
        let s = MicStats { duration_sec: 60.0, speech_sec: 8.0, noise_dbfs: -70.0 };
        assert!(!too_little_speech(&s), "8 с из минуты — больше 10%");
        let s = MicStats { duration_sec: 60.0, speech_sec: 5.0, noise_dbfs: -70.0 };
        assert!(too_little_speech(&s));
        let s = MicStats { duration_sec: 3600.0, speech_sec: 30.0, noise_dbfs: -70.0 };
        assert!(!too_little_speech(&s), "ровно порог не считается");
    }

    #[test]
    fn порог_речи_зажат() {
        assert_eq!(speech_threshold(FLOOR_DBFS), SPEECH_MIN_DBFS);
        assert_eq!(speech_threshold(-66.0), -56.0);
        assert_eq!(speech_threshold(-30.0), SPEECH_MAX_DBFS);
    }

    #[test]
    fn пустая_расшифровка_владельца_даёт_пометку_даже_при_звуке() {
        let s = MicStats { duration_sec: 1800.0, speech_sec: 400.0, noise_dbfs: -60.0 };
        assert!(quiet_mic(&s, Some(0)));
        assert!(quiet_mic(&s, Some(MIN_OWNER_WORDS - 1)));
        assert!(!quiet_mic(&s, Some(MIN_OWNER_WORDS)));
        assert!(!quiet_mic(&s, None), "нет расшифровки — решает звук");
        let short = MicStats { duration_sec: 90.0, speech_sec: 20.0, noise_dbfs: -60.0 };
        assert!(!quiet_mic(&short, Some(3)), "у короткой пробы слова не проверяем");
    }

    use crate::transcribe::{merge_markdown, Label, Segment, TrackResult};

    fn track(label: Label, items: &[(f64, &str)]) -> TrackResult {
        TrackResult {
            segments: items
                .iter()
                .map(|&(start, text)| Segment { start, label, text: text.to_string(), speaker: None })
                .collect(),
            text: items.iter().map(|i| i.1).collect::<Vec<_>>().join(" "),
        }
    }

    /// Разбор идёт по тому же `.md`, что пишет `merge_markdown`, а не по
    /// придуманному образцу.
    #[test]
    fn слова_владельца_из_настоящей_разметки() {
        let mic = track(Label::Owner, &[(5.0, "да, согласна"), (65.0, "давайте перенесём на четверг …")]);
        let sys = track(Label::Others, &[(0.0, "всем привет, начнём"), (30.0, "кто за?")]);
        assert_eq!(owner_words(&merge_markdown(&mic, &sys)), Some(6));
    }

    #[test]
    fn слова_владельца_из_хвостового_блока() {
        let mic = TrackResult { segments: vec![], text: "раз два три\n\nчетыре".into() };
        let sys = TrackResult { segments: vec![], text: "чужие слова здесь".into() };
        assert_eq!(owner_words(&merge_markdown(&mic, &sys)), Some(4));
        let sys = track(Label::Others, &[(0.0, "чужие слова")]);
        assert_eq!(owner_words(&merge_markdown(&mic, &sys)), Some(4));
    }

    #[test]
    fn владелец_молчит_ноль_слов() {
        let mic = TrackResult::default();
        let sys = track(Label::Others, &[(0.0, "много чужих слов")]);
        assert_eq!(owner_words(&merge_markdown(&mic, &sys)), Some(0));
    }

    #[test]
    fn нерасшифрованная_дорожка_владельца_это_не_ноль_слов() {
        let md = "_Дорожка владельца не транскрибирована: таймаут_\n\n**[00:00]** _Собеседники_ привет";
        assert_eq!(owner_words(md), None);
    }

    /// Ни одного сэмпла — не «тишина», а отсутствие данных: пометки нет.
    #[test]
    fn пустой_файл_без_пометки() {
        let (dir, path) = write_wav("empty", 0, None, |_| 0.0);
        assert!(mic_stats(&path).is_err());
        assert!(!Cache::default().quiet_mic(&path, None, false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Оборвавшаяся запись: данные на диске есть, а в заголовке длина 0.
    #[test]
    fn нулевая_длина_в_заголовке_при_данных_без_пометки() {
        let (dir, path) = write_wav("zero-hdr", 5, None, |_| 0.0);
        let mut b = std::fs::read(&path).unwrap();
        let pos = b.windows(4).position(|w| w == b"data").unwrap();
        b[pos + 4..pos + 8].copy_from_slice(&0u32.to_le_bytes());
        std::fs::write(&path, &b).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > 100_000);
        assert!(!Cache::default().quiet_mic(&path, None, false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn нечитаемый_файл_это_ошибка_и_пометки_нет() {
        let dir = std::env::temp_dir().join(format!("mr-quiet-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mic.wav");
        std::fs::write(&path, b"not a wav").unwrap();
        assert!(mic_stats(&path).is_err());
        assert!(!Cache::default().quiet_mic(&path, None, false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Кеш смотрит и на расшифровку: появилась пустая расшифровка у записи со
    /// звуком — пометка появляется без перезапуска.
    #[test]
    fn кеш_учитывает_расшифровку() {
        let a = amp_for_rms(-30.0);
        let (dir, path) = write_wav("cache-md", 400, Some(-65.0), |t| if (t as u64) % 3 == 0 { a } else { 0.0 });
        let md = dir.join("t.md");
        let cache = Cache::default();
        assert!(!cache.quiet_mic(&path, None, false));
        std::fs::write(&md, "**[00:00]** _Собеседники_ привет всем").unwrap();
        assert!(cache.quiet_mic(&path, Some(&md), false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Файл длиннее выборки (2000 окон по 100 мс = 200 с): идём по ветке `seek`.
    /// Первые 10% тишина, дальше синус. Если seek не двигается, выборка читает
    /// одну тишину и речи не находит. Длительность — по всему файлу.
    #[test]
    fn длинный_файл_читается_выборкой_с_seek() {
        let total = (WINDOWS as usize * 100 / 1000) + 40; // секунд: 240 > 200
        let (dir, path) = write_wav("long", total, None, |t| if t < total as f64 * 0.1 { 0.0 } else { 0.5 });
        let s = stats(&path);
        assert!((s.duration_sec - total as f32).abs() < 0.01, "{s:?}");
        assert!((s.speech_sec - total as f32 * 0.9).abs() < 3.0, "{s:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ручной прогон по реальным записям (только чтение):
    /// `MEETREC_RECORDINGS=<папка> cargo test -p meeting-recorder-gui real_recordings -- --ignored --nocapture`
    /// Папка — корень записей (с подпапками по месяцам) или одна папка с `.mic.wav`.
    #[test]
    #[ignore]
    fn real_recordings() {
        let Ok(root) = std::env::var("MEETREC_RECORDINGS") else { return };
        let mut files = Vec::new();
        let mut dirs = vec![PathBuf::from(&root)];
        while let Some(d) = dirs.pop() {
            for f in std::fs::read_dir(&d).unwrap().flatten() {
                let p = f.path();
                if p.is_dir() && !p.to_string_lossy().ends_with(".transcript") {
                    dirs.push(p);
                } else if p.to_string_lossy().ends_with(".mic.wav") {
                    files.push(p);
                }
            }
        }
        files.sort();
        println!("файл\tмин\tречь,с\tшум,dBFS\tслов владельца\tпометка");
        for f in files {
            let name = f.file_name().unwrap().to_string_lossy().into_owned();
            let base = name.trim_end_matches(".mic.wav");
            let tdir = f.with_file_name(format!("{base}.transcript"));
            let md = std::fs::read_to_string(tdir.join(format!("{base}.md"))).ok();
            let words = md.as_deref().and_then(owner_words);
            match mic_stats(&f) {
                Ok(s) => println!(
                    "{name}\t{:.1}\t{:.0}\t{:.1}\t{}\t{}",
                    s.duration_sec / 60.0,
                    s.speech_sec,
                    s.noise_dbfs,
                    words.map_or("—".into(), |w| w.to_string()),
                    if quiet_mic(&s, words) { "FLAG" } else { "" }
                ),
                Err(e) => println!("{name}\tERR {e}"),
            }
        }
    }

    /// Микрофон выключили почти на всю часовую встречу, включённые 2 минуты
    /// в тихой комнате человек говорил 20 с. Без поправки порог — 30 с речи от часа, и он
    /// получает «голос почти не записался» за собственное выключение. С
    /// поправкой порог считается от включённых 2 минут.
    #[test]
    fn выключенный_микрофон_не_даёт_пометки() {
        let mut окна = vec![FLOOR_DBFS; 34_800]; // 58 мин тишины выключения по 100 мс
        окна.extend(vec![-70.0; 1_000]); // 100 с тихой комнаты
        окна.extend(vec![-20.0; 200]); // 20 с речи
        let без = stats_from_levels(&окна, 3600.0);
        assert!(too_little_speech(&без), "без поправки — ложная пометка");
        let с_поправкой = stats_without_muted(&окна, 3600.0).expect("включённый кусок есть");
        assert!((с_поправкой.duration_sec - 120.0).abs() < 0.5, "длительность = включённая доля: {}", с_поправкой.duration_sec);
        assert!(!quiet_mic(&с_поправкой, None));
    }

    #[test]
    fn микрофон_выключенный_всю_запись_не_судится() {
        assert_eq!(stats_without_muted(&[FLOOR_DBFS; 50], 5.0), None);
    }

    /// Поправка убирает только цифровой ноль: молчание во включённом куске
    /// по-прежнему даёт пометку.
    #[test]
    fn молчание_во_включённом_куске_помечается_и_с_поправкой() {
        let mut окна = vec![FLOOR_DBFS; 1_000];
        окна.extend(vec![-50.0; 3_000]); // 5 мин включённого шума без речи
        let с_поправкой = stats_without_muted(&окна, 400.0).expect("включённый кусок есть");
        assert!(too_little_speech(&с_поправкой));
    }

}
