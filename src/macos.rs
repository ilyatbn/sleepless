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

use std::cell::RefCell;
use std::path::Path;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use std::{env, fs, io, thread};

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSApplicationActivationPolicy,
    NSBackingStoreType, NSColor, NSCursor, NSEvent, NSEventMask, NSModalResponse, NSScreen,
    NSSecureTextField, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::NSDefaultRunLoopMode;

use crate::unix::{self, fail, run_ok, run_output, spawn, spawn_as_root, wait_for_our_exit_sh};

pub use crate::unix::{debug_log, init_logging};

// NSScreenSaverWindowLevel: above the menu bar, Dock and full-screen apps.
const SCREEN_SAVER_WINDOW_LEVEL: isize = 1000;

/// Set once the user is back, and whether a password has to be asked for on
/// the way in. Read from the main thread, written by the thread watching input.
static GATE_ARMED: AtomicBool = AtomicBool::new(false);

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
    let answer = show_modal(&alert, mtm);
    app.setActivationPolicy(policy);
    secret_from(&field, answer, "password dialog")
}

/// The lock screen's prompt: the same field, but raised above the black
/// windows, on the display the pointer is on, with the app activated, and with
/// no Cancel button.
///
/// All of that matters: a beta left the prompt underneath the black windows,
/// which looked exactly like a machine that wouldn't wake up. The repeating
/// timer below keeps re-asserting it, because AppKit's modal loop runs in its
/// own run-loop mode and something else can end up on top meanwhile.
fn prompt_secret_locked(title: &str, message: &str) -> Option<Secret> {
    let Some(mtm) = MainThreadMarker::new() else {
        log!("prompt_secret_locked: not on the main thread, cannot show a prompt.");
        return None;
    };
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)] // activate() is macOS 14+; this still works there
    app.activateIgnoringOtherApps(true);

    let (alert, field) = secret_alert(mtm, title, message, false);
    let answer = show_modal(&alert, mtm);
    secret_from(&field, answer, "lock screen prompt")
}

/// Shows an alert modally and keeps it where the user can actually see it: the
/// app activated, the panel raised above everything this app puts on screen
/// (including the lock screen's own black windows), centred on the display the
/// pointer is on, and re-asserted twice a second while it's up.
///
/// Every dialog goes through here. A beta left the lock screen's password box
/// underneath the black windows, which looked exactly like a machine that
/// wouldn't wake up, and an enrolment dialog started while a session is running
/// would be hidden the same way - the black windows are at the screensaver
/// level, so an ordinary panel can never reach them.
fn show_modal(alert: &NSAlert, mtm: MainThreadMarker) -> NSModalResponse {
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)] // activate() is macOS 14+; this still works there
    app.activateIgnoringOtherApps(true);

    let window = alert.window();
    raise_above_black_out(&window);
    let where_ = centre_on_cursor_display(&window, mtm);
    keep_prompt_on_top(&window);
    let frame = window.frame();
    log!(
        "Dialog {:?} shown {where_}: level {}, frame ({}, {}, {}x{}), key window: {}.",
        window.title().to_string(),
        window.level(),
        frame.origin.x,
        frame.origin.y,
        frame.size.width,
        frame.size.height,
        window.isKeyWindow()
    );
    let answer = alert.runModal();
    stop_keeping_prompt_on_top();
    answer
}

// The prompt window, where the keep-on-top timer can reach it (both run on
// the main thread).
thread_local! {
    static PROMPT_WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
    static PROMPT_TIMER: RefCell<Option<Retained<NSTimer>>> = const { RefCell::new(None) };
}

