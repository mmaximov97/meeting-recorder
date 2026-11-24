//! Обновления: есть ли на GitHub релиз новее запущенного и, на macOS, его
//! установка прямо из приложения.
//!
//! Проверка — сразу после старта и раз в сутки (`spawn_update_worker` в
//! `main.rs`) и по кнопке «Проверить обновления» в настройках. Фоновая
//! показывает баннер и уважает «Позже»: пропущенная версия лежит в конфиге, и
//! баннер вернётся только со следующим релизом. Её отказы (сеть, лимит API,
//! мусор в ответе) — молча, в лог: обновление не повод мешать записывать
//! встречу. Ручная проверка «Позже» не учитывает и об отказе говорит —
//! человек спросил сам и ждёт ответа.
//!
//! Установка своя, а не Tauri updater: тому нужны ключи подписи и манифест
//! обновлений, а сборка у нас ad-hoc. Gatekeeper ей не мешает, и вот почему:
//! при запуске он проверяет только файлы с атрибутом карантина, а этот атрибут
//! ставят браузер и прочие загрузчики, помечающие скачанное. Наш процесс не в
//! песочнице и сам ничего не помечает, поэтому бандл, скачанный reqwest'ом и
//! распакованный `/usr/bin/tar`, запускается так же, как уже установленный.
//! Подписи разработчика нет, и подлинность держится на другом: GitHub отдаёт в
//! API релиза SHA-256 каждого ассета (`digest`), а архив без дайджеста или с
//! несовпадением не распаковывается.
//!
//! Цена ad-hoc подписи та же, что при ручной замене приложения: у новой сборки
//! другой хеш кода, и macOS может заново спросить доступ к микрофону и
//! системному звуку.
//!
//! На Windows кнопка по-прежнему открывает страницу релиза: встроенной
//! установки там нет.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Откуда берётся последний релиз. Черновики и pre-release сюда не попадают —
/// `latest` у GitHub это только полноценные релизы.
pub const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/mmaximov97/meeting-recorder/releases/latest";

/// Как часто спрашивать, пока приложение живёт в трее. Сутки: релизы выходят
/// не чаще, а лимит анонимных запросов к API — 60 в час на адрес.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Предел одного запроса. Ответ — одна страница JSON; без предела запрос на
/// протухшей сети висел бы, а вместе с ним и вся задача проверки.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// GitHub отвечает 403 на запрос без User-Agent.
const USER_AGENT: &str = concat!("MeetRec/", env!("CARGO_PKG_VERSION"));

/// Что показать человеку: номер без `v` и ссылка на страницу релиза.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Release {
    pub version: String,
    pub url: String,
}

/// Ровно те поля ответа `releases/latest`, что нужны. `draft`/`prerelease`
/// на `latest` не приходят в виде `true` никогда, но проверить дешевле, чем
/// полагаться на это.
#[derive(Deserialize)]
struct LatestPayload {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    /// Нужны только установке, а она есть только на macOS.
    #[cfg(target_os = "macos")]
    #[serde(default)]
    assets: Vec<install::Asset>,
}

fn strip_v(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('v').unwrap_or(s)
}

/// `v0.3.1` / `0.3.1` → `(0, 3, 1)`. Всё, что не три числа через точку, —
/// `None`: сравнивать такое не с чем, а угадывать опасно (можно предложить
/// «обновиться» на старую версию).
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = strip_v(s).split('.').map(|p| p.parse::<u64>().ok());
    let major = parts.next()??;
    let minor = parts.next()??;
    let patch = parts.next()??;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Релиз новее запущенного. Неразобранная версия с любой стороны — `false`:
/// лучше промолчать, чем предложить откат.
pub fn is_newer(current: &str, latest: &str) -> bool {
    match (parse_version(current), parse_version(latest)) {
        (Some(c), Some(l)) => l > c,
        _ => false,
    }
}

/// Ответ GitHub → полноценный релиз и его версия без `v`. Черновик,
/// pre-release, неразобранная версия и мусор — `None`.
fn parse_payload(json: &str) -> Option<(LatestPayload, String)> {
    let p: LatestPayload = serde_json::from_str(json).ok()?;
    if p.draft || p.prerelease {
        return None;
    }
    let version = strip_v(&p.tag_name).to_string();
    parse_version(&version)?;
    Some((p, version))
}

/// Разобрать ответ GitHub в `Release`. Черновик, pre-release и мусор — `None`.
pub fn parse_latest(json: &str) -> Option<Release> {
    let (p, version) = parse_payload(json)?;
    Some(Release { version, url: p.html_url })
}

/// Показывать ли баннер: релиз новее запущенной версии и человек не нажимал
/// «Позже» именно на нём. «Позже» на 0.3.0 не гасит 0.3.1.
pub fn decide(current: &str, latest: Option<Release>, skipped: Option<&str>) -> Option<Release> {
    let r = latest?;
    if !is_newer(current, &r.version) {
        return None;
    }
    if skipped.map(strip_v) == Some(r.version.as_str()) {
        return None;
    }
    Some(r)
}

/// Ответ ручной проверки: что запущено и что есть новее (`None` — новее нет).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManualCheck {
    pub current: String,
    pub latest: Option<Release>,
}

/// Ручная проверка, в отличие от `decide`, на «Позже» не смотрит: человек
/// нажал кнопку сам, и «у вас последняя версия» при вышедшей новой было бы
/// неправдой.
pub fn check_manually(current: &str, latest: Release) -> ManualCheck {
    let newer = is_newer(current, &latest.version);
    ManualCheck { current: current.to_string(), latest: newer.then_some(latest) }
}

