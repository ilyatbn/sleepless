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
use std::io::Write;
use std::path::Path;
use std::process::Child;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use std::{env, fs, io, thread};

use objc2::rc::Retained;
use objc2::ClassType;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy,
    NSBackingStoreType, NSColor, NSCursor, NSEvent, NSEventMask, NSFont, NSRunningApplication,
    NSScreen, NSSecureTextField, NSTextField, NSWindow, NSWindowCollectionBehavior,
    NSWindowStyleMask,
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
fn black_out(mtm: MainThreadMarker) -> Vec<Retained<SleeplessBlackWindow>> {
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
            // Key-capable on purpose: this same window hosts the password box
            // when the user comes back.
            let window = match unsafe { try_objc(|| black_window(frame)) } {
                Ok(Some(window)) => window,
                Ok(None) => fail("could not create a black window"),
                Err(why) => fail(&format!("could not create a black window: {why}")),
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
    // Activation is a request since macOS 14, so ask and then *check*: the
    // keypress that wakes the display goes to the frontmost app, and if that
    // isn't us, the password typed a moment later is a shell command in someone
    // else's window. The log records which way it went.
    let outcome = unsafe { try_objc(|| activate_app(&app)) };
    if let Err(why) = outcome {
        log!("Activating the app failed: {why} - continuing anyway.");
    }
    let settled = wait_for_active(mtm, Duration::from_millis(400));
    log!("Black screen: this app is the one the keyboard belongs to: {settled}.");
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
// There is no dialog here at all. An enrolment dialog shown by this process is
// shown by a background command-line child, and since macOS 14 activation is
// cooperative: the system decides, and it does not hand focus to a child process
// while the user is typing in Terminal. beta3 through beta5 tried to force it -
// activation-policy flips, a twice-a-second `activate`, window-level games - and
// the result was a panel that was on screen at the right frame and invisible,
// with the user's focus being dragged back every other tick.
//
// So: the credential prompts a *person* answers live in the tray app
// (sleepless-gui, which is the app the user just clicked, `gui/src/enrol.rs`),
// and this process reads from the terminal, which is what someone running
// `sleepless --set-password` by hand or over ssh wants anyway.
// ---------------------------------------------------------------------

use crate::lock::{self, Secret};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

/// Asks **once** for this process to become the active app.
///
/// Three ways, because macOS 14 changed the rules and we still run on older
/// ones: the deprecated forceful call (still honoured there),
/// `NSRunningApplication::activateWithOptions(ActivateAllWindows)` - only that
/// option, since the `ActivateIgnoringOtherApps` *option* is documented to have
/// no effect since 14 - and 14's cooperative `-activate`, which exists only on 14
/// and so sits behind `respondsToSelector:` (sending a method the system doesn't
/// have is an unrecognized-selector exception, and that cost two betas already).
///
/// Once, never in a poll loop: cooperative activation is a request the system may
/// grant or withdraw, and beta5 asked twice a second, which is what made the key
/// window flap and dragged focus back from the user's terminal.
fn activate_app(app: &NSApplication) {
    #[allow(deprecated)] // the pre-14 way; still accepted there
    app.activateIgnoringOtherApps(true);
    let me = NSRunningApplication::currentApplication();
    let forced = me.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
    // Typed (NSObjectProtocol): no hand-written signature to get wrong.
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

/// Asks for a secret in the terminal, echoing nothing. None if there's no
/// terminal or the user cancelled.
pub fn prompt_secret_interactive(_title: &str, message: &str) -> Option<Secret> {
    if !unix::has_tty() {
        log!("No terminal to ask in, and this process shows no dialog: use the menu bar, or --stdin.");
        eprintln!(
            "Sleepless: there's no terminal here to type a password into.\n\
             Use the menu-bar icon, or pipe the password in with `--set-password --stdin`."
        );
        return None;
    }
    // Same trick the Linux backend uses: stty turns echo off, so what's typed
    // doesn't appear on screen; put it back however it was afterwards.
    let echo_off = std::process::Command::new("stty")
        .args(["-echo"])
        .status()
        .is_ok_and(|s| s.success());
    eprint!("{message} ");
    let _ = <io::Stderr as io::Write>::flush(&mut io::stderr());
    let mut line = String::new();
    let read = io::stdin().read_line(&mut line);
    if echo_off {
        let _ = std::process::Command::new("stty").args(["echo"]).status();
    }
    eprintln!();
    match read {
        Ok(n) if n > 0 => Some(Secret::new(line.trim_end_matches(['\n', '\r']))),
        _ => None,
    }
}

/// One-line result message. Printed, not popped up: whoever started this process
/// (the tray app, a terminal, ssh) is the one that can show it.
pub fn report(message: &str) {
    log!("Report to the user: {}", message.replace('\n', " "));
    for line in message.lines() {
        eprintln!("Sleepless: {line}");
    }
    let _ = io::stderr().flush();
}

/// Yes/no from the terminal. With no terminal, no: nothing asked it.
pub fn ask_yes_no(_title: &str, message: &str) -> bool {
    if !unix::has_tty() {
        log!("No terminal to ask {message:?} in; taking that as no.");
        return false;
    }
    eprint!("{message} [y/N] ");
    let _ = <io::Stderr as io::Write>::flush(&mut io::stderr());
    let mut answer = String::new();
    let _ = io::stdin().read_line(&mut answer);
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

// ---------------------------------------------------------------------
// The lock screen: the password box lives *in* the black window
//
// A borderless NSWindow answers NO to canBecomeKeyWindow, which is why an earlier
// design layered a separate panel on top of the black windows and then had to
// fight to keep that panel key and visible. The fight is what broke: a panel
// above windows at the screensaver level, in a process the system keeps
// deactivating, is exactly the situation where it ends up invisible.
//
// So the black window is a subclass that can be key, and the secure text field,
// the question and the status line are subviews of it. Nothing is layered above
// the black windows, so nothing can be hidden behind them. Keyboard input comes
// to the field the ordinary way, and Return / keypad-Enter / Escape are picked up
// by one local event monitor - which only ever runs while this box is on screen.
// ---------------------------------------------------------------------

// A unique class name: registering something generic into a process that loads
// AppKit is a collision waiting to happen.
define_class!(
    #[unsafe(super(NSWindow))]
    struct SleeplessBlackWindow;

    impl SleeplessBlackWindow {
        // A borderless window answers NO to this by default, which is why the
        // field could never be typed into without the subclass.
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            true
        }
    }
);

/// Makes one black window.
///
/// Two hand-written messages, and the reason is that a class defined at runtime
/// has no generated constructor for its own type: `+alloc` and
/// `-initWithContentRect:styleMask:backing:defer:` are Apple's documented ones
/// (the typed NSWindow binding is `NSWindow.rs:570`), the argument types are
/// taken straight from that declaration, and the result is *checked* with a
/// runtime downcast instead of asserted. A failed init returns nil, which is why
/// the return type is `Option`.
///
/// # Safety
///
/// Sends Objective-C messages.
unsafe fn black_window(frame: NSRect) -> Option<Retained<SleeplessBlackWindow>> {
    let alloced: *mut AnyObject = msg_send![SleeplessBlackWindow::class(), alloc];
    if alloced.is_null() {
        log!("Black window: +alloc returned nil.");
        return None;
    }
    let init: *mut AnyObject = msg_send![
        alloced,
        initWithContentRect: frame,
        styleMask: NSWindowStyleMask::Borderless,
        backing: NSBackingStoreType::Buffered,
        defer: false,
    ];
    let Some(window) = Retained::retain(init) else {
        log!("Black window: -initWithContentRect:styleMask:backing:defer: returned nil.");
        return None;
    };
    window.downcast::<SleeplessBlackWindow>().ok()
}

/// The widgets of the password box, and what the user did with them. Main
/// thread only, like everything here: the event monitor and the field editor
/// both run where events are dispatched.
struct GateBox {
    window: Retained<SleeplessBlackWindow>,
    field: Retained<NSSecureTextField>,
    status: Retained<NSTextField>,
    monitor: Retained<AnyObject>,
}

thread_local! {
    static GATE_BOX: RefCell<Option<GateBox>> = const { RefCell::new(None) };
    // What the last key press did: filled in by the event monitor, taken by
    // `wait_for_box`. `Entered` carries what was typed.
    static GATE_KEY: RefCell<GateKey> = const { RefCell::new(GateKey::Nothing) };
    /// The question the gate is asking, so that becoming active again can put the
    /// same words back where they were.
    static GATE_MESSAGE: RefCell<String> = const { RefCell::new(String::new()) };
    /// Set while this app is *not* the active one: the status line tells the user
    /// to click, and nothing overwrites it until the click arrives.
    static GATE_NEEDS_CLICK: Cell<bool> = const { Cell::new(false) };
    /// Set by Escape: its notice stays up for the round that follows instead of
    /// being painted over by the next prompt.
    static GATE_ESCAPE_NOTICE: Cell<bool> = const { Cell::new(false) };
    /// The app's active state as last seen, so a change is logged once.
    static GATE_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

enum GateKey {
    Nothing,
    Entered(Secret),
    Escaped,
}

const KEY_RETURN: u16 = 36;
const KEY_PAD_ENTER: u16 = 76;
const KEY_ESCAPE: u16 = 53;

/// Which black window gets the password box: the one under the pointer, so the
/// question is where the user is looking. Falls back to the first window when the
/// pointer is somehow outside them all.
///
/// The right and top edges are exclusive and the left and bottom inclusive, which
/// is how AppKit tiles the coordinate spaces: a point exactly on the shared edge
/// belongs to the display that *starts* there.
fn pick_box_window(frames: &[NSRect], cursor: NSPoint) -> usize {
    let mut chosen = 0usize;
    for (index, frame) in frames.iter().enumerate() {
        let inside = cursor.x >= frame.origin.x
            && cursor.x < frame.origin.x + frame.size.width
            && cursor.y >= frame.origin.y
            && cursor.y < frame.origin.y + frame.size.height;
        if inside {
            chosen = index;
        }
    }
    chosen
}

/// Puts the question, the secure field and the status line into the black window
/// on the display the pointer is on, and takes keyboard focus.
///
/// False means the box could not be built; the caller then ends the session
/// rather than leaving a black screen nobody can answer.
fn show_password_box(mtm: MainThreadMarker, windows: &[Retained<SleeplessBlackWindow>]) -> bool {
    if windows.is_empty() {
        log!("Password box: there are no black windows to put it in.");
        return false;
    }
    let cursor = NSEvent::mouseLocation();
    let frames: Vec<NSRect> = windows.iter().map(|window| window.frame()).collect();
    let chosen = pick_box_window(&frames, cursor);
    let describe: Vec<String> = frames
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            format!(
                "#{index} {}x{}@{},{}",
                frame.size.width, frame.size.height, frame.origin.x, frame.origin.y
            )
        })
        .collect();
    log!(
        "Password box: pointer at ({}, {}); displays {}; using #{chosen}.",
        cursor.x,
        cursor.y,
        describe.join(", ")
    );
    let window = &windows[chosen];

    let outcome = unsafe {
        try_objc(|| {
            let Some(content) = window.contentView() else {
                log!("Password box: the window has no content view.");
                return false;
            };
            let area = content.frame();
            // Room for a wide field and two lines of text, centred a little
            // below the middle (AppKit's origin is bottom-left).
            let width: f64 = 340.0;
            let left = (area.size.width - width) / 2.0;
            let middle = area.size.height / 2.0;

            let question = make_label(
                &NSString::from_str("Enter your password to come back"),
                15.0,
            );
            question.setFrame(NSRect::new(
                NSPoint::new(left, middle + 46.0),
                NSSize::new(width, 22.0),
            ));
            content.addSubview(&question);

            let field = NSSecureTextField::new(mtm);
            field.setFrame(NSRect::new(
                NSPoint::new(left, middle - 14.0),
                NSSize::new(width, 26.0),
            ));
            field.setEditable(true);
            field.setPlaceholderString(Some(&NSString::from_str("password")));
            content.addSubview(&field);

            let status = make_label(&NSString::new(), 13.0);
            status.setFrame(NSRect::new(
                NSPoint::new(left, middle - 52.0),
                NSSize::new(width, 22.0),
            ));
            content.addSubview(&status);

            // Return and Escape, while this box is up. The monitor is a local
            // one, so it only sees events for this app - which is the whole
            // point: it never interferes with anything else, and it goes away
            // with `hide_password_box`.
            let monitor = NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                NSEventMask::KeyDown,
                &RcBlock::new(|event: NonNull<NSEvent>| {
                    let event: &NSEvent = event.as_ref();
                    match event.keyCode() {
                        KEY_RETURN | KEY_PAD_ENTER => {
                            let typed = GATE_BOX.with(|slot| {
                                slot.borrow()
                                    .as_ref()
                                    .map(|held| {
                                        // Take what is being typed out of the
                                        // field editor. While an edit is in
                                        // progress the control's stringValue is
                                        // the *committed* value, not the live
                                        // text, because a shared field editor is
                                        // the real first responder; -validateEditing
                                        // is AppKit's documented way to copy the
                                        // editor's contents back into the cell,
                                        // and unlike resigning first responder it
                                        // leaves the caret in the box.
                                        held.field.validateEditing();
                                        let submitted = held.field.stringValue().to_string();
                                        // Take the submitted text out of the box
                                        // straight away, and let the next round
                                        // put the caret back. Otherwise the last
                                        // guess sits there through the whole
                                        // back-off wait - visible to anyone
                                        // looking over the shoulder, and in the
                                        // way of the next attempt.
                                        held.window.makeFirstResponder(None);
                                        held.field.setStringValue(&NSString::new());
                                        submitted
                                    })
                                    .unwrap_or_default()
                            });
                            let entered = typed.chars().count();
                            GATE_KEY.with(|key| {
                                *key.borrow_mut() = GateKey::Entered(Secret::take(typed))
                            });
                            log!("Password box: Return pressed, {entered} characters committed.");
                            // Swallow it: no beep, and the field doesn't start
                            // a new editing session behind our back.
                            ptr::null_mut()
                        }
                        KEY_ESCAPE => {
                            GATE_BOX.with(|slot| {
                                if let Some(held) = slot.borrow().as_ref() {
                                    // Escape never unlocks; it empties the box.
                                    // -abortEditing throws away the in-progress
                                    // edit (the field editor's text, not the
                                    // cell's), which is what the user means by
                                    // "clear what I typed"; the value is set
                                    // empty as well in case nothing was being
                                    // edited. Focus stays in the box.
                                    held.field.abortEditing();
                                    held.field.setStringValue(&NSString::new());
                                }
                            });
                            GATE_KEY.with(|key| *key.borrow_mut() = GateKey::Escaped);
                            ptr::null_mut()
                        }
                        _ => event as *const NSEvent as *mut NSEvent,
                    }
                }),
            );
            let Some(monitor) = monitor else {
                log!("Password box: could not install the key monitor.");
                return false;
            };

            window.setInitialFirstResponder(Some(&field));
            window.makeKeyAndOrderFront(None);
            let focused = window.makeFirstResponder(Some(&field));
            log!(
                "Password box: window key {}, field is first responder: {focused}.",
                window.isKeyWindow()
            );
            // A box nobody can type into is not a lock screen, it's a trap: the
            // caller hands the display back instead.
            if !focused {
                log!("Password box: the field did not take keyboard focus.");
                return false;
            }
            GATE_BOX.with(|slot| {
                *slot.borrow_mut() = Some(GateBox {
                    window: window.clone(),
                    field,
                    status,
                    monitor,
                })
            });
            true
        })
    };
    match outcome {
        Ok(true) => true,
        Ok(false) => false,
        // An exception in there is a log line, not an abort.
        Err(why) => {
            log!("Password box could not be built: {why}");
            false
        }
    }
}

/// A line of white text on the black window: no box, no background.
fn make_label(text: &NSString, size: f64) -> Retained<NSTextField> {
    let label = NSTextField::new(nil_marker());
    label.setStringValue(text);
    label.setEditable(false);
    label.setSelectable(false);
    label.setBezeled(false);
    label.setDrawsBackground(false);
    label.setTextColor(Some(&NSColor::whiteColor()));
    label.setFont(Some(&NSFont::systemFontOfSize(size)));
    label
}

/// What the box says right now, and a clean field to type the next attempt into.
fn set_box_status(message: &str) {
    GATE_MESSAGE.with(|held| *held.borrow_mut() = message.to_string());
    set_status_line(message);
    GATE_BOX.with(|slot| {
        if let Some(held) = slot.borrow().as_ref() {
            // Never leave the attempt that just failed sitting in the box: the
            // in-progress edit goes away first, then the cell's value.
            held.field.abortEditing();
            held.field.setStringValue(&NSString::new());
            // Keyboard focus stays where it was, on the box.
            if !held.window.isKeyWindow() {
                held.window.makeKeyAndOrderFront(None);
            }
            let _ = held.window.makeFirstResponder(Some(&held.field));
        }
    });
}

/// Only the line under the field: for notices that must not disturb the box.
fn set_status_line(message: &str) {
    GATE_BOX.with(|slot| {
        if let Some(held) = slot.borrow().as_ref() {
            held.status.setStringValue(&NSString::from_str(message));
        }
    });
}

/// Whether AppKit thinks this app is the active one.
///
/// This - not "the field is first responder" - is the evidence that keystrokes
/// reach the box. A field in a window of an *inactive* app is still that window's
/// first responder, and the typing goes to whoever is frontmost: on a developer's
/// machine that is usually Terminal, which would run the password as a shell
/// command and keep it in the history. So the gate never pretends to be
/// answerable while this is false.
fn app_is_active(mtm: MainThreadMarker) -> bool {
    NSApplication::sharedApplication(mtm).isActive()
}

/// Waits briefly, pumping, for the system to make this app active.
fn wait_for_active(mtm: MainThreadMarker, how_long: Duration) -> bool {
    let until = Instant::now() + how_long;
    loop {
        if app_is_active(mtm) {
            note_active(mtm, true);
            return true;
        }
        if Instant::now() >= until || session_was_taken() || !pump_slice(mtm) {
            note_active(mtm, app_is_active(mtm));
            return false;
        }
    }
}

/// Asks for activation, and asks **once** more if the first request was declined.
/// Never in a poll loop: cooperative activation is a request, and beta5 asked
/// twice a second, which made the key window flap and pulled focus away from the
/// user's terminal every other tick.
fn activate_and_wait(mtm: MainThreadMarker, what: &str) -> bool {
    let app = NSApplication::sharedApplication(mtm);
    let asked = unsafe { try_objc(|| activate_app(&app)) };
    if let Err(why) = asked {
        log!("{what}: the activation request threw: {why}");
    }
    if wait_for_active(mtm, Duration::from_millis(600)) {
        return true;
    }
    log!("{what}: not the active app yet - asking once more.");
    let outcome = unsafe { try_objc(|| activate_app(&app)) };
    if let Err(why) = outcome {
        log!("{what}: the second activation request threw: {why}");
        return false;
    }
    let active = wait_for_active(mtm, Duration::from_millis(600));
    log!("{what}: active after the second request: {active}.");
    active
}

/// Records a change in "does the keyboard belong to us" and puts up, or takes
/// down, the affordance that goes with it.
///
/// A click on one of our own windows always activates our app - the one path
/// cooperative activation cannot decline - so while the app is inactive the box
/// says to click, and the moment it is active again the field takes the focus and
/// the question comes back.
fn note_active(mtm: MainThreadMarker, active: bool) {
    let _ = mtm;
    let was = GATE_ACTIVE.with(|slot| slot.replace(active));
    if was == active {
        return;
    }
    if active {
        log!("The keyboard belongs to Sleepless again.");
        if GATE_NEEDS_CLICK.with(|slot| slot.replace(false)) {
            GATE_BOX.with(|slot| {
                if let Some(held) = slot.borrow().as_ref() {
                    held.window.makeKeyAndOrderFront(None);
                    let _ = held.window.makeFirstResponder(Some(&held.field));
                }
            });
            let message = GATE_MESSAGE.with(|held| held.borrow().clone());
            set_box_status(&message);
        }
    } else {
        log!(
            "The keyboard does NOT belong to Sleepless: anything typed goes to              another app."
        );
        GATE_NEEDS_CLICK.with(|slot| slot.set(true));
        set_status_line("Click this screen to type your password");
    }
}

/// The back-off penalty, waited out with AppKit running.
///
/// Sleeping here would freeze the main thread for up to sixteen seconds: no
/// drawing, no keyboard, and - because `com.apple.screenIsLocked` is delivered on
/// the main run loop - no Ctrl+Cmd+Q either, so the OS lock would not be noticed
/// until the penalty expired. This pumps instead, and the countdown is the status
/// line. False means the session was taken over during the wait.
fn wait_pumped(mtm: MainThreadMarker, how_long: Duration, announce: bool) -> bool {
    let until = Instant::now() + how_long;
    let mut shown = u64::MAX;
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.as_millis() == 0 {
            return true;
        }
        let secs = left.as_secs() + u64::from(left.subsec_millis() > 0);
        if announce && secs != shown {
            shown = secs;
            set_status_line(&format!("Too many attempts - wait {secs}s"));
        }
        if !pump_slice(mtm) {
            log!("Waiting out the penalty: AppKit stopped answering.");
            release_and_exit();
        }
        if session_was_taken() {
            return false;
        }
    }
}

