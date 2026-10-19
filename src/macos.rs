// macOS backend
//
//   1. `caffeinate -i -m -s -w <our pid>` holds a power assertion that
//      stops idle/system sleep (but deliberately not display sleep). The
//      -w makes it exit by itself the moment we do.
//   2. --asroot: a root helper runs `pmset -a disablesleep 1`, which is
//      what stops a MacBook from sleeping when the lid closes, then waits
//      for us to exit and puts the previous value back.
//   3. `pmset displaysleepnow` turns the display off.
//   4. Wait for keyboard/mouse activity (CoreGraphics' HID idle counter),
//      then exit - which releases both helpers. macOS wakes the display
//      by itself on input.

use std::time::Duration;

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

fn sleep_disabled() -> Option<bool> {
    let out = run_output("pmset", &["-g"])?;
    out.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .find(|f| f.first() == Some(&"SleepDisabled"))
        .and_then(|f| f.get(1).map(|v| *v == "1"))
}

pub fn run(opts: crate::Options) {
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

    // --- 3. Display off ---
    log!("Turning the display off...");
    if !run_ok("pmset", &["displaysleepnow"]) {
        fail("`pmset displaysleepnow` failed");
    }

    // --- 4. Wait for the user to come back ---
    unix::wait_for_activity(|| Some(seconds_since_input()));
    log!("Welcome back - releasing the no-sleep assertion.");
    let _ = caffeinate.kill();
    let _ = caffeinate.wait();
}