/// Puts the prompt on the display the pointer is on, centred. Without this it
/// lands on the main display, which with more than one monitor can be the one
/// nobody is looking at.
fn centre_on_cursor_display(window: &NSWindow, mtm: MainThreadMarker) -> String {
    let cursor = NSEvent::mouseLocation();
    let screens = NSScreen::screens(mtm);
    let inside = |frame: NSRect| {
        cursor.x >= frame.origin.x
            && cursor.x < frame.origin.x + frame.size.width
            && cursor.y >= frame.origin.y
            && cursor.y < frame.origin.y + frame.size.height
    };
    let mut chosen = None;
    let mut names = Vec::new();
    for screen in screens.iter() {
        let frame = screen.frame();
        names.push(format!(
            "{}x{}@{},{}",
            frame.size.width, frame.size.height, frame.origin.x, frame.origin.y
        ));
        if chosen.is_none() && inside(frame) {
            chosen = Some(frame);
        }
    }
    let Some(frame) = chosen.or_else(|| screens.firstObject().map(|s| s.frame())) else {
        return "no display found; leaving the prompt where AppKit put it".to_string();
    };
    let size = window.frame().size;
    let origin = NSPoint::new(
        frame.origin.x + (frame.size.width - size.width) / 2.0,
        frame.origin.y + (frame.size.height - size.height) / 2.0,
    );
    window.setFrame_display(NSRect::new(origin, size), true);
    format!("on the display under the pointer ({})", names.join(", "))
}

/// Re-asserts the prompt's level, brings it to the front and re-activates the
/// app, twice a second, until the prompt is finished.
fn keep_prompt_on_top(window: &Retained<NSWindow>) {
    PROMPT_WINDOW.with(|slot| *slot.borrow_mut() = Some(window.clone()));
    let block = RcBlock::new(|_timer: NonNull<NSTimer>| {
        PROMPT_WINDOW.with(|slot| {
            let Some(window) = slot.borrow().clone() else {
                return;
            };
            raise_above_black_out(&window);
            // Not just ordered front: the box has to be *key*, or keystrokes
            // go to whatever else the system decided is frontmost.
            window.makeKeyAndOrderFront(None);
            if let Some(mtm) = MainThreadMarker::new() {
                #[allow(deprecated)] // activate() is macOS 14+; this still works there
                NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
            }
        });
    });
    let timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.5, true, &block) };
    // The alert's modal loop runs in NSModalPanelRunLoopMode, so the timer has
    // to be registered for every common mode to fire at all.
    unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    PROMPT_TIMER.with(|slot| *slot.borrow_mut() = Some(timer));
}