/// Waits for the user to press Return (or Escape) in the box, pumping AppKit's
/// queue meanwhile so the field editor works. There's no timeout on purpose:
/// standing in front of a locked screen is allowed. The session being taken over
/// by the OS ends the wait.
fn wait_for_box(mtm: MainThreadMarker) -> lock::Ask {
    GATE_KEY.with(|key| *key.borrow_mut() = GateKey::Nothing);
    let started = Instant::now();
    let mut last_breadcrumb = started;
    loop {
        // Keeps the field editor alive: this is what delivers the keystrokes.
        if !pump_slice(mtm) {
            // A box that cannot hear anything is not a lock screen: hand the
            // display back rather than hold a screen nobody can answer.
            log!("Password box: AppKit stopped answering; ending the session.");
            release_and_exit();
        }
        // The one fact that decides whether typing here reaches the box.
        note_active(mtm, app_is_active(mtm));

        let pressed = GATE_KEY.with(|key| {
            let mut slot = key.borrow_mut();
            core::mem::replace(&mut *slot, GateKey::Nothing)
        });
        match pressed {
            GateKey::Entered(text) => return lock::Ask::Guess(text),
            GateKey::Escaped => {
                // Escape is not a guess, so it costs no back-off, and it never
                // unlocks. The notice is written on the status line only, and
                // flagged, so the gate's next pass leaves it standing instead of
                // painting the prompt back over it.
                log!("Password box: Escape pressed; that clears the box and does not unlock.");
                GATE_ESCAPE_NOTICE.with(|slot| slot.set(true));
                set_status_line("Escape does not unlock Sleepless");
                return lock::Ask::Dismissed;
            }
            GateKey::Nothing => {}
        }
        if session_was_taken() {
            log!("Password box: the session was taken over while waiting.");
            return lock::Ask::Dismissed;
        }

        // Someone standing in front of a locked screen is allowed to take their
        // time, so there's no timeout here - but say, every twenty seconds, that
        // the box is still up and still waiting, so a log read after the fact can
        // tell "nobody came back" from "the box was there and deaf".
        let now = Instant::now();
        if now - last_breadcrumb >= Duration::from_secs(20) {
            last_breadcrumb = now;
            log!(
                "Password box: still waiting for a password after {}s (window key: {},                  keyboard is ours: {});",
                (now - started).as_secs(),
                GATE_BOX.with(|slot| {
                    slot.borrow()
                        .as_ref()
                        .map(|held| held.window.isKeyWindow())
                        .unwrap_or(false)
                }),
                app_is_active(mtm)
            );
        }
    }
}