/// Ключи строк `update.err*` из `ui/i18n/strings.json`. Переводит их фронтенд,
/// как и остальные экраны: Rust отдаёт ключ, а не готовую фразу. Большая часть
/// нужна только установке, то есть только на macOS.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub mod err {
    pub const CHECK: &str = "update.errCheck";
    pub const BUSY: &str = "update.errBusy";
    pub const RECORDING: &str = "update.errRecording";
    pub const DEV_BUILD: &str = "update.errDevBuild";
    pub const READ_ONLY: &str = "update.errReadOnly";
    pub const NOT_NEWER: &str = "update.errNotNewer";
    pub const NO_ASSET: &str = "update.errNoAsset";
    pub const NO_DIGEST: &str = "update.errNoDigest";
    pub const DOWNLOAD: &str = "update.errDownload";
    pub const CHECKSUM: &str = "update.errChecksum";
    pub const BAD_BUNDLE: &str = "update.errBadBundle";
    pub const INSTALL: &str = "update.errInstall";
}

/// Отказ проверки или установки, который увидит человек: ключ строки (см.
/// [`err`]) и техническая подробность — для подсказки и лога. У `READ_ONLY`
/// подробность — папка, и строка подставляет её в текст.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateError {
    pub key: &'static str,
    pub detail: Option<String>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl UpdateError {
    pub fn new(key: &'static str) -> Self {
        Self { key, detail: None }
    }
    pub fn with(key: &'static str, detail: impl std::fmt::Display) -> Self {
        Self { key, detail: Some(detail.to_string()) }
    }
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.detail {
            Some(d) => write!(f, "{}: {d}", self.key),
            None => f.write_str(self.key),
        }
    }
}

/// Что нашла последняя фоновая проверка, плюс состояние установки.
///
/// Релиз для баннера лежит здесь потому, что webview может подняться позже
/// события `update-available` (события Tauri не буферизуются — см.
/// `status.rs`): окно читает его командой `update_status` при загрузке, тем же
/// приёмом, что `get_state`.
#[derive(Default)]
pub struct UpdateState {
    banner: Mutex<Option<Release>>,
    installing: AtomicBool,
    relaunch: Mutex<Option<PathBuf>>,
}

impl UpdateState {
    pub fn set(&self, r: Option<Release>) {
        if let Ok(mut g) = self.banner.lock() {
            *g = r;
        }
    }
    pub fn get(&self) -> Option<Release> {
        self.banner.lock().ok().and_then(|g| g.clone())
    }
    /// Занять установку. `false` — она уже идёт: второе нажатие (из баннера и
    /// из настроек сразу) не должно начать вторую поверх первой.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn begin_install(&self) -> bool {
        !self.installing.swap(true, Ordering::SeqCst)
    }
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn end_install(&self) {
        self.installing.store(false, Ordering::SeqCst);
    }
    /// Новый бандл на месте: после выхода запустить его (см. [`relaunch`]).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn request_relaunch(&self, bundle: PathBuf) {
        if let Ok(mut g) = self.relaunch.lock() {
            *g = Some(bundle);
        }
    }
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn relaunch_target(&self) -> Option<PathBuf> {
        self.relaunch.lock().ok().and_then(|g| g.clone())
    }
}

/// Запустить новую версию, если её только что поставили. Зовётся из
/// `RunEvent::Exit` в `main.rs`, то есть когда процесс уже уходит обычным
/// путём «Выхода»: аудио-поток отпустил микрофон и закрыл файлы.
///
/// Через LaunchServices (`open`), а не `AppHandle::restart`: тот запускает
/// бинарь дочерним процессом, а за дочерний процесс macOS считает
/// ответственным родителя — запросы доступа к микрофону и звуку системы шли бы
/// от имени уже завершённого старого приложения. `open` запускает бандл так
/// же, как Finder. Сначала ждём, пока старый процесс уйдёт: при живом старом
/// `open` просто вывел бы его вперёд, а не запустил новый. Фиксированная пауза
/// тут не годится — сколько процесс закрывается, зависит от машины. Ждём не
/// дольше 10 с: дальше `open` всё равно лучше, чем ничего.
#[cfg(target_os = "macos")]
pub fn relaunch(app: &tauri::AppHandle) {
    use tauri::Manager;
    let Some(bundle) = app.state::<UpdateState>().relaunch_target() else {
        return;
    };
    let spawned = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(RELAUNCH_SCRIPT)
        .arg(&bundle)
        .arg(std::process::id().to_string())
        .spawn();
    match spawned {
        Ok(_) => log::info!("обновление: новая версия запустится из {}", bundle.display()),
        Err(e) => log::error!("обновление: новая версия не запущена, откройте её сами: {e}"),
    }
}

/// `$0` — бандл, `$1` — pid старого процесса. 50 × 0.2 с = 10 с ожидания.
#[cfg(target_os = "macos")]
const RELAUNCH_SCRIPT: &str = r#"i=0
while kill -0 "$1" 2>/dev/null && [ "$i" -lt 50 ]; do sleep 0.2; i=$((i + 1)); done
exec /usr/bin/open "$0""#;

pub fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder().build().map_err(|e| format!("HTTP-клиент не создан: {e}"))
}

/// Тело ответа `releases/latest`. `Err` — сеть, таймаут или не-2xx (403 при
/// исчерпанном лимите API, 404 у репозитория без релизов): текст для лога и
/// подсказки.
pub async fn fetch_latest_json(client: &reqwest::Client) -> Result<String, String> {
    let resp = client
        .get(LATEST_RELEASE_URL)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("GitHub ответил {status}"));
    }
    resp.text().await.map_err(|e| e.to_string())
}

/// Спросить GitHub. `Ok(None)` — ответ пришёл, но релиза в нём не разобрать.
pub async fn fetch_latest(client: &reqwest::Client) -> Result<Option<Release>, String> {
    Ok(parse_latest(&fetch_latest_json(client).await?))
}

