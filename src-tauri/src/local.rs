//! Каркас локальной расшифровки — «на этом компьютере», без шлюза.
//!
//! Три отдельные заботы: (1) что такое модель и где она лежит на диске, (2)
//! её жизненный цикл — проверить/скачать с прогрессом/удалить, (3) сама
//! расшифровка. Первые две — рабочий код: они нужны интерфейсу настроек уже
//! сейчас, чтобы показать «модель не скачана» / «скачиваю» / «модель на
//! месте» честно, по факту на диске. Третья — **честная заглушка**: движок
//! распознавания речи (whisper.cpp/ggml, faster-whisper — выбор за тем, кто
//! будет его подключать) сюда не вписан и вписывать его в рамках этой задачи
//! не нужно. `transcribe_local` не пытается ничего распознать — она
//! единственно возвращает ошибку с ключом словаря `local.notWired`.
//!
//! Модель выбирает разработчик движка: `MODEL` ниже — одна заглушечная
//! константа с понятным именем полей, не настоящий адрес и не настоящий вес
//! файла.

use crate::transcribe::{Label, TrackResult};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::AsyncWriteExt;

/// Описание модели: имя, вес, откуда качать, как называется файл на диске.
///
/// Поля объявлены здесь ради интерфейса настроек (весом считает `{size}` в
/// текстах `local.modelDownload`/`local.modelReady`/`local.noSpace`) — сама
/// модель за этой структурой не стоит, её выбор впереди.
pub struct ModelSpec {
    /// Для журнала и для того, кто будет читать этот код, а не для
    /// интерфейса: имя модели человеку показывают тексты `local.*` из
    /// `ui/i18n/strings.json`, не это поле.
    #[allow(dead_code)]
    pub name: &'static str,
    /// Байты, не мегабайты — тем же способом, что и `Recording::size` в
    /// `main.rs`: округление в мегабайты/гигабайты для человека делает
    /// `размер()` во фронтенде, здесь — точное число.
    pub size_bytes: u64,
    pub url: &'static str,
    pub file_name: &'static str,
}

/// ЗАГЛУШКА: имя, адрес и вес — не настоящие. Какую именно модель встраивать
/// (whisper.cpp/ggml, faster-whisper, что-то ещё), с какого зеркала её
/// качать и сколько она весит на самом деле — решает разработчик, который
/// будет подключать движок распознавания. До этого момента константа нужна
/// ровно для того, чтобы интерфейс настроек и код скачивания ниже было на
/// чём собрать и проверить: `размер()` во фронтенде и `check_space` здесь
/// работают с любым числом байт одинаково честно.
pub const MODEL: ModelSpec = ModelSpec {
    name: "заглушка-модели-локальной-расшифровки",
    size_bytes: 1_600_000_000,
    url: "https://example.invalid/models/PLACEHOLDER.bin",
    file_name: "model.bin",
};

/// Каталог моделей — подкаталог `app_data_dir()`, не `app_config_dir()`
/// (тот держит только `config.json`, см. `config.rs`): модель весит гигабайт
/// с лишним, а не байты настройки, и её место — рядом с прочими данными
/// приложения, не рядом с текстовым конфигом.
pub fn resolve_models_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("не найден каталог данных приложения: {e}"))?;
    Ok(base.join("models"))
}

pub fn model_path(models_dir: &Path) -> PathBuf {
    models_dir.join(MODEL.file_name)
}

/// Модель скачана — по факту на диске, не по записи в конфиге: конфиг может
/// соврать (ручное удаление файла, прерванная установка), а лишний `stat`
/// стоит дёшево.
pub fn is_downloaded(models_dir: &Path) -> bool {
    model_path(models_dir).exists()
}

/// Итог сравнения свободного места с весом модели — без диска, чистая
/// функция, тестируется без временных каталогов и без `sysinfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceCheck {
    Ok,
    NotEnough { need: u64, free: u64 },
}

pub fn check_space_given(free: u64, need: u64) -> SpaceCheck {
    if free >= need {
        SpaceCheck::Ok
    } else {
        SpaceCheck::NotEnough { need, free }
    }
}

