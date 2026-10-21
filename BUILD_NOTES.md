# Sleepless Build and Debug Notes

## Project Overview
- Repository: `c:\Users\ilya\code\sleepless`
- Main source: `src/main.rs`
- Build system: Rust / Cargo
- Target platforms: Windows (src/windows.rs), macOS (src/macos.rs), Linux (src/linux.rs); shared Unix helpers in src/unix.rs
- Icon: regenerate with `python3 assets/make_icon.py` (needs Pillow)
- Shared, platform-independent modules:
  - `src/store.rs` - per-user key/value store (`HKCU\Software\Sleepless\<section>` on
    Windows, `sleepless.conf` in the config directory elsewhere). `SLEEPLESS_CONFIG_DIR`
    overrides the unix path, which is how the unit tests get a private store.
  - `src/lock.rs` - the come-back credential: PBKDF2-HMAC-SHA256, verify, back-off, and
    `unlock_loop` (the ask-until-it-matches loop all three lock screens share).
  - `src/enroll.rs` - what `--set-password` / `--set-passkey` and friends do.
  - `src/logging.rs` - the single log file both binaries append to (`%LOCALAPPDATA%\Sleepless`,
    `~/Library/Logs/Sleepless`, `~/.local/state/sleepless`; `SLEEPLESS_LOG` overrides). The
    `log!` macro always writes there and additionally to the console under `--debug`.
  - `gui/` - the tray / menu bar app (separate crate, `sleepless-gui`); its own small
    logger in `gui/src/log.rs` writes to the same file so one report shows both halves.
- Dependencies:
  - all platforms: `pbkdf2 = { version = "0.13", features = ["sha2"] }`, `getrandom = "0.4"`
  - Windows only: `windows-sys = { version = "0.59", features = ["Win32_Foundation", "Win32_Graphics_Gdi", "Win32_Security", "Win32_System_Console", "Win32_System_Power", "Win32_System_Registry", "Win32_UI_Input_KeyboardAndMouse", "Win32_UI_WindowsAndMessaging"] }`
    (`Win32_Graphics_Gdi` for the painting calls the lock screen needs, `Win32_Security`
    because `RegCreateKeyExW` is declared with a `SECURITY_ATTRIBUTES` parameter)
  - macOS only: `objc2`, `objc2-app-kit`, `objc2-foundation`, `block2` (the
    LocalAuthentication completion handler is an ObjC block)

## Key build commands
- `cargo check --workspace`
- `cargo build --release --workspace`
- `cargo test` (the credential logic; run on any host - see the note about the test store)

## Checking the other platforms without their hardware
Type-checks (no linker, so no SDK or mingw needed):
```
rustup target add x86_64-pc-windows-gnu aarch64-apple-darwin
cargo check --workspace --target x86_64-pc-windows-gnu
cargo check --workspace --target aarch64-apple-darwin
```
That catches wrong `windows-sys` / objc2 signatures and missing features, which is most
of the risk in the GUI code. It does not prove the lock screen *behaves*; only the device
can do that.

Driving a prompt-driven CLI command without a GUI:
```
(sleep 1.5; printf 'pw\npw\n') | script -qec "env -u DISPLAY -u WAYLAND_DISPLAY \
    SLEEPLESS_CONFIG_DIR=/tmp/sl ./target/debug/sleepless --set-password" /dev/null
```
- Force `DISPLAY`/`WAYLAND_DISPLAY` off, otherwise zenity finds the real session and pops
  a dialog on the desktop (the answers then come from clicks, not from the pipe).
- The `sleep`s matter: input that arrives before `stty -echo` runs gets echoed by the tty,
  which looks like the echo suppression is broken when it isn't.