/// Takes the box away again, before the session ends.
fn hide_password_box() {
    GATE_BOX.with(|slot| {
        if let Some(held) = slot.borrow_mut().take() {
            unsafe { NSEvent::removeMonitor(&held.monitor) };
            // The window is kept here so it can't go away while its subviews are
            // answering; give up first responder on the way out.
            held.window.makeFirstResponder(None);
        }
    });
}

/// objc2 needs a main-thread marker to build a view; this function is only ever
/// called from the main thread, and the borrow of the marker is what says so.
fn nil_marker() -> MainThreadMarker {
    MainThreadMarker::new().expect("built from the main thread")
}

/// Runs AppKit by hand for one short slice, and returns false if it threw.
///
/// Two things happen in a slice, and both are needed:
///
/// * the event queue is drained and dispatched - this is what makes typing in the
///   password field, and Apple's own Touch ID panel, work at all. A plain sleep on
///   this thread delivers nothing;
/// * the run loop is then given a brief turn, because that is how everything
///   *else* arrives: the `com.apple.screenIsLocked` distributed notification
///   (NSDistributedNotificationCenter delivers with `performSelectorOnMainThread:`),
///   timers, and any other mach-port input source. Pumping only the event queue
///   would leave the session-lock watchdog deaf exactly when it matters - while the
///   gate is up.
///
/// The event fetch is bounded rather than `untilDate: nil` on purpose: with no
/// timeout this call sleeps until an event arrives *for this app*, and a black
/// screen session is usually not the active app, so the flags another thread sets
/// (back / unlocked / session taken) could go unseen forever - a black screen with
/// no prompt and no way out.
fn pump_slice(mtm: MainThreadMarker) -> bool {
    let app = NSApplication::sharedApplication(mtm);
    let drained = unsafe {
        try_objc(|| {
            let mut count = 0usize;
            while let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                Some(&NSDate::distantPast()),
                NSDefaultRunLoopMode,
                true,
            ) {
                app.sendEvent(&event);
                count += 1;
            }
            count
        })
    };
    if drained.is_err() {
        log!(
            "AppKit threw while dispatching events: {}",
            drained.unwrap_err()
        );
        return false;
    }
    let slice = NSDate::dateWithTimeIntervalSinceNow(0.04);
    let spun = unsafe {
        try_objc(|| NSRunLoop::currentRunLoop().runMode_beforeDate(NSDefaultRunLoopMode, &slice))
    };
    if spun.is_err() {
        log!(
            "AppKit threw while spinning the run loop: {}",
            spun.unwrap_err()
        );
        return false;
    }
    true
}

