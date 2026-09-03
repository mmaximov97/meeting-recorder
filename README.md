<p align="center">
  <img src="ui/assets/logo-white-red.svg#gh-dark-mode-only" width="96" alt="MeetRec">
  <img src="ui/assets/logo-black-red.svg#gh-light-mode-only" width="96" alt="MeetRec">
</p>

<h1 align="center">MeetRec</h1>

<p align="center">
  Records your calls from your own machine. No bot joins the meeting, and the files stay on your disk.
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT"></a>
  <img src="https://img.shields.io/badge/macOS-14.4%2B-lightgrey" alt="macOS 14.4+">
  <img src="https://img.shields.io/badge/Windows-10%2F11-lightgrey" alt="Windows 10/11">
  <a href="README.ru.md">Русская версия</a>
</p>

---

MeetRec notices when a call starts, offers to record it, and writes two separate tracks: your microphone and the system audio. Afterwards it can turn the recording into text with the speakers kept apart.

<!-- TODO: заменить на настоящий скриншот или gif, когда будет чем снять -->
<p align="center"><em>Screenshot goes here.</em></p>

## What it does

- **Notices calls.** Zoom, Teams, Slack and Discord are recognised by their process. A small panel appears over the call window and asks whether to record.
- **Keeps the first seconds.** Audio runs through a ring buffer, so recording starts a few seconds before you answer the question. Opening lines are not lost.
- **Two separate tracks.** Your microphone and the system audio go into separate WAV files. You and the other side never overlap.
- **Stays out of the way.** An icon in the menu bar, a global shortcut, monthly folders, renaming, per-device microphone choice.

## What makes it different

Most meeting recorders put a bot into the call and keep the files on their servers. MeetRec takes the audio from your own machine and writes it to your own disk. Nobody in the call sees another participant, and no recording leaves the computer unless you ask for a transcript.

## Install

Download the latest build from [Releases](../../releases).

**macOS** — `MeetRec_x.y.z_aarch64.dmg` for Apple Silicon, `MeetRec_x.y.z_x64.dmg` for Intel.

The app is not signed with an Apple developer certificate, so the first launch is blocked. Right-click the app and choose Open, or run:

```sh
xattr -cr /Applications/MeetRec.app
```

**Windows** — `MeetRec_x.y.z_x64-setup.exe` to install, or `MeetRec_vx.y.z_x64-portable.exe` to run without installing.

## Requirements

- **macOS 14.4 or newer.** System audio is captured through the Core Audio process tap API, which does not exist in earlier versions.
- **Windows 10 or 11.** System audio goes through WASAPI loopback.

## Permissions

On macOS the system asks twice: once for the microphone, once for screen and system audio recording. Both are needed — without the second one, only your own voice is recorded and calls are not detected at all, because MeetRec recognises a call by the system audio.

The permission is tied to the exact binary. After you replace the app with a new build, macOS may ask again.

On Windows no separate permission is required.

## Where the files go

```
~/Recordings/2026-09/
  2026-09-02_14-30_zoom.mic.wav       your microphone
  2026-09-02_14-30_zoom.system.wav    everyone else
  2026-09-02_14-30_zoom.transcript/   text, if you asked for it
```

On Windows the same tree lives in `%USERPROFILE%\Recordings\`.

## Transcription and privacy

Recording is entirely local. Transcription is not, and this is worth being precise about.

MeetRec does not ship with a transcription server. You put the address and the access key of your own gateway into settings, and the audio files are uploaded there, one track at a time, over HTTPS. If you leave those fields empty, nothing is ever sent anywhere and the app is a plain local recorder.

If you don't have a gateway, [selfhost-ai-lab](https://github.com/mmaximov97/selfhost-ai-lab) is one you can run on your own hardware. It speaks the API MeetRec expects — `POST /v1/audio/transcriptions/async` to submit a track, `GET /v1/jobs/:id` to poll it — and setting it up is documented there. Any server exposing the same two endpoints will do.

A local mode, where the audio is transcribed on your own machine and nothing leaves it, is in progress.

## Shortcuts

`Ctrl+Shift+R` starts and stops recording. It works with the window closed.

## Build from source

Requires Rust and Node.

```sh
git clone https://github.com/mmaximov97/meeting-recorder
cd meeting-recorder
cargo test --workspace
npm install
npx tauri build
```

Use `npx tauri build`, not `cargo tauri build`.

Every push runs the test suite and a build on both macOS and Windows. Tagging `vX.Y.Z` builds and publishes the installers.

## Contributing

Issues and pull requests are welcome. Two things worth knowing before you open one:

- The code and its comments are written in Russian. Function and variable names too. Pull requests in either language are fine.
- The audio path is covered by tests, and they are expected to stay green. Run `cargo test --workspace` before you push.

## Authors

<!-- TODO: подставить ссылку на Cypher Products, когда будет сайт или страница -->
Built by [Cypher Products](#).

- Mikhail Maksimov — development — [github.com/mmaximov97](https://github.com/mmaximov97)
- Anna Dorogova — design — [adorogova.com](https://adorogova.com) · [github.com/blinbirka](https://github.com/blinbirka)

## Support

MeetRec is free and always will be. If it saved you an hour, you can buy the two of us a coffee.

**USDT, TRON network (TRC-20)**

```
TFzpPkaSRQXLCEg9ZYf4MiwHNbzD4bYCgE
```

Send only on the TRON network. A transfer on any other network cannot be recovered.

## License

MIT. See [LICENSE](LICENSE).
