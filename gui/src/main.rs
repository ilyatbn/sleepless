// Sleepless GUI
//
// A tray icon (Windows taskbar, macOS menu bar, Linux StatusNotifierItem)
// that runs the `sleepless` command-line tool from its menu:
//
//   Turn display off          runs `sleepless`, plus `--asroot` when...
//   Keep awake with lid closed   ...this is ticked (remembered between runs)
//                             also on a global shortcut, see hotkey.rs
//   Launch at login           adds / removes an autostart entry
//   Quit
//
// All the actual work stays in the CLI, which is looked up next to this
// executable first and then on PATH. Each session is a separate
// `sleepless` process that exits by itself once you come back.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod autostart;
mod hotkey;

use std::cell::RefCell;
use std::env;
use std::fs::{self, File};
use std::io::{self, Cursor};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// Set while a `sleepless` session started from here is still running.
static RUNNING: AtomicBool = AtomicBool::new(false);
/// Settings::asroot, readable from the hotkey thread.
static ASROOT: AtomicBool = AtomicBool::new(false);

struct App {
    tray: Option<TrayIcon>,
    start: MenuItem,
    lid: CheckMenuItem,
    login: CheckMenuItem,
    quit: MenuItem,
    settings: Settings,
    _hotkey: Option<global_hotkey::GlobalHotKeyManager>,
}

// Menu items aren't Send, so the app lives on the main thread and every
// menu event is handled there (see run_event_loop).
thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------
// Settings (just the --asroot toggle) and where they live
// ---------------------------------------------------------------------

/// Per-user directory for our settings and lock file:
///   Windows: %APPDATA%\Sleepless
///   macOS:   ~/Library/Application Support/Sleepless
///   Linux:   $XDG_CONFIG_HOME/sleepless (~/.config/sleepless)
fn data_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        PathBuf::from(env::var_os("APPDATA")?).join("Sleepless")
    } else if cfg!(target_os = "macos") {
        PathBuf::from(env::var_os("HOME")?).join("Library/Application Support/Sleepless")
    } else {
        autostart::xdg_config_home()?.join("sleepless")
    };
    Some(base)
}

#[derive(Default)]
struct Settings {
    asroot: bool,
}

impl Settings {
    fn path() -> Option<PathBuf> {
        Some(data_dir()?.join("sleepless-gui.conf"))
    }

    fn load() -> Self {
        let text = Self::path()
            .and_then(|p| fs::read_to_string(p).ok())
            .unwrap_or_default();
        Settings {
            asroot: text.lines().any(|l| l.trim() == "asroot=true"),
        }
    }

    fn save(&self) {
        let Some(path) = Self::path() else { return };
        let _ = fs::create_dir_all(path.parent().unwrap());
        if let Err(e) = fs::write(&path, format!("asroot={}\n", self.asroot)) {
            eprintln!("Sleepless: could not save {}: {e}", path.display());
        }
    }
}

/// Only one tray icon per user: a second copy (e.g. started by hand while
/// the login item already runs one) just exits. The lock is released by
/// the OS when the process ends, however it ends.
fn already_running() -> bool {
    let Some(dir) = data_dir() else { return false };
    let _ = fs::create_dir_all(&dir);
    let Ok(file) = File::create(dir.join("sleepless-gui.lock")) else {
        return false;
    };
    if file.try_lock().is_err() {
        return true;
    }
    std::mem::forget(file);
    false
}

// ---------------------------------------------------------------------
// Running the CLI
// ---------------------------------------------------------------------

fn sleepless_path() -> PathBuf {
    let name = if cfg!(windows) {
        "sleepless.exe"
    } else {
        "sleepless"
    };
    env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join(name)))
        .filter(|p| p.is_file())
        // Bare name: Command searches PATH.
        .unwrap_or_else(|| PathBuf::from(name))
}

pub fn is_running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

/// Runs `sleepless` unless a session started from here is still going.
/// Called from the menu (main thread) and the shortcut (any thread).
pub fn start_session() -> io::Result<()> {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let path = sleepless_path();
    let mut cmd = Command::new(&path);
    if ASROOT.load(Ordering::SeqCst) {
        cmd.arg("--asroot");
    }
    match cmd.spawn() {
        Ok(mut child) => {
            thread::spawn(move || {
                let _ = child.wait();
                RUNNING.store(false, Ordering::SeqCst);
            });
            Ok(())
        }
        Err(e) => {
            RUNNING.store(false, Ordering::SeqCst);
            Err(io::Error::new(
                e.kind(),
                format!("could not run {}: {e}", path.display()),
            ))
        }
    }
}

impl App {
    fn turn_display_off(&mut self) {
        if let Err(e) = start_session() {
            let msg = format!("Sleepless: {e}");
            eprintln!("{msg}");
            if let Some(tray) = &self.tray {
                let _ = tray.set_tooltip(Some(msg));
            }
        }
    }

    fn toggle_lid(&mut self) {
        self.settings.asroot = !self.settings.asroot;
        ASROOT.store(self.settings.asroot, Ordering::SeqCst);
        self.settings.save();
        self.lid.set_checked(self.settings.asroot);
    }

    fn toggle_login(&mut self) {
        if let Err(e) = autostart::set(!autostart::is_enabled()) {
            eprintln!("Sleepless: could not change the login item: {e}");
        }
        // Show what's actually there, whether or not that worked.
        self.login.set_checked(autostart::is_enabled());
    }
}

