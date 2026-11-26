/// Ключи Google для календаря. Переменные окружения главнее (так их отдаёт
/// CI из секретов); если их нет — `~/.config/meetrec/google-oauth.env`,
/// строки `MEETREC_GOOGLE_CLIENT_ID=…` и `MEETREC_GOOGLE_CLIENT_SECRET=…`.
/// Файл лежит вне репозитория и в git не попадает.
///
/// Нет файла — нет и `rerun-if-changed` на него: иначе cargo пересобирал бы
/// крейт на каждом запуске. Создала файл позже — `touch src-tauri/build.rs`.
fn google_keys_from_file() {
    const NAMES: [&str; 2] = ["MEETREC_GOOGLE_CLIENT_ID", "MEETREC_GOOGLE_CLIENT_SECRET"];
    for name in NAMES {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let Some(home) = std::env::var_os("HOME") else { return };
    let path = std::path::Path::new(&home).join(".config/meetrec/google-oauth.env");
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    println!("cargo:rerun-if-changed={}", path.display());
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim();
        if !NAMES.contains(&k) {
            continue;
        }
        let from_env = std::env::var(k).map(|s| !s.trim().is_empty()).unwrap_or(false);
        if from_env {
            continue;
        }
        let v = v.trim().trim_matches('"').trim_matches('\'');
        println!("cargo:rustc-env={k}={v}");
    }
}

// Слабая линковка CoreAudio — та же и по той же причине, что в корневом
// `build.rs`: без неё символы Process Tap на macOS старее 14.4 убивают процесс в
// dyld до `main`, и guard версии из `src/capture/macos.rs` не успевает сказать
// человеку, что нужна 14.4. Директива не наследуется от крейта-зависимости:
// линк у этого бинаря свой, значит и флаг нужен свой.
//
// Сторож этой строки — тест `gui_крейт_линкует_coreaudio_слабо` в
// `src-tauri/src/main.rs`.
fn main() {
    google_keys_from_file();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-weak_framework,CoreAudio");
        // ScreenCaptureKit (запись экрана, src/screen/macos.rs) — по той же
        // причине: на системе без него процесс должен дожить до проверки версии.
        println!("cargo:rustc-link-arg=-Wl,-weak_framework,ScreenCaptureKit");
    }
    tauri_build::build()
}
