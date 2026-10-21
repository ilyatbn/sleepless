// macOS backend
//
//   1. `caffeinate -d -i -m -s -w <our pid>` holds power assertions that
//      stop display and system sleep, so the monitors stay on (turning
//      them off is what brings the lock screen up). The -w makes it exit
//      by itself the moment we do.
//   2. --asroot: a root helper runs `pmset -a disablesleep 1`, which is
//      what stops a MacBook from sleeping when the lid closes, then waits
//      for us to exit and puts the previous value back.
//   3. Cover every display with a borderless black window above
//      everything else (menu bar, Dock, full-screen apps), with the cursor
//      hidden. Not the system screensaver: on recent macOS that shows the
//      lock-screen clock and wallpaper, and asking for a password after it
//      is a common setting.
//   4. Wait for keyboard/mouse activity (CoreGraphics' HID idle counter)
//      on a background thread while AppKit runs the windows on the main
//      thread, then exit - which removes the windows and releases both
//      helpers.
//
// When run from Sleepless.app it also installs the app's Finder Quick
// Action (right-click > Quick Actions > "Sleepless as root") into
// ~/Library/Services.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{env, fs, io, thread};

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSApplicationActivationPolicy,
    NSBackingStoreType, NSColor, NSCursor, NSEventMask, NSModalResponse, NSScreen,
    NSSecureTextField, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::NSDefaultRunLoopMode;

use crate::unix::{self, fail, run_ok, run_output, spawn, spawn_as_root, wait_for_our_exit_sh};

pub use crate::unix::{debug_log, init_logging};

// NSScreenSaverWindowLevel: above the menu bar, Dock and full-screen apps.
const SCREEN_SAVER_WINDOW_LEVEL: isize = 1000;

/// Set by the thread watching for input once the user is back.
static BACK: AtomicBool = AtomicBool::new(false);
/// Set by the main thread once the password has been typed correctly.
static UNLOCKED: AtomicBool = AtomicBool::new(false);

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

/// Puts a black, borderless window over every display. They stay up until
/// the process exits.
fn black_out(mtm: MainThreadMarker) -> Vec<Retained<NSWindow>> {
    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon, but still able to come to the front, so the keypress
    // that brings you back lands here rather than in whatever app was
    // active.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let windows: Vec<_> = NSScreen::screens(mtm)
        .iter()
        .map(|screen| {
            let frame = screen.frame();
            let window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    NSWindow::alloc(mtm),
                    frame,
                    NSWindowStyleMask::Borderless,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            unsafe { window.setReleasedWhenClosed(false) };
            window.setBackgroundColor(Some(&NSColor::blackColor()));
            window.setLevel(SCREEN_SAVER_WINDOW_LEVEL);
            window.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::Stationary
                    | NSWindowCollectionBehavior::FullScreenAuxiliary
                    | NSWindowCollectionBehavior::IgnoresCycle,
            );
            window.setHasShadow(false);
            window.setFrame_display(frame, true);
            window.orderFrontRegardless();
            window
        })
        .collect();
    log!("Blacked out {} display(s).", windows.len());
    #[allow(deprecated)] // activate() is macOS 14+; this still works there
    app.activateIgnoringOtherApps(true);
    NSCursor::hide();
    windows
}

fn sleep_disabled() -> Option<bool> {
    let out = run_output("pmset", &["-g"])?;
    out.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .find(|f| f.first() == Some(&"SleepDisabled"))
        .and_then(|f| f.get(1).map(|v| *v == "1"))
}

// ---------------------------------------------------------------------
// Asking for the credential
//
// An NSAlert with a secure text field as its accessory view: the alert's own
// buttons run the modal loop, so Return accepts and Escape cancels without us
// having to look at key events, and the field never echoes what's typed.
// ---------------------------------------------------------------------

use crate::lock::{self, Secret};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

/// The alert panel we're currently showing, if any, so the lock screen can
/// raise it above the black windows.
fn raise_above_black_out(window: &NSWindow) {
    window.setLevel(SCREEN_SAVER_WINDOW_LEVEL + 1);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
}

