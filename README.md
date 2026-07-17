# meeting-recorder

Захват аудио со встреч на Windows: замечает начало звонка, предлагает записать, пишет две
раздельные дорожки (микрофон и системный звук) в `C:\Users\<username>\Recordings`.

Урезанный MVP «открытого аналога Granola» — **только запись**. Транскрипция делается вручную
через ai-lab (скилл `ailab-transcribe`) и в это приложение не входит.

## Статус

Спека написана, реализации ещё нет.

## Документация

Дизайн и обоснование решений живут в Obsidian-vault, не здесь:

- Спека: `<vault>/Brainstorming/Открытый аналог Granola/2026-07-17-meeting-recorder-mvp-design.md`
- Ресёрч, на который она опирается: `<vault>/Brainstorming/Открытый аналог Granola/` (MOC + 5 заметок)

## Стек

Rust + Tauri. Windows сейчас, macOS заложен точкой расширения (трейты `MeetingDetector` и
`AudioSource`), но не проработан.

- захват: `cpal` / WASAPI loopback
- детект встречи: WASAPI audio sessions через `windows-rs`
- UI: Tauri (трей + окно со списком) + плагин `global-shortcut`

## Важно

Аудио пишется **вне** этого репозитория и вне vault — vault синкается git'ом с автокоммитами,
~150 МБ на встречу туда попасть не должны.