fn stop_keeping_prompt_on_top() {
    PROMPT_TIMER.with(|slot| {
        if let Some(timer) = slot.borrow_mut().take() {
            timer.invalidate();
        }
    });
    PROMPT_WINDOW.with(|slot| *slot.borrow_mut() = None);
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
    show_modal(&alert, mtm);
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
    let answer = show_modal(&alert, mtm);
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

use std::ptr::{self, NonNull};
use std::time::Instant;

use block2::RcBlock;
use objc2::runtime::ProtocolObject;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::{msg_send, sel};
use objc2_app_kit::{NSWorkspace, NSWorkspaceSessionDidResignActiveNotification};
use objc2_foundation::{
    NSDate, NSError, NSNotification, NSObjectProtocol, NSRunLoop, NSRunLoopCommonModes, NSTimer,
};

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

/// Where the system's answer lands: 0 = still waiting, 1 = yes, 2 = no.
/// An atomic slot rather than a channel because the reply can arrive after we
/// gave up, and a late callback must not be able to confuse anything.
static TOUCH_ID_ANSWER: AtomicU8 = AtomicU8::new(0);

/// How long the system prompt may stay unanswered before we stop waiting for
/// it. Enrolment has to end or time out; it must never hang.
const TOUCH_ID_TIMEOUT: Duration = Duration::from_secs(45);

/// Describes the NSError the reply block hands us: "code -1001
/// com.apple.LocalAuthentication" plus its message is what makes a device
/// report diagnosable.
fn describe_error(error: *mut NSError) -> String {
    if error.is_null() {
        return "no error information".to_string();
    }
    let error: &NSError = unsafe { &*error };
    let domain = error.domain();
    let why = error.localizedDescription();
    format!("code {}, {domain}: {why}", error.code())
}

/// Shows the system's Touch ID prompt and waits for the answer.
///
/// The reply block is delivered on the main queue, and this *is* the main
/// thread, so waiting means keeping AppKit's event queue running - exactly
/// what NSApplication::run does. Waiting on a plain run-mode slice instead
/// left the prompt on screen but unreachable: nothing was pumping the modal
/// panel it lives in, and the caller hung until it was killed.
fn evaluate(ctx: &AnyObject, policy: isize, reason: &str, mtm: MainThreadMarker) -> bool {
    let answer = RcBlock::new(move |ok: Bool, error: *mut NSError| {
        if ok.as_bool() {
            log!("Touch ID answered: confirmed.");
            TOUCH_ID_ANSWER.store(1, Ordering::SeqCst);
        } else {
            log!(
                "Touch ID answered: not confirmed ({}).",
                describe_error(error)
            );
            TOUCH_ID_ANSWER.store(2, Ordering::SeqCst);
        }
    });
    TOUCH_ID_ANSWER.store(0, Ordering::SeqCst);

    let why = NSString::from_str(reason);
    let started: Bool =
        unsafe { msg_send![ctx, evaluatePolicy: policy, localizedReason: &*why, reply: &*answer] };
    if !started.as_bool() {
        log!("LocalAuthentication refused to start the Touch ID request at all.");
        return false;
    }
    log!(
        "Touch ID prompt requested; waiting up to {}s for it, keeping AppKit running.",
        TOUCH_ID_TIMEOUT.as_secs()
    );

    let app = NSApplication::sharedApplication(mtm);
    let deadline = Instant::now() + TOUCH_ID_TIMEOUT;
    while Instant::now() < deadline {
        match TOUCH_ID_ANSWER.load(Ordering::SeqCst) {
            1 => return true,
            2 => return false,
            _ => {}
        }
        // One short slice of the event queue at a time, so the answer (and the
        // prompt's own UI) gets served while we wait.
        let until = NSDate::dateWithTimeIntervalSinceNow(0.1);
        while let Some(event) = unsafe {
            app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                Some(&until),
                NSDefaultRunLoopMode,
                true,
            )
        } {
            app.sendEvent(&event);
        }
    }
    log!(
        "Touch ID never answered within {}s - giving up on it (this is why the          password is always enrolled first).",
        TOUCH_ID_TIMEOUT.as_secs()
    );
    false
}

/// The passkey this machine can offer, named the way Apple names it.
pub fn passkey_supported() -> Option<&'static str> {
    let Some(cls) = AnyClass::get(c"LAContext") else {
        log!("LocalAuthentication's LAContext class isn't available.");
        return None;
    };
    let _ = cls;
    let ctx = la_context()?;
    if can_evaluate(&ctx, LA_POLICY_BIOMETRICS) {
        log!("Touch ID is available on this machine.");
        Some("Touch ID")
    } else {
        log!("Touch ID is not available (no sensor enrolled, or the system won't let us ask).");
        None
    }
}

/// Asks the system once, so a machine that can't do this finds out now rather
/// than at the lock screen. Always returns: the prompt either answers, refuses,
/// or runs out the clock.
pub fn passkey_enroll() -> Result<&'static str, String> {
    let Some(label) = passkey_supported() else {
        return Err("no usable Touch ID on this machine".to_string());
    };
    let ctx = la_context().ok_or_else(|| "LocalAuthentication is not available".to_string())?;
    let Some(mtm) = MainThreadMarker::new() else {
        return Err("not on the main thread; cannot show the Touch ID prompt".to_string());
    };
    // The prompt only shows up in front of an active app, and a tray-started
    // helper is normally an accessory process.
    let app = NSApplication::sharedApplication(mtm);
    let policy = app.activationPolicy();
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    #[allow(deprecated)] // activate() is macOS 14+; this still works there
    app.activateIgnoringOtherApps(true);
    log!("Asking the system to confirm your identity for the passkey.");
    let confirmed = evaluate(
        &ctx,
        LA_POLICY_BIOMETRICS,
        "Sleepless: allow Touch ID to come back",
        mtm,
    );
    app.setActivationPolicy(policy);
    if confirmed {
        log!("Passkey accepted by the system.");
        Ok(label)
    } else {
        Err("Touch ID did not confirm your identity (see the log above)".to_string())
    }
}

