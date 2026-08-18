// CoreAudio на macOS линкуется СЛАБО. Не удобство и не оптимизация: без этого
// guard версии в `src/capture/macos.rs` недостижим — символы Process Tap,
// которых нет на macOS старее 14.4, убивают процесс в dyld до `main`, и
// сообщение «нужна macOS 14.4 или новее» никто не видит. Со слабым импортом
// отсутствующий символ становится нулевым указателем, процесс доживает до
// `main` и отказывает словами. Полное рассуждение и замеры — в докблоке
// `MIN_MACOS` (`src/capture/macos.rs`).
//
// Флаг ложится ПОВЕРХ `-framework CoreAudio`, который приезжает от
// objc2-core-audio: проверено C-пробником, weak-import выигрывает при любом
// порядке флагов. Тот же флаг нужен и GUI-крейту — у него свой линк, свой
// `src-tauri/build.rs`.
//
// Проверка на собранном бинаре: `scripts/check-tap-lazy-bind.sh`. Сторож этой
// строки — тест `корневой_крейт_линкует_coreaudio_слабо` в `src/lib.rs` (там, а
// не в `capture::macos`, чтобы падал и при сборке не с macOS).
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-weak_framework,CoreAudio");
    }
}
