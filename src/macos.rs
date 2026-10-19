// macOS backend
//
//   1. `caffeinate -i -m -s -w <our pid>` holds a power assertion that
//      stops idle/system sleep (but deliberately not display sleep). The
//      -w makes it exit by itself the moment we do.
//   2. --asroot: a root helper runs `pmset -a disablesleep 1`, which is
//      what stops a MacBook from sleeping when the lid closes, then waits
//      for us to exit and puts the previous value back.
//   3. --lock: lock the session (same as Ctrl+Cmd+Q).
//   4. `pmset displaysleepnow` turns the display off.
//   5. Wait for keyboard/mouse activity (CoreGraphics' HID idle counter),
//      or with --lock until the session is unlocked, then exit - which
//      releases both helpers. macOS wakes the display by itself on input.
//
// When run from Sleepless.app it also installs the app's Finder Quick
// Actions (right-click > Quick Actions > "Sleepless as root" etc.) into
// ~/Library/Services.

use std::ffi::{c_char, c_void};
use std::path::Path;
use std::time::Duration;
use std::{env, fs, io, thread};

use crate::unix::{self, fail, run_ok, run_output, spawn, spawn_as_root, wait_for_our_exit_sh};

pub use crate::unix::{debug_log, init_logging};

// kCGEventSourceStateHIDSystemState / kCGAnyInputEventType
const HID_SYSTEM_STATE: i32 = 1;
const ANY_INPUT_EVENT: u32 = u32::MAX;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(source: i32, event_type: u32) -> f64;
}

fn seconds_since_input() -> f64 {
    unsafe { CGEventSourceSecondsSinceLastEventType(HID_SYSTEM_STATE, ANY_INPUT_EVENT) }
}

// ---------------------------------------------------------------------
// Session lock (--lock)
// ---------------------------------------------------------------------

extern "C" {
    fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}
const RTLD_LAZY: i32 = 1;

/// Locks the screen immediately, exactly like Ctrl+Cmd+Q / "Lock Screen"
/// in the Apple menu. There's no public API or CLI for this, so it calls
/// SACLockScreenImmediate from the login framework the menu item itself
/// uses, looked up at runtime so a missing symbol is an error, not a crash.
fn lock_screen() -> bool {
    unsafe {
        let lib = dlopen(
            c"/System/Library/PrivateFrameworks/login.framework/Versions/Current/login".as_ptr(),
            RTLD_LAZY,
        );
        if lib.is_null() {
            log!("Could not load login.framework");
            return false;
        }
        let sym = dlsym(lib, c"SACLockScreenImmediate".as_ptr());
        if sym.is_null() {
            log!("SACLockScreenImmediate not found");
            return false;
        }
        let lock: extern "C" fn() -> i32 = std::mem::transmute(sym);
        lock() == 0
    }
}

fn screen_is_locked() -> Option<bool> {
    let out = run_output("ioreg", &["-n", "Root", "-d1"])?;
    Some(out.contains("\"CGSSessionScreenIsLocked\"=Yes"))
}

// ---------------------------------------------------------------------
// Finder Quick Actions shipped inside Sleepless.app
// ---------------------------------------------------------------------

fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((fs::read(a), fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

/// Copies Contents/Resources/Services/*.workflow into ~/Library/Services
/// when they're missing or out of date, so they show up in Finder's
/// right-click menu. Does nothing for the bare command-line binary.
fn install_quick_actions() {
    let Ok(exe) = env::current_exe() else { return };
    let Some(src) = exe
        .parent()
        .and_then(Path::parent)
        .map(|c| c.join("Resources/Services"))
    else {
        return;
    };
    let (Ok(entries), Some(home)) = (fs::read_dir(&src), env::var_os("HOME")) else {
        return;
    };
    let services = Path::new(&home).join("Library/Services");
    let mut changed = false;
    for entry in entries.flatten() {
        let from = entry.path();
        let to = services.join(entry.file_name());
        let wflow = Path::new("Contents/document.wflow");
        if same_file(&from.join(wflow), &to.join(wflow)) {
            continue;
        }
        log!("Installing Quick Action {}", to.display());
        let _ = fs::remove_dir_all(&to);
        match copy_dir(&from, &to) {
            Ok(()) => changed = true,
            Err(e) => log!("  failed: {e}"),
        }
    }
    if changed {
        // Refresh the Services registry so Finder picks them up right away.
        run_ok("/System/Library/CoreServices/pbs", &["-update"]);
    }
}

fn sleep_disabled() -> Option<bool> {
    let out = run_output("pmset", &["-g"])?;
    out.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .find(|f| f.first() == Some(&"SleepDisabled"))
        .and_then(|f| f.get(1).map(|v| *v == "1"))
}

pub fn run(opts: crate::Options) {
    install_quick_actions();

    // --- 1. Keep the system awake ---
    let pid = std::process::id().to_string();
    let caffeinate = ["caffeinate", "-i", "-m", "-s", "-w", &pid].map(String::from);
    let mut caffeinate =
        spawn(&caffeinate).unwrap_or_else(|e| fail(&format!("could not start caffeinate: {e}")));
    log!("caffeinate is holding a no-sleep assertion.");

    // --- 2. --asroot: don't sleep when the lid closes ---
    if opts.asroot {
        log!("SleepDisabled is currently {:?}", sleep_disabled());
        let script = format!(
            "prev=$(pmset -g | awk '/SleepDisabled/ {{print $2}}'); \
             pmset -a disablesleep 1; \
             {}; \
             pmset -a disablesleep ${{prev:-0}}",
            wait_for_our_exit_sh()
        );
        let argv = ["/bin/sh", "-c", &script].map(String::from);
        let mut helper = spawn_as_root(&argv)
            .unwrap_or_else(|e| fail(&format!("could not start the root helper: {e}")));
        // Generous timeout: the user may be typing a password into the
        // admin dialog.
        if !unix::wait_until(&mut helper, Duration::from_secs(120), || {
            sleep_disabled() == Some(true)
        }) {
            fail("could not disable lid-close sleep (password prompt cancelled?)");
        }
        log!("Lid-close sleep disabled; it will be restored when Sleepless exits.");
    }

    // --- 3. --lock: lock the session first ---
    if opts.lock {
        log!("Locking the screen...");
        if !lock_screen() {
            fail("could not lock the screen; not turning the display off unprotected");
        }
        let mut waited = 0;
        while screen_is_locked() != Some(true) && waited < 50 {
            thread::sleep(Duration::from_millis(100));
            waited += 1;
        }
        log!("Screen locked: {:?}", screen_is_locked());
    }
    let watch_lock = opts.lock && screen_is_locked() == Some(true);

    // --- 4. Display off ---
    log!("Turning the display off...");
    if !run_ok("pmset", &["displaysleepnow"]) {
        fail("`pmset displaysleepnow` failed");
    }

    // --- 5. Wait for the user to come back ---
    if watch_lock {
        // Stay up (and keep the lid helper alive) until the password has
        // actually been entered, not just until the mouse moves.
        log!("Waiting for the screen to be unlocked...");
        while screen_is_locked() != Some(false) {
            thread::sleep(Duration::from_secs(1));
        }
        log!("Screen unlocked.");
    } else {
        unix::wait_for_activity(|| Some(seconds_since_input()));
    }
    log!("Welcome back - releasing the no-sleep assertion.");
    let _ = caffeinate.kill();
    let _ = caffeinate.wait();
}