/// What the lock screen tries before it shows the password box.
fn passkey_unlock(mtm: MainThreadMarker) -> bool {
    let Some(ctx) = la_context() else {
        return false;
    };
    if !can_evaluate(&ctx, LA_POLICY_BIOMETRICS) {
        log!("A passkey is enrolled but Touch ID isn't available right now.");
        return false;
    }
    // The system's prompt only appears in front of an active app.
    #[allow(deprecated)] // activate() is macOS 14+; this still works there
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    log!("Lock screen trying the passkey before the password box.");
    evaluate(
        &ctx,
        LA_POLICY_BIOMETRICS,
        "Sleepless is keeping the display off",
        mtm,
    )
}

// ---------------------------------------------------------------------
// The OS taking the session over
//
// Ctrl+Cmd+Q, the lock button, a login window or a fast user switch all mean
// the user handed the session to macOS. Sleepless gets out of the way at once:
// the black windows are part of this process, so exiting removes them, the
// helpers (caffeinate, the root helper) go with it, and the lock screen the
// user chose is free to appear. This is also the guaranteed escape from a
// Sleepless whose own prompt has gotten stuck - which is why it must work even
// when nothing else does.
// ---------------------------------------------------------------------

static OS_SESSION_TAKEN: AtomicBool = AtomicBool::new(false);

// Observers must be kept alive, and the blocks with them.
thread_local! {
    static OBSERVERS: RefCell<Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>> =
        const { RefCell::new(Vec::new()) };
}

pub fn session_was_taken() -> bool {
    OS_SESSION_TAKEN.load(Ordering::SeqCst)
}

/// Registers both signals, because they cover different things:
///
/// - `NSWorkspaceSessionDidResignActiveNotification` is the documented one, and
///   it fires when the *login session* is handed over - a fast user switch, or a
///   logout. It does not fire for a plain screen lock: on lock the session is
///   still ours and still on the console.
/// - `com.apple.screenIsLocked` is what loginwindow posts for `Ctrl+Cmd+Q`, the
///   lock button and a screensaver lock. It has never been documented, but it has
///   been stable since well before the workspace notification, and it is the only
///   signal for the case that matters most here.
///
/// So the workspace one catches switching users, the distributed one catches
/// locking the screen. Both end this run.
fn watch_session_locks(mtm: MainThreadMarker) {
    let block = RcBlock::new(|note: NonNull<NSNotification>| {
        let name = unsafe { note.as_ref() }.name();
        log!("Session notification {name:?}: Sleepless is getting out of the way.");
        OS_SESSION_TAKEN.store(true, Ordering::SeqCst);
    });
    let _ = mtm; // sharedWorkspace doesn't need the marker, but the watch is main-thread work
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    let token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceSessionDidResignActiveNotification),
            None,
            None,
            &block,
        )
    };
    OBSERVERS.with(|observers| observers.borrow_mut().push(token));
    log!("Watching for the session being resigned (NSWorkspace).");

    watch_screen_is_locked();
}

fn watch_screen_is_locked() {
    let Some(cls) = AnyClass::get(c"NSDistributedNotificationCenter") else {
        log!("No NSDistributedNotificationCenter to ask about com.apple.screenIsLocked.");
        return;
    };
    let center: *mut AnyObject = unsafe { msg_send![cls, distributedNotificationCenter] };
    if center.is_null() {
        return;
    }
    let responds: Bool = unsafe {
        msg_send![&*center, respondsToSelector: sel!(addObserverForName:object:queue:options:usingBlock:)]
    };
    if !responds.as_bool() {
        log!("NSDistributedNotificationCenter has no block observer API here; skipping.");
        return;
    }
    let block = RcBlock::new(|_note: NonNull<NSNotification>| {
        log!("com.apple.screenIsLocked: Sleepless is getting out of the way.");
        OS_SESSION_TAKEN.store(true, Ordering::SeqCst);
    });
    let name = NSString::from_str("com.apple.screenIsLocked");
    let token: *mut AnyObject = unsafe {
        msg_send![&*center, addObserverForName: &*name, object: ptr::null_mut::<AnyObject>(),
                   queue: ptr::null_mut::<AnyObject>(), options: 0u64, usingBlock: &*block]
    };
    if token.is_null() {
        log!("Could not subscribe to com.apple.screenIsLocked.");
    } else {
        log!("Watching for com.apple.screenIsLocked.");
    }
}

