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
| Windows (x64) | `sleepless.exe` (+ `sleepless-gui.exe` for the tray icon, in the same folder) or `sleepless-windows-x86_64.zip` (both) |
| macOS (Apple Silicon + Intel) | `sleepless-macos-universal-app.zip` (Sleepless.app), `sleepless-macos-universal-gui-app.zip` (Sleepless GUI.app, menu bar icon) or `sleepless-macos-universal.tar.gz` (command line) |
| Linux (x86_64 / aarch64) | `sleepless-linux-<arch>.tar.gz` (command line + tray icon) |

## Tray / menu bar icon (`sleepless-gui`)
`sleepless-gui` puts Sleepless's eyes in the Windows taskbar tray, the macOS menu bar or the Linux system tray. **Click** them to turn the display off; **right-click** for the menu:

- **Turn display off**: runs `sleepless` (with `--asroot` if the next item is ticked). Also on a global keyboard shortcut: **Ctrl+Cmd+S** on macOS, **Ctrl+Win+S** on Windows, **Ctrl+Super+S** on Linux. It waits until you let go of the keys, so releasing them doesn't wake the screen right back up.
- **Keep awake with the lid closed (--asroot)**: remembered between runs.
- **Launch at login**: starts the tray icon when you log in. It uses each system's own per-user mechanism, so you can also see / remove it there: the `Sleepless` value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (Task Manager > Startup apps), `~/Library/LaunchAgents/io.github.ilyatbn.sleepless-gui.plist` (System Settings > General > Login Items) or `~/.config/autostart/sleepless-gui.desktop`.
- **Quit Sleepless**: removes the icon. A screen-off session that's already running carries on until you come back.

It's only a launcher: it looks for the `sleepless` command-line tool next to itself (then on `PATH`), so keep the two together. On macOS, Sleepless GUI.app already contains it; install it the same way as Sleepless.app below, including the `xattr` step. Only one tray icon runs per user; starting it again does nothing.

If another app already owns the shortcut, the menu item just shows no shortcut. On Linux the shortcut only works in an X11 session: Wayland doesn't let apps grab keys, so bind a custom shortcut to `sleepless` in your desktop's keyboard settings instead.

On Linux the icon uses the StatusNotifierItem tray (KDE Plasma, Ubuntu, XFCE, Cinnamon...). Stock GNOME has no tray: install the *AppIndicator and KStatusNotifierItem Support* extension.

### macOS: installing the unsigned app
The app isn't signed with an Apple Developer ID, so Gatekeeper will say it *"is damaged and can't be opened"* or *"cannot be verified"*. To allow it:

1. Unzip `sleepless-macos-universal-app.zip` and drag **Sleepless.app** into **Applications**.
2. Remove the quarantine flag macOS put on the download:
   ```
   xattr -dr com.apple.quarantine /Applications/Sleepless.app
   ```
   (For the command-line binary: `xattr -d com.apple.quarantine ./sleepless`.)
3. Double-click Sleepless in Launchpad / Applications. It has no Dock icon: it covers every display with a black window and keeps the monitors on (it never turns them off, and doesn't use the screensaver, so neither brings up the lock screen). Move the mouse or press a key to come back.

#### Right-click menu
The first time you open Sleepless.app it adds a Finder **Quick Action**. It's a **Finder** item, not a Dock one (macOS only lets an app add Dock menu items while it's running). Right-click **Sleepless.app** in the **Applications folder in Finder** and pick **Quick Actions > Sleepless as root** to start it in lid mode (`--asroot`); it asks for your admin password.

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

The tray app is a separate crate in `gui/`; `cargo build --release --workspace` builds both (`target/release/sleepless-gui`). It has no system library dependencies on any platform.

macOS app bundle (universal):
```
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
lipo -create -output sleepless target/{aarch64,x86_64}-apple-darwin/release/sleepless
packaging/macos/build-app.sh sleepless 0.3.0 .     # -> ./Sleepless.app
```
For `Sleepless GUI.app`, build with `--workspace`, `lipo` `sleepless-gui` the same way, then `packaging/macos/build-gui-app.sh sleepless-gui sleepless 0.3.0 .`.

The icon is generated by `python3 assets/make_icon.py` (needs Pillow), which writes `icon.png`, `icon-512.png`, `icon.ico`, `icon.icns` and the tray icons `tray.png` / `tray-mac.png` (macOS menu bar) into `assets/`.

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
- **Stay awake**: `caffeinate -d -i -m -s -w <pid>`: power assertions against display and system sleep that end by itself when Sleepless exits. The monitors are never turned off, because on macOS that's what brings up the lock screen.
- **Black screen**: a borderless black `NSWindow` on every display at screensaver window level (above the menu bar, Dock and full-screen apps), with the cursor hidden. Not the system screensaver: on recent macOS that shows the lock-screen clock and wallpaper, and often asks for the password afterwards.
- **Waiting**: polls CoreGraphics' `CGEventSourceSecondsSinceLastEventType` (HID idle time) and exits as soon as it drops, i.e. on any keyboard/mouse input, which also removes the windows.

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
  Sleepless doesn't touch security settings. (macOS avoids both: the display stays on and the
  screensaver isn't used, so this doesn't apply there.)


#### Code generated almost entirely using Claude.