# Sleepless

A tiny, windowless Windows utility. When run, it:

1. Reads your current **sleep** and **screen-off** timeouts (both AC and battery) and saves them.
2. Sets both to **Never**.
3. Points the screensaver at Windows' built-in **blank** screensaver and starts it.
4. Waits until you dismiss the screensaver (move the mouse / press a key), detected via
   `SystemParametersInfo(SPI_GETSCREENSAVERRUNNING)`.
5. Restores your original timeouts and screensaver settings, then exits.

No window, no tray icon, no console — unless you pass `/debug`, in which case it attaches to
the console you launched it from and logs every step.

## Build

You'll need the Rust toolchain with the MSVC target (default on Windows):

```
cargo build --release
```

The binary will be at `target\release\sleepless.exe`.

## Run

```
sleepless.exe            REM silent, no windows
sleepless.exe /debug     REM logs everything to the current cmd window
```

## How it works, briefly

- **Timeouts**: `PowerGetActiveScheme` + `PowerReadACValueIndex`/`PowerReadDCValueIndex` on the
  `GUID_STANDBY_IDLE` (sleep) and `GUID_VIDEO_POWERDOWN_TIMEOUT` (screen) settings, then
  `PowerWriteACValueIndex`/`PowerWriteDCValueIndex` + `PowerSetActiveScheme` to apply `0` ("Never").
  This only touches your *active* power plan and does not require admin rights, same as dragging
  the sliders in Settings > Power & sleep.
- **Screensaver**: temporarily rewrites `HKCU\Control Panel\Desktop\SCRNSAVE.EXE` to
  `%WINDIR%\System32\scrnsave.scr` (the built-in blank saver), enables it via
  `SPI_SETSCREENSAVEACTIVE`, and force-starts it with the standard
  `SendMessage(HWND_BROADCAST, WM_SYSCOMMAND, SC_SCREENSAVE, 0)` trick.
- **Waiting**: polls `SPI_GETSCREENSAVERRUNNING` once a second. It first waits (up to 5s) for the
  flag to go *true* to avoid a startup race, then waits for it to go back to *false*.
- **No UI**: built with `#![windows_subsystem = "windows"]`. With `/debug`, it calls
  `AttachConsole(ATTACH_PARENT_PROCESS)` to reuse the calling cmd window (falling back to
  `AllocConsole` if launched without a console, e.g. double-clicked), then logs manually via
  `WriteConsoleW` rather than `println!`, since a GUI-subsystem process has no valid stdout handle
  until a console is attached.

## Caveats / things worth knowing

- **Not compiled/tested here** — I don't have a Windows machine or network access in the
  environment I wrote this in, so this hasn't been run through `cargo build`. I verified every
  function signature against Microsoft's docs and docs.rs for the exact `windows-sys` version
  pinned in `Cargo.toml`, but if a newer/older `windows-sys` shifts a type slightly, the compiler
  errors should point at a one-line fix (usually a cast). The most likely spot, if anything: the
  `HWND_BROADCAST as HWND` cast or the `REG_VALUE_TYPE`/`REG_SAM_FLAGS` constants, if a future
  crate version wraps them in a newtype instead of a plain `u32`.
- **No crash/kill-switch recovery**: if the process is killed (Task Manager, `taskkill /F`, log
  off) while the screensaver is up, the original timeouts won't be restored automatically. A
  `Ctrl+C`/console-handler or a "restore last saved values" flag would be a reasonable follow-up
  if that matters to you.
- The four power-setting GUIDs (`GUID_SLEEP_SUBGROUP`, `GUID_STANDBY_IDLE`,
  `GUID_VIDEO_SUBGROUP`, `GUID_VIDEO_POWERDOWN_TIMEOUT`) are stable, documented Windows constants,
  but `windows-sys` doesn't ship them as named constants (they're meant to be supplied by the
  application), so they're hardcoded from their published values in `main.rs`.