## Password gate gotchas
- **`NSAlert.runModal` answers with `NSAlertFirstButtonReturn` (1000) /
  NSAlertSecondButtonReturn (1001)**, numbered by the order the buttons were added - *not*
  `NSModalResponseOK` (1), which belongs to hand-rolled modal windows. Comparing against
  `NSModalResponseOK` made every macOS dialog look cancelled, which is why starting a
  password from the menu bar reported "cancelled: no password was set" (and would have
  locked the macOS lock screen permanently, since its prompt can't be cancelled). The
  comparison lives in one `accepted()` function with a test on it.
- A test-build for a macOS target links against the macOS SDK, which this machine hasn't
  got: `cargo check --target aarch64-apple-darwin` works (no linking), `cargo test
  --target aarch64-apple-darwin` does not. The macOS-only tests are compiled by that
  command's front end but can only be *run* on a Mac.
- The Linux job runs `cargo test --workspace` on the host arch; the aarch64 leg only
  cross-compiles, so tests aren't run there.
- `windows-sys`: `HWND` is `*mut c_void` (test with `is_null()`), `BeginPaint`/`EndPaint`/
  `PAINTSTRUCT`/`InvalidateRect` live in `Win32::Graphics::Gdi`, and `ES_*`/`BS_*`/
  `TRANSPARENT` are `i32` where the `WS_*` styles are `u32`.
- The lock screen window isn't a dialog, but pumping it through `IsDialogMessageW` is what
  turns Enter into a press of the `BS_DEFPUSHBUTTON` child - no dialog template needed.
- `HWND`-class registration with `hbrBackground: CreateSolidBrush(0)` gives the free
  black erase; the brush is created once and intentionally never deleted.
- macOS: AppKit is main-thread-only, so the thread that sees input only sets a flag and the
  main thread pumps `nextEventMatchingMask_untilDate_inMode_dequeue` itself. `NSAlert`'s
  buttons own the modal loop, which is why the prompt needs no key handling of its own.
- macOS: ObjC `BOOL` is `objc2::runtime::Bool` inside a block signature (a Rust `bool` has no
  `Encode`), the error argument has to be a raw pointer (a closure signature can't be
  higher-ranked), `RcBlock::new` needs the target type written out, and `NSDefaultRunLoopMode`
  is an extern static (unsafe to name). `LAContext` used to need a manual
  `#[link(name = "LocalAuthentication", kind = "framework")]` so `AnyClass::get("LAContext")`
  could find it; the typed crate links the framework itself, so that's gone.
- **Waiting for a Touch ID answer needs the AppKit event queue, not a run-mode slice.**
  `NSRunLoop::runMode_beforeDate(NSDefaultRunLoopMode, ..)` left the authentication panel
  on screen with nothing servicing it and the caller stuck until it was killed, because the
  panel is presented in our own process. `evaluate()` now dequeues and dispatches events
  itself (the same thing `NSApplication::run` does) and gives up after `TOUCH_ID_TIMEOUT`
  with the `NSError` domain/code logged, so enrolment can only finish, fail, or time out -
  never hang. (The answer itself arrives on a framework-private queue - see below - so it
  goes through an atomic, not a run-loop drain.)
- Related: an accessory process (a tray-started helper) does not reliably get that prompt
  shown in front of it, so `passkey_enroll` flips the activation policy to `Regular` and
  activates before asking, then puts the policy back.
- **Anything that has to stay visible during `NSAlert::runModal` needs its timer in the
  common modes**: a modal alert runs the loop in `NSModalPanelRunLoopMode`, so
  `NSTimer::scheduledTimerWithTimeInterval_repeats_block` alone never fires; add the same
  timer again with `NSRunLoop::addTimer_forMode(&timer, NSRunLoopCommonModes)`. That timer
  is what re-asserts the prompt's window level, ordering and app activation twice a second,
  because a beta ended up with the prompt behind its own black windows.
- **`-[LAContext evaluatePolicy:localizedReason:reply:]` returns void.** Reading a `Bool`
  out of it (`let started: Bool = msg_send![...]`) turned a leftover register into a
  "refusal", so the passkey step failed instantly while Apple's panel was still on screen.
  Use `objc2-local-authentication`'s typed binding - it has no return value to get wrong -
  with features `LAContext` + `block2` (the block variant) + `LAPublicDefines`. Two traps in
  that crate: the `kLAPolicy...` constants are `c_uint` while the methods want the
  `LAPolicy(NSInteger)` newtype, and `msg_send_id!` is deprecated in objc2 0.6 (plain
  `msg_send!` now converts to `Retained`).
- Apple renders the authentication reason as `"<app name>" is trying to <localizedReason>`,
  so the reason is a verb phrase ("set up Touch ID to unlock Sleepless"), never
  "Sleepless: ...". A nil or empty reason throws `NSInvalidArgumentException`.
- The reply block is documented as running on "a private queue internal to the framework",
  with no guarantee of thread or run loop, so the answer has to arrive through something
  thread-agnostic (an `AtomicU8`), and the waiting loop only needs the event queue for the
  panel's own UI. Also documented: never call `canEvaluatePolicy` from inside the reply
  block - it can deadlock.
- **A hand-written `msg_send!` on a class with no bindings killed a release.** The
  distributed-centre call was written out (`addObserverForName:object:queue:options:usingBlock:`,
  `options: 0u64`, an assumed return value); Foundation throws when it dislikes a block
  argument, an Objective-C exception cannot unwind through Rust frames, and the process
  aborted with nothing in the log. Now: typed bindings (a `Retained::downcast` to
  `NSNotificationCenter` checks the hierarchy), the ancient documented
  `addObserver:selector:name:object:` through a `define_class!` subclass as the primary
  route, and `objc2::exception::catch` where available.
- `catch-objc-exceptions` is an off-by-default cargo feature because catching needs
  `objc2-exception-helper`'s `.m` compiled for the target - a Linux cross-build has no
  Objective-C compiler. The macOS CI job enables it. To type-check that path from Linux,
  stub the toolchain: `CC_<target>=<script that touches the -o file>` plus
  `AR_<target>=<script that touches the .a>` satisfy `cc-rs` and `cargo check` compiles the
  Rust without linking.
- `NSDistributedNotificationCenter` has no bindings in objc2-app-kit 0.3 (it's documented
  under Foundation). It is an `NSNotificationCenter` subclass, which is what makes the typed
  observer methods apply once the shared centre has been downcast to it.
- `isKeyWindow` is false for an alert panel *before* `runModal` even when everything works
  - a panel becomes key inside the modal loop. Measure it from a timer that fires inside
  that loop. An accessory process can't become active either, so a dialog needs
  `setActivationPolicy:Regular` (and both `activateIgnoringOtherApps:`, which macOS 14 may
  ignore, and `-activate`, which doesn't exist before 14 - hence `respondsToSelector:`).
- **Never guess an Objective-C selector.** `NSDistributedNotificationCenter` has
  `+defaultCenter` (and `+notificationCenterForType:`), *not* `+distributedNotificationCenter`
  - sending the invented one gave `doesNotRecognizeSelector:` -> an `NSException` -> SIGABRT,
  twice, and it cost two releases. Check the class in Apple's docs (the JSON behind a docs
  page is fetchable: `https://developer.apple.com/tutorials/data/documentation/<framework>/<page>.json`
  - it lists `relationshipsSections` including "Inherits From", which is how the
  `NSNotificationCenter` superclass was confirmed), and cross-check the method names against
  the generated bindings under `~/.cargo/registry/src/*/objc2-*/src/generated/`.
- **`panic = "abort"` and Objective-C exceptions do not mix.** With abort every Rust frame is
  nounwind, so a thrown exception cannot reach a `@catch` - not even one inside
  `objc2::exception::catch` (whose crate carries a "test this with panic=abort" TODO) - and the
  crash report shows only `panic in a function that cannot unwind`. The release profile
  unwinds now; that is what makes the guards meaningful.
- Both binaries install, on macOS, `objc_setExceptionPreprocessor` (logs the exception's class,
  name and reason the moment it is thrown, before unwinding) and
  `objc_setUncaughtExceptionHandler` (logs on the way out), through `objc2::ffi`. With the panic
  hook that means: any death leaves a name in the log. Reachable via
  `platform::install_crash_reporting()`, called from `main()` for every subcommand.
- `nextEventMatchingMask:untilDate:inMode:dequeue:` with `untilDate: nil` **blocks until an
  event arrives for this app**. A black-screen session is usually not the active app, so its
  queue can stay empty while the user types: flags set by other threads are then never seen.
  Always pass a short `NSDate`.
- `NSCursor`'s `hide`/`unhide` are counted by AppKit, so they must be balanced; keep a flag
  rather than calling `unhide` blindly. And a hidden cursor over a lock screen means the user
  cannot see the pointer for the password box - or for Apple's Touch ID panel buttons.
- `GetDlgItemTextW` (and friends) return a count that should be clamped to the buffer before
  slicing: `utf16_prefix`. A slice past the end panics, and on the Windows lock screen a panic
  kills the process - uncovering the desktop with the password never asked.
- `catch-objc-exceptions` is enabled **only** in the macOS CI job (see the earlier bullet on
  the Objective-C compiler); without it `try_objc` is a pass-through, so the exception
  handlers above are what make a failure readable.
- **`NSAlert`'s window is an `NSPanel`, and panels default to `hidesOnDeactivate = YES`.** A
  background process that the system keeps deactivating therefore hides its own dialog: the
  log shows the right level, an on-screen frame and a key window one tick later, and the user
  sees nothing. `setHidesOnDeactivate(false)` on every window this app shows, dialogs and
  black windows alike.
- **Never ask for activation in a poll loop.** Since macOS 14 `-activate` is cooperative - a
  request the system grants and withdraws as it likes. Asking twice a second made the key
  window flap between true and false and stole focus from the user's terminal every other
  tick. Activate once before entering the modal loop; if a timer must re-assert, make it act
  only when the window has actually lost key, and rate-limit the activation part.
- An app can be `.accessory` and still be active and own a key window; `.regular` is only
  needed for Apple's own biometric prompt (and even there, only because the system won't show
  it for a process that can't become active). Flipping to `.regular` for ordinary dialogs is
  what puts an unexplained Dock icon in front of the user.
- `NSApplicationActivationOptions::ActivateIgnoringOtherApps` is documented as having no
  effect since macOS 14, and `-[NSApplication activateIgnoringOtherApps:]` is deprecated -
  `NSRunningApplication::activateWithOptions(ActivateAllWindows)` plus (on 14+)
  `-[NSApplication activate]` is the current pair, and the tray process the user clicked can
  `-[NSApplication yieldActivationToApplication:]` the child's `NSRunningApplication` to make
  that request stick.
- `+[NSRunningApplication runningApplicationWithProcessIdentifier:]` is behind objc2-app-kit's
  **`libc`** feature (the argument is `pid_t`), and `NSObjectProtocol::respondsToSelector`
  needs `objc2-foundation` with the `NSObject` feature - use those instead of writing a
  `msg_send!` for a feature probe.
- Escape hatches to keep in mind when changing any of this: nothing may leave the black screen
  up with no prompt - the gate is cancellable by the session-taken flag, the main loop wakes on
  a timer rather than on events alone, a dying process always removes the windows, and
  `SLEEPLESS_NO_SCREEN_LOCK_WATCH=1` disables the riskiest watcher.
- With `panic = "abort"` in the release profile a panic leaves the log ending mid-sentence
  and looks like a killed process. `logging::install_panic_hook()` writes
  `PANIC at file:line: message` straight to the log file (not through the normal sink, whose
  lock the panic may be holding).
- macOS session-lock signals: `NSWorkspace::sharedWorkspace().notificationCenter()` +
  `addObserverForName_object_queue_usingBlock` only exists in the bindings with foundation
  features `NSNotification`, `NSOperation`, `NSString` and `block2`, and the returned token
  (plus the block) must be kept alive. `NSDistributedNotificationCenter` has no bindings in
  `objc2-app-kit` 0.3, so `com.apple.screenIsLocked` is subscribed through `AnyClass::get` +
  `msg_send!` behind a `respondsToSelector` check.
- Windows session lock: `WTSRegisterSessionNotification` lives in
  `Win32::System::RemoteDesktop` (needs that feature) and arrives as `WM_WTSSESSION_CHANGE`
  on a window, so a *message-only* window (`HWND_MESSAGE`) is created for the whole run.
  A waiting loop that uses `thread::sleep` can't see it:
  `MsgWaitForMultipleObjectsEx(0, null, ms, QS_ALLINPUT, 0)` plus a `PeekMessageW` drain can.
  `WTS_SESSION_LOCK`/`WTS_SESSION_UNLOCK` are in `UI::WindowsAndMessaging`, not RemoteDesktop.
- Linux has no subscription primitive without a D-Bus client library, so the lock is
  *polled*: `loginctl show-session <id> -p LockedHint`, the id from `XDG_SESSION_ID` when set
  and otherwise the first row of `loginctl list-sessions --no-legend` (it is unset here).
  Cinnamon's ScreenSaver service answers `GetActive` with a D-Bus error, so that check has
  to stay a `None` rather than become a false positive.
- `lock::unlock_loop` takes a `cancelled` predicate and returns whether the password was
  accepted. The OS taking the session over is the one thing that must always interrupt a
  lock screen, so every platform passes its own flag and then follows its normal restore
  path. On macOS a separate watchdog thread just ends the process, because a main thread
  inside `runModal` can't check anything itself.
- Tests: the store location is an environment variable, so the test helper takes a process
 -wide lock. Without it, two tests switch the directory under each other and an "ask until
  correct" loop never terminates - which looks like a hung test suite, not a broken test.
- Don't smoke-test a session on the development desktop: the Linux backend detects the real
  GNOME session and turns the actual monitor off, restoring only on a clean exit. Test the
  credential commands instead.
- `std::thread::spawn` panics on failure; on a locked screen that is the difference between
  "the prompt is up" and "the desktop is uncovered". Use `unix::spawn_thread`, which returns
  `io::Result` and is logged.
- Cap test runs: `timeout 60 cargo test`; bisect a hang with `cargo test <name>`.

## Releasing
`.github/workflows/release.yml` builds Windows, macOS (universal + `.app`) and Linux
(x86_64 + aarch64) on pushes to `main`, pull requests, and tag pushes.
- push to `main` -> release `v<Cargo.toml version>`, marked Latest
- push a tag `v*` -> publishes exactly that tag; a name containing `alpha`/`beta`/`rc`/
  `pre`/`dev` is published as a pre-release and not marked Latest
- a manual run can also be given a tag to publish

## History: the console HANDLE problem
(kept because the same trick comes up again)
- Error: `E0277: *mut c_void cannot be shared between threads safely`,
  `E0277: *mut c_void cannot be sent between threads safely`,
  `E0308: mismatched types` for `WriteConsoleW`
- `HANDLE` in `windows-sys` is `*mut c_void`, which isn't `Sync`, so it can't live in a
  `static` directly; `OnceLock<isize>` + a cast at the point of use fixes it, and
  `WriteConsoleW` wants `*const u16`, i.e. `text.as_ptr()` from a UTF-16 buffer.
- See `CONSOLE_OUT` / `debug_log()` / `init_logging()` in `src/windows.rs`.

## Notes for next time
- If the build fails in GitHub Actions, check whether the failing source matches the latest commit.
- On Windows-specific Rust code, prefer storing raw handles as integer-sized values in statics when thread-safety traits are required.
- For Windows API calls in Rust, verify the pointer argument types from the generated `windows-sys` signatures (the vendored copy under `~/.cargo/registry/src/*/windows-sys-0.59.0/` is the fastest reference), and the same for the objc2 crates.
- Cross-checked targets before pushing beats finding out in CI.

## Recommended checklist before opening a PR
- `cargo fmt --all`
- `cargo test` (capped with `timeout`)
- `cargo check --workspace` for the host, `x86_64-pc-windows-gnu` and `aarch64-apple-darwin`
- Make sure the GitHub Actions targets still match what the crate needs (the `windows-sys`
  and `objc2-*` feature lists are easy to under-specify)