/// Builds the alert and its secure field, shared by the enrolment prompt and
/// the lock screen. Without a cancel button there is no way out of the lock
/// screen except typing the password.
fn secret_alert(
    mtm: MainThreadMarker,
    title: &str,
    message: &str,
    cancel_button: bool,
) -> (Retained<NSAlert>, Retained<NSSecureTextField>) {
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(&NSString::from_str(message));
    // The first button is the default one, so Return is what submits it.
    alert.addButtonWithTitle(&NSString::from_str(if cancel_button {
        "OK"
    } else {
        "Come back"
    }));
    if cancel_button {
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    }

    let field = NSSecureTextField::new(mtm);
    field.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(280.0, 24.0),
    ));
    alert.setAccessoryView(Some(&field));

    let window = alert.window();
    window.setTitle(&NSString::from_str(title));
    window.makeFirstResponder(Some(&field));
    (alert, field)
}

/// An NSAlert numbers its buttons by the order they were added, starting at
/// NSAlertFirstButtonReturn (1000) - *not* NSModalResponseOK (1), which is what
/// a hand-rolled modal window uses. Comparing against NSModalResponseOK made
/// every button look like a cancellation, which is exactly the "cancelled: no
/// password was set" symptom.
/// NSAlert's buttons answer with NSAlertFirstButtonReturn / Second / Third,
/// numbered by the order they were added; NSModalResponseAbort means the alert
/// was dismissed some other way (Escape, the close box).
pub fn accepted(answer: NSModalResponse) -> bool {
    answer == NSAlertFirstButtonReturn
}

/// Reads the field out of a finished secret alert, logging what the user
/// actually pressed so a device report can tell "cancelled" from "wrong API".
fn secret_from(field: &NSSecureTextField, answer: NSModalResponse, where_: &str) -> Option<Secret> {
    log!(
        "{where_}: alert response {answer} -> {}.",
        if accepted(answer) {
            "accepted"
        } else {
            "not accepted"
        }
    );
    accepted(answer).then(|| Secret::new(field.stringValue().to_string().trim_end()))
}

/// Asks for a secret. None when the user cancelled.
pub fn prompt_secret(title: &str, message: &str) -> Option<Secret> {
    let Some(mtm) = MainThreadMarker::new() else {
        log!("prompt_secret: not on the main thread, cannot show a dialog.");
        return None;
    };
    let app = NSApplication::sharedApplication(mtm);
    let policy = app.activationPolicy();
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    #[allow(deprecated)] // activate() is macOS 14+; this still works there
    app.activateIgnoringOtherApps(true);

    log!("Showing the password dialog: {title} / {message}");
    let (alert, field) = secret_alert(mtm, title, message, true);
    let answer = alert.runModal();
    app.setActivationPolicy(policy);
    secret_from(&field, answer, "password dialog")
}

/// The lock screen's prompt: the same field, but on top of the black windows
/// and with no Cancel button.
fn prompt_secret_locked(title: &str, message: &str) -> Option<Secret> {
    let Some(mtm) = MainThreadMarker::new() else {
        log!("prompt_secret_locked: not on the main thread.");
        return None;
    };
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)] // activate() is macOS 14+; this still works there
    app.activateIgnoringOtherApps(true);
    let (alert, field) = secret_alert(mtm, title, message, false);
    raise_above_black_out(&alert.window());
    log!("Showing the lock screen prompt: {message}");
    let answer = alert.runModal();
    secret_from(&field, answer, "lock screen prompt")
}

/// One-line result message for the credential commands.
pub fn report(message: &str) {
    log!("Report to the user: {}", message.replace('\n', " "));
    notify("Sleepless", message);
}

/// Enrolment asks through the same dialog: there's no terminal path here.
pub fn prompt_secret_interactive(title: &str, message: &str) -> Option<Secret> {
    prompt_secret(title, message)
}

pub fn notify(title: &str, message: &str) {
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("Sleepless: {message}");
        return;
    };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(message));
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    alert.runModal();
}

pub fn ask_yes_no(title: &str, message: &str) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(message));
    // The first button is the default one, so it's what Return gives us.
    alert.addButtonWithTitle(&NSString::from_str("Yes"));
    alert.addButtonWithTitle(&NSString::from_str("No"));
    let answer = alert.runModal();
    log!(
        "Confirmation dialog {title:?}: response {answer} -> {}.",
        if accepted(answer) { "yes" } else { "no" }
    );
    accepted(answer)
}

