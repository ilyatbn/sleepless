// Global keyboard shortcut for "Turn display off":
//   macOS:         Ctrl+Cmd+S
//   Windows/Linux: Ctrl+Win+S (Ctrl+Super+S)
//
// Linux only gets it on an X11 session: Wayland doesn't let apps grab keys
// globally, so there it's a custom shortcut in the desktop's own keyboard
// settings that runs `sleepless` instead.
//
// The shortcut doesn't start Sleepless until every key is released again.
// Otherwise letting go of Ctrl / Cmd would count as "you're back" and wake
// the screen straight away.

use std::thread;
use std::time::{Duration, Instant};

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tray_icon::menu::accelerator::{self, Accelerator};

const MODS: Modifiers = Modifiers::CONTROL.union(Modifiers::META);

/// "Turn display off" with the shortcut shown next to it. macOS draws a
/// real ⌃⌘S accelerator; Windows menus can't hold Win-key accelerators, so
/// there (and on Linux) it's part of the text, right-aligned after a tab.
pub fn label_and_accelerator() -> (&'static str, Option<Accelerator>) {
    if cfg!(target_os = "macos") {
        let accel = Accelerator::new(MODS, accelerator::Code::KeyS);
        ("Turn display off", Some(accel))
    } else if cfg!(windows) {
        ("Turn display off\tCtrl+Win+S", None)
    } else {
        ("Turn display off (Ctrl+Super+S)", None)
    }
}

fn supported() -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        return true;
    }
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland");
    std::env::var_os("DISPLAY").is_some() && !wayland
}

/// Registers the shortcut. It only works for as long as the returned
/// manager is kept alive. None if it couldn't be registered (e.g. another
/// app already uses it).
pub fn register() -> Option<GlobalHotKeyManager> {
    if !supported() {
        return None;
    }
    let hotkey = HotKey::new(Some(MODS), Code::KeyS);
    let manager = GlobalHotKeyManager::new()
        .and_then(|m| m.register(hotkey).map(|()| m))
        .map_err(|e| eprintln!("Sleepless: could not register the keyboard shortcut: {e}"))
        .ok()?;
    let id = hotkey.id();
    // Called on the main thread on Windows / macOS, on global-hotkey's own
    // thread on Linux; start_session is fine with either.
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.id == id && event.state == HotKeyState::Pressed && !crate::is_running() {
            thread::spawn(|| {
                wait_for_keys_released();
                if let Err(e) = crate::start_session() {
                    eprintln!("Sleepless: {e}");
                }
            });
        }
    }));
    Some(manager)
}

fn wait_for_keys_released() {
    let start = Instant::now();
    while keys_held() && start.elapsed() < Duration::from_secs(5) {
        thread::sleep(Duration::from_millis(30));
    }
    // A moment for the key-up events themselves to be delivered.
    thread::sleep(Duration::from_millis(200));
}

#[cfg(windows)]
fn keys_held() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    // Every virtual key, mouse buttons included; high bit = down right now.
    (1..=0xFE).any(|vk| unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000 != 0)
}

#[cfg(target_os = "macos")]
fn keys_held() -> bool {
    // kCGEventSourceStateHIDSystemState
    const HID_SYSTEM_STATE: i32 = 1;
    // Shift, Control, Option, Command (kCGEventFlagMask*)
    const MODIFIER_FLAGS: u64 = 0x20000 | 0x40000 | 0x80000 | 0x100000;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceFlagsState(state: i32) -> u64;
        fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
    }
    unsafe {
        CGEventSourceFlagsState(HID_SYSTEM_STATE) & MODIFIER_FLAGS != 0
            || (0..128).any(|key| CGEventSourceKeyState(HID_SYSTEM_STATE, key))
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn keys_held() -> bool {
    use x11rb::protocol::xproto::ConnectionExt;

    let Ok((conn, _)) = x11rb::connect(None) else {
        return false;
    };
    conn.query_keymap()
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .is_some_and(|reply| reply.keys.iter().any(|&bits| bits != 0))
}
