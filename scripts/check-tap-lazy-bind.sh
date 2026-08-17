#!/usr/bin/env bash
#
# Проверка после сборки: отсутствие символов Core Audio Process Tap не убьёт
# процесс в dyld до `main`.
#
# Зачем. `unsupported_reason()` в `src/capture/macos.rs` объясняет человеку на
# старой macOS, что нужна 14.4. Дойти до этого объяснения процесс может только
# если загрузчик не споткнётся о `AudioHardwareCreateProcessTap`, которого на
# системах старее 14.4 нет вовсе. Достаточно любого из двух: символ помечен
# `[weak-import]` (нынешняя опора — `-Wl,-weak_framework,CoreAudio` в обоих
# `build.rs`) либо связывается лениво (`lazy-bind`, прежняя опора). Полное
# рассуждение и замеры — в докблоке `MIN_MACOS` (`src/capture/macos.rs`).
#
# Что ломает проверку. Пропажа `-weak_framework CoreAudio` из `build.rs` —
# корневого или `src-tauri/`. Ленивое связывание подстраховкой больше не служит:
# на `ld-1053.12` его нет уже при deployment target 11.0. Тесты при этом
# остаются зелёными, сборка проходит, и отказ на старой системе молча
# превращается в падение загрузчика.
#
# Запуск (после `npx tauri build`):
#     scripts/check-tap-lazy-bind.sh
#
# Или по явному пути к любому Mach-O:
#     scripts/check-tap-lazy-bind.sh path/to/binary
#
# Молчаливо пропустить проверку нельзя: отсутствие бандла — это провал, а не
# «нечего проверять». Иначе проверка, которая ничего не проверяет, выглядела бы
# так же, как проверка, которая прошла.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEFAULT_BIN="$ROOT/target/release/bundle/macos/meeting-recorder.app/Contents/MacOS/meeting-recorder-gui"
BIN="${1:-$DEFAULT_BIN}"

if [[ "$(uname -s)" != "Darwin" ]]; then
	echo "проверка имеет смысл только на macOS (здесь: $(uname -s))" >&2
	exit 2
fi

if [[ ! -f "$BIN" ]]; then
	echo "нечего проверять: $BIN не найден" >&2
	echo "сначала соберите бандл: npx tauri build" >&2
	exit 2
fi

FIXUPS="$(xcrun dyld_info -fixups "$BIN")"
TAP="$(printf '%s\n' "$FIXUPS" | grep -i 'ProcessTap' || true)"

if [[ -z "$TAP" ]]; then
	echo "ПРОВАЛ: в $BIN нет импортов *ProcessTap* вообще." >&2
	echo "Либо бинарь не тот, либо тап перестал линковаться — проверьте вручную:" >&2
	echo "    xcrun dyld_info -fixups '$BIN'" >&2
	exit 1
fi

# `weak-import` — нынешняя опора: отсутствующий символ становится нулевым
# указателем, и жадное связывание перестаёт быть смертельным. `lazy-bind` —
# прежняя, она тоже годится: судьбу процесса решает первый вызов, а не загрузка.
if printf '%s\n' "$TAP" | grep -qE 'weak-import|weak-bind|lazy-bind'; then
	echo "OK: отсутствие символов тапа не убьёт загрузку — guard версии macOS достижим."
	printf '%s\n' "$TAP"
	exit 0
fi

MINOS="$(otool -l "$BIN" | awk '/LC_BUILD_VERSION/{f=1} f&&/minos/{print $2; exit}')"
cat >&2 <<EOF
ПРОВАЛ: символы Core Audio Process Tap связываются ЖАДНО и не помечены weak.

$TAP

LC_BUILD_VERSION minos = ${MINOS:-неизвестно}

Значит, на macOS старее 14.4 процесс умрёт в dyld с
"Symbol not found: _AudioHardwareCreateProcessTap" ДО main, и сообщение
"нужна macOS 14.4 или новее" из src/capture/macos.rs никто не увидит.

Почти наверняка причина: из build.rs пропал флаг
    println!("cargo:rustc-link-arg=-Wl,-weak_framework,CoreAudio");
Он нужен в ДВУХ файлах — в корневом build.rs и в src-tauri/build.rs: линк у
консольного и у GUI-бинаря отдельный, директива между крейтами не наследуется.

Проверять сам флаг, не дожидаясь сборки, умеют юнит-тесты
корневой_крейт_линкует_coreaudio_слабо (src/lib.rs) и
gui_крейт_линкует_coreaudio_слабо (src-tauri/src/main.rs).

Не пытайтесь вернуть ленивое связывание правкой deployment target: на
ld-1053.12 его нет уже при 11.0. Докблок MIN_MACOS в src/capture/macos.rs.
EOF
exit 1