/// Runs AppKit's event queue by hand until `stop` says so.
///
/// `NSApplication::run()` would do this, but it never gives us a chance to
/// notice that the thread watching for input saw the user come back, and the
/// password prompt has to be shown on the main thread.
fn pump_events_until(mtm: MainThreadMarker, stop: impl Fn() -> bool) {
    let app = NSApplication::sharedApplication(mtm);
    while !stop() {
        while let Some(event) = unsafe {
            app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                None,
                NSDefaultRunLoopMode,
                true,
            )
        } {
            app.sendEvent(&event);
        }
        thread::sleep(Duration::from_millis(40));
    }
}

// ---------------------------------------------------------------------
// Passkey: Touch ID, through LocalAuthentication
//
// The system does the verifying, so Sleepless holds no credential material
// that could be stolen or got wrong: enrolling just records that a passkey is
// allowed for this user, and coming back asks the system whether the person at
// the keyboard is them. It never goes through the lock screen - the Touch ID
// dialog is an ordinary prompt inside our own session, so the VPN and
// everything else stays up.
//
// Anything that doesn't work - no Touch ID, the prompt cancelled, a system
// that refuses to talk to us - falls through to the password, which is always
// enrolled first.
// ---------------------------------------------------------------------

use std::ptr;
use std::sync::mpsc::{self, TryRecvError};
use std::time::Instant;

use block2::RcBlock;
use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2_foundation::{NSDate, NSError, NSRunLoop};

// LocalAuthentication isn't loaded by default; linking it makes dyld bring it
// in so AnyClass::get("LAContext") finds it.
#[link(name = "LocalAuthentication", kind = "framework")]
extern "C" {}

/// LAPolicyDeviceOwnerAuthenticationWithBiometrics: fingerprint only. Using
/// the wider device-owner policy would quietly accept the account password
/// instead, and that's the OS credential, not a Sleepless one.
const LA_POLICY_BIOMETRICS: isize = 1;

fn la_context() -> Option<Retained<AnyObject>> {
    let cls: &AnyClass = AnyClass::get(c"LAContext")?;
    let allocated: *mut AnyObject = unsafe { msg_send![cls, alloc] };
    let initialised: *mut AnyObject = unsafe { msg_send![allocated, init] };
    unsafe { Retained::from_raw(initialised) }
}

fn can_evaluate(ctx: &AnyObject, policy: isize) -> bool {
    let mut error: *mut AnyObject = ptr::null_mut();
    let ok: Bool = unsafe { msg_send![ctx, canEvaluatePolicy: policy, error: &mut error] };
    ok.as_bool()
}

/// Shows the system's Touch ID prompt and waits for it. The reply block runs
/// on the main queue, which is this thread, so the run loop is pumped until
/// the answer arrives - or the sensor gives up and we ask for the password.
fn evaluate(ctx: &AnyObject, policy: isize, reason: &str) -> bool {
    let (sender, receiver) = mpsc::sync_channel::<bool>(1);
    // ObjC's BOOL has to travel as objc2's Bool inside a block signature, and
    // the error arrives as a plain pointer (a reference in a closure signature
    // can't be made higher-ranked, which is what the block wants).
    let block: RcBlock<dyn Fn(Bool, *mut NSError)> =
        RcBlock::new(move |ok: Bool, _error: *mut NSError| {
            let _ = sender.send(ok.as_bool());
        });
    let reason = NSString::from_str(reason);
    let started: Bool = unsafe {
        msg_send![ctx, evaluatePolicy: policy, localizedReason: &*reason, reply: &*block]
    };
    if !started.as_bool() {
        log!("LocalAuthentication refused to start the Touch ID request.");
        return false;
    }
    let run_loop = NSRunLoop::mainRunLoop();
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        match receiver.try_recv() {
            Ok(ok) => return ok,
            Err(TryRecvError::Disconnected) => return false,
            Err(TryRecvError::Empty) => {}
        }
        let until = NSDate::dateWithTimeIntervalSinceNow(0.1);
        // NSDefaultRunLoopMode is an extern static, hence the unsafe.
        unsafe { run_loop.runMode_beforeDate(NSDefaultRunLoopMode, &until) };
    }
    log!("Touch ID request timed out; falling back to the password.");
    false
}

/// The passkey this machine can offer, named the way Apple names it.
pub fn passkey_supported() -> Option<&'static str> {
    let ctx = la_context()?;
    if can_evaluate(&ctx, LA_POLICY_BIOMETRICS) {
        Some("Touch ID")
    } else {
        None
    }
}

