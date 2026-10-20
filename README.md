# Sleepless

<img src="assets/icon.png" alt="Sleepless icon" width="128" align="right">

A tiny utility for **Windows, macOS and Linux** that turns your screen off while preventing sleep, then puts everything back the moment you return.  

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


## Download
Grab the latest build from the [Releases](../../releases) page:

| Platform | File |
| --- | --- |
| Windows (x64) | `sleepless.exe` or `sleepless-windows-x86_64.zip` |
| macOS (Apple Silicon + Intel) | `sleepless-macos-universal-app.zip` (Sleepless.app) or `sleepless-macos-universal.tar.gz` (command line) |
| Linux (x86_64 / aarch64) | `sleepless-linux-<arch>.tar.gz` |

### macOS: installing the unsigned app
The app isn't signed with an Apple Developer ID, so Gatekeeper will say it *"is damaged and can't be opened"* or *"cannot be verified"*. To allow it:

1. Unzip `sleepless-macos-universal-app.zip` and drag **Sleepless.app** into **Applications**.
2. Remove the quarantine flag macOS put on the download:
   ```
   xattr -dr com.apple.quarantine /Applications/Sleepless.app
   ```
   (For the command-line binary: `xattr -d com.apple.quarantine ./sleepless`.)
3. Double-click Sleepless in Launchpad / Applications. It has no window or Dock icon: the display just turns off. Move the mouse or press a key to come back.

#### Right-click menu
The first time you open Sleepless.app it adds a Finder **Quick Action**. Right-click **Sleepless.app** (in Applications or the Dock's *Show in Finder*) and pick **Quick Actions > Sleepless as root** to start it in lid mode (`--asroot`); it asks for your admin password.

It's installed into `~/Library/Services` (delete `Sleepless as root.workflow` there to remove it). The item also appears when you right-click other apps, but it always starts Sleepless. If it doesn't show up, check that it is enabled in System Settings > General > Login Items & Extensions > Finder (on older macOS: Privacy & Security > Extensions > Finder).

You can also pass options from Terminal:
```
open -a Sleepless --args --asroot
```

### Linux
```
tar xzf sleepless-linux-x86_64.tar.gz
cd sleepless-linux-x86_64
./install.sh        # installs to ~/.local (bin, icon, app-menu entry); as root: /usr/local
```

## Run

```
sleepless                  # silent, display goes off, back when you move the mouse
sleepless --debug          # log everything (Windows: to the cmd window, also /debug; macOS/Linux: stderr)
sleepless --asroot         # also allow closing the lid without anything disconnecting or sleeping
sleepless --help
```

### `--asroot`: closing the lid
Normally closing a laptop lid suspends it, which drops Wi-Fi, SSH sessions, downloads, etc. With `--asroot` the machine keeps running with the lid shut until you come back:

| Platform | What it does | Needs |
| --- | --- | --- |
| macOS | `pmset -a disablesleep 1` (the only switch that stops a MacBook sleeping on lid close), previous value restored on exit | Your admin password: asked in the terminal via `sudo`, or with the standard macOS admin dialog when launched as the app |
| Linux | Adds `handle-lid-switch` to the logind inhibitor lock, taken as root | Your password via `sudo` (terminal) or `pkexec` (graphical prompt) |
| Windows | Sets "When I close the lid" to *Do nothing* (AC and battery) on the active power plan, restored on exit | Nothing, this setting doesn't need admin rights on Windows |

Run Sleepless as your **normal user** and let it ask for the password; don't wrap it in `sudo` yourself. Only a small helper runs as root, and the rest of Sleepless has to stay in your desktop session to control the display. The root helper watches the Sleepless process and undoes its change as soon as Sleepless exits, even if it's killed or crashes.

Coming back: open the lid and move the mouse / press a key. Sleepless restores everything and exits. With the lid shut, the machine keeps running on battery and keeps the heat in, so think twice before putting it in a bag.

## Build
You'll need the Rust toolchain.

```
cargo build --release
```

The binary will be at `target/release/sleepless` (`sleepless.exe` on Windows, which also gets the icon embedded; that step needs the MSVC toolchain's `rc.exe`, which comes with the default Windows setup).

macOS app bundle (universal):
```
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
lipo -create -output sleepless target/{aarch64,x86_64}-apple-darwin/release/sleepless
packaging/macos/build-app.sh sleepless 0.3.0 .     # -> ./Sleepless.app
```

The icon is generated by `python3 assets/make_icon.py` (needs Pillow), which writes `icon.png`, `icon-512.png`, `icon.ico` and `icon.icns` into `assets/`.

GitHub Actions (`.github/workflows/release.yml`) builds Windows, macOS (universal binary + `.app`) and Linux (x86_64 + aarch64) on every push and pull request, and publishes a `v<version>` release from `Cargo.toml` on pushes to `main`.

## How it works, briefly

### Windows
- **Timeouts**: `PowerGetActiveScheme` + `PowerReadACValueIndex`/`PowerReadDCValueIndex` on the
  `GUID_STANDBY_IDLE` (sleep) and `GUID_VIDEO_POWERDOWN_TIMEOUT` (screen) settings, then
  `PowerWriteACValueIndex`/`PowerWriteDCValueIndex` + `PowerSetActiveScheme` to apply `0` ("Never").
  This only touches your *active* power plan and does not require admin rights, same as dragging
  the sliders in Settings > Power & sleep. `--asroot` does the same for the lid close action.
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

### macOS
- **Stay awake**: `caffeinate -i -m -s -w <pid>`: a power assertion against idle/system sleep (but not display sleep) that ends by itself when Sleepless exits.
- **Display off**: `pmset displaysleepnow`.
- **Waiting**: polls CoreGraphics' `CGEventSourceSecondsSinceLastEventType` (HID idle time) and exits as soon as it drops, i.e. on any keyboard/mouse input. macOS wakes the display on its own.

### Linux
- **Stay awake**: `systemd-inhibit --mode=block --what=sleep:idle` (plus `handle-lid-switch` with `--asroot`) wrapped around a tiny shell loop that exits when Sleepless does. `elogind-inhibit` works too.
- **Display off**, depending on the session:
  - **GNOME** (Wayland or X11): Mutter's `org.gnome.Mutter.DisplayConfig.PowerSaveMode` over D-Bus (`gdbus`); user activity via `org.gnome.Mutter.IdleMonitor`.
  - **KDE Plasma** (Wayland or X11): `kscreen-doctor --dpms off/on`; user activity via `org.freedesktop.ScreenSaver.GetSessionIdleTime`.
  - **Other X11** sessions: `xset dpms force off`, then waits for the X server to report the monitor back on.
  - Anything else (e.g. wlroots compositors like Sway): the display is left alone, but the machine is kept awake until you press Enter / Ctrl+C.

## Caveats / things worth knowing
- **Windows: no crash/kill-switch recovery**: if the process is killed (Task Manager, `taskkill /F`, log
  off) while the screensaver is up, the original timeouts won't be restored automatically. A
  `Ctrl+C`/console-handler or a "restore last saved values" flag would be a reasonable follow-up
  if that matters to you. (macOS and Linux don't have this problem: everything they change is
  held by helper processes that clean up when Sleepless exits.)
- **Lock screen**: if your OS is set to require a password after the display turns off / the
  screensaver starts, you'll still get the lock screen when you come back. That's by design;
  Sleepless doesn't touch security settings.


#### Code generated almost entirely using Claude.