/// То же самое, но по-настоящему: свободное место того диска, на котором
/// лежит (или ляжет) `models_dir` — ищем самую специфичную точку монтирования
/// (самый длинный `mount_point`, из-под которого начинается наш путь), а не
/// первый попавшийся диск в списке, иначе на машине с отдельным томом под
/// `~/Library` (или отдельным разделом на Windows) число было бы для другого
/// диска. `None` — диск не опознан (редкий случай, например путь ещё не
/// существует ни на одном примонтированном томе); интерфейсу тогда нечего
/// показать, кроме «скачать» без гарантии на месте ли будет модель.
pub fn check_space(models_dir: &Path) -> Option<SpaceCheck> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let free = disks
        .list()
        .iter()
        .filter(|d| models_dir.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(sysinfo::Disk::available_space)?;
    Some(check_space_given(free, MODEL.size_bytes))
}

/// Ошибка заглушки локальной расшифровки. Ключ, а не готовая фраза: `Display`
/// отдаёт буквально `"local.notWired"` — раздел и имя строки в
/// `ui/i18n/strings.json`. Тот же приём, что `status::DEAD` (там —
/// `"permission.streamDead"`): фронтенд и Rust-сторона (`i18n::t`) переводят
/// такой текст сами, а не показывают его как есть.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LocalError {
    #[error("local.notWired")]
    NotWired,
}

/// Расшифровка на этом компьютере. **Заглушка.** Никакого движка
/// распознавания здесь нет и не появится в рамках этой задачи — функция
/// не трогает ни `path`, ни `label`, она единственно возвращает
/// `LocalError::NotWired`.
///
/// Сигнатура зеркалит вход и выход серверного пути (`transcribe::submit` +
/// `transcribe::poll_until_done`: путь к WAV-дорожке, метка
/// владелец/собеседники, `TrackResult` на успехе) — тому, кто будет
/// подключать движок, менять сигнатуру или код в `transcribe.rs`/`main.rs`,
/// который её вызывает, не придётся, только тело этой функции.
pub async fn transcribe_local(_path: &Path, _label: Label) -> Result<TrackResult, LocalError> {
    Err(LocalError::NotWired)
}

