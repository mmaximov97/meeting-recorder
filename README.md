# meeting-recorder

Records your meetings on Windows and macOS: it notices a call starting, offers to record, and
writes **two separate tracks** — your microphone and the system audio — as plain WAV files on
disk. Optionally it transcribes them with speaker labels through a self-hosted gateway.

A trimmed-down, open MVP in the spirit of Granola. Two tracks instead of one is the whole
point: keeping your voice and everyone else's in separate files is what makes speaker
diarisation reliable afterwards, instead of guessing who spoke from a single mixed recording.

> **The application UI is in Russian.** So are the source comments and the design documents
> under `docs/`. This README is the only English-language part of the project. If that is a
> blocker for you, it is a fair reason to skip this project — translating the UI is not
> currently planned.

## Requirements

| | |
|---|---|
| Windows | 10 or 11, x64 |
| macOS | **14.4 or newer** — the system-audio track uses the Core Audio Process Tap API, which does not exist before 14.4. On an older macOS the app refuses to start with an explicit message rather than silently losing the other party's audio. |
| To build | Rust stable (edition 2021), Node.js for the Tauri 2 CLI, and Xcode command line tools on macOS |
| To transcribe (optional) | An OpenAI-compatible STT gateway — see [Transcription](#transcription) |

## Install

You don't have to build it. Every `vX.Y.Z` tag makes GitHub Actions
(`.github/workflows/release.yml`) build Windows and both macOS architectures and publish them
to [Releases](https://github.com/mmaximov97/meeting-recorder/releases).

| Platform | File | What to do with it |
|---|---|---|
| Windows, installed | `meeting-recorder_X.Y.Z_x64-setup.exe` | An ordinary NSIS installer — run it and follow the wizard |
| Windows, portable | `meeting-recorder_vX.Y.Z_x64-portable.exe` | No install — put it anywhere and run it; touches neither the registry nor Program Files |
| Mac, Apple Silicon (M1 and newer) | `meeting-recorder_X.Y.Z_aarch64.dmg` | Open the `.dmg`, drag to Applications |
| Mac, Intel | `meeting-recorder_X.Y.Z_x64.dmg` | Same |

Not sure which Mac you have: Apple menu (top-left of the screen) → `About This Mac` → the "Chip" line. `Apple M…` means
Apple Silicon, `Intel` means Intel.

There is no portable build for macOS. Gatekeeper blocks an unsigned bare binary exactly as it
blocks a `.app`, but without the "Open anyway" dialog you'd use to get around it — the `.dmg`
is genuinely the easier path.

The `*.app.tar.gz` files next to the `.dmg` are not for manual installation. They exist for
Tauri's updater, in case auto-update is ever wired up; nothing uses them today.

### macOS will complain on first launch

The build is **not signed with an Apple Developer ID** (`signingIdentity: "-"`, i.e. ad-hoc —
see [Building](#building-on-macos)). So instead of double-clicking: right-click the app in
Applications → **Open** → confirm in the dialog. A plain Finder double-click gets refused
silently. If even that doesn't work:

```bash
xattr -cr /Applications/meeting-recorder.app
```

## Usage

On Windows the app runs **without a terminal** — double-click the exe or use the desktop
shortcut:

```
target\release\meeting-recorder-gui.exe
```

Don't launch the GUI from a WSL terminal: when the terminal closes, WSL kills the whole tree of
Windows processes along with the app.

On macOS you run the bundle:

```
target/release/bundle/macos/meeting-recorder.app
```

The first launch on macOS asks for **two different** system permissions — microphone and system
audio capture. They are separate categories, and without both the recording is incomplete.
Because the signature is ad-hoc it has no stable Team ID, and TCC ties permissions to the
binary's hash — so permissions may be requested again after a rebuild.

Day to day:

- **Tray icon** → the window with the list of recordings.
- **"Начать запись" / "Остановить запись"** button, or the global hotkey **Ctrl+Shift+R**. On
  macOS it is Ctrl too, not Cmd — deliberately, so the muscle memory carries across machines.
- When a call is detected the app offers to record it by itself (a "Записать?" toast). On macOS
  detection only knows `zoom.us`, `Microsoft Teams` and `Slack` by process name, and also
  requires that system audio is actually playing. Browsers are excluded on purpose: from inside
  Chrome a Google Meet tab is indistinguishable from any other tab — such a meeting won't be
  auto-detected, but the button and the hotkey work for it as usual.
- **"Микрофон"** dropdown — pick the input device. The choice survives restarts; if the device
  goes missing, recording falls back to the system default and the window shows a warning.
- **"Проверить"** button — opens the microphone and shows a level meter for both tracks, so you
  can confirm the mic being recorded is the one you're talking into. It switches itself off
  after a minute.
- **Files**: two WAV tracks in `%USERPROFILE%\Recordings\YYYY-MM\` on Windows and
  `~/Recordings/YYYY-MM/` on macOS. The month folder is created automatically.
- **"Переименовать"** on a row changes the tail of the name — date and time stay fixed —
  renaming both tracks and the transcript folder together.

Recordings deliberately land **outside** the repository: a meeting is roughly 150 MB, which has
no business in git. On macOS the root is `~/Recordings` rather than `~/Documents` for a second
reason — Documents is synced to iCloud Drive by default.

## Transcription

Transcription and diarisation are built into the app, but they need an external
OpenAI-compatible STT gateway — this project does not run a speech model itself. It was built
against [selfhost-ai-lab](https://github.com/mmaximov97/selfhost-ai-lab), a self-hosted gateway
that exposes local Whisper and diarisation over an OpenAI-shaped API; anything serving
`POST /v1/audio/transcriptions/async` the same way should work.

Configure it once in the **"Транскрипция"** section of the window — two fields, gateway URL and
key. They save on blur (Tab or a click elsewhere), there is no separate save button.

- **Gateway URL** — base URL only, e.g. `http://your-gateway.local:8080`. No `/v1/...` suffix;
  the client appends it.
- **Key** — an API key with the `stt` scope. Issue one key per person rather than sharing one,
  so access can be revoked individually.

A recording that has **both** tracks (mic + system) grows a **"Транскрибировать"** button.
Clicking it queues the recording: if nothing is being processed it starts immediately,
otherwise the button shows "В очереди (N)" and it waits its turn. Exactly one recording is
processed at a time, because GPU capacity on the gateway is not elastic. Progress runs
"Загрузка…" → "Обработка…" → "Слияние…" → "Готово".

The result is a `<recording>.transcript/` folder with a `.md` carrying timestamps and speaker
labels, and a `.txt` of plain running text.

Diarisation is real: the gateway distinguishes voices *within* each track (`diarize=true` in
the request). If the `system` track has more than one participant, they get distinct labels —
"Собеседники (1)", "Собеседники (2)" — numbered in order of first appearance. A single voice on
a track gets no number, just "Собеседники".

## Building (on Windows, from WSL)

Windows toolchain only, through interop — a plain `cargo` would build a Linux binary:

```bash
cargo.exe build --workspace                          # debug
cargo.exe build --release -p meeting-recorder-gui
cargo.exe test --workspace
```

`meeting-recorder-cli.exe` is the console build of the core without a GUI. It is a debugging
tool, not leftover junk — it's the only way to exercise detection and recording without a
webview. Don't delete it.

## Building (on macOS)

Natively, no WSL scaffolding — Xcode command line tools and a Rust toolchain are enough:

```bash
cargo build --workspace            # debug
cargo test --workspace

npm install                        # @tauri-apps/cli, from package.json
npx tauri build                    # release .app and .dmg
```

It has to be `npx tauri`, not `cargo tauri`: the CLI lives in `package.json`, not as a `cargo`
subcommand. Artifacts land in `target/release/bundle/` (`macos/*.app`, `dmg/*.dmg`) — the
workspace has a single `target/` at the repository root, not one inside `src-tauri/`.

The signature is ad-hoc: the `.dmg` is neither signed with a Developer ID nor notarised, so on
someone else's machine Gatekeeper will meet it.

### Three settings that look like oversights and must not be "fixed"

- **`-Wl,-weak_framework,CoreAudio` in both `build.rs` files** (root and `src-tauri/`). This is
  what makes the "needs macOS 14.4" refusal possible at all: without weak linking, the missing
  Process Tap symbol kills the process inside dyld before `main`, and nobody ever sees the
  explanation. Two unit tests guard the flag — `корневой_крейт_линкует_coreaudio_слабо`
  (`src/lib.rs`) and `gui_крейт_линкует_coreaudio_слабо` (`src-tauri/src/main.rs`); the built
  binary is checked by `npm run check-tap-lazy-bind`. Full reasoning and measurements are in the
  `MIN_MACOS` doc block in `src/capture/macos.rs`. The previous approach — relying on lazy
  binding at deployment target 11.x — turned out to depend on the linker version: on `ld-1053.12`
  (CLT 15.3) binding is eager even at 11.0. Editing `minimumSystemVersion` will not bring it
  back; don't try.
- **`bundle.macOS.minimumSystemVersion` is `11.0`** even though the app needs 14.4. That key is
  `LSMinimumSystemVersion`, i.e. the Finder gate: set it to 14.4 and the OS refuses in its own
  words instead of ours, and the user never learns what is actually missing. Guarded by the unit
  test `минимальная_версия_macos_в_бандле_осталась_11_0` (`src-tauri/src/main.rs`).
- **`bundle.macOS.hardenedRuntime` is `false`** on purpose. With hardened runtime enabled and no
  entitlements file, macOS blocks microphone access silently — no dialog, no error. The price is
  that notarisation is impossible in this configuration; it was never in scope.

## Project status

This is an MVP that works, not a polished product. Everything in the MVP plan
(`docs/2026-07-17-meeting-recorder-mvp-plan.md`) is done — detector, ring buffer, state machine,
WAV storage, mic + loopback capture, console build and the Tauri shell (tray, toast, window with
the recordings list, global hotkey). So is the follow-up work on microphone selection, monthly
folders and renaming (`docs/2026-07-30-device-folders-rename-plan.md`), and the macOS port
(`docs/2026-08-10-macos-port-plan.md`).

**What has actually been verified, and where:**

- On macOS the system track was confirmed correct on three output devices — built-in speakers,
  AirPods and a USB headset. Tray icon, level meters, surviving window close, permission dialogs
  and the resulting audio were all checked by hand in the GUI. On the packaged `.app`: no Dock
  icon, tray opens the window, recording writes both tracks.
- The mic-only path (system-audio permission denied) was verified too: banner visible, window
  opens by itself, a single `.mic.wav` with no pair lands on disk. Force it with
  `MR_FORCE_NO_SYSTEM_AUDIO=1`, because `tccutil` cannot revoke that permission again.
- The "needs macOS 14.4" refusal was confirmed on a real macOS 14.3 machine. macOS 13 and below
  are still covered by tests only — no hardware available.
- Re-checked on a second machine and a different major version, macOS 26.5: `npx tauri build`
  produces `.app` and `.dmg`, `cargo test --workspace` is green, `npm run check-tap-lazy-bind`
  passes.
- **Windows was rebuilt and tested after the macOS port**, on the real Windows toolchain (not
  cross-compiled) — 131 core-crate tests and 92 GUI-crate tests green, `cargo build` clean for
  both the debug and release profiles. The shared-code changes from the port (see
  `docs/2026-08-10-macos-port-plan.md` for the list) checked out; the most user-visible one is
  that a manual start no longer prepends audio that was playing *before* you pressed the button.
  The release pipeline itself is Windows proof by construction: every tagged release runs a
  `windows-latest` GitHub Actions job that builds both the NSIS installer and the portable `.exe`
  from scratch.

**Known gaps — read these before relying on it:**

- **A second instance goes unnoticed.** The `single-instance` plugin is not in the build, and
  `open -a` on an already-running app starts a second copy instead of surfacing the first one.
  Observed live. Two copies at once both hold the microphone and the Process Tap and write into
  the same folder.
- **The tray icon can be invisible, and then nothing opens the window.** In a crowded menu bar
  items get pushed off the left edge — on the test machine neighbouring items sat at `x=7` and
  `x=-1` on a 1512-point-wide screen. There is no "open window" item in the tray menu, and a
  synthetic click via the Accessibility API does not raise it; a real left click on the icon is
  required. The `Ctrl+Shift+R` hotkey still works in that state, and it can still start
  recordings without the icon.
- **`bundle_dmg.sh` can trip over leftovers from an interrupted run.** A `.dmg` build failed once
  while a `/Volumes/dmg.*` from a previous attempt was still mounted; `hdiutil detach` and it
  built with no other change. Causation is unproven — it never reproduced — so treat it as a lead,
  not a diagnosis.

## Architecture

Rust + Tauri 2, two platforms. Platform-specific code lives behind the `MeetingDetector` and
`AudioSource` traits; everything else is shared.

| Concern | Windows | macOS |
|---|---|---|
| Microphone capture | `cpal` | `cpal` |
| System audio | WASAPI loopback (`cpal`) | Core Audio Process Tap (`src/capture/macos.rs`) — hence the 14.4 requirement |
| Meeting detection | WASAPI audio sessions via `windows-rs` | `sysinfo` on known process names, gated on system audio actually playing |
| UI | Tauri tray + window, `global-shortcut` plugin | same |

```
src/            core: detector, ring buffer, state machine, WAV storage, capture
src-tauri/      Tauri shell: tray, window commands, config, transcription client
ui/             the window itself — plain HTML + JS, no framework
docs/           design and implementation documents (Russian)
scripts/        build checks and a one-off recordings migration
```

## Releasing

For maintainers. Bump `version` in **two** places first, or the file names in Releases won't
match the tag — which happened once: a release tagged `v0.1.1` containing
`meeting-recorder_0.1.0_x64-setup.exe`, because `tauri` takes the version for the file name from
the config, not from the git tag.

- `src-tauri/tauri.conf.json` → `"version"`
- `package.json` → `"version"`

Then tag with the same number:

```bash
git tag v0.2.0
git push origin v0.2.0
```

The workflow does the rest: three parallel builds (~10–15 minutes), published to Releases as
soon as they pass — no drafts, `releaseDraft: false`. If a single platform fails,
`workflow_dispatch` in the Actions tab re-runs everything without a new tag.

## Contributing

Issues and pull requests are welcome. Two things worth knowing before you open one: the source
comments and design docs are in Russian, and platform-specific changes really do need to be
built on that platform — see the Windows gap above for what happens otherwise.

## License

MIT — see [LICENSE](LICENSE).