/// Ends the run now: stop the helpers and let the process (and with it the
/// black windows) go away. Called from whichever thread noticed first.
fn release_and_exit() -> ! {
    log!("Releasing the no-sleep assertions and exiting.");
    if let Some(mut child) = CAFFEINATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        let _ = child.kill();
        let _ = child.wait();
    }
    std::process::exit(0);
}

/// The no-sleep helper, kept somewhere either thread can stop it.
static CAFFEINATE: Mutex<Option<Child>> = Mutex::new(None);

pub fn run(opts: crate::Options) {
    install_quick_actions();

    // --- 1. Keep the system awake ---
    let pid = std::process::id().to_string();
    let caffeinate = ["caffeinate", "-d", "-i", "-m", "-s", "-w", &pid].map(String::from);
    let child =
        spawn(&caffeinate).unwrap_or_else(|e| fail(&format!("could not start caffeinate: {e}")));
    *CAFFEINATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(child);
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
    watch_session_locks(mtm);
    // A prompt that never got shown, or a session the user locked behind our
    // back, must never be what stands between them and their desktop: this
    // thread ends the run the moment macOS says the session is ours no more.
    thread::spawn(|| {
        while !session_was_taken() {
            thread::sleep(Duration::from_millis(500));
        }
        release_and_exit();
    });
    let _windows = black_out(mtm);

    // --- 4. Wait for the user to come back ---
    // AppKit needs the main thread to keep the windows up, so the waiting
    // happens on another one, which also ends the process. With the gate armed
    // that thread only announces the return: the prompt has to be shown on the
    // main thread, and nothing is released until it's answered.
    //
    // Whether the gate is armed is decided when the user comes back, not when
    // the run started, so a password enrolled while the screen was black still
    // gets asked for. The thread that sees the input records the answer in
    // GATE_ARMED before it raises BACK, so both threads agree.
    thread::spawn(move || {
        unix::wait_for_activity(|| Some(seconds_since_input()), session_was_taken);
        if session_was_taken() {
            log!("The session was taken over before anyone came back.");
            release_and_exit();
        }
        let armed = lock::is_enrolled();
        GATE_ARMED.store(armed, Ordering::SeqCst);
        BACK.store(true, Ordering::SeqCst);
        if armed {
            log!("Welcome back - the password comes first.");
            while !UNLOCKED.load(Ordering::SeqCst) && !session_was_taken() {
                thread::sleep(Duration::from_millis(100));
            }
        }
        release_and_exit();
    });

    // Main thread: keep AppKit running, then handle the return.
    pump_events_until(mtm, || BACK.load(Ordering::SeqCst) || session_was_taken());
    if session_was_taken() {
        log!("The session was taken over while the screen was black.");
        release_and_exit();
    }
    if GATE_ARMED.load(Ordering::SeqCst) {
        // Touch ID first if it was enrolled; the password is the way back when
        // the sensor isn't available or the person would rather type.
        let unlocked = (lock::has_passkey() && passkey_unlock(mtm))
            || lock::unlock_loop(
                |message| prompt_secret_locked("Sleepless", message),
                session_was_taken,
            );
        if !unlocked {
            log!("Gate ended without a password: the session was taken over.");
            release_and_exit();
        }
        UNLOCKED.store(true, Ordering::SeqCst);
    } else {
        log!("No password enrolled while the screen was black: coming back needs nothing.");
    }
    // The waiting thread does the cleanup and exits; keep the windows up (and
    // the events flowing) until it does.
    pump_events_until(mtm, || false);
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
