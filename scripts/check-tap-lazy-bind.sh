#!/usr/bin/env bash
#
# Проверка после сборки: символы Core Audio Process Tap в собранном бинаре
# связываются ЛЕНИВО.
#
# Зачем. `unsupported_reason()` в `src/capture/macos.rs` объясняет человеку на
# старой macOS, что нужна 14.4. Дойти до этого объяснения процесс может только
# потому, что `AudioHardwareCreateProcessTap` связывается лениво: символ не
# weak-import, и при жадном связывании dyld убил бы процесс ещё до `main`.
# Ленивое связывание держится ровно до deployment target 11.x — с 12.0 ld
# переключается на chained fixups, где ленивого связывания нет. Полное
# рассуждение и замеры — в докблоке `MIN_MACOS` (`src/capture/macos.rs`).
#
# Что ломает проверку. Правка `bundle.macOS.minimumSystemVersion` в
# `src-tauri/tauri.conf.json` на 12.0 или выше (например, «приведём в
# соответствие с 14.4») — тесты при этом остаются зелёными, сборка проходит,
# и отказ на старой системе молча превращается в падение dyld.
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

# Достаточно `lazy-bind` (нынешнее состояние) ИЛИ `weak-bind` — если импорты
# когда-нибудь сделают weak, жадное связывание перестанет быть смертельным, и
# отказ снова будет доходить до человека.
if printf '%s\n' "$TAP" | grep -qE 'lazy-bind|weak-bind'; then
	echo "OK: символы тапа связываются лениво/слабо — guard версии macOS достижим."
	printf '%s\n' "$TAP"
	exit 0
fi

MINOS="$(otool -l "$BIN" | awk '/LC_BUILD_VERSION/{f=1} f&&/minos/{print $2; exit}')"
cat >&2 <<EOF
ПРОВАЛ: символы Core Audio Process Tap связываются ЖАДНО.

$TAP

LC_BUILD_VERSION minos = ${MINOS:-неизвестно}

Значит, на macOS старее 14.4 процесс умрёт в dyld с
"Symbol not found: _AudioHardwareCreateProcessTap" ДО main, и сообщение
"нужна macOS 14.4 или новее" из src/capture/macos.rs никто не увидит.

Почти наверняка причина: bundle.macOS.minimumSystemVersion в
src-tauri/tauri.conf.json стал 12.0 или выше. Этот ключ задаёт не только
LSMinimumSystemVersion, но и MACOSX_DEPLOYMENT_TARGET. Верните 11.0 —
или сделайте импорты weak, см. докблок MIN_MACOS в src/capture/macos.rs.
EOF
exit 1
