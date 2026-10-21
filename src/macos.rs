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

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use std::{env, fs, io, thread};

use objc2::rc::Retained;
use objc2::ClassType;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSApplicationActivationOptions,
    NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSCursor, NSEvent, NSEventMask,
    NSModalResponse, NSRunningApplication, NSScreen, NSSecureTextField, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::NSDefaultRunLoopMode;

use crate::unix::{
    self, fail, run_ok, run_output, spawn, spawn_as_root, spawn_thread, wait_for_our_exit_sh,
};

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
    let screens = NSScreen::screens(mtm);
    log!(
        "Making one black window per display ({} of them).",
        screens.len()
    );
    let windows: Vec<_> = screens
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
            // Explicit, because the alternative is a "black screen" that
            // disappears the moment another app becomes active: panels default
            // to hiding on deactivate, and this is a plain window, but the
            // enrolment dialog bug showed how much rests on this one flag.
            window.setHidesOnDeactivate(false);
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
    log!("Bringing the app forward and hiding the pointer.");
    // Guarded because a failure here must not take the black windows with it.
    let outcome = unsafe { try_objc(|| activate_app(&app)) };
    if let Err(why) = outcome {
        log!("Activating the app failed: {why} - continuing anyway.");
    }
    let outcome = unsafe { try_objc(|| hide_cursor()) };
    if let Err(why) = outcome {
        log!("Hiding the pointer failed: {why} - continuing anyway.");
    }
    log!("Black screen ready.");
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

/// Puts a window above this app's black windows (which sit at the screensaver
/// level) and makes it follow the user across spaces, so a dialog is never
/// underneath the very thing it is supposed to be drawn on top of.
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

/// NSAlert's buttons answer with NSAlertFirstButtonReturn (1000) / Second (1001)
/// / Third (1002), numbered by the order they were added - *not* with
/// NSModalResponseOK (1), which is what a hand-rolled modal window uses.
/// NSModalResponseAbort means the alert was dismissed another way (Escape, the
/// close box). Comparing against NSModalResponseOK made every button look like a
/// cancellation, which is exactly the "cancelled: no password was set" symptom.
/// The test below pins the numbers.
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
    log!("Showing the password dialog: {title} / {message}");
    // show_modal handles activation and the activation policy for us.
    let (alert, field) = secret_alert(mtm, title, message, true);
    // Not the lock screen: no policy flip, no timer, no Dock icon.
    let answer = show_modal(&alert, mtm, false);
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
    // The pointer has to be visible for this: it's the one thing on the black
    // screen the user might want to point at, and beta3's report of "no prompt
    // anywhere" was helped by there being no cursor to see either.
    show_cursor();
    let (alert, field) = secret_alert(mtm, title, message, false);
    let answer = show_modal(&alert, mtm, true);
    // Back to black between attempts.
    hide_cursor();
    secret_from(&field, answer, "lock screen prompt")
}

