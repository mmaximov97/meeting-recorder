//! Ядро записи встреч: детект, машина состояний, кольцо, захват, диск.
//!
//! Крейт собирается и как библиотека, и как консольный бинарь
//! (`meeting-recorder-cli`, см. `main.rs`). Библиотекой он стал ради Tauri-крейта
//! в `src-tauri/`, которому нужны те же `App`/`WindowsDetector`/`poll_to_event`.
//!
//! Консольный бинарь при этом остаётся и не является legacy: это единственный
//! способ прогнать ядро без GUI — то есть отладить детект и запись, не поднимая
//! webview.
//!
//! # Что здесь `!Send` и почему это важно потребителю
//!
//! [`detector::WindowsDetector`] держит `IMMDeviceEnumerator` (COM привязан к
//! апартаменту потока), а захват внутри [`app::App`] — `cpal::Stream`. Оба
//! `!Send`, поэтому и детектор, и `App` обязаны конструироваться на том потоке,
//! где они будут жить, и через границу потока не передаются. GUI обязан
//! общаться с ними сообщениями, а не шарингом.

pub mod app;
pub mod capture;
pub mod detector;
pub mod ringbuf;
pub mod session;
pub mod storage;

#[cfg(test)]
mod tests {
    /// Караул над флагом, без которого отказ на старой macOS превращается в
    /// падение загрузчика.
    ///
    /// `-Wl,-weak_framework,CoreAudio` в `build.rs` — единственное, чем держится
    /// достижимость `capture::macos::unsupported_reason`: без него отсутствующий
    /// на macOS до 14.4 `AudioHardwareCreateProcessTap` убивает процесс в dyld
    /// ещё до `main`. Рассуждение целиком — в докблоке `MIN_MACOS`
    /// (`src/capture/macos.rs`).
    ///
    /// Пропажа флага не ломает ни сборку, ни один другой тест, и заметить её
    /// можно только на живой macOS ниже 14.4 — то есть, скорее всего, никогда.
    /// Отсюда сторож.
    ///
    /// Файл берётся `include_str!`: так тест не зависит от рабочего каталога, а
    /// исчезновение файла всплывает уже при компиляции. `cfg(target_os)` нет
    /// намеренно — `build.rs` правят и не с macOS, а тест должен падать у того,
    /// кто правит.
    #[test]
    fn корневой_крейт_линкует_coreaudio_слабо() {
        const BUILD_RS: &str = include_str!("../build.rs");
        assert!(
            BUILD_RS.contains("-Wl,-weak_framework,CoreAudio"),
            "\n\
             В build.rs корневого крейта пропал флаг слабой линковки CoreAudio:\n\
             \x20   println!(\"cargo:rustc-link-arg=-Wl,-weak_framework,CoreAudio\");\n\
             \n\
             Без него символы Core Audio Process Tap связываются жадно, и на macOS\n\
             старее 14.4 dyld убивает процесс с \"Symbol not found\" ДО main —\n\
             вместо объяснения \"нужна macOS 14.4 или новее\" из \
             capture::macos::unsupported_reason.\n\
             \n\
             Ленивое связывание подстраховкой не служит: на ld-1053.12 его нет уже\n\
             при deployment target 11.0. Проверка на собранном бинаре:\n\
             \x20   scripts/check-tap-lazy-bind.sh\n"
        );
    }
}