/// Установка на macOS: выбрать архив, скачать, сверить, распаковать рядом с
/// бандлом, проверить и подменить. Перезапуск и проверку «не идёт ли запись»
/// делает `main.rs` — им нужен `AppHandle`, а этому модулю нет.
#[cfg(target_os = "macos")]
pub mod install {
    use super::{err, is_newer, parse_payload, UpdateError, USER_AGENT};
    use serde::Deserialize;
    use std::ffi::OsStr;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    /// Ассет релиза — ровно те поля, что нужны установке.
    #[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
    pub struct Asset {
        pub name: String,
        pub browser_download_url: String,
        pub size: u64,
        /// `"sha256:<hex>"`. GitHub считает его сам при загрузке ассета; у
        /// ассетов из старых релизов поля может не быть.
        #[serde(default)]
        pub digest: Option<String>,
    }

    /// Что ставить: версия и архив `.app` под эту архитектуру.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Package {
        pub version: String,
        pub url: String,
        pub size: u64,
        pub sha256: String,
    }

    /// Предел скачивания архива. ~5 МБ — это секунды, но на плохой сети
    /// минута-другая ещё не повод сдаваться, а бесконечное ожидание — повод.
    pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(5 * 60);

    /// Хвост имени архива под архитектуру — так их называет tauri-action в
    /// `release.yml`: `MeetRec_<версия>_aarch64.app.tar.gz` и `_x64.app.tar.gz`.
    pub fn asset_suffix(arch: &str) -> Option<&'static str> {
        match arch {
            "aarch64" => Some("_aarch64.app.tar.gz"),
            "x86_64" => Some("_x64.app.tar.gz"),
            _ => None,
        }
    }

    pub fn pick_asset<'a>(assets: &'a [Asset], arch: &str) -> Option<&'a Asset> {
        let suffix = asset_suffix(arch)?;
        assets.iter().find(|a| a.name.ends_with(suffix))
    }

    /// `"sha256:<64 hex>"` → hex в нижнем регистре. Другой алгоритм или не
    /// хеш — `None`: сверять не с чем, и это отказ, а не «сверку пропустим».
    pub fn parse_sha256(digest: &str) -> Option<String> {
        let hex = digest.trim().strip_prefix("sha256:")?;
        (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| hex.to_ascii_lowercase())
    }

    pub fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(bytes))
    }

    /// Ответ `releases/latest` → что ставить. Релиз новее запущенного, с
    /// архивом под эту архитектуру и с дайджестом — иначе отказ с причиной.
    pub fn package_for(json: &str, current: &str, arch: &str) -> Result<Package, UpdateError> {
        let (p, version) =
            parse_payload(json).ok_or_else(|| UpdateError::with(err::CHECK, "ответ GitHub не разобран"))?;
        if !is_newer(current, &version) {
            return Err(UpdateError::with(err::NOT_NEWER, version));
        }
        let asset = pick_asset(&p.assets, arch).ok_or_else(|| UpdateError::with(err::NO_ASSET, arch))?;
        let sha256 = asset
            .digest
            .as_deref()
            .and_then(parse_sha256)
            .ok_or_else(|| UpdateError::with(err::NO_DIGEST, &asset.name))?;
        Ok(Package { version, url: asset.browser_download_url.clone(), size: asset.size, sha256 })
    }

    /// Скачанное — ровно то, что выложено в релиз: размер и SHA-256 сходятся с
    /// тем, что сообщил API.
    pub fn verify(body: &[u8], pkg: &Package) -> Result<(), UpdateError> {
        if body.len() as u64 != pkg.size {
            return Err(UpdateError::with(
                err::CHECKSUM,
                format!("пришло {} байт вместо {}", body.len(), pkg.size),
            ));
        }
        let actual = sha256_hex(body);
        if actual != pkg.sha256 {
            return Err(UpdateError::with(err::CHECKSUM, format!("SHA-256 {actual}, ожидался {}", pkg.sha256)));
        }
        Ok(())
    }

    /// `…/MeetRec.app/Contents/MacOS/<бинарь>` → `…/MeetRec.app`. Что-то
    /// другое (запуск из `target/` при разработке) — `None`: подменять там
    /// нечего.
    pub fn bundle_from_exe(exe: &Path) -> Option<PathBuf> {
        let macos = exe.parent()?;
        let contents = macos.parent()?;
        let bundle = contents.parent()?;
        let ok = macos.file_name()? == "MacOS"
            && contents.file_name()? == "Contents"
            && bundle.extension()? == "app"
            && bundle.parent().is_some();
        ok.then(|| bundle.to_path_buf())
    }

    /// Соседи бандла: каталог распаковки и имя, под которым старый бандл ждёт,
    /// пока новый встанет на место. Тот же каталог — значит, тот же том, и
    /// `rename` атомарен. Оба скрытые: в «Программах» не должна мелькать
    /// вторая копия.
    fn sibling(bundle: &Path, suffix: &str) -> PathBuf {
        let name = bundle.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        bundle.with_file_name(format!(".{name}.{suffix}"))
    }

    pub fn staging_path(bundle: &Path) -> PathBuf {
        sibling(bundle, "update")
    }

    pub fn backup_path(bundle: &Path) -> PathBuf {
        sibling(bundle, "old")
    }

    /// Единственный `.app` в каталоге. Ноль или два — не тот архив.
    pub fn find_app(dir: &Path) -> Option<PathBuf> {
        let mut apps = std::fs::read_dir(dir)
            .ok()?
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.path())
            .filter(|p| p.extension() == Some(OsStr::new("app")));
        let app = apps.next()?;
        apps.next().is_none().then_some(app)
    }

    /// Распакованное — то самое приложение той самой версии, и его бинарь на
    /// месте. Иначе после подмены Tauri перезапустил бы не то или ничего.
    pub fn validate_bundle(app: &Path, identifier: &str, version: &str) -> Result<(), String> {
        let path = app.join("Contents").join("Info.plist");
        let info: plist::Dictionary = plist::from_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let get = |key: &str| info.get(key).and_then(plist::Value::as_string);
        let id = get("CFBundleIdentifier");
        if id != Some(identifier) {
            return Err(format!("CFBundleIdentifier {id:?}, ожидался {identifier}"));
        }
        let v = get("CFBundleShortVersionString");
        if v != Some(version) {
            return Err(format!("CFBundleShortVersionString {v:?}, ожидалась {version}"));
        }
        let exe = get("CFBundleExecutable").ok_or("в Info.plist нет CFBundleExecutable")?;
        let exe = app.join("Contents").join("MacOS").join(exe);
        if !exe.is_file() {
            return Err(format!("нет исполняемого файла {}", exe.display()));
        }
        Ok(())
    }

    /// Поставить `new` на место `current`.
    ///
    /// Старый бандл сначала отъезжает в `backup`, а не удаляется: не встал
    /// новый на место — старый возвращается, и человек остаётся с рабочим
    /// приложением, а не без него. Удаляется старый только после удачной
    /// подмены, и неудача удаления — уже не неудача обновления: новый на
    /// месте, остаток уходит в лог. Приложения по пути `current` нет только
    /// между двумя `rename`.
    pub fn swap_bundles(current: &Path, new: &Path, backup: &Path) -> io::Result<()> {
        std::fs::rename(current, backup)?;
        if let Err(e) = std::fs::rename(new, current) {
            return match std::fs::rename(backup, current) {
                Ok(()) => Err(e),
                Err(back) => Err(io::Error::new(
                    e.kind(),
                    format!("{e}; вернуть прежний бандл не удалось: {back}, он лежит в {}", backup.display()),
                )),
            };
        }
        if let Err(e) = std::fs::remove_dir_all(backup) {
            log::warn!("обновление: прежний бандл {} не удалён: {e}", backup.display());
        }
        Ok(())
    }

    /// Каталог распаковки рядом с бандлом. Живёт, пока жив этот объект:
    /// любой отказ по дороге убирает его вместе с архивом и распакованным.
    pub struct Staging {
        dir: PathBuf,
    }

    impl Staging {
        /// Заодно проверка, можем ли мы писать рядом с бандлом, — до
        /// скачивания: незачем качать архив, который некуда положить.
        pub fn create(bundle: &Path) -> Result<Self, UpdateError> {
            let dir = staging_path(bundle);
            // Остаток прошлой попытки, оборванной на полпути (процесс убили
            // посреди скачивания). Нет его — не ошибка.
            let _ = std::fs::remove_dir_all(&dir);
            match std::fs::create_dir(&dir) {
                Ok(()) => Ok(Self { dir }),
                Err(e) if matches!(e.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem) => {
                    let folder = bundle.parent().unwrap_or(bundle);
                    Err(UpdateError::with(err::READ_ONLY, folder.display()))
                }
                Err(e) => Err(UpdateError::with(err::INSTALL, format!("{}: {e}", dir.display()))),
            }
        }

        pub fn dir(&self) -> &Path {
            &self.dir
        }
    }

    impl Drop for Staging {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Скачать архив и сверить его с релизом. `on_progress(получено, всего)`
    /// зовётся на каждый кусок — прореживает вызывающий.
    pub async fn download(
        client: &reqwest::Client,
        pkg: &Package,
        mut on_progress: impl FnMut(u64, u64),
    ) -> Result<Vec<u8>, UpdateError> {
        let fail = |e: &dyn std::fmt::Display| UpdateError::with(err::DOWNLOAD, e);
        let mut resp = client
            .get(&pkg.url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .timeout(DOWNLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|e| fail(&e))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(fail(&format!("сервер ответил {status}")));
        }
        let mut body = Vec::new();
        on_progress(0, pkg.size);
        while let Some(chunk) = resp.chunk().await.map_err(|e| fail(&e))? {
            body.extend_from_slice(&chunk);
            // Больше заявленного — уже не тот файл; дочитывать незачем.
            if body.len() as u64 > pkg.size {
                break;
            }
            on_progress(body.len() as u64, pkg.size);
        }
        verify(&body, pkg)?;
        Ok(body)
    }

    fn run(program: &str, args: &[&OsStr]) -> Result<(), String> {
        let out = Command::new(program).args(args).output().map_err(|e| format!("{program}: {e}"))?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(format!("{program}: {} {}", out.status, stderr.trim()))
    }

    /// Архив → проверенный `.app` внутри каталога распаковки.
    ///
    /// Распаковывает системный `tar`: он сохраняет права и симлинки бандла и
    /// по умолчанию не пишет за пределы `-C` (отбрасывает `..` и абсолютные
    /// пути). `codesign --verify` в конце — не проверка подлинности (подпись
    /// ad-hoc, подлинность уже сверена по SHA-256), а проверка, что бандл
    /// цел: на Apple Silicon бинарь с битой подписью ядро не запустит, и
    /// после подмены человек остался бы без приложения.
    pub fn unpack(staging: &Staging, archive: &[u8], identifier: &str, version: &str) -> Result<PathBuf, UpdateError> {
        let install = |e: String| UpdateError::with(err::INSTALL, e);
        let tarball = staging.dir().join("update.tar.gz");
        std::fs::write(&tarball, archive).map_err(|e| install(format!("{}: {e}", tarball.display())))?;
        let out = staging.dir().join("unpacked");
        std::fs::create_dir(&out).map_err(|e| install(format!("{}: {e}", out.display())))?;
        run("/usr/bin/tar", &["-xzf".as_ref(), tarball.as_os_str(), "-C".as_ref(), out.as_os_str()]).map_err(install)?;
        let bad = |e: String| UpdateError::with(err::BAD_BUNDLE, e);
        let app = find_app(&out).ok_or_else(|| bad("в архиве не один .app".into()))?;
        validate_bundle(&app, identifier, version).map_err(bad)?;
        run("/usr/bin/codesign", &["--verify".as_ref(), "--strict".as_ref(), app.as_os_str()]).map_err(bad)?;
        Ok(app)
    }

    /// Подменить запущенный бандл новым (см. [`swap_bundles`]).
    pub fn replace(bundle: &Path, new_app: &Path) -> Result<(), UpdateError> {
        let backup = backup_path(bundle);
        // Остаток прошлой подмены, который тогда не удалось удалить.
        let _ = std::fs::remove_dir_all(&backup);
        swap_bundles(bundle, new_app, &backup).map_err(|e| UpdateError::with(err::INSTALL, e))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::path::Path;

        /// Уникальный временный каталог на тест: тесты идут параллельно и
        /// подрались бы за один.
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
                let path = std::env::temp_dir().join(format!("mr-update-{tag}-{pid}-{nanos}-{n}"));
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

        const ID: &str = "net.meetrec.app";

        /// Подставной бандл: Info.plist с нужными ключами, бинарь-заглушка и
        /// файл-метка, по которому видно, какая копия где оказалась. Метка — в
        /// `Resources`, а не прямо в `Contents`: лишний файл там `codesign`
        /// считает неподписываемым вложением и отказывается подписывать бандл.
        fn бандл(path: &Path, id: &str, version: &str, метка: &str) {
            let macos = path.join("Contents").join("MacOS");
            let resources = path.join("Contents").join("Resources");
            std::fs::create_dir_all(&macos).unwrap();
            std::fs::create_dir_all(&resources).unwrap();
            std::fs::write(macos.join("meeting-recorder-gui"), "#!/bin/sh\n").unwrap();
            std::fs::write(resources.join("label"), метка).unwrap();
            let plist = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>meeting-recorder-gui</string>
<key>CFBundleIdentifier</key><string>{id}</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
</dict></plist>"#
            );
            std::fs::write(path.join("Contents").join("Info.plist"), plist).unwrap();
        }

        fn метка(path: &Path) -> String {
            std::fs::read_to_string(path.join("Contents").join("Resources").join("label")).unwrap()
        }

        fn ассет(name: &str, digest: Option<&str>) -> Asset {
            Asset {
                name: name.into(),
                browser_download_url: format!("https://example.test/{name}"),
                size: 5,
                digest: digest.map(Into::into),
            }
        }

        const HEX: &str = "1eb3ce45172019480ad3f6f4c74fe41ef4195c0070d080bda0b8d7060a565aea";

        fn ответ(tag: &str, assets: &str) -> String {
            format!(r#"{{"tag_name":"{tag}","html_url":"https://example.test/r","assets":[{assets}]}}"#)
        }

        /// Так ассеты лежат в настоящем релизе v0.2.1: dmg и app.tar.gz на обе
        /// архитектуры и два exe для Windows.
        fn ассеты_как_в_релизе() -> Vec<Asset> {
            [
                "MeetRec_0.2.1_aarch64.app.tar.gz",
                "MeetRec_0.2.1_aarch64.dmg",
                "MeetRec_0.2.1_x64-setup.exe",
                "MeetRec_0.2.1_x64.app.tar.gz",
                "MeetRec_0.2.1_x64.dmg",
                "MeetRec_v0.2.1_x64-portable.exe",
            ]
            .into_iter()
            .map(|n| ассет(n, None))
            .collect()
        }

        #[test]
        fn архив_выбирается_по_архитектуре_а_не_dmg_и_не_exe() {
            let a = ассеты_как_в_релизе();
            assert_eq!(pick_asset(&a, "aarch64").unwrap().name, "MeetRec_0.2.1_aarch64.app.tar.gz");
            assert_eq!(pick_asset(&a, "x86_64").unwrap().name, "MeetRec_0.2.1_x64.app.tar.gz");
        }

        #[test]
        fn нет_архива_под_архитектуру_нет_и_выбора() {
            let только_arm = vec![ассет("MeetRec_0.3.0_aarch64.app.tar.gz", None)];
            assert_eq!(pick_asset(&только_arm, "x86_64"), None);
            assert_eq!(pick_asset(&ассеты_как_в_релизе(), "riscv64"), None);
        }

        #[test]
        fn дайджест_разбирается_только_как_sha256_из_64_hex() {
            assert_eq!(parse_sha256(&format!("sha256:{HEX}")).as_deref(), Some(HEX));
            assert_eq!(
                parse_sha256(&format!("sha256:{}", HEX.to_uppercase())).as_deref(),
                Some(HEX),
                "регистр не важен"
            );
            assert_eq!(parse_sha256(HEX), None, "без префикса алгоритма");
            assert_eq!(parse_sha256(&format!("sha512:{HEX}")), None);
            assert_eq!(parse_sha256("sha256:abc"), None);
            assert_eq!(parse_sha256(&format!("sha256:{}", "z".repeat(64))), None);
        }

        #[test]
        fn sha256_считается_как_у_shasum() {
            // echo -n abc | shasum -a 256
            assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        }

        #[test]
        fn пакет_собирается_из_ответа_github() {
            let json = ответ(
                "v0.3.0",
                &format!(
                    r#"{{"name":"MeetRec_0.3.0_aarch64.dmg","browser_download_url":"https://x/dmg","size":9,"digest":"sha256:{HEX}"}},
                       {{"name":"MeetRec_0.3.0_aarch64.app.tar.gz","browser_download_url":"https://x/arm","size":4702792,"digest":"sha256:{HEX}"}}"#
                ),
            );
            assert_eq!(
                package_for(&json, "0.2.1", "aarch64"),
                Ok(Package {
                    version: "0.3.0".into(),
                    url: "https://x/arm".into(),
                    size: 4702792,
                    sha256: HEX.into()
                })
            );
        }

        #[test]
        fn без_дайджеста_не_ставим() {
            let json = ответ(
                "v0.3.0",
                r#"{"name":"MeetRec_0.3.0_aarch64.app.tar.gz","browser_download_url":"https://x/arm","size":1}"#,
            );
            assert_eq!(package_for(&json, "0.2.1", "aarch64").unwrap_err().key, err::NO_DIGEST);
        }

        #[test]
        fn без_архива_под_архитектуру_понятный_отказ() {
            let json = ответ(
                "v0.3.0",
                &format!(
                    r#"{{"name":"MeetRec_0.3.0_aarch64.app.tar.gz","browser_download_url":"https://x/arm","size":1,"digest":"sha256:{HEX}"}}"#
                ),
            );
            let e = package_for(&json, "0.2.1", "x86_64").unwrap_err();
            assert_eq!(e.key, err::NO_ASSET);
            assert_eq!(e.detail.as_deref(), Some("x86_64"));
        }

        /// Установка заново спрашивает GitHub, и за это время релиз мог
        /// оказаться не новее (например, его откатили) — тогда ставить нечего.
        #[test]
        fn не_новее_запущенного_не_ставим() {
            let json = ответ("v0.2.1", "");
            assert_eq!(package_for(&json, "0.2.1", "aarch64").unwrap_err().key, err::NOT_NEWER);
            assert_eq!(package_for("Not Found", "0.2.1", "aarch64").unwrap_err().key, err::CHECK);
        }

        #[test]
        fn скачанное_сверяется_по_размеру_и_хешу() {
            let pkg = Package {
                version: "0.3.0".into(),
                url: String::new(),
                size: 3,
                sha256: sha256_hex(b"abc"),
            };
            assert_eq!(verify(b"abc", &pkg), Ok(()));
            assert_eq!(verify(b"abd", &pkg).unwrap_err().key, err::CHECKSUM);
            assert_eq!(verify(b"ab", &pkg).unwrap_err().key, err::CHECKSUM, "оборванное");
            assert_eq!(verify(b"abcd", &pkg).unwrap_err().key, err::CHECKSUM, "лишнее");
        }

        #[test]
        fn бандл_выводится_из_пути_бинаря() {
            assert_eq!(
                bundle_from_exe(Path::new("/Applications/MeetRec.app/Contents/MacOS/meeting-recorder-gui")),
                Some(PathBuf::from("/Applications/MeetRec.app"))
            );
            assert_eq!(
                bundle_from_exe(Path::new("/Users/me/Apps/MeetRec 2.app/Contents/MacOS/meeting-recorder-gui")),
                Some(PathBuf::from("/Users/me/Apps/MeetRec 2.app")),
                "переименованная копия тоже бандл"
            );
        }

        /// `npx tauri dev` запускает голый бинарь из `target/`: подменять там
        /// нечего, и это отказ, а не попытка заменить каталог сборки.
        #[test]
        fn запуск_не_из_бандла_не_даёт_пути() {
            assert_eq!(bundle_from_exe(Path::new("/repo/target/debug/meeting-recorder-gui")), None);
            assert_eq!(bundle_from_exe(Path::new("/x/Foo/Contents/MacOS/bin")), None, "не .app");
            assert_eq!(bundle_from_exe(Path::new("/x/Foo.app/Resources/MacOS/bin")), None);
            assert_eq!(bundle_from_exe(Path::new("meeting-recorder-gui")), None);
        }

        #[test]
        fn временные_соседи_скрытые_и_в_том_же_каталоге() {
            let b = Path::new("/Applications/MeetRec.app");
            assert_eq!(staging_path(b), PathBuf::from("/Applications/.MeetRec.app.update"));
            assert_eq!(backup_path(b), PathBuf::from("/Applications/.MeetRec.app.old"));
        }

        #[test]
        fn распакованный_бандл_находится_если_он_один() {
            let d = ScratchDir::new("find");
            assert_eq!(find_app(&d), None);
            бандл(&d.join("MeetRec.app"), ID, "0.3.0", "new");
            std::fs::write(d.join("README"), "").unwrap();
            assert_eq!(find_app(&d), Some(d.join("MeetRec.app")));
            бандл(&d.join("Other.app"), ID, "0.3.0", "other");
            assert_eq!(find_app(&d), None, "два .app — не тот архив");
        }

        #[test]
        fn проверка_бандла_ловит_чужой_идентификатор_и_не_ту_версию() {
            let d = ScratchDir::new("validate");
            let app = d.join("MeetRec.app");
            бандл(&app, ID, "0.3.0", "new");
            assert_eq!(validate_bundle(&app, ID, "0.3.0"), Ok(()));
            assert!(validate_bundle(&app, ID, "0.3.1").is_err(), "не та версия");
            assert!(validate_bundle(&app, "com.example.other", "0.3.0").is_err(), "чужое приложение");
            std::fs::remove_file(app.join("Contents/MacOS/meeting-recorder-gui")).unwrap();
            assert!(validate_bundle(&app, ID, "0.3.0").is_err(), "нет бинаря — нечего запускать");
            assert!(validate_bundle(&d.join("Missing.app"), ID, "0.3.0").is_err());
        }

        #[test]
        fn подмена_ставит_новый_бандл_и_убирает_старый() {
            let d = ScratchDir::new("swap");
            let current = d.join("MeetRec.app");
            let new = d.join("staging").join("MeetRec.app");
            бандл(&current, ID, "0.2.1", "old");
            бандл(&new, ID, "0.3.0", "new");
            let backup = backup_path(&current);
            swap_bundles(&current, &new, &backup).unwrap();
            assert_eq!(метка(&current), "new");
            assert!(!new.exists());
            assert!(!backup.exists(), "старый бандл удалён");
        }

        /// Новый на место не встал — старый обязан вернуться: человек не
        /// должен остаться без приложения из-за неудачного обновления.
        #[test]
        fn неудачная_подмена_возвращает_старый_бандл() {
            let d = ScratchDir::new("rollback");
            let current = d.join("MeetRec.app");
            бандл(&current, ID, "0.2.1", "old");
            let backup = backup_path(&current);
            let нет_такого = d.join("staging").join("MeetRec.app");
            assert!(swap_bundles(&current, &нет_такого, &backup).is_err());
            assert_eq!(метка(&current), "old");
            assert!(!backup.exists());
        }

        #[test]
        fn без_текущего_бандла_ничего_не_трогаем() {
            let d = ScratchDir::new("nocurrent");
            let current = d.join("MeetRec.app");
            let new = d.join("staging").join("MeetRec.app");
            бандл(&new, ID, "0.3.0", "new");
            assert!(swap_bundles(&current, &new, &backup_path(&current)).is_err());
            assert_eq!(метка(&new), "new", "новый остался где был — его уберёт Staging");
            assert!(!current.exists());
        }

        /// Остаток прошлой подмены, который тогда не удалось удалить, не должен
        /// навсегда запереть обновления: `rename` в непустой каталог не идёт.
        #[test]
        fn старый_остаток_подмены_не_мешает_следующей() {
            let d = ScratchDir::new("stale");
            let current = d.join("MeetRec.app");
            let new = d.join("staging").join("MeetRec.app");
            бандл(&current, ID, "0.2.1", "old");
            бандл(&new, ID, "0.3.0", "new");
            бандл(&backup_path(&current), ID, "0.1.0", "stale");
            replace(&current, &new).unwrap();
            assert_eq!(метка(&current), "new");
        }

        #[test]
        fn каталог_распаковки_убирается_за_собой() {
            let d = ScratchDir::new("staging");
            let current = d.join("MeetRec.app");
            бандл(&current, ID, "0.2.1", "old");
            std::fs::create_dir_all(staging_path(&current).join("junk")).unwrap();
            let s = Staging::create(&current).unwrap();
            assert!(s.dir().exists());
            assert!(!s.dir().join("junk").exists(), "остаток прошлой попытки снесён");
            let dir = s.dir().to_path_buf();
            drop(s);
            assert!(!dir.exists());
        }

        /// `.app` в корне архива — так его собирает tauri-action.
        fn архив(d: &Path, имя: &str, подписать: bool) -> Vec<u8> {
            let src = d.join(format!("src-{имя}"));
            бандл(&src.join("MeetRec.app"), ID, "0.3.0", "new");
            if подписать {
                let app = src.join("MeetRec.app");
                run("/usr/bin/codesign", &["--sign".as_ref(), "-".as_ref(), app.as_os_str()]).expect("ad-hoc подпись");
            }
            let tarball = d.join(format!("{имя}.tar.gz"));
            let args = ["-czf".as_ref(), tarball.as_os_str(), "-C".as_ref(), src.as_os_str(), "MeetRec.app".as_ref()];
            run("/usr/bin/tar", &args).unwrap();
            std::fs::read(&tarball).unwrap()
        }

        /// Распаковка через настоящие `tar` и `codesign`; бандл подписан
        /// ad-hoc, как в релизе.
        #[test]
        fn архив_распаковывается_и_проверяется() {
            let d = ScratchDir::new("unpack");
            let current = d.join("MeetRec.app");
            бандл(&current, ID, "0.2.1", "old");
            let archive = архив(&d, "signed", true);

            let staging = Staging::create(&current).unwrap();
            let app = unpack(&staging, &archive, ID, "0.3.0").unwrap();
            assert_eq!(метка(&app), "new");
            drop(staging);

            let staging = Staging::create(&current).unwrap();
            assert_eq!(unpack(&staging, &archive, ID, "0.3.1").unwrap_err().key, err::BAD_BUNDLE);
        }

        /// Бандл без целой подписи на Apple Silicon не запустится — ставить
        /// такой значит оставить человека без приложения.
        #[test]
        fn бандл_без_подписи_не_проходит() {
            let d = ScratchDir::new("unsigned");
            let current = d.join("MeetRec.app");
            бандл(&current, ID, "0.2.1", "old");
            let archive = архив(&d, "unsigned", false);
            let staging = Staging::create(&current).unwrap();
            assert_eq!(unpack(&staging, &archive, ID, "0.3.0").unwrap_err().key, err::BAD_BUNDLE);
        }

        #[test]
        fn не_архив_это_отказ_установки() {
            let d = ScratchDir::new("garbage");
            let current = d.join("MeetRec.app");
            бандл(&current, ID, "0.2.1", "old");
            let staging = Staging::create(&current).unwrap();
            assert_eq!(unpack(&staging, b"not a tarball", ID, "0.3.0").unwrap_err().key, err::INSTALL);
        }

        /// Весь путь на настоящем релизе: ответ GitHub, архив под эту
        /// архитектуру, сверка SHA-256, распаковка и проверка бандла. Бандл
        /// рядом подставной, и подмена не делается — тест ничего не ставит.
        #[tokio::test]
        #[ignore = "ходит в сеть за настоящим релизом — гонять руками"]
        async fn настоящий_релиз_скачивается_и_проходит_проверку() {
            let client = super::super::client().unwrap();
            let json = super::super::fetch_latest_json(&client).await.unwrap();
            let pkg = package_for(&json, "0.0.0", std::env::consts::ARCH).unwrap();
            let archive = download(&client, &pkg, |_, _| {}).await.unwrap();
            let d = ScratchDir::new("live");
            let current = d.join("MeetRec.app");
            бандл(&current, ID, "0.0.0", "old");
            let staging = Staging::create(&current).unwrap();
            let app = unpack(&staging, &archive, ID, &pkg.version).unwrap();
            assert_eq!(validate_bundle(&app, ID, &pkg.version), Ok(()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(v: &str) -> Release {
        Release { version: v.into(), url: format!("https://example.test/{v}") }
    }

    #[test]
    fn версия_с_буквой_v_и_без_разбирается_одинаково() {
        assert_eq!(parse_version("v0.3.1"), Some((0, 3, 1)));
        assert_eq!(parse_version("0.3.1"), Some((0, 3, 1)));
        assert_eq!(parse_version(" v1.0.0 "), Some((1, 0, 0)));
    }

    #[test]
    fn не_три_числа_это_не_версия() {
        assert_eq!(parse_version("0.3"), None);
        assert_eq!(parse_version("0.3.1.4"), None);
        assert_eq!(parse_version("latest"), None);
        assert_eq!(parse_version("0.3.1-beta"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn новее_сравнивается_по_числам_а_не_по_строкам() {
        assert!(is_newer("0.2.0", "0.3.0"));
        assert!(is_newer("0.2.0", "v0.2.1"));
        assert!(is_newer("0.9.0", "0.10.0"), "10 > 9, хотя строка «0.10.0» < «0.9.0»");
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(!is_newer("0.3.0", "0.2.9"));
    }

    /// Неразобранная версия — молчание, а не предложение «обновиться».
    #[test]
    fn мусор_в_версии_не_даёт_обновления() {
        assert!(!is_newer("0.2.0", "latest"));
        assert!(!is_newer("dev", "0.3.0"));
    }

    #[test]
    fn ответ_github_разбирается_в_версию_и_ссылку() {
        let json = r#"{"tag_name":"v0.3.0","html_url":"https://github.com/x/y/releases/tag/v0.3.0","draft":false,"prerelease":false,"name":"MeetRec v0.3.0"}"#;
        assert_eq!(
            parse_latest(json),
            Some(Release {
                version: "0.3.0".into(),
                url: "https://github.com/x/y/releases/tag/v0.3.0".into()
            })
        );
    }

    /// Ассеты в ответе есть всегда; баннеру они не нужны, но и разбор ломать
    /// не должны — ни на macOS, где их читает установка, ни на Windows.
    #[test]
    fn ассеты_в_ответе_не_мешают_разбору() {
        let json = r#"{"tag_name":"v0.3.0","html_url":"u","assets":[{"name":"a","browser_download_url":"b","size":1,"digest":null,"content_type":"application/gzip"}]}"#;
        assert_eq!(parse_latest(json), Some(Release { version: "0.3.0".into(), url: "u".into() }));
    }

    #[test]
    fn черновик_и_prerelease_не_считаются_релизом() {
        let draft = r#"{"tag_name":"v0.3.0","html_url":"u","draft":true}"#;
        let pre = r#"{"tag_name":"v0.3.0","html_url":"u","prerelease":true}"#;
        assert_eq!(parse_latest(draft), None);
        assert_eq!(parse_latest(pre), None);
    }

    #[test]
    fn мусор_вместо_json_не_паникует() {
        assert_eq!(parse_latest("Not Found"), None);
        assert_eq!(parse_latest(r#"{"message":"Not Found"}"#), None);
        assert_eq!(parse_latest(r#"{"tag_name":"nightly","html_url":"u"}"#), None);
    }

    #[test]
    fn баннер_только_на_версию_новее_и_не_пропущенную() {
        assert_eq!(decide("0.2.0", Some(release("0.3.0")), None), Some(release("0.3.0")));
        assert_eq!(decide("0.3.0", Some(release("0.3.0")), None), None);
        assert_eq!(decide("0.2.0", None, None), None);
        assert_eq!(decide("0.2.0", Some(release("0.3.0")), Some("0.3.0")), None, "нажали «Позже»");
        assert_eq!(decide("0.2.0", Some(release("0.3.0")), Some("v0.3.0")), None, "«Позже» с буквой v");
    }

    /// «Позже» гасит один релиз, а не все будущие.
    #[test]
    fn позже_не_гасит_следующий_релиз() {
        assert_eq!(decide("0.2.0", Some(release("0.3.1")), Some("0.3.0")), Some(release("0.3.1")));
    }

    /// Человек нажал «Проверить» сам — «Позже» в баннере не повод ответить
    /// «у вас последняя версия». `check_manually` пропуска даже не получает.
    #[test]
    fn ручная_проверка_показывает_новее_и_ничего_больше() {
        assert_eq!(
            check_manually("0.2.1", release("0.3.0")),
            ManualCheck { current: "0.2.1".into(), latest: Some(release("0.3.0")) }
        );
        assert_eq!(check_manually("0.2.1", release("0.2.1")), ManualCheck { current: "0.2.1".into(), latest: None });
        assert_eq!(
            check_manually("0.3.0", release("0.2.1")).latest,
            None,
            "откат — не обновление"
        );
    }

    #[test]
    fn состояние_хранит_последний_результат() {
        let s = UpdateState::default();
        assert_eq!(s.get(), None);
        s.set(Some(release("0.3.0")));
        assert_eq!(s.get(), Some(release("0.3.0")));
        s.set(None);
        assert_eq!(s.get(), None);
    }

    #[test]
    fn вторая_установка_поверх_идущей_не_начинается() {
        let s = UpdateState::default();
        assert!(s.begin_install());
        assert!(!s.begin_install(), "первая ещё идёт");
        s.end_install();
        assert!(s.begin_install(), "после отказа можно снова");
    }

    #[test]
    fn перезапуск_просится_только_явно() {
        let s = UpdateState::default();
        assert_eq!(s.relaunch_target(), None, "обычный «Выход» — выход, а не перезапуск");
        s.request_relaunch(PathBuf::from("/Applications/MeetRec.app"));
        assert_eq!(s.relaunch_target(), Some(PathBuf::from("/Applications/MeetRec.app")));
    }

    #[test]
    fn отказ_пишется_в_лог_с_ключом_и_подробностью() {
        assert_eq!(UpdateError::new(err::BUSY).to_string(), "update.errBusy");
        assert_eq!(UpdateError::with(err::CHECK, "GitHub ответил 403").to_string(), "update.errCheck: GitHub ответил 403");
    }
}