/// Что может пойти не так при скачивании модели — отдельно от `LocalError`:
/// то про заглушку расшифровки, это про файл на диске и сеть.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("не хватает места: нужно {need}, свободно {free}")]
    NoSpace { need: u64, free: u64 },
    #[error("сеть: {0}")]
    Network(#[from] reqwest::Error),
    #[error("файл: {0}")]
    Io(#[from] std::io::Error),
    #[error("зеркало отдало модель кодом {0}")]
    Http(reqwest::StatusCode),
}

/// Ключ и параметры для интерфейса — `ModelError` сам по себе несёт
/// технический текст (в лог), а не готовую фразу; `local.noSpace` вдобавок
/// принимает `{need}`/`{free}`, которые фронтенд подставит через `размер()`,
/// тем же способом, что `transcribe.progress` принимает `{stage}`/`{n}` в
/// `ui/main.js`.
pub fn error_payload(e: &ModelError) -> serde_json::Value {
    match e {
        ModelError::NoSpace { need, free } => {
            serde_json::json!({ "key": "local.noSpace", "need": need, "free": free })
        }
        _ => serde_json::json!({ "key": "local.downloadFailed" }),
    }
}

fn emit_model_progress(app: &AppHandle, done: u64, total: u64) {
    let _ = app.emit("local-model-progress", serde_json::json!({ "done": done, "total": total }));
}

/// Скачать модель в `models_dir`, сообщая ход дела событием
/// `local-model-progress` на каждый заметный шаг — не на каждый чанк
/// `reqwest` (это были бы тысячи событий на гигабайтный файл), а не реже, чем
/// на каждый примерный процент (и не позже последнего байта — финальное
/// событие шлётся всегда, даже если файл меньше 1% от общего веса).
///
/// Пишет во временный `<файл>.part` и переименовывает в цель одним атомарным
/// `rename` по завершении — оборванное скачивание не оставляет на диске
/// файл, который `is_downloaded` принял бы за готовую модель.
///
/// Место на диске проверяется ДО того, как что-то льётся в сеть: скачать
/// гигабайт, чтобы в конце упереться в диск, — трата и трафика, и терпения.
///
/// Отмены нет — не просили (см. задачу): скачивание либо доходит до конца,
/// либо падает само, по сети или по диску.
pub async fn download_model(app: &AppHandle, models_dir: &Path) -> Result<(), ModelError> {
    if let Some(SpaceCheck::NotEnough { need, free }) = check_space(models_dir) {
        return Err(ModelError::NoSpace { need, free });
    }
    std::fs::create_dir_all(models_dir)?;
    let tmp_path = model_path(models_dir).with_extension("part");

    let client = reqwest::Client::new();
    let mut resp = client.get(MODEL.url).send().await?;
    if !resp.status().is_success() {
        return Err(ModelError::Http(resp.status()));
    }
    let total = resp.content_length().unwrap_or(MODEL.size_bytes);

    let mut file = tokio::fs::File::create(&tmp_path).await?;
    let mut done: u64 = 0;
    let mut last_emitted: u64 = 0;
    // Шаг ~1% от веса модели, но не мельче 256 КБ — иначе на маленьком
    // тестовом файле (или на модели, которая однажды окажется меньше
    // заглушечного веса) шаг ушёл бы в ноль и события посыпались бы на
    // каждый чанк.
    let step = (total / 100).max(256 * 1024);
    emit_model_progress(app, 0, total);

    let result: Result<(), ModelError> = async {
        while let Some(chunk) = resp.chunk().await? {
            file.write_all(&chunk).await?;
            done += chunk.len() as u64;
            if done.saturating_sub(last_emitted) >= step || done >= total {
                emit_model_progress(app, done, total);
                last_emitted = done;
            }
        }
        Ok(())
    }
    .await;

    drop(file);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    std::fs::rename(&tmp_path, model_path(models_dir))?;
    Ok(())
}

/// Удалить модель. Файла уже нет — не ошибка, тем же принципом, что
/// `retention::delete_audio`: второй проход по уже убранному не повод
/// жаловаться.
pub fn remove_model(models_dir: &Path) -> std::io::Result<()> {
    let path = model_path(models_dir);
    if path.exists() {
        std::fs::remove_file(path)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Уникальный временный каталог — тот же приём, что `ScratchDir` в
    /// `retention.rs`/`delete.rs`.
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
            let path = std::env::temp_dir().join(format!("mr-local-{tag}-{pid}-{nanos}-{n}"));
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

    #[test]
    fn заглушка_возвращает_ровно_ключ_notwired() {
        assert_eq!(LocalError::NotWired.to_string(), "local.notWired");
    }

    #[tokio::test]
    async fn transcribe_local_всегда_ошибка_с_ключом_notwired() {
        let err = transcribe_local(Path::new("/dev/null"), Label::Owner).await.unwrap_err();
        assert_eq!(err, LocalError::NotWired);
        assert_eq!(err.to_string(), "local.notWired");
    }

    #[test]
    fn места_хватает() {
        assert_eq!(check_space_given(10_000, 5_000), SpaceCheck::Ok);
    }

    #[test]
    fn места_ровно_впритык_это_ещё_хватает() {
        assert_eq!(check_space_given(5_000, 5_000), SpaceCheck::Ok);
    }

    /// Гвоздь задачи: свободного меньше размера модели — отказ, с честными
    /// числами внутри, а не просто «нет».
    #[test]
    fn места_не_хватает() {
        assert_eq!(
            check_space_given(1_000, 5_000),
            SpaceCheck::NotEnough { need: 5_000, free: 1_000 }
        );
    }

    #[test]
    fn скачанной_модели_нет_в_пустом_каталоге() {
        let dir = ScratchDir::new("missing");
        assert!(!is_downloaded(&dir));
    }

    #[test]
    fn модель_на_месте_после_записи_файла() {
        let dir = ScratchDir::new("ready");
        std::fs::write(model_path(&dir), b"x").expect("создать файл модели");
        assert!(is_downloaded(&dir));
    }

    #[test]
    fn удаление_снимает_модель_с_диска() {
        let dir = ScratchDir::new("remove");
        std::fs::write(model_path(&dir), b"x").expect("создать файл модели");
        remove_model(&dir).expect("удалить модель");
        assert!(!is_downloaded(&dir));
    }

    /// Второй проход по уже удалённой модели — не ошибка.
    #[test]
    fn удаление_без_модели_не_ошибка() {
        let dir = ScratchDir::new("remove-twice");
        remove_model(&dir).expect("удалять нечего — не повод падать");
        remove_model(&dir).expect("и второй раз — тоже");
    }

    #[test]
    fn ошибка_no_space_даёт_ключ_и_числа() {
        let payload = error_payload(&ModelError::NoSpace { need: 5_000, free: 1_000 });
        assert_eq!(payload["key"], "local.noSpace");
        assert_eq!(payload["need"], 5_000);
        assert_eq!(payload["free"], 1_000);
    }

    #[test]
    fn сетевая_ошибка_даёт_ключ_downloadfailed() {
        let payload = error_payload(&ModelError::Http(reqwest::StatusCode::NOT_FOUND));
        assert_eq!(payload["key"], "local.downloadFailed");
    }
}