/// Asks once for this process to become the active app.
///
/// Three ways, because macOS 14 changed the rules and we still run on older
/// ones: the deprecated forceful call (honoured before 14), the
/// `NSRunningApplication` one (with `ActivateAllWindows` only - the
/// `ActivateIgnoringOtherApps` *option* is documented as having no effect since
/// 14), and 14's cooperative `-activate`, which exists only there and so is
/// behind `respondsToSelector:` (calling a method the system doesn't have is an
/// unrecognized-selector exception, and that cost two betas already).
///
/// It must be called *once*, before entering the modal loop. beta5 called it
/// twice a second from the keep-on-top timer: cooperative activation is only a
/// request, the system granted and withdrew it in turn, and the enrolment dialog
/// spent two minutes flickering between visible and hidden while stealing focus
/// from Terminal - `key window: true/false` flipping every tick in the log is
/// exactly that fight.
fn activate_app(app: &NSApplication) {
    #[allow(deprecated)] // the pre-14 way; still accepted there
    app.activateIgnoringOtherApps(true);
    let me = NSRunningApplication::currentApplication();
    let forced = me.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
    // Typed (NSObjectProtocol), so there is no hand-written signature here.
    let responds = app.respondsToSelector(sel!(activate));
    let cooperated = if responds {
        app.activate();
        true
    } else {
        false
    };
    log!(
        "Asking for activation once (ignoringOtherApps, activateWithOptions -> {forced}, \
         cooperative activate available: {cooperated})."
    );
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
/// Shows an alert modally.
///
/// `at_the_lock_screen` is what separates the two situations this has to serve:
///
/// - **an ordinary dialog** (enrolment, a report, a confirmation): activate once,
///   show it, and then leave the user's focus alone. There is nothing to fight
///   for - the desktop is right there, and the app the user is typing into is
///   theirs. beta5 kept fighting twice a second and the dialog came out
///   invisible: the panel was hidden every time the system deactivated us, and
///   shown again when it let us activate, which is the flicker and the focus
///   stealing in the log.
/// - **the lock screen's password box**: this process owns the whole screen at
///   that point (the black windows are at the screensaver level), so it has to
///   keep the box above them and keep it key, which is what the timer is for -
///   and only for that case.
fn show_modal(alert: &NSAlert, mtm: MainThreadMarker, at_the_lock_screen: bool) -> NSModalResponse {
    let app = NSApplication::sharedApplication(mtm);
    // Regular policy only at the lock screen, and only while the box is up: with
    // black windows covering everything the Dock icon can't be seen anyway, and
    // an accessory app that the system won't let become active can't have a key
    // window - so the box couldn't take typing. For every other dialog the
    // process keeps the policy it started with, which is what beta3 did and what
    // keeps a Dock icon from appearing while someone enrols a password.
    let policy = at_the_lock_screen.then(|| {
        let previous = app.activationPolicy();
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        previous
    });

    let window = alert.window();
    // The single most likely reason a dialog of ours can be on screen, at the
    // right level, at the right frame, and still not be seen: NSAlert's window
    // is an NSPanel, and panels default to `hidesOnDeactivate = YES`. A process
    // the system deactivates (which is what happens when the user is typing in
    // Terminal) then hides its own panel, and it stays hidden until something
    // makes the app active again.
    window.setHidesOnDeactivate(false);
    raise_above_black_out(&window);
    let where_ = centre_on_cursor_display(&window, mtm);
    activate_app(&app);
    window.makeKeyAndOrderFront(None);
    if at_the_lock_screen {
        keep_prompt_on_top(&window);
    }
    let frame = window.frame();
    // Key state is not meaningful here: a panel becomes key inside the modal
    // loop, so it is false at this point even when everything works. The timer
    // reports it from inside the loop when there is a timer; for the plain
    // dialog this line plus the alert's own response code is the record.
    log!(
        "Dialog {:?} shown {where_}: level {}, frame ({}, {}, {}x{}), hidesOnDeactivate {}, \
         visible {}, occlusion {:?}, app active {}, timer {}.",
        window.title().to_string(),
        window.level(),
        frame.origin.x,
        frame.origin.y,
        frame.size.width,
        frame.size.height,
        window.hidesOnDeactivate(),
        window.isVisible(),
        window.occlusionState(),
        app.isActive(),
        at_the_lock_screen
    );
    let answer = alert.runModal();
    if at_the_lock_screen {
        stop_keeping_prompt_on_top();
    }
    if let Some(policy) = policy {
        app.setActivationPolicy(policy);
    }
    answer
}

// The prompt window, where the keep-on-top timer can reach it (both run on
// the main thread).
thread_local! {
    static PROMPT_WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
    static PROMPT_TIMER: RefCell<Option<Retained<NSTimer>>> = const { RefCell::new(None) };
    // What the timer has seen, so it can report the interesting transitions
    // instead of twice a second the same thing.
    static PROMPT_TICKS: Cell<u32> = const { Cell::new(0) };
    static PROMPT_KEY: Cell<bool> = const { Cell::new(false) };
    // Which tick last asked for activation, so the ask is rate limited: with a
    // start value of 0 the first re-activation happens two seconds in (four
    // ticks), by which point show_modal's own single ask has had time to land.
    static PROMPT_LAST_ACTIVATE: Cell<u32> = const { Cell::new(0) };
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
    PROMPT_TICKS.set(0);
    PROMPT_KEY.set(false);
    PROMPT_LAST_ACTIVATE.set(0);
    let block = RcBlock::new(|_timer: NonNull<NSTimer>| {
        PROMPT_WINDOW.with(|slot| {
            let Some(window) = slot.borrow().clone() else {
                return;
            };
            let tick = PROMPT_TICKS.get();
            PROMPT_TICKS.set(tick + 1);
            let key = window.isKeyWindow();
            let was = PROMPT_KEY.get();

            if tick == 0 {
                // The state that actually decides whether the user can see and
                // type into the box, taken while the modal loop is running.
                if let Some(mtm) = MainThreadMarker::new() {
                    log!(
                        "Lock prompt in its modal loop: level {}, key window: {key}, visible {}, hidesOnDeactivate {}, occlusion {:?}, app active {}.",
                        window.level(),
                        window.isVisible(),
                        window.hidesOnDeactivate(),
                        window.occlusionState(),
                        NSApplication::sharedApplication(mtm).isActive()
                    );
                }
            }

            if !key {
                // Lost key: raise and take it back. Not just ordered front -
                // keystrokes go to whatever window the system decided is key.
                raise_above_black_out(&window);
                window.makeKeyAndOrderFront(None);
                // Re-activating is the part the OS can refuse, and beta5's
                // twice-a-second asking is what made focus flap. At most once
                // every two seconds, and only while the box isn't key.
                if tick.saturating_sub(PROMPT_LAST_ACTIVATE.get()) >= 4 {
                    PROMPT_LAST_ACTIVATE.set(tick);
                    if let Some(mtm) = MainThreadMarker::new() {
                        log!("Lock prompt is not key; asking for activation again (tick {}).", tick + 1);
                        activate_app(&NSApplication::sharedApplication(mtm));
                    }
                }
            }

            if tick == 0 || key != was {
                PROMPT_KEY.set(key);
                log!(
                    "Dialog visible in its modal loop (tick {}): level {}, key window: {key}.",
                    tick + 1,
                    window.level()
                );
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
    show_modal(&alert, mtm, false);
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
    let answer = show_modal(&alert, mtm, false);
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
        // A bounded wait, deliberately: with `untilDate: nil` this call sleeps
        // until an event arrives *for this app*, and a black-screen session is
        // usually not the active app - the keyboard/mouse input that the other
        // thread notices goes to whoever is. So the flags it sets
        // (BACK / UNLOCKED / session taken) could go unseen forever, which is a
        // black screen with no prompt and no way out. Waking every 40ms means
        // the flags are always seen, event or no event.
        let until = NSDate::dateWithTimeIntervalSinceNow(0.04);
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
}

// ---------------------------------------------------------------------
// The pointer
//
// It's hidden for the black screen, so it isn't sitting in the middle of a
// dark monitor. But a prompt you can't see the pointer for is worse than a
// stray dot, so it comes back for anything the user has to read or answer -
// including Apple's own Touch ID panel, whose "Use Password…" and "Cancel"
// buttons are otherwise unreachable by mouse. Hide/unhide are counted by
// AppKit, so the counter below keeps them balanced, and both are only ever
// called on the main thread.
// ---------------------------------------------------------------------

static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);

fn hide_cursor() {
    if !CURSOR_HIDDEN.swap(true, Ordering::SeqCst) {
        return;
    }
    log!("Hiding the pointer.");
    NSCursor::hide();
}

fn show_cursor() {
    if !CURSOR_HIDDEN.swap(false, Ordering::SeqCst) {
        return;
    }
    log!("Showing the pointer again.");
    NSCursor::unhide();
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

use std::ptr::NonNull;
use std::time::Instant;

use block2::RcBlock;
use objc2::runtime::ProtocolObject;
use objc2::runtime::{AnyClass, AnyObject, Bool, NSObject};
use objc2::{define_class, msg_send, sel};
use objc2_app_kit::{NSWorkspace, NSWorkspaceSessionDidResignActiveNotification};
use objc2_foundation::{
    NSDate, NSError, NSNotification, NSNotificationCenter, NSObjectProtocol, NSRunLoop,
    NSRunLoopCommonModes, NSTimer,
};

use objc2_local_authentication::{LAContext, LAPolicy};

/// LAPolicyDeviceOwnerAuthenticationWithBiometrics: fingerprint only. The wider
/// device-owner policy would quietly accept the account password instead, and
/// that's the OS credential, not a Sleepless one.
const BIOMETRICS: LAPolicy = LAPolicy::DeviceOwnerAuthenticationWithBiometrics;

/// Where the system's answer lands: 0 = still waiting, 1 = yes, 2 = no.
/// An atomic slot rather than a channel because the reply can arrive after we
/// gave up, and a late callback must not be able to confuse anything.
static TOUCH_ID_ANSWER: AtomicU8 = AtomicU8::new(0);

/// How long the system prompt may stay unanswered before we stop waiting for
/// it. Enrolment has to end or time out; it must never hang.
const TOUCH_ID_TIMEOUT: Duration = Duration::from_secs(45);

/// How the reason is phrased matters: Apple renders it as
/// "<app name> is trying to <reason>", so a reason that starts with "Sleepless:"
/// came out as "sleepless is trying to Sleepless: allow Touch ID...".
const REASON_ENROL: &str = "set up Touch ID to unlock Sleepless";
const REASON_UNLOCK: &str = "unlock Sleepless";

/// Describes an NSError: "code -1001 com.apple.LocalAuthentication: ..." plus
/// its message is what makes a device report diagnosable. The code is also how
/// we tell a cancelled prompt from a refused one (-1001 user cancel, -1002
/// fallback chosen, -1004/-1001... not available, -1000 general).
fn describe_error(error: &NSError) -> String {
    let domain = error.domain();
    let why = error.localizedDescription();
    format!("code {}, {domain}: {why}", error.code())
}

fn la_context() -> Retained<LAContext> {
    unsafe { LAContext::new() }
}

/// Preflight, so a machine that can't do this finds out now rather than at the
/// lock screen.
fn can_evaluate(ctx: &LAContext) -> Result<(), String> {
    match unsafe { ctx.canEvaluatePolicy_error(BIOMETRICS) } {
        Ok(()) => Ok(()),
        Err(error) => Err(describe_error(&error)),
    }
}

/// Shows the system's Touch ID prompt and waits for the answer.
///
/// `-[LAContext evaluatePolicy:localizedReason:reply:]` returns **void**: every
/// interesting outcome arrives in the reply block. A beta read a BOOL out of it
/// anyway, got whatever was in the register (zero), concluded LocalAuthentication
/// had refused, and gave up in the same second the system panel appeared - the
/// "No passkey" alert went up beside a Touch ID prompt that was still waiting for
/// a finger. So: send it as void, and always wait for the block.
///
/// Apple documents the reply block as running "on a private queue internal to the
/// framework", with no guarantee about queue, thread or run loop, so the answer
/// goes into an atomic slot rather than anything thread-shaped, and the waiting
/// thread doesn't depend on being able to drain some particular queue.
///
/// It still has to keep AppKit's event queue running while it waits, because the
/// authentication panel is presented in this process: a run-mode slice (`runMode:
/// beforeDate:`, default mode only) left the panel on screen but unreachable and
/// the caller hung until it was killed. Draining events the way
/// `NSApplication::run` does is what lets the user actually use the prompt.
fn evaluate(ctx: &LAContext, reason: &str, mtm: MainThreadMarker) -> bool {
    let answer = RcBlock::new(|ok: Bool, error: *mut NSError| {
        if ok.as_bool() {
            log!("Touch ID answered: confirmed.");
            TOUCH_ID_ANSWER.store(1, Ordering::SeqCst);
        } else {
            let why = match unsafe { Retained::retain(error) } {
                Some(error) => describe_error(&error),
                None => "no error information".to_string(),
            };
            log!("Touch ID answered: not confirmed ({why}).");
            TOUCH_ID_ANSWER.store(2, Ordering::SeqCst);
        }
    });
    TOUCH_ID_ANSWER.store(0, Ordering::SeqCst);

    let why = NSString::from_str(reason);
    // No return value to inspect by design; the context has to stay alive
    // through the evaluation, which it does (the caller owns it).
    unsafe { ctx.evaluatePolicy_localizedReason_reply(BIOMETRICS, &why, &answer) };
    log!(
        "Touch ID request handed to the system; waiting up to {}s for the reply block, keeping AppKit running.",
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
        // One short slice of the event queue at a time, so the answer - and the
        // prompt's own UI - gets served while we wait.
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
    // Give the system prompt a hard close, so a timed-out request doesn't leave
    // Apple's panel sitting on the screen in front of whatever comes next.
    unsafe { ctx.invalidate() };
    log!(
        "Touch ID never answered within {}s - gave up and invalidated the request (the password is always enrolled first, which is why this is survivable).",
        TOUCH_ID_TIMEOUT.as_secs()
    );
    false
}

/// The passkey this machine can offer, named the way Apple names it.
pub fn passkey_supported() -> Option<&'static str> {
    let ctx = la_context();
    match can_evaluate(&ctx) {
        Ok(()) => {
            log!("Touch ID is available on this machine.");
            Some("Touch ID")
        }
        Err(why) => {
            log!("Touch ID is not available: {why}");
            None
        }
    }
}

/// Asks the system once, so a machine that can't do this finds out now rather
/// than at the lock screen. Always returns: the prompt either answers, refuses,
/// or runs out the clock.
pub fn passkey_enroll() -> Result<&'static str, String> {
    let Some(label) = passkey_supported() else {
        return Err("no usable Touch ID on this machine".to_string());
    };
    let Some(mtm) = MainThreadMarker::new() else {
        return Err("not on the main thread; cannot show the Touch ID prompt".to_string());
    };
    let ctx = la_context();
    // The prompt only shows up in front of an active app, and a tray-started
    // helper is normally an accessory process.
    let app = NSApplication::sharedApplication(mtm);
    let policy = app.activationPolicy();
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    activate_app(&app);
    log!("Asking the system to confirm your identity for the passkey.");
    // No black screen during enrolment, so this only undoes a hide if one
    // happened; nothing needs re-hiding afterwards.
    show_cursor();
    let confirmed = evaluate(&ctx, REASON_ENROL, mtm);
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
    let ctx = la_context();
    if let Err(why) = can_evaluate(&ctx) {
        log!("A passkey is enrolled but Touch ID isn't available right now: {why}");
        return false;
    }
    // The system's prompt only appears in front of an active app, and an
    // accessory process can't become active at all.
    let app = NSApplication::sharedApplication(mtm);
    let policy = app.activationPolicy();
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    activate_app(&app);
    log!("Lock screen trying the passkey before the password box.");
    // Apple's panel has Cancel and "Use Password…" buttons; with the pointer
    // hidden they can't be reached.
    show_cursor();
    let answered = evaluate(&ctx, REASON_UNLOCK, mtm);
    hide_cursor();
    app.setActivationPolicy(policy);
    answered
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
// when nothing else does, and must never be the thing that breaks the run.
// ---------------------------------------------------------------------

static OS_SESSION_TAKEN: AtomicBool = AtomicBool::new(false);

// Observer tokens have to be kept alive for as long as we want the notification
// to arrive.
thread_local! {
    static OBSERVERS: RefCell<Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>> =
        const { RefCell::new(Vec::new()) };
}

pub fn session_was_taken() -> bool {
    OS_SESSION_TAKEN.load(Ordering::SeqCst)
}

fn session_taken(source: &str) {
    log!("{source}: Sleepless is getting out of the way.");
    OS_SESSION_TAKEN.store(true, Ordering::SeqCst);
}

/// The documented session notification: it fires when the *login session* is
/// handed over - a fast user switch, or a logout. It does not fire for a plain
/// screen lock, where the session stays ours and stays on the console, which is
/// why `watch_screen_is_locked` exists as well.
///
/// This one is registered before the black windows go up; the distributed
/// subscription, which relies on a class with no bindings, comes after, so
/// nothing there can stop the screen from blanking.
fn watch_session_resign() {
    let block = RcBlock::new(|note: NonNull<NSNotification>| {
        let name = unsafe { note.as_ref() }.name();
        session_taken(&format!("Session notification {name:?}"));
    });
    log!("Session watch: asking NSWorkspace for its notification centre.");
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    log!("Session watch: subscribing to the session-resign notification.");
    let outcome = unsafe {
        try_objc(|| {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSWorkspaceSessionDidResignActiveNotification),
                None,
                None,
                &block,
            )
        })
    };
    match outcome {
        Ok(token) => {
            log!("Session watch: subscribed; keeping the token alive.");
            OBSERVERS.with(|observers| observers.borrow_mut().push(token));
            log!("Watching for the session being resigned (NSWorkspace).");
        }
        Err(why) => {
            log!("Could not watch for the session being resigned: {why} - continuing without it.")
        }
    }
}

// A tiny NSObject subclass whose only job is to be the target of a selector
// based notification observer. `addObserver:selector:name:object:` is the
// oldest subscription API NSDistributedNotificationCenter has ever implemented
// (the block variant on that centre is a different matter), and using it means
// nothing in this file hand-writes a selector's argument list.
define_class!(
    #[unsafe(super(NSObject))]
    struct SleeplessLockWatcher;

    impl SleeplessLockWatcher {
        #[unsafe(method(sleeplessSessionLockChanged:))]
        fn session_lock_changed(&self, note: Option<&NSNotification>) {
            let name = note
                .map(|note| note.name().to_string())
                .unwrap_or_else(|| "an unnamed notification".to_string());
            session_taken(&format!("Distributed notification {name:?}"));
        }
    }
);

// The observer objects have to outlive the subscription, unlike the tokens
// from the block API.
thread_local! {
    static WATCHERS: RefCell<Vec<Retained<SleeplessLockWatcher>>> = const { RefCell::new(Vec::new()) };
}

/// `com.apple.screenIsLocked`, posted by loginwindow for `Ctrl+Cmd+Q`, the lock
/// button and a screensaver lock. Never documented as public API, stable for
/// decades, and the only signal for the most common case.
///
/// Two betas died here. The first hand-wrote
/// `addObserverForName:object:queue:options:usingBlock:`; the second sent
/// `+distributedNotificationCenter`, which **does not exist** - Apple's
/// `NSDistributedNotificationCenter` has `+defaultCenter` (and
/// `+notificationCenterForType:`), and the made-up one produced
/// `-[NSDistributedNotificationCenter distributedNotificationCenter]: unrecognized
/// selector`, an Objective-C exception that cannot unwind through Rust frames, so
/// the process aborted a moment after the black windows went up and the display
/// came straight back. So this version: the right selector, every step logged
/// before and after, the whole body inside one exception guard, and an outer
/// panic guard, so the worst case is a log line and a session without the
/// screen-lock shortcut.
fn watch_screen_is_locked() {
    if env::var_os("SLEEPLESS_NO_SCREEN_LOCK_WATCH").is_some_and(|v| v != "0") {
        log!(
            "Screen-lock watch disabled by SLEEPLESS_NO_SCREEN_LOCK_WATCH: Ctrl+Cmd+Q won't \
              end the run, but everything else works."
        );
        return;
    }
    // Two independent guards: the panic one catches a Rust panic (which with
    // catch-all builds is also how a stray Objective-C exception reaches us),
    // the Objective-C one inside catches the exception itself.
    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(watch_screen_is_locked_inner));
    match outcome {
        Ok(()) => {}
        Err(payload) => log!(
            "Screen-lock watch stopped itself: {} - carrying on without it.",
            crate::logging::panic_message(payload.as_ref(), None)
        ),
    }
}

fn watch_screen_is_locked_inner() {
    log!("Screen-lock watch: looking up NSDistributedNotificationCenter.");
    if AnyClass::get(c"NSDistributedNotificationCenter").is_none() {
        log!("No NSDistributedNotificationCenter on this system; a screen lock won't be noticed.");
        return;
    }

    // +defaultCenter, inside the guard: this is where beta3 and beta4 threw
    // (beta4 sent +distributedNotificationCenter, which isn't a method of this
    // class - the documented ones are +defaultCenter and +notificationCenterForType:).
    let center = unsafe { try_objc(|| distributed_center()) };
    let center = match center {
        Ok(Some(center)) => center,
        Ok(None) => {
            log!("Screen-lock watch: no usable distributed notification centre; giving up.");
            return;
        }
        Err(why) => {
            log!("Screen-lock watch: asking for the distributed centre raised {why}; giving up.");
            return;
        }
    };

    // Primary: the selector observer, the oldest subscription API this class
    // has ever had. Everything Objective-C, including the strings, is inside a
    // guard - no step of this function runs unprotected.
    let subscribed = unsafe {
        try_objc(|| {
            let name = NSString::from_str("com.apple.screenIsLocked");
            log!("Screen-lock watch: creating the SleeplessLockWatcher observer.");
            let observer: Retained<SleeplessLockWatcher> =
                msg_send![SleeplessLockWatcher::class(), new];
            log!("Screen-lock watch: addObserver:selector:name:object:.");
            center.addObserver_selector_name_object(
                &observer,
                sel!(sleeplessSessionLockChanged:),
                Some(&name),
                None,
            );
            log!("Screen-lock watch: subscribed; keeping the observer alive.");
            WATCHERS.with(|watchers| watchers.borrow_mut().push(observer));
        })
    };
    match subscribed {
        Ok(()) => {
            log!("Watching for com.apple.screenIsLocked (selector observer).");
            return;
        }
        Err(why) => log!(
            "Screen-lock watch: the selector observer failed: {why}; trying the block observer."
        ),
    }

    // Fallback: the block observer, kept alive by its token.
    let block_outcome = unsafe {
        try_objc(|| {
            let name = NSString::from_str("com.apple.screenIsLocked");
            let block = RcBlock::new(|_note: NonNull<NSNotification>| {
                session_taken("com.apple.screenIsLocked");
            });
            log!("Screen-lock watch: addObserverForName:object:queue:usingBlock:.");
            let token =
                center.addObserverForName_object_queue_usingBlock(Some(&name), None, None, &block);
            OBSERVERS.with(|observers| observers.borrow_mut().push(token));
        })
    };
    match block_outcome {
        Ok(()) => log!("Watching for com.apple.screenIsLocked (block observer)."),
        Err(why) => log!(
            "Could not watch for com.apple.screenIsLocked: {why} - a screen lock is still caught \
             by the session-resign watch where that applies, and killing the process always works."
        ),
    }
}

/// The shared distributed notification centre, checked to be an
/// `NSNotificationCenter` (which Apple documents as its superclass, and which is
/// what makes objc2-foundation's typed observer methods valid on it). Every step
/// is logged, so a report says exactly how far this got.
///
/// # Safety
///
/// Sends Objective-C messages; call it through `try_objc`.
unsafe fn distributed_center() -> Option<Retained<NSNotificationCenter>> {
    let cls = AnyClass::get(c"NSDistributedNotificationCenter")?;
    log!("Screen-lock watch: +defaultCenter.");
    let center: *mut AnyObject = msg_send![cls, defaultCenter];
    let Some(center) = (unsafe { Retained::retain(center) }) else {
        log!("Screen-lock watch: +defaultCenter returned nothing.");
        return None;
    };
    log!("Screen-lock watch: centre retained; checking the class.");
    let Ok(center) = center.downcast::<NSNotificationCenter>() else {
        log!("Screen-lock watch: the centre isn't an NSNotificationCenter here.");
        return None;
    };
    log!("Screen-lock watch: centre is an NSNotificationCenter.");
    Some(center)
}

// ---------------------------------------------------------------------
// Objective-C exceptions: name themselves in the log
//
// An Objective-C exception cannot unwind through Rust frames. With
// `panic = "abort"` in the release profile *every* Rust frame was nounwind, so
// any exception - even one we'd wrapped in `try_objc` - ended as a SIGABRT whose
// only trace was "panic in a function that cannot unwind". The release build now
// unwinds, so the guards below can actually do their job, and these two handlers
// make sure an exception we didn't think to guard still says what it was:
//
//   - the preprocessor runs the moment an exception is thrown, before any
//     unwinding, so the log names the class, selector and reason;
//   - the uncaught handler runs on the way out if nothing catches it.
//
// Both come from libobjc (`objc-exception.h`) and are reached through objc2's
// raw FFI declarations, since there's no safer binding for them.
// ---------------------------------------------------------------------

unsafe extern "C" fn report_thrown_exception(exception: *mut AnyObject) -> *mut AnyObject {
    let described: &objc2::exception::Exception =
        unsafe { &*(exception as *const objc2::exception::Exception) };
    crate::logging::note(&format!("Objective-C exception thrown: {described:?}"));
    // Hand the same exception back on, unchanged.
    exception
}

unsafe extern "C" fn report_uncaught_exception(exception: *mut AnyObject) {
    let described: &objc2::exception::Exception =
        unsafe { &*(exception as *const objc2::exception::Exception) };
    crate::logging::note(&format!(
        "Uncaught Objective-C exception; the process is about to die: {described:?}"
    ));
}

/// Called for every macOS process, session or credential command: both talk to
/// AppKit and LocalAuthentication, so both can meet an Objective-C exception.
pub fn install_crash_reporting() {
    install_exception_reporting();
}

fn install_exception_reporting() {
    unsafe {
        objc2::ffi::objc_setExceptionPreprocessor(report_thrown_exception);
        objc2::ffi::objc_setUncaughtExceptionHandler(report_uncaught_exception);
    }
    log!("Objective-C exception reporting installed.");
}

/// Runs an Objective-C call, turning an exception into a logged failure instead
/// of an abort - the way this stays non-fatal even if Foundation doesn't like
/// something we asked it to do.
///
/// Needs the `catch-objc-exceptions` feature, because catching means compiling
/// a small Objective-C helper (`objc2-exception-helper`) for the target, and a
/// Linux cross-build has no Objective-C compiler. The macOS CI job builds with
/// it on; without it the call is simply made directly.
unsafe fn try_objc<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    #[cfg(feature = "catch-objc-exceptions")]
    return objc2::exception::catch(std::panic::AssertUnwindSafe(f))
        .map_err(|exception| exception_summary(exception));

    #[cfg(not(feature = "catch-objc-exceptions"))]
    return Ok(f());
}

/// Turns an Objective-C exception into something worth reading in a log.
#[cfg(feature = "catch-objc-exceptions")]
fn exception_summary(exception: Option<Retained<objc2::exception::Exception>>) -> String {
    match exception {
        Some(exception) => format!("{}", exception),
        None => "an exception with no information".to_string(),
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
    log!("Session starting (asroot: {}).", opts.asroot);
    // Before anything else that can throw, so whatever the cause, the log names
    // the exception rather than the run just stopping.
    install_exception_reporting();
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
    log!("Stage: the black screen.");
    let mtm = MainThreadMarker::new().unwrap_or_else(|| fail("must run on the main thread"));
    watch_session_resign();
    // A prompt that never got shown, or a session the user locked behind our
    // back, must never be what stands between them and their desktop: this
    // thread ends the run the moment macOS says the session is ours no more.
    if let Err(e) = spawn_thread("sleepless-session-watcher", || {
        while !session_was_taken() {
            thread::sleep(Duration::from_millis(500));
        }
        release_and_exit();
    }) {
        log!("Could not start the session watcher: {e} - Ctrl+Cmd+Q won't end the run by itself.");
    }
    let _windows = black_out(mtm);
    // The riskier subscription comes after the screen is already black, so a
    // problem there can never stop the display from going off.
    watch_screen_is_locked();

    // --- 4. Wait for the user to come back ---
    log!("Stage: waiting to be interrupted (activity, or the session being taken over).");
    // AppKit needs the main thread to keep the windows up, so the waiting
    // happens on another one, which also ends the process. With the gate armed
    // that thread only announces the return: the prompt has to be shown on the
    // main thread, and nothing is released until it's answered.
    //
    // Whether the gate is armed is decided when the user comes back, not when
    // the run started, so a password enrolled while the screen was black still
    // gets asked for. The thread that sees the input records the answer in
    // GATE_ARMED before it raises BACK, so both threads agree.
    let worker = std::thread::Builder::new()
        .name("sleepless-activity-watcher".to_string())
        .spawn(move || {
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
    if let Err(e) = worker {
        // Without the thread that notices the return, the gate can never be
        // armed: say so, and end the run rather than leaving a black screen
        // nobody can answer.
        log!("Could not start the activity watcher: {e} - releasing and exiting.");
        release_and_exit();
    }

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