/// Asks the system once, so a machine that can't do this finds out now rather
/// than at the lock screen.
pub fn passkey_enroll() -> Result<&'static str, String> {
    let Some(label) = passkey_supported() else {
        return Err("no usable Touch ID on this machine".to_string());
    };
    let ctx = la_context().ok_or_else(|| "LocalAuthentication is not available".to_string())?;
    if evaluate(
        &ctx,
        LA_POLICY_BIOMETRICS,
        "Sleepless: allow Touch ID to come back",
    ) {
        Ok(label)
    } else {
        Err("Touch ID did not confirm your identity".to_string())
    }
}

/// What the lock screen tries before it shows the password box.
fn passkey_unlock() -> bool {
    let Some(ctx) = la_context() else {
        return false;
    };
    if !can_evaluate(&ctx, LA_POLICY_BIOMETRICS) {
        log!("A passkey is enrolled but Touch ID isn't available right now.");
        return false;
    }
    // The system's prompt only appears in front of an active app.
    if let Some(mtm) = MainThreadMarker::new() {
        #[allow(deprecated)] // activate() is macOS 14+; this still works there
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    }
    evaluate(
        &ctx,
        LA_POLICY_BIOMETRICS,
        "Sleepless is keeping the display off",
    )
}

pub fn run(opts: crate::Options) {
    install_quick_actions();

    // --- 1. Keep the system awake ---
    let pid = std::process::id().to_string();
    let caffeinate = ["caffeinate", "-d", "-i", "-m", "-s", "-w", &pid].map(String::from);
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

    // --- 3. Black screen ---
    let mtm = MainThreadMarker::new().unwrap_or_else(|| fail("must run on the main thread"));
    let _windows = black_out(mtm);

    // --- 4. Wait for the user to come back ---
    // AppKit needs the main thread to keep the windows up, so the waiting
    // happens on another one, which also ends the process. With a password
    // enrolled that thread only announces the return: the prompt has to be
    // shown on the main thread, and nothing is released until it's answered.
    let enrolled = lock::is_enrolled();
    if enrolled {
        log!("A password is enrolled: coming back will need it.");
    }
    thread::spawn(move || {
        unix::wait_for_activity(|| Some(seconds_since_input()));
        BACK.store(true, Ordering::SeqCst);
        if enrolled {
            log!("Welcome back - the password comes first.");
            while !UNLOCKED.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(100));
            }
        }
        log!("Welcome back - releasing the no-sleep assertion.");
        let _ = caffeinate.kill();
        let _ = caffeinate.wait();
        log!("Done. Exiting.");
        std::process::exit(0);
    });

    if enrolled {
        pump_events_until(mtm, || BACK.load(Ordering::SeqCst));
        // Touch ID first if it was enrolled; the password is the way back when
        // the sensor isn't available or the person would rather type.
        if lock::has_passkey() && passkey_unlock() {
            log!("Passkey accepted.");
        } else {
            lock::unlock_loop(|message| prompt_secret_locked("Sleepless", message));
        }
        UNLOCKED.store(true, Ordering::SeqCst);
        // The waiting thread does the cleanup and exits; keep the windows up
        // (and the events flowing) until it does.
        pump_events_until(mtm, || false);
    } else {
        NSApplication::sharedApplication(mtm).run();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_app_kit::{
        NSAlertSecondButtonReturn, NSModalResponseAbort, NSModalResponseCancel, NSModalResponseOK,
    };

    /// The rule that broke password enrolment from the menu bar: an NSAlert
    /// answers with NSAlertFirstButtonReturn (1000) and friends, never with
    /// NSModalResponseOK (1). Comparing against OK made every dialog look
    /// cancelled - and on the lock screen that would have meant nobody could
    /// come back at all.
    #[test]
    fn only_the_first_alert_button_counts_as_accepted() {
        assert_eq!(NSAlertFirstButtonReturn, 1000);
        assert_eq!(NSAlertSecondButtonReturn, 1001);

        assert!(accepted(NSAlertFirstButtonReturn));
        assert!(!accepted(NSAlertSecondButtonReturn));
        assert!(!accepted(NSModalResponseOK));
        assert!(!accepted(NSModalResponseCancel));
        assert!(!accepted(NSModalResponseAbort));
    }
}