/// Runs AppKit by hand until `stop` says so, or until the session is taken over.
///
/// `NSApplication::run()` would do this too, but it never gives us the chance to
/// notice that the thread watching for input has seen the user come back, and the
/// password box has to be built on the main thread.
fn pump_events_until(mtm: MainThreadMarker, stop: impl Fn() -> bool) {
    while !stop() {
        if !pump_slice(mtm) {
            return;
        }
        if session_was_taken() {
            log!("Session was taken over while pumping events: giving the screen back.");
            return;
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

/// Whether a hide/show request should actually act, given the state the pointer
/// is thought to be in - and the state it ends up in. AppKit **counts** these
/// calls, so acting twice in one direction leaves the pointer wrong for the rest
/// of the session. The bug this replaces: the guard was the wrong way round, so
/// the first `hide_cursor` returned without hiding and no black screen ever
/// hid the pointer.
///
/// (`hidden`, `want_hide`) -> `(new state, act)`
fn cursor_action(hidden: bool, want_hide: bool) -> (bool, bool) {
    match (hidden, want_hide) {
        (true, true) => (true, false),    // already hidden
        (false, true) => (true, true),    // hide now
        (false, false) => (false, false), // already visible
        (true, false) => (false, true),   // show again
    }
}

fn hide_cursor() {
    let (hidden, act) = cursor_action(CURSOR_HIDDEN.load(Ordering::SeqCst), true);
    CURSOR_HIDDEN.store(hidden, Ordering::SeqCst);
    if !act {
        return;
    }
    log!("Hiding the pointer.");
    NSCursor::hide();
}

fn show_cursor() {
    let (hidden, act) = cursor_action(CURSOR_HIDDEN.load(Ordering::SeqCst), false);
    CURSOR_HIDDEN.store(hidden, Ordering::SeqCst);
    if !act {
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

use block2::RcBlock;
use objc2::runtime::ProtocolObject;
use objc2::runtime::{AnyClass, AnyObject, Bool, NSObject};
use objc2::{define_class, msg_send, sel};
use objc2_app_kit::{NSWorkspace, NSWorkspaceSessionDidResignActiveNotification};
use objc2_foundation::{
    NSDate, NSError, NSNotification, NSNotificationCenter, NSObjectProtocol, NSRunLoop,
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

    let deadline = Instant::now() + TOUCH_ID_TIMEOUT;
    while Instant::now() < deadline {
        match TOUCH_ID_ANSWER.load(Ordering::SeqCst) {
            1 => return true,
            2 => return false,
            _ => {}
        }
        // One slice at a time: events dispatched (Apple's panel is drawn and
        // handled by this process, so a sleeping thread means a dead panel) and
        // the run loop serviced (the reply block and any session-lock
        // notification arrive that way).
        if !pump_slice(mtm) {
            log!("Touch ID wait: AppKit stopped answering.");
            break;
        }
        if session_was_taken() {
            log!("Session was taken over while Touch ID was being answered.");
            unsafe { ctx.invalidate() };
            return false;
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
    // accessory process is a harder case for the system to bring forward, so the
    // policy goes Regular for the length of the prompt and straight back after.
    // (Ordinary dialogs never do this: this is the one place it's still used, and
    // it is Apple's own window we are trying to get on screen.)
    let app = NSApplication::sharedApplication(mtm);
    let policy = app.activationPolicy();
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    activate_app(&app);
    log!("Lock screen trying the passkey before the password box.");
    // Apple's panel has Cancel and "Use Password…" buttons; with the pointer
    // hidden they can't be reached.
    show_cursor();
    let answered = evaluate(&ctx, REASON_UNLOCK, mtm);
    app.setActivationPolicy(policy);
    // The pointer deliberately stays visible: when Touch ID doesn't help, the
    // password box underneath is what the user has to answer.
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
        // The box goes up first, so a Touch ID attempt happens with our window
        // already key: if the sensor can't help, the field is simply still there
        // and already focused (and it is only attempted at all while this app owns
        // the keyboard - see below).
        if !show_password_box(mtm, &_windows) {
            // Nothing to type into. Handing the screen back is better than
            // holding a black monitor nobody can answer.
            log!("No password box could be shown; ending the session rather than holding the screen.");
            release_and_exit();
        }
        // Ask for the keyboard, then find out whether the system granted it. If
        // it did not, the box says so in words: typing into an app that isn't
        // active goes to whatever app is, and a password typed into a shell is a
        // shell command that also ends up in the shell's history.
        let mine = activate_and_wait(mtm, "Password box");
        GATE_NEEDS_CLICK.with(|slot| slot.set(!mine));
        if !mine {
            set_status_line("Click this screen to type your password");
        }
        show_cursor();
        // Touch ID is only worth asking while the keyboard and the screen are
        // ours: with the app inactive Apple's panel may never come up at all, and
        // the wait for it is bounded but slow - the user would be staring at
        // "click this screen" for 45 seconds. Clicking first costs nothing.
        let touch_id_now = lock::has_passkey() && app_is_active(mtm);
        if lock::has_passkey() && !touch_id_now {
            log!(
                "Not asking Touch ID while another app owns the keyboard; the password                  box is waiting for a click."
            );
        }
        let unlocked = (touch_id_now && passkey_unlock(mtm))
            || lock::unlock_loop_with_wait(
                |message| {
                    // The question is always remembered; it is only painted over
                    // a notice the user still needs to see (click first, or the
                    // Escape that just happened).
                    GATE_MESSAGE.with(|held| *held.borrow_mut() = message.to_string());
                    let notice_stands = GATE_NEEDS_CLICK.with(|slot| slot.get())
                        || GATE_ESCAPE_NOTICE.with(|slot| slot.replace(false));
                    if !notice_stands {
                        set_box_status(message);
                    }
                    wait_for_box(mtm)
                },
                session_was_taken,
                |pause| match pause {
                    // A penalty is announced in the box; the brief breath after
                    // a dismissed prompt is not, or Escape's own notice would be
                    // relabelled as a penalty it never was.
                    lock::Pause::Penalty(how_long) => wait_pumped(mtm, how_long, true),
                    lock::Pause::Brief(how_long) => wait_pumped(mtm, how_long, false),
                },
            );
        hide_password_box();
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

    fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
    }

    /// The box has to land on the display the pointer is on; on a two-screen desk
    /// the alternative is a black monitor with the question on the other one.
    #[test]
    fn the_password_box_follows_the_pointer() {
        let main = rect(0.0, 0.0, 1920.0, 1080.0);
        let second = rect(1920.0, 0.0, 2560.0, 1440.0);
        let frames = [main, second];

        assert_eq!(pick_box_window(&frames, NSPoint::new(100.0, 100.0)), 0);
        assert_eq!(pick_box_window(&frames, NSPoint::new(2000.0, 1000.0)), 1);
        // Exactly on the shared edge belongs to the display that starts there,
        // because the previous display's right edge is exclusive.
        assert_eq!(pick_box_window(&frames, NSPoint::new(1920.0, 10.0)), 1);
        assert_eq!(pick_box_window(&frames, NSPoint::new(1919.0, 10.0)), 0);
        // Off every display (a pointer yanked out mid-session) still gets a
        // window rather than nothing at all.
        assert_eq!(pick_box_window(&frames, NSPoint::new(-500.0, -500.0)), 0);
        // A second display placed to the left, a common layout that puts
        // negative coordinates in play.
        let left = rect(-2560.0, 0.0, 2560.0, 1440.0);
        assert_eq!(
            pick_box_window(&[main, left], NSPoint::new(-100.0, 500.0)),
            1
        );
        // Stacked above rather than beside: the point above the main display
        // lands on the upper one.
        let above = rect(0.0, 1080.0, 1920.0, 1080.0);
        assert_eq!(
            pick_box_window(&[main, above], NSPoint::new(10.0, 1081.0)),
            1
        );
    }

    /// AppKit counts hide/show, so a request acts only when it would change the
    /// state. The bug this pins: the guard was the wrong way round, so the first
    /// `hide_cursor` returned without hiding, and the pointer stayed visible over
    /// a black screen for the whole session.
    #[test]
    fn the_pointer_counter_acts_once_per_change() {
        assert_eq!(cursor_action(false, true), (true, true)); // hide now
        assert_eq!(cursor_action(true, true), (true, false)); // already hidden
        assert_eq!(cursor_action(true, false), (false, true)); // show again
        assert_eq!(cursor_action(false, false), (false, false)); // already shown

        // Repeated requests in one direction stay balanced: an action always
        // flips the state and a no-op never does, which is what keeps AppKit's
        // own count straight.
        let mut hidden = false;
        let (mut hides, mut shows) = (0, 0);
        for want_hide in [true, true, false, false, true, false, true, true, false] {
            let (next, act) = cursor_action(hidden, want_hide);
            assert_eq!(
                next != hidden,
                act,
                "from hidden={hidden}, asking to hide={want_hide}"
            );
            hidden = next;
            if act {
                if want_hide {
                    hides += 1;
                } else {
                    shows += 1;
                }
            }
        }
        // Nine requests, and only three of each kind did anything - so a session
        // that hides the pointer once and shows it once is never left with the
        // pointer stuck either way.
        assert_eq!((hides, shows), (3, 3));
        assert!(!hidden, "ending where it started: visible");
    }
}
