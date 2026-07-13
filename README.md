# Sleepless

A tiny Windows utility to simulate screen-off (using the built-in Blank screensaver) and preventing sleep.  

## Why not use native Windows features?
Windows 11 ties the Screen Off feature together with Sleep, so you cannot configure one without the other. Another caveat is that when the screen turns off, Windows immediately locks your computer regardless of any settings I tried. This means that tools like PowerToys Awake do not work either while the screen is off, so the computer goes to sleep using the configured OS sleep time. Very odd.

## So what is the point?
I wanted to protect my OLED monitor from burn-ins while keeping the computer active. At the same time, I wanted to keep my usual sleep timers while I'm actually away. This proved challenging with Windows.   

## How Sleepless solves this issue
1. Reads your current **sleep** and **screen-off** timeouts (both AC and battery) and saves them.
2. Sets both to **Never**.
3. Points the screensaver at Windows' built-in **blank** screensaver and starts it.
4. Waits until you dismiss the screensaver (move the mouse / press a key), detected via
   `SystemParametersInfo(SPI_GETSCREENSAVERRUNNING)`.
5. Restores your original timeouts and screensaver settings, then exits.

While this is not a true replacement for Screen Off, it's the next best thing. Obviously works much better on OLED screens since a blank screensaver basically turns off all the pixels.  


## Build
You'll need the Rust toolchain with the MSVC target (default on Windows)

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
- **No crash/kill-switch recovery**: if the process is killed (Task Manager, `taskkill /F`, log
  off) while the screensaver is up, the original timeouts won't be restored automatically. A
  `Ctrl+C`/console-handler or a "restore last saved values" flag would be a reasonable follow-up
  if that matters to you.


#### Code generated almost entirely using Claude.