/// Runs on the main thread on Windows / macOS and on the tray's D-Bus
/// thread on Linux, so it only touches thread-safe state.
fn on_tray_click(event: TrayIconEvent) {
    if let TrayIconEvent::Click {
        button: MouseButton::Left,
        button_state: MouseButtonState::Up,
        ..
    } = event
    {
        if let Err(e) = start_session() {
            eprintln!("Sleepless: {e}");
        }
    }
}

fn handle(event: MenuEvent) {
    APP.with_borrow_mut(|app| {
        let Some(app) = app else { return };
        let id = event.id();
        if id == app.start.id() {
            app.turn_display_off();
        } else if id == app.lid.id() {
            app.toggle_lid();
        } else if id == app.login.id() {
            app.toggle_login();
        } else if id == app.quit.id() {
            // Remove the icon now rather than leaving a ghost in the
            // Windows tray until the mouse passes over it.
            drop(app.tray.take());
            std::process::exit(0);
        }
    });
}

// ---------------------------------------------------------------------
// Tray icon
// ---------------------------------------------------------------------

fn load_icon(png_bytes: &[u8]) -> Icon {
    let mut reader = png::Decoder::new(Cursor::new(png_bytes))
        .read_info()
        .expect("bundled tray icon is a valid PNG");
    let mut rgba = vec![0; reader.output_buffer_size().expect("tray icon size")];
    let info = reader
        .next_frame(&mut rgba)
        .expect("bundled tray icon decodes");
    assert_eq!(
        info.color_type,
        png::ColorType::Rgba,
        "tray icon must be RGBA"
    );
    rgba.truncate(info.buffer_size());
    Icon::from_rgba(rgba, info.width, info.height).expect("tray icon dimensions")
}

fn build_app() -> tray_icon::Result<App> {
    let settings = Settings::load();
    ASROOT.store(settings.asroot, Ordering::SeqCst);
    let login_enabled = autostart::is_enabled();
    if login_enabled {
        // Point the login item at wherever we live now, in case the app
        // was moved since it was turned on.
        let _ = autostart::set(true);
    }

    let hotkey = hotkey::register();
    let start = match hotkey.is_some() {
        true => {
            let (label, accel) = hotkey::label_and_accelerator();
            MenuItem::new(label, true, accel)
        }
        false => MenuItem::new("Turn display off", true, None),
    };
    let lid = CheckMenuItem::new(
        "Keep awake with the lid closed (--asroot)",
        true,
        settings.asroot,
        None,
    );
    let login = CheckMenuItem::new("Launch at login", true, login_enabled, None);
    let quit = MenuItem::new("Quit Sleepless", true, None);
    let menu = Menu::with_items(&[
        &start,
        &PredefinedMenuItem::separator(),
        &lid,
        &login,
        &PredefinedMenuItem::separator(),
        &quit,
    ])
    .expect("tray menu");

    // Just the eyes: wide on the macOS menu bar, square in the other trays.
    #[cfg(target_os = "macos")]
    let icon = load_icon(include_bytes!("../../assets/tray-mac.png"));
    #[cfg(not(target_os = "macos"))]
    let icon = load_icon(include_bytes!("../../assets/tray.png"));

    // Left click turns the display off, right click opens the menu.
    TrayIconEvent::set_event_handler(Some(on_tray_click));
    let tray = TrayIconBuilder::new()
        .with_id("sleepless")
        .with_tooltip("Sleepless: click to turn the display off, right-click for options")
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()?;

    Ok(App {
        tray: Some(tray),
        start,
        lid,
        login,
        quit,
        settings,
        _hotkey: hotkey,
    })
}

// ---------------------------------------------------------------------
// Event loops: whatever each platform needs to keep the tray alive, with
// menu events delivered to `handle` on the main thread.
// ---------------------------------------------------------------------

#[cfg(windows)]
fn init_platform() {}

#[cfg(windows)]
fn run_event_loop() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, TranslateMessage, MSG,
    };
    // The tray's hidden window lives on this thread, so its menu events
    // arrive here, during DispatchMessageW.
    MenuEvent::set_event_handler(Some(handle));
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(target_os = "macos")]
fn init_platform() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    let mtm = MainThreadMarker::new().expect("must start on the main thread");
    // Menu bar only, no Dock icon (Info.plist says the same with
    // LSUIElement; this covers running the bare binary too).
    NSApplication::sharedApplication(mtm)
        .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
}

#[cfg(target_os = "macos")]
fn run_event_loop() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    // AppKit calls menu actions on the main thread.
    MenuEvent::set_event_handler(Some(handle));
    let mtm = MainThreadMarker::new().expect("must start on the main thread");
    NSApplication::sharedApplication(mtm).run();
}

#[cfg(not(any(windows, target_os = "macos")))]
fn init_platform() {}

#[cfg(not(any(windows, target_os = "macos")))]
fn run_event_loop() {
    // The ksni backend runs its D-Bus service on its own thread and queues
    // menu events; handle them here so they're on the main thread.
    for event in MenuEvent::receiver().iter() {
        handle(event);
    }
}

fn main() {
    if already_running() {
        return;
    }
    init_platform();
    let app = build_app().unwrap_or_else(|e| {
        eprintln!("Sleepless: could not create the tray icon: {e}");
        if cfg!(target_os = "linux") {
            eprintln!(
                "Your desktop needs a StatusNotifierItem / AppIndicator tray \
                 (on GNOME: the \"AppIndicator and KStatusNotifierItem Support\" extension)."
            );
        }
        std::process::exit(1);
    });
    APP.with_borrow_mut(|slot| *slot = Some(app));
    run_event_loop();
}
