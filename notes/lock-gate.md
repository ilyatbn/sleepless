# Password-protected exit from the black screen

Base: branch `lock-gate`, created from `origin/claude/macos-linux-support-lhfrj9`
(v0.3.0 tree: CLI + `sleepless-gui` tray app, Windows / macOS / Linux).

## Goal

Coming back from the Sleepless black screen asks for a credential that Sleepless
checks itself. The OS lock screen is never used, because it tears down VPN and
other session state. A passkey (macOS Touch ID / Windows Hello) is optional on
top of the password; the password is always enrolled first.

Threat model: a deterrent against someone at the keyboard, not a security
boundary. Killing the process stays a documented way out — that is deliberate,
because the OS lock screen is the real boundary when the user wants one.

## Design

Two new shared modules (no platform code, unit-testable on any host):

- `src/store.rs` — tiny per-user key/value store, one "section" per concern.
  - Windows: `REG_SZ` values under `HKCU\Software\Sleepless\<section>`
  - macOS: `~/Library/Application Support/Sleepless/sleepless.conf`
  - Linux: `$XDG_CONFIG_HOME/sleepless/sleepless.conf` (mode 0600)
  - `SLEEPLESS_CONFIG_DIR` overrides the unix path (used by the tests).
- `src/lock.rs` — the credential: PBKDF2-HMAC-SHA256 over a 16-byte random salt,
  hex in the store; `enroll` / `check` (constant-time) / `remove` / `is_enrolled`;
  a `Secret` string that is zeroed when dropped; a failure back-off counter.

Platform pieces, one per backend:

- prompt for a secret (enrolment + Linux unlock): Windows dialog box, macOS
  AppKit secure-text panel, Linux `zenity`/`kdialog` askpass, terminal with echo
  off as the fallback.
- the unlock gate itself:
  - macOS: the existing black `NSWindow`s stay up; an `NSSecureTextField` is
    added to the display the cursor is on, Enter submits through a local key
    monitor. Nothing but the windows is ever revealed.
  - Windows: a topmost black window is created over the whole virtual desktop
    *before* the blank screensaver starts, so when the saver ends on the first
    keypress the overlay is already there (no desktop flash). Password box +
    topmost/foreground re-asserted from a timer; `WM_CLOSE` ignored while locked.
  - Linux: display is already back on when input arrives (DPMS is the compositor's
    business), so the prompt is an askpass dialog.

Windows self-heal: the values that are about to be overwritten are written to
`HKCU\Software\Sleepless\restore` first and deleted after a successful restore; a
run that starts with that section present restores it before doing anything else.
macOS/Linux already undo their changes through helper processes that watch our
pid, so they need nothing.

CLI:

```
sleepless --set-password      enrol (or replace) the password, offers a passkey after
sleepless --clear-password    remove it
sleepless --has-password      exit 0 when a credential is enrolled (used by the tray app)
sleepless --set-passkey       enrol the platform passkey only
sleepless --clear-passkey
```

Tray app: menu items for the same, with the enabled/checked state read through
`sleepless --has-password`.

## Facts gathered from the base branch

- macOS black screen = borderless black `NSWindow` per display at level 1000
  (`NSScreenSaverWindowLevel`), cursor hidden, `caffeinate -d -i -m -s -w <pid>`
  holds the no-sleep assertions. Activity is detected with
  `CGEventSourceSecondsSinceLastEventType(HID, Any)`.
- Windows black screen = the built-in `scrnsave.scr` forced through
  `WM_SYSCOMMAND/SC_SCREENSAVE`, watched with `SPI_GETSCREENSAVERRUNNING`.
- Linux = `systemd-inhibit` + DPMS off via Mutter/kscreen-doctor/xset.
- `unix::wait_for_activity` treats a *drop* in the idle counter as "the user is
  back"; it sleeps 1.5s first so the input that started us isn't counted.
- Release CI builds Windows / macOS (universal) / Linux on pushes to `main` and
  on pull requests; `workflow_dispatch` takes a `tag` input published as a
  pre-release. A plain branch push (no PR, no tag) runs nothing.

## Status / log

- [x] toolchain here: rust 1.99.1, targets `x86_64-pc-windows-gnu` +
      `aarch64-apple-darwin` added; `cargo check --workspace` passes for all
      three targets before any change (baseline).
- [x] deps: `pbkdf2 0.13` (feature `sha2`), `getrandom 0.4` — `cargo fetch`ed,
      API confirmed from the vendored sources:
      `pbkdf2::pbkdf2_hmac::<pbkdf2::sha2::Sha256>(pw, salt, rounds: u32, &mut out)`,
      `getrandom::fill(&mut [u8]) -> Result<(), Error>`.
- [x] `src/store.rs` + `src/lock.rs`: 10 tests green natively. Measured
      PBKDF2-HMAC-SHA256 at 600 000 rounds: **96 ms** in the release build on
      this machine (debug ~3 s), so the shipped work factor stays at OWASP's
      600k. `cargo test -- --ignored timing` is the benchmark.
- [x] `sleepless --set-password / --clear-password / --has-password` driven
      through a real pty on Linux with `DISPLAY` forced off: prompts shown,
      `stty -echo` really suppresses the echo, the weak-password warning fires
      under 8 chars, mismatch / cancel / replace-confirm paths behave, the file
      is written 0600 with only salt+hash, and `--has-password`'s exit code
      flips with enrolment.
- [x] Linux gate: once the display comes back, `lock::unlock_loop` asks
      (zenity/kdialog askpass, or the terminal) and holds the power-settings
      restore until the password matches.
- [x] macOS gate: the black `NSWindow`s stay up; the prompt is an `NSAlert`
      with an `NSSecureTextField` accessory raised to screensaver level + 1,
      with no Cancel button. The input-watching thread only signals the return
      (`BACK`); the main thread pumps AppKit events and answers, the worker
      then releases caffeinate and exits. `NSApplication::run()` is still used
      when nothing is enrolled, so the existing path is untouched.
- [x] Windows gate: black topmost overlay over the whole virtual desktop,
      created *before* `SC_SCREENSAVE`, so the keypress that ends the saver
      reveals the overlay instead of the desktop; the password box and Unlock
      button stay hidden until locked; `IsDialogMessageW` makes Enter press
      Unlock (Escape and Alt+F4 are ignored while locked); a 200 ms timer
      re-asserts topmost/foreground and focus.
- [x] Windows restore point: written to `HKCU\Software\Sleepless\restore`
      before the timeouts change, deleted once they're back, and a run that
      finds a leftover applies it first - which also closes the long-standing
      "no crash/kill-switch recovery" caveat in the README. The GUID <-> text
      helpers were checked out of tree against the values Microsoft publishes,
      including rejecting malformed input.
- [x] cross-checks after every step: `cargo check --workspace` for
      x86_64-unknown-linux-gnu, x86_64-pc-windows-gnu, aarch64-apple-darwin.
- [x] macOS passkey: Touch ID / device-owner authentication through
      LocalAuthentication (`LAContext`), best effort, password always required
      first and always still accepted.
- [x] tray / menu bar items for enrolling and removing the password.
- [x] README + BUILD_NOTES.
- [x] CI: a pushed `v*` tag publishes exactly that tag, with `beta`/`alpha`/`rc`/
      `pre`/`dev` names marked as pre-releases (and not "Latest").
- [x] final state: `cargo check --workspace` clean (0 warnings) for
      x86_64-unknown-linux-gnu, x86_64-pc-windows-gnu, aarch64-apple-darwin;
      `cargo test` 10 passed; `cargo build --release` fine.
- [x] tag `v0.2.2beta1` pushed; the workflow built Windows / macOS universal / Linux
      x86_64 + aarch64 and published the pre-release (run 11, success).
- [x] beta 1 field report: every macOS dialog answered as "cancelled" (NSAlert returns
      1000/1001, not NSModalResponseOK) - fixed, plus the shared log file for reports.
      Tagged `v0.2.2beta2` (run 12: all jobs success, unit tests now part of CI).
- [ ] device testing (nothing here can do this): the Windows overlay covering
      every monitor and taking focus, the macOS alert above the black windows,
      the Touch ID prompt, and a killed process healing the power settings.

## Things found on the way

- `pbkdf2 0.13` needs its `sha2` feature for `pbkdf2_hmac::<Sha256>`;
  `getrandom 0.4` is `getrandom::fill`.
- windows-sys 0.59: `RegCreateKeyExW` sits behind the `Win32_Security` feature
  (its parameter is a `SECURITY_ATTRIBUTES`); `BeginPaint`/`EndPaint`/
  `InvalidateRect`/`PAINTSTRUCT` are in `Win32::Graphics::Gdi`, not
  `WindowsAndMessaging`; `HWND` is `*mut c_void` (so `is_null()`, not `== 0`);
  `TRANSPARENT`/`ES_*`/`BS_*` are `i32` while `WS_*` are `u32`.
- objc2: `NSSecureTextField` has its own app-kit feature; `NSDefaultRunLoopMode`
  needs `objc2-foundation/NSRunLoop`; `NSModalResponseOK` is a static;
  `NSString::from_str` for the `std` feature.
- A test helper that switches a process-wide env variable must be serialised:
  unsynchronised, one test read another test's config directory and the
  "ask until correct" loop spun forever. Caught by the 60s test cap.
- Testing prompts on a desktop machine pops real dialogs - `zenity` found
  `DISPLAY=:0` here, so a "terminal" test was actually being answered through
  GUI dialogs. Force `DISPLAY`/`WAYLAND_DISPLAY` off for terminal tests.
- Linux: enrolling from a terminal should stay in that terminal; the graphical
  askpass is for when there is no terminal (started from the tray/desktop).

## Dead ends

- Windows Hello *passkey*: the only local option is the WebAuthn C API
  (`webauthn.dll`), i.e. parsing the attestation JSON, CBOR and a COSE key and
  verifying an ECDSA signature through CNG - several hundred lines nothing
  here can execute. Left out of this pre-release rather than shipping
  unverifiable crypto; the password path is unaffected.
- Windows prompts that are OS-verified but not the lock screen
  (`UserConsentVerifier` WinRT, `CredUIPromptForWindowsCredentials`): WinRT ABI
  churn, or unable to verify a Hello PIN at all.
- macOS: CoreGraphics alone cannot create a window, so AppKit is required for
  the overlay however the prompt is done.

## Open questions / risks

- Everything GUI-side can only be *type-checked* here; only the device can prove
  the overlay behaves. The password logic itself is unit-tested on Linux.
- Windows WebAuthn needs COSE/CBOR parsing + a CNG ECDSA verify: highest risk of
  the whole feature, so it lands last and must never block the password path.
- macOS Touch ID from an ad-hoc signed bundle may be refused by LocalAuthentication;
  handled as "passkey unavailable".

## Beta 1 field report: "cancelled: no password was set" from the macOS menu bar

Cause, confirmed from the bindings rather than guessed: `-[NSAlert runModal]` returns the
button constant **`NSAlertFirstButtonReturn` (1000)**, `NSAlertSecondButtonReturn` (1001),
assigned in the order buttons are added. `NSModalResponseOK` is **1** and belongs to
hand-rolled modal windows. All three macOS prompt sites compared against
`NSModalResponseOK`, so *every* macOS prompt - enrolment, the "replace it?" confirmation,
the Touch ID offer, and the lock screen itself - was treated as cancelled.

That last one is the more serious consequence: `prompt_secret_locked` returning `None`
would have made `unlock_loop` re-ask forever, i.e. coming back from a black screen on
macOS would have been impossible except by killing the process. Enrolment failing first is
the only reason it was noticed.

Fix: one `accepted(NSModalResponse) -> bool` comparing with `NSAlertFirstButtonReturn`,
plus a macOS test asserting 1000/1001 and that `NSModalResponseOK`/`Cancel`/`Abort` are
all rejected (`cargo test --target aarch64-apple-darwin --no-run` compiles it; only a Mac
can run it - linking a macOS test binary here needs an SDK this machine hasn't got).

Diagnosis help added with it, since "it said cancelled" from a menu-bar app is otherwise
undiagnosable:

- `src/logging.rs` + `gui/src/log.rs`: one shared log file, written whether or not
  `--debug` was passed. Windows `%LOCALAPPDATA%\Sleepless\sleepless.log`, macOS
  `~/Library/Logs/Sleepless/sleepless.log`, Linux `~/.local/state/sleepless/sleepless.log`;
  `SLEEPLESS_LOG` overrides. Capped at 512 KiB, then starts over with a marker line.
- The `log!` macro now always writes to the file (and to the console with `--debug`).
- The tray app logs which CLI it resolved, the arguments, the child's captured stdout and
  stderr and its exit status; the CLI logs every enrolment step, the numeric response each
  dialog returned, passkey availability, and the exit code it finished with. Passwords are
  never logged, only their lengths.
- Two bugs the new tests caught immediately: `OpenOptions` needed `write(true)` for the
  truncating case (append alone isn't enough), and the file-touching tests had to be
  serialised over the static sink + `SLEEPLESS_LOG` (same class of mistake as the earlier
  config-dir race).
- The date formatting (`civil_from_days`, no crate for it) was cross-checked against
  Python's date arithmetic over 14 286 sampled days including pre-1970 negatives; my
  *hand-written test expectations* were wrong twice (2000-02-29 is day 11016, not 11017),
  the algorithm wasn't.
- CI: the Linux x86_64 leg now runs `cargo test --workspace` before building.

## Release

Pushed as branch `lock-gate` from `claude/macos-linux-support-lhfrj9`, squashed to one
commit (`1f716e0`, the pre-squash tip kept locally as `lock-gate-pre-squash` until the
release was confirmed good, then deleted - it was never pushed), and tagged
`v0.2.2beta1`: the first release built from a tag push rather than a manual run.

Run 11 of "Build and Release" **completed successfully**: Windows (MSVC), macOS
universal + `.app`, Linux x86_64 and aarch64. The release page shows
"Sleepless v0.2.2beta1" as a pre-release, not Latest, with all eight assets.

## What the shipped artifacts were checked against

- Released Linux x86_64 `sleepless`: runs, prints the new flags, and enrolling through a
  real pty with `DISPLAY` unset wrote `password.kdf/iterations/salt/hash` (600 000 rounds)
  with no plaintext in the file; `--has-password` then exited 0.
- Released `sleepless.exe`: `PE32+ executable (GUI) x86-64`, imports user32/gdi32/
  advapi32/kernel32, and contains the new code's strings - the `SleeplessLock` window
  class and "Unlock", the `SleeplessPrompt` dialog with its STATIC/EDIT/BUTTON children,
  the `pbkdf2-sha256` record keys, and "Segoe UI" for the lock screen's font.
- Released macOS binary: universal Mach-O (x86_64 + arm64) that references `LAContext`,
  `canEvaluatePolicy:error:`, `evaluatePolicy:localizedReason:reply:` and
  `NSSecureTextField`, with LocalAuthentication appearing in its load commands - so the
  Touch ID path is actually linked into the shipped bundle, not just compiled.

None of that proves the lock screens *behave*; see the checklist above.

## Device testing checklist (nothing here can do these)

1. `sleepless --set-password` on each platform: Windows and macOS show their own dialog,
   Linux a terminal prompt when started from a terminal (and zenity when started from the tray).
2. Windows: start a session with a password enrolled - does the black window cover every
   monitor, is the desktop never flashed when the screensaver ends, does the box have focus,
   does Alt+Tab / a notification / the taskbar manage to get on top of it, and are Escape and
   Alt+F4 really harmless while locked?
3. Windows heal: kill `sleepless.exe` while locked (Task Manager), then start it again -
   `--debug` should log "Found a restore point from a run that was killed" and the power
   sliders should be back where they were.
4. macOS: the alert must appear above the black windows (level + 1) on the display you're
   looking at, and the Touch ID prompt should come before the box when a passkey is enrolled.
   The Touch ID part is the most likely thing to be refused on an ad-hoc signed bundle - it
   must then fall back to the password box without hanging.
5. macOS: with the passkey path, check the wait for the completion handler doesn't sit there
   for the full 120 s timeout when the prompt is cancelled.
6. Tray app: "Remove the password" greyed out until something is enrolled, and correct
   afterwards; on Windows the tray icon is unresponsive while the CLI dialog is open
   (deliberate: the menu event handler waits for the child).
7. Linux (GNOME/KDE/X11): the askpass dialog appears after the display comes back, and the
   power settings are only restored once the password is right.

## beta3: three reports from the device (2026-10-21)

Three problems came back from testing `v0.2.2beta2` on macOS. All three are fixed here;
none of them could be reproduced on this machine, so the fix is paired with logging that
will name the cause if any part of it is still wrong.

### 1. Enrolment hung after the Touch ID prompt

Status: fixed, most likely cause identified.

- `evaluate()` asked `LAContext` for the prompt and then waited by running the run loop in
  `NSDefaultRunLoopMode` in 0.1 s slices, polling a channel for the reply. On the device the
  prompt appeared and then nothing ever came back, so the process sat until it was killed.
- What that shape means: the prompt is a panel AppKit presents in *our* process, and a
  loop that only runs `NSDefaultRunLoopMode` in slices never services it - the panel is on
  screen but unreachable, so nothing completes and the block never runs. (Correction from
  beta4: the docs say the reply block runs "on a private queue internal to the framework",
  with no guarantee of queue, thread or run loop - so the answer must not depend on
  draining any particular queue. It goes into an atomic slot for exactly that reason.)
- Fix: wait the way `NSApplication::run` does - `nextEventMatchingMask:untilDate:inMode:
  dequeue:` (0.1 s slices) + `sendEvent:` - so the prompt's own UI and the main queue are
  both serviced. The answer lands in a static `AtomicU8` (0 waiting / 1 yes / 2 no) instead
  of a channel, because a reply that arrives after the timeout must not be able to confuse
  anything.
- Hard bound: `TOUCH_ID_TIMEOUT` is 45 s. On timeout the log says so and `passkey_enroll`
  returns `Err`, which `enroll` turns into "No passkey: ..." - the password is still saved
  and the process exits 0. Enrolment can now finish, fail, or time out; it cannot hang.
- Every step is logged: whether `LAContext` exists, `canEvaluatePolicy`'s answer, the
  request being started, the reply with the `NSError` code + domain + description, and the
  timeout. The error text is what a device report needs to be diagnosable: `code -1001`
  (user cancelled) and `code -1000`/`-1004` (system refused us) are completely different
  bugs.
- Belt and braces: `passkey_enroll` now sets the activation policy to `Regular` and activates
  the app before asking. An accessory process started by the tray is not a reliable host for
  a system prompt, and a prompt nobody can see is indistinguishable from a hang.

### 2. Black screen with no prompt after the hung process was killed

Status: fixed as far as it can be without the hardware; the escape hatch is the point.

- The beta's prompt was an `NSAlert` raised to level 1001 once, before `runModal`. If
  anything else ordered itself front afterwards - the modal loop, another process, another
  display - it ended up under the level-1000 black windows, which looks exactly like a
  machine that will not wake up.
- Fixes: the app is activated before the prompt; the alert is centred **on the display the
  pointer is on** (it used to land on the main display, which with more than one monitor can
  be the one nobody is looking at); and a repeating 0.5 s timer registered in
  `NSRunLoopCommonModes` re-asserts the level, `orderFrontRegardless` and the activation for
  as long as the prompt is up. The log records the display list, the chosen frame, the level
  and whether the password box became first responder.
- `lock::unlock_loop` gained a `cancelled` predicate and returns whether the password was
  accepted (test: `the_loop_stops_when_the_session_is_taken_over`). A prompt loop that
  cannot be interrupted is what turned this into a dead end.

### 3. The OS lock screen must force Sleepless out

Status: implemented on all three platforms. Locking the machine is now a documented,
guaranteed way out of Sleepless - which is also the answer to a stuck prompt.

- macOS: `NSWorkspaceSessionDidResignActiveNotification` (public API, posted on lock and on
  fast user switch) and `com.apple.screenIsLocked` on `NSDistributedNotificationCenter`
  (no bindings, so runtime class lookup + `msg_send!`, guarded by `respondsToSelector`).
  Either one sets `OS_SESSION_TAKEN`. A watchdog thread polls that flag every 500 ms and
  calls `release_and_exit()`, which stops `caffeinate` (kept in a `static Mutex<Option<Child>>`
  so any thread can reach it) and exits - the black windows belong to the process, so they
  go with it, and the root helper's own `while kill -0` loop restores `pmset`. The wait
  loop and the gate both take the flag as their cancel predicate too.
- Windows: a message-only window registered with `WTSRegisterSessionNotification`;
  `WTS_SESSION_LOCK` sets `SESSION_TAKEN` and `PostQuitMessage(0)` breaks out of any prompt
  message loop, so `run()` continues through its normal path - power settings written back,
  restore point deleted, overlay destroyed. `WM_QUERYENDSESSION` is answered with "allow",
  and anything left undone is still covered by the restore point at the next start. The
  blank-phase sleeps became `MsgWaitForMultipleObjectsEx` + `PeekMessageW` so the message is
  actually seen while waiting.
- Linux: a poller thread asks logind for the session's `LockedHint` (plus the freedesktop
  screensaver's `GetActive` where that service implements it) every 3 s; on locked it sets
  `SESSION_TAKEN`, which cancels `unix::wait_for_activity` and the gate, and the display
  settings are restored through the normal path. `unix::wait_for_activity` gained the
  `cancel` parameter for this reason; macOS passes its own flag to it.

### Verification done here

- `cargo check --workspace` clean (0 warnings) on `x86_64-unknown-linux-gnu`,
  `x86_64-pc-windows-gnu`, `aarch64-apple-darwin`.
- `timeout 60 cargo test --workspace`: 15 passed, 1 ignored, plus the gui crate's 1.
- macOS/Windows test builds: compiled by the front end, cannot link here (no macOS SDK, no
  mingw `dlltool`), same as before.
- Linux lock detection checked against the real machine: `loginctl list-sessions
  --no-legend` -> first column is the session id, `loginctl show-session 1 -p LockedHint`
  -> `LockedHint=no`; on this desktop `XDG_SESSION_ID` is unset, which is why the fallback
  parse exists. Cinnamon answers `GetActive` with `NotSupported`, which is treated as
  "nothing to ask" rather than "locked".
- Dead end: the black-screen report cannot be reproduced here. Two candidate causes fit
  (prompt under the black windows, or the wrong display); the new logging distinguishes them
  in one line.

### Published: v0.2.2beta3

- Commit `e9d6a51` on `lock-gate`, tag `v0.2.2beta3` ->
  https://github.com/ilyatbn/sleepless/releases/tag/v0.2.2beta3 (pre-release, not Latest).
- CI run 13: version / linux x86_64 / linux aarch64 / windows / macos / release all
  completed success, including the `cargo test --workspace` step on the Linux x86_64 leg.
- All 8 assets published. Checked against the shipped binaries rather than trusting the
  build: the macOS `sleepless` contains the new log strings ("Touch ID prompt requested;
  waiting up to ..s", "Watching for com.apple.screenIsLocked.", "on the display under the
  pointer (", "Lock screen giving up after ..", "Gate ended without a password: the session
  was taken over."), and the Windows `sleepless.exe` contains the `SleeplessWatch` class,
  "Session watch window ready; WTSRegisterSessionNotification -> .." and
  "Windows locked the session: Sleepless is getting out of the way.".
- Enrolment was exercised end to end on this machine under a pty with the display
  variables unset (terminal prompt path): two entries, confirmation, "Password enrolled in
  <dir>", "No passkey available on this platform", exit 0, and the same lines in the log.

### What has to be checked on the device (beta3)

1. Enrolment end to end: password, then Touch ID. It must return within 45 s at the worst,
   with either a passkey or a "No passkey: ..." line, and the process must exit by itself.
2. Lock screen: the prompt must be visible and focused on the display being looked at.
3. `Ctrl+Cmd+Q` while the screen is black: Sleepless must vanish immediately, and the
   display settings come back. Same for `Win+L` on Windows and a logind lock on Linux.
4. Kill the process while locked (still a supported way out) and confirm the Windows heal
   and the macOS helper release still work.

## beta4: three reports from the device (2026-10-21)

`v0.2.2beta3` was worse on the machine than here: Touch ID gave up instantly, and every
"turn display off" run died at startup (`session process finished:
Ok(ExitStatus(unix_wait_status(6)))` = SIGABRT).

### Bug A: the Touch ID request was treated as a failure the moment it started

Status: fixed, and the cause is certain.

- `-[LAContext evaluatePolicy:localizedReason:reply:]` returns **void**. The call was
  written as `let started: Bool = msg_send![...]`, so a leftover register was read as a
  BOOL, came back 0, and `evaluate()` returned false in the same second the system panel
  appeared. The log showed exactly that: "Asking the system to confirm your identity..."
  immediately followed by "LocalAuthentication refused to start the Touch ID request at
  all.", and the "No passkey" alert went up beside Apple's panel while it was still
  waiting for a finger.
- Fix: `objc2-local-authentication` (typed LAContext bindings) whose
  `evaluatePolicy_localizedReason_reply` returns `()`, so the mistake is impossible. The
  reply block is the only outcome channel and is now always waited for (still capped by
  `TOUCH_ID_TIMEOUT`, and the context is `invalidate()`d on timeout so a stale panel can't
  linger).
- `localizedReason` wording: Apple renders "<app> is trying to <reason>", so the strings
  are now "set up Touch ID to unlock Sleepless" and "unlock Sleepless". The app name in
  that sentence is the process/bundle name, which is why it reads "sleepless is trying
  to..." for the bare binary and "Sleepless ..." inside the .app bundle.

### Bug B: the session process aborted before the black windows existed

Status: fixed as far as the evidence allows, with several independent layers.

- The log stopped at "Watching for the session being resigned (NSWorkspace)." and every
  branch of `watch_screen_is_locked()` logs except one, so the abort was inside it. The
  crash report was SIGABRT with `abort()` called from our own frames and no
  `lastExceptionBacktrace`.
- Most likely mechanism, and the reason the fix is what it is: that function hand-wrote
  `msg_send![center, addObserverForName:object:queue:options:usingBlock:]` - invented
  argument types (`options: 0u64`), a block parameter typed `NonNull<NSNotification>` by
  guesswork, and an assumed return value. Foundation validates block arguments and throws
  `NSInvalidArgumentException` when it doesn't like one; an Objective-C exception cannot
  unwind through Rust frames, so the process aborts. No Rust panic is involved, so nothing
  was logged.
- Layers of fix:
  1. The subscription now uses objc2-foundation's **typed** bindings, reached through a
     runtime-checked `downcast` of the shared centre to `NSNotificationCenter` (the
     distributed centre is that subclass, which is what makes the observer methods apply).
  2. Primary registration is the oldest documented form,
     `-addObserver:selector:name:object:`, aimed at a small `LockWatcher` NSObject subclass
     defined with `define_class!`; the block observer is only the fallback.
  3. Both attempts run inside `try_objc`, which is `objc2::exception::catch` when the
     `catch-objc-exceptions` feature is on, so an exception becomes a log line. That
     feature is **off by default and on in the macOS CI job**: catching means compiling
     `objc2-exception-helper`'s `.m` file for the target, and a Linux cross-build has no
     Objective-C compiler. (Checked locally with stub `CC_`/`AR_` scripts that satisfy
     `cc-rs` without linking - enough to type-check the catching path, which is the only
     way to see it at all from here.)
  4. The distributed watcher now runs **after** `black_out`, so even a fatal problem there
     cannot stop the display going off; the workspace watcher (which was already the typed
     API, and demonstrably worked) still runs before it.
  5. A panic hook in both binaries writes `PANIC at <file>:<line>: <message> - aborting the
     run.` to the log file straight past the normal sink (the panic may hold that lock).
     With `panic = "abort"` in the release profile that is otherwise invisible, which is
     exactly the "silent death" shape of this bug.

### The rest of the report

- `key window: false` on every dialog: measured wrong rather than broken. A panel becomes
  key *inside* the modal loop, so `isKeyWindow` is false at the point it was being logged.
  The presenter now also sets the activation policy to `Regular` for the duration (an
  accessory process cannot become active, and a window cannot be key in an app that isn't),
  unhides the app, calls both activation APIs, and calls `makeKeyAndOrderFront` before the
  modal loop. The keep-on-top timer now reports the key state from inside the loop - on the
  first tick and on every change - which is the measurement that means something.
- Version: `Cargo.toml` and `gui/Cargo.toml` now say `0.2.2-beta.4` so the log line and the
  release name agree. (Verified in beta3's own log: it printed 0.3.0 while the tag said
  v0.2.2beta3.)

### Objective-C call audit (everything in src/macos.rs)

*(This beta4 table is kept because it records what was believed at the time, but two rows
in it are wrong - the distributed-centre selector and the block-observer variant. The
beta5 section's audit is the authoritative one: it was checked against Apple's docs page
for the class and against the generated bindings, line by line.)*

Hand-written `msg_send!` - three, all documented, all with the declaration checked against
the Apple headers / the objc2 generated bindings:

| Call | Apple declaration | Return | Notes |
| --- | --- | --- | --- |
| ~~`[NSDistributedNotificationCenter distributedNotificationCenter]`~~ | ~~`+ (NSDistributedNotificationCenter *)distributedNotificationCenter;`~~ | *this line was wrong*: that declaration does not exist in any header, and it was asserted rather than checked - the beta5 section's audit replaces it with `+defaultCenter`, which is what Apple documents | **no**: it is the crash (see the beta5 section) |
| `[app respondsToSelector:@selector(activate)]` | `- (BOOL)respondsToSelector:(SEL)aSelector;` (NSObject) | `BOOL` -> `objc2::runtime::Bool` | guard for the macOS 14 `-activate` |
| `[LockWatcher class] new` | `+ (instancetype)new;` (NSObject, init family) | converted to `Retained<LockWatcher>` by `msg_send!` | `msg_send_id!` is deprecated in objc2 0.6; plain `msg_send!` does the conversion |

Everything else is a generated binding, i.e. the signature came from the headers rather
than from me. The ones worth naming, with the Objective-C declaration they mirror:

| Binding | Declaration |
| --- | --- |
| `LAContext::new()` | `+ (instancetype)new;` / `- (instancetype)init;` |
| `canEvaluatePolicy_error(BIOMETRICS) -> Result<(), Retained<NSError>>` | `- (BOOL)canEvaluatePolicy:(LAPolicy)policy error:(NSError * _autoreleasing *)error;` |
| `evaluatePolicy_localizedReason_reply(LAPolicy, &NSString, &DynBlock<dyn Fn(Bool, *mut NSError)>)` | `- (void)evaluatePolicy:(LAPolicy)policy localizedReason:(NSString *)localizedReason reply:(void (^)(BOOL success, NSError * _Nullable error))reply;` - **void**, the bug |
| `invalidate()` | `- (void)invalidate;` |
| `NSWorkspace::sharedWorkspace()` | `+ (NSWorkspace *)sharedWorkspace;` (no main-thread marker in these bindings) |
| `notificationCenter()` | `- (NSNotificationCenter *)notificationCenter;` |
| `addObserverForName_object_queue_usingBlock(name, None, None, &block) -> Retained<ProtocolObject<dyn NSObjectProtocol>>` | `- (id<NSObject>)addObserverForName:(NSNotificationName)name object:(id _Nullable)obj queue:(NSOperationQueue * _Nullable)queue usingBlock:(void (^)(NSNotification * _Nullable note))block;` |
| `addObserver_selector_name_object(&observer, sel!(...), Some(&name), None)` | `- (void)addObserver:(NSObject *)observer selector:(SEL)aSelector name:(NSNotificationName _Nullable)aName object:(id _Nullable)anObject;` |
| our `sleeplessSessionLockChanged:` | implemented on a `define_class!` subclass; one object argument, returns void, matching the selector the observer is registered with |
| `NSApplication::{sharedApplication, activationPolicy, setActivationPolicy:, unhide:, activateIgnoringOtherApps: -> BOOL, activate}` | `-activate` is macOS 14+, which is why it is behind `respondsToSelector:` |
| `NSWindow::{level, setLevel:, setCollectionBehavior:, setFrame_display:, makeKeyAndOrderFront:, orderFrontRegardless, isKeyWindow -> BOOL, makeFirstResponder: -> BOOL, firstResponder, title, frame}` | as generated |
| `NSApp nextEventMatchingMask_untilDate_inMode_dequeue -> Option<Retained<NSEvent>>`, `sendEvent:` | `- (nullable NSEvent *)nextEventMatchingMask:...` and `- (void)sendEvent:` |
| `NSTimer::scheduledTimerWithTimeInterval_repeats_block`, `NSRunLoop::addTimer_forMode`, `invalidate` | `- (void)addTimer:(NSTimer *)timer forMode:(NSRunLoopMode)mode;` |
| `NSEvent::mouseLocation() -> NSPoint`, `NSScreen::screens(mtm)` | `+ (NSPoint)mouseLocation;`, `+ (NSArray<NSScreen *> *)screens;` |
| `NSError::{code -> NSInteger, domain, localizedDescription}` | used to log the LocalAuthentication refusal |
| `NSNotification::name()` | `- (NSNotificationName)name;` |

Two binding details that matter when reading the code: `NSNotificationName` is a Rust
typealias for `NSString`, and `LAPolicy` is a newtype over `NSInteger` (the
`kLAPolicy...` constants exported by the crate are `c_uint`, which is the wrong width -
use `LAPolicy::DeviceOwnerAuthenticationWithBiometrics`).

### Verification done here (beta4)

- `cargo check --workspace` clean on `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-gnu`,
  `aarch64-apple-darwin` **and** `x86_64-apple-darwin` (the macOS job builds both arches;
  earlier checks had only covered aarch64).
- Same for `--features catch-objc-exceptions` with stubbed `CC`/`AR`: the catching path
  type-checks.
- `timeout 60 cargo test --workspace`: 16 passed, 1 ignored, plus the gui crate's 1. New
  test: `a_panic_names_the_place_and_the_reason` (both the `&str` and the `String` panic
  payload shapes).
- Test builds for the darwin and windows targets: no front-end errors (linking still
  impossible here).
- Dead end worth recording: the SIGABRT cannot be reproduced or observed from here - no
  Objective-C runtime, and the crash report is the only witness. That is why beta4's
  answer is "several independent layers plus a panic hook" rather than one root cause.

### Published: v0.2.2beta4

- Commit `ac535be` on `lock-gate`, tag `v0.2.2beta4` ->
  https://github.com/ilyatbn/sleepless/releases/tag/v0.2.2beta4 (pre-release, not Latest).
- CI run 14: version / linux x86_64 / linux aarch64 / windows / macos / release all
  completed success. The macOS job is now the one that builds with
  `--features sleepless/catch-objc-exceptions`, so the exception-catching path was compiled
  by a real Objective-C toolchain there (locally it was only type-checked with stubbed
  CC/AR).
- Checked in the shipped artifacts rather than trusting the build:
  - the universal `sleepless` reports `Sleepless 0.2.2-beta.4 starting (pid ...)`;
  - it contains "Touch ID request handed to the system; waiting up to ", both new
    localizedReason strings, "Watching for com.apple.screenIsLocked (selector observer)."
    and the "(block observer)" fallback line, ", key window: " from the timer, "PANIC at ",
    the `CLASS_sleepless::platform::LockWatcher` symbols for the defined observer class, and
    `objc2_exception_helper` (so the guard is linked in);
  - `Sleepless.app` carries `CFBundleShortVersionString = 0.2.2-beta.4`.

### What has to be checked on the device (beta4)

1. Enrolment: Touch ID must actually wait for the finger (log shows
   "Touch ID request handed to the system; waiting up to 45s..." and then either
   "Touch ID answered: confirmed." or an answer with an error code).
2. "Turn display off" must produce black windows and keep the display awake; the log must
   reach "Blacked out N display(s)." and then the watcher line.
3. Coming back: the prompt visible, on the display under the pointer, and the timer line
   must say `key window: true`.
4. `Ctrl+Cmd+Q` / fast user switch: immediate exit, with either the
   "Distributed notification ..." or "Session notification ..." line.
5. If anything still dies, the log should end with a `PANIC at ...` line.

## beta5: full self-review of the branch (2026-10-21)

Three betas shipped bugs that a careful read would have caught, so the whole branch was
re-read as if written by someone not to be trusted, then re-read again after fixing.
Every finding below is fixed unless marked as a known limitation.

### The root cause of beta3 and beta4's missing black screen

`watch_screen_is_locked()` sent **`+distributedNotificationCenter`, which is not a method
of `NSDistributedNotificationCenter`**. Apple documents the class methods as
`+defaultCenter` and `+notificationCenterForType:`; the made-up one produced
`-[NSDistributedNotificationCenter distributedNotificationCenter]: unrecognized selector`,
i.e. `doesNotRecognizeSelector:` -> an `NSException` thrown through
`___forwarding___`/`_CF_forwarding_prep_0`, which cannot unwind through Rust frames.
Confirmed independently: the user's `OBJC_PRINT_EXCEPTION_THROW=YES` trace showed exactly
that path, and Apple's own docs page for the class lists only the two real accessors
(plus "Inherits From: NSNotificationCenter", which is what makes the typed observer methods
valid). beta3 died inside the same function with a hand-written block-observer selector,
so both betas were the same class of mistake: **an invented Objective-C API**.

### Findings, in the order they were found

| # | Where | What was wrong | What it became |
| --- | --- | --- | --- |
| 1 | `watch_screen_is_locked` | `+distributedNotificationCenter` does not exist -> unrecognized selector -> abort, a moment after the black windows went up (so "turn display off" appeared to do nothing) | `+defaultCenter`, every step logged before and after |
| 2 | `Cargo.toml` | `panic = "abort"` marks every Rust frame nounwind, so *no* Objective-C exception can ever reach a `@catch` - including one inside `objc2::exception::catch`, whose own source carries a "test this with panic=abort" TODO. It also turned each crash into the contentless "panic in a function that cannot unwind" | `panic = "unwind"` in the release profile, with the reason in the comment |
| 3 | `try_objc` coverage | Only the two subscribe calls were guarded. The centre lookup, the retain, the `downcast`, the first `define_class!` class registration and the string creation were all outside it | one guard around `distributed_center()`, one around the selector subscription, one around the block fallback; plus `std::panic::catch_unwind` around the whole watcher so a panic there is a log line too |
| 4 | nowhere | No last-resort reporting of an Objective-C exception: two crashes produced no name, no reason, no selector | `objc_setExceptionPreprocessor` (logs at throw time, with the exception's class, name and reason) and `objc_setUncaughtExceptionHandler` (logs on the way out), installed by `install_crash_reporting()` in **every** macOS process, session or credential command (`main.rs`), no-op on Windows/Linux |
| 5 | `pump_events_until` | `nextEventMatchingMask:untilDate:inMode:dequeue:` was called with `untilDate: nil`, i.e. *block until an event arrives for this app*. During a black screen this app is usually not the active one, so the input the worker thread notices may never reach our queue: `BACK` / `UNLOCKED` / session-taken would go unseen forever - a black screen with no prompt and no exit, which is the failure this feature must never produce | bounded 40 ms slices, so the flags are always seen |
| 6 | `black_out` | `NSCursor::hide()` was never balanced by an unhide: the pointer stayed invisible during the password prompt and during Apple's Touch ID panel (whose "Use Password…" / "Cancel" buttons were therefore unreachable by mouse); hide/unhide are counted by AppKit, so a bare unhide would have been wrong anyway | counted `hide_cursor()` / `show_cursor()` on a static flag, pointer back for every prompt (`prompt_secret_locked`, `passkey_unlock`, `passkey_enroll`), hidden again between attempts |
| 7 | `watch_session_locks` (Linux) | Polling `loginctl`/the screensaver from the very start of the run: a session that was *already* showing a screensaver when Sleepless started would end the run instantly | the poller starts after the display is actually off |
| 8 | X11 wait loop (Linux) | `while x11_monitor_on() == Some(false)` had no cancel: a locked session with the monitor still off would spin forever | `&& !session_was_taken()` plus a log line |
| 9 | "keep awake" fallback (Linux) | With no terminal, `loop { sleep(3600) }` held the inhibitor lock forever, ignoring the lock watcher | waits on `session_was_taken()` and exits |
| 10 | `edit_text` / `lock_overlay_ask` (Windows) | `&buf[..len.min(buf.len() as u32) as usize]` - `GetDlgItemTextW`'s return is `u32` here but the pattern is wrong in general, and any over-long/negative length would slice out of bounds and **panic on the lock screen**, killing the process and uncovering the desktop without ever asking for the password | one `utf16_prefix(buf, len)` helper that clamps to the buffer, used at both sites, with the failure mode spelled out in its comment |
| 11 | Windows screensaver restore | If no screensaver was configured before, restoring wrote nothing: `SCRNSAVE.EXE` stayed pointed at the blank saver, quietly enabling it for the user | `RegDeleteValueW` when there was no previous value, with the result logged |
| 12 | `unlock_loop` callers (Windows, Linux) | Logged "Password accepted." even when the gate ended because the session was taken over - a report would have said the opposite of what happened | each caller branches on the bool and logs which it was |
| 13 | `prompt_secret` (macOS) | Duplicated the activation-policy save/set/restore that `show_modal` already does | dropped; one place owns it |
| 14 | `passkey_unlock` (macOS) | Activated but left the process as an accessory, which cannot become active at all, so the Touch ID panel could not be frontmost; it also used the deprecated activation call directly | sets `Regular` for the duration (like `passkey_enroll`) and goes through `activate_app` (old API plus the macOS 14 `-activate`, the latter behind `respondsToSelector:`) |
| 15 | `LockWatcher` | A bare, generic class name registered into every process that runs the session - AppKit and third-party frameworks define a lot of classes, and a collision is exactly the kind of thing that produces an obscure runtime failure | `SleeplessLockWatcher` |
| 16 | `run()` | No trace of how far a session got: the last line was whatever had always been logged | stage lines (`Session starting`, `Stage: the black screen`, `Making one black window per display (N of them)`, `Black screen ready`, `Stage: waiting to be interrupted`) and a breadcrumb before every Objective-C step in both watchers |
| 17 | `gui` | `path.parent().unwrap()` in the autostart files and the log-dir creation - only unreachable in practice, but a panic in the tray app takes the menu bar icon with it | guarded |
| 18 | version strings | `Cargo.toml` said 0.3.0 while tags said v0.2.2beta*, and the log line quoted the wrong build | both packages at `0.2.2-beta.5` |

| 19 | `run()` (macOS), `watch_session_locks` (Linux) | `std::thread::spawn` panics if the OS refuses to start a thread. With the black windows already up, that panic would end the process - display back, password never asked | `unix::spawn_thread` (Builder + name, returns io::Result); a missing watcher is a logged degradation, and a missing *activity* watcher releases everything and exits instead of leaving an unanswerable screen |
| 20 | the exception handlers | installed only for the session | `install_crash_reporting()` is called from `main()` for every subcommand: the credential commands talk to AppKit (NSAlert) and LocalAuthentication too |

| 21 | `unlock_loop` | If a store ever holds a passkey without a password (hand-edited, or a password cleared from another session while the screen is black), `check_password` can never succeed and the gate asks **forever** - the exact "black screen with no way back" the feature must never produce | the loop checks `has_password()` first and ends the gate (which ends the session) with a log line explaining why; tested by `the_gate_ends_when_no_password_is_enrolled` |

### Second pass over the same code (what it found, and what it confirmed)

- **Threading.** `release_and_exit()` only touches the `caffeinate` child and `exit(0)`; no
  AppKit from a worker thread (which is also why the cursor unhide lives on the main-thread
  paths and not in `release_and_exit`). The reply block of `evaluatePolicy` and the
  distributed observer both only touch an `AtomicBool`/`AtomicU8` and the logger. The
  `PROMPT_*` thread_locals are touched only by `show_modal` and the timer, both main thread.
  `SleeplessLockWatcher` has no main-thread marker, so AppKit may call it anywhere; its body
  is thread-safe.
- **Lifetimes.** Observer tokens in `OBSERVERS`, observer objects in `WATCHERS`, the timer in
  `PROMPT_TIMER` (invalidated when the prompt ends), the black windows held in `_windows` for
  the whole run (dropping them early would uncover the desktop), and the `LAContext` owned by
  the caller for the whole evaluation - Apple warns that releasing it mid-evaluation cancels
  it.
- **Escape hatches, branch by branch.** Gate with nothing to check -> ends (row 21). Prompt loop cancelled -> `cancelled()` -> restore
  path; session taken -> watchdog thread (500 ms poll) or the wait loop's cancel ->
  `release_and_exit`; prompt throws -> the process dies, the windows die with it and the
  display comes back (deliberately *not* caught: catching `runModal` would loop forever
  re-throwing); watcher fails -> logged and the run continues; Linux display can't be
  controlled -> waits for Enter, or for a lock if there's no terminal; Windows overlay
  creation fails -> falls back to the CLI dialog. Known limitation: if the screen-lock watch
  is unavailable, `Ctrl+Cmd+Q` no longer ends the run by itself - the OS lock screen covers
  ours, and killing the process (or the new `SLEEPLESS_NO_SCREEN_LOCK_WATCH=1`) still works.
- **Still hand-written Objective-C after the cleanup: exactly four messages** - the three
  below and `sel!(sleeplessSessionLockChanged:)`, which names a method we define ourselves.
  Everything else goes through generated bindings.

### Objective-C call audit (beta5)

| Selector / call | Class | Verified against | ok |
| --- | --- | --- | --- |
| `+defaultCenter` | NSDistributedNotificationCenter | Apple docs, Foundation/DistributedNotificationCenter (class methods: `default()`, `forType(_:)`; navigatorTitles `+ defaultCenter`, `+ notificationCenterForType:`) + the archived Cocoa Fundamentals guide ("you access with the `NSDistributedNotificationCenter` class method `defaultCenter`") | yes (was `+distributedNotificationCenter`: **no**) |
| `-downcast::<NSNotificationCenter>()` | the centre | same page: "Inherits From: NSNotificationCenter"; objc2 `Retained::downcast` performs `isKindOfClass:` at runtime | yes |
| `-addObserver:selector:name:object:` | NSNotificationCenter (inherited) | objc2-foundation `generated/NSNotification.rs:143`, returns void | yes |
| `-addObserverForName:object:queue:usingBlock:` | NSNotificationCenter (inherited) | objc2-foundation `generated/NSNotification.rs:210`, returns `Retained<ProtocolObject<dyn NSObjectProtocol>>`, needs features `NSNotification`/`NSOperation`/`NSString`/`block2` | yes (beta3's 5-argument `options:` variant was hand-written: **no**) |
| `+new` | SleeplessLockWatcher (NSObject subclass) | objc2 `extern_methods!` superclass block; `msg_send!` converts the `id` return to `Retained` (`msg_send_id!` is deprecated) | yes |
| `-respondsToSelector:` + `-activate` | NSApplication | objc2-app-kit `NSApplication.rs:569` (`pub fn activate(&self)`, no availability gate, macOS 14+ in AppKit) behind the NSObject guard | yes |
| `-evaluatePolicy:localizedReason:reply:` | LAContext | objc2-local-authentication `LAContext.rs:301` -> **void**, block `Fn(Bool, *mut NSError)` | yes (beta2/3 read a BOOL: **no**) |
| `-canEvaluatePolicy:error:` | LAContext | same file, line 234 -> `Result<(), Retained<NSError>>` | yes |
| `-invalidate` | LAContext | same file, line 323 | yes |
| `+sharedWorkspace`, `-notificationCenter` | NSWorkspace | objc2-app-kit `NSWorkspace.rs:50`, `:54` | yes |
| `+distributedNotificationCenter` | - | does not exist anywhere | removed |
| `+ [NSCursor hide]` / `+ [NSCursor unhide]` | NSCursor | objc2-app-kit `NSCursor.rs:130`, `:134`, class methods, void | yes (now balanced by a counter) |
| `-runModal` | NSAlert | `NSAlert.rs:231` -> `NSModalResponse` (buttons answer 1000/1001, see the earlier gotcha) | yes |
| `-window`, `-setAccessoryView:`, `-addButtonWithTitle:`, `-setMessageText:`, `-setInformativeText:`, `-setAlertStyle:` | NSAlert | `NSAlert.rs:287/219/139/...` | yes |
| `-nextEventMatchingMask:untilDate:inMode:dequeue:`, `-sendEvent:`, `-setActivationPolicy:`, `-activationPolicy`, `-unhide:`, `-activateIgnoringOtherApps:` | NSApplication | `NSApplication.rs:906` (returns `Option<Retained<NSEvent>>`, `expiration: Option<&NSDate>` - nil means *block forever*), `:893`, `:752`, `:741`, `:507`, `:553` | yes (the nil-expiration use was the bug in row 5) |
| `-setLevel:`, `-setCollectionBehavior:`, `-setFrame:display:`, `-frame`, `-title`, `-level`, `-makeKeyAndOrderFront:`, `-orderFrontRegardless`, `-makeFirstResponder:`, `-isKeyWindow`, `-setBackgroundColor:`, `-setHasShadow:`, `-setReleasedWhenClosed:` | NSWindow | `NSWindow.rs:1092/1113/952/1157/2081/972/...` | yes |
| `+screens`, `-frame` | NSScreen / `NSArray::firstObject/iter/len` | `NSScreen.rs`, `NSArray.rs` | yes |
| `-stringValue` | NSControl (NSSecureTextField's ancestor) | `NSControl.rs:204` -> `Retained<NSString>` | yes |
| `-addTimer:forMode:`, `+mainRunLoop`, `+RunLoopCommonModes`, `+scheduledTimerWithTimeInterval:repeats:block:`, `-invalidate` | NSRunLoop / NSTimer | `NSRunLoop.rs:57`, `NSRunLoop.rs:20`, `NSTimer.rs:118` | yes |
| `+dateWithTimeIntervalSinceNow:` | NSDate | `NSDate.rs` | yes |
| `objc_setExceptionPreprocessor`, `objc_setUncaughtExceptionHandler` | libobjc (`objc-exception.h`) | objc2 `src/ffi/exception.rs:92`, `:97`; handler types `unsafe extern "C" fn(*mut AnyObject) -> *mut AnyObject` and `unsafe extern "C" fn(*mut AnyObject)`, documented as non-null parameters, available on all apple targets except 32-bit macos x86 (we ship arm64 + x86_64) | yes |
| `-name`, `-reason`, `-isKindOfClass:` on the exception | NSException, via `objc2::exception::Exception`'s Debug impl | objc2 `src/exception.rs:160-200` (checks `isKindOfClass:` before touching `name`/`reason`) | yes |

### Hazard recorded so it isn't repeated

Running a bare `sleepless` on the development desktop is **not** a safe smoke test: with
`XDG_CURRENT_DESKTOP=zorin:gnome` it picks the GNOME backend and switches the real monitor
off (`org.gnome.Mutter.DisplayConfig PowerSaveMode 3`), restoring only if it exits cleanly.
Unsetting `XDG_SESSION_TYPE`/`DISPLAY` does not prevent it. Smoke-test the credential
commands (`--has-password`, `--clear-password`) instead; `timeout` around a session run
leaves the display in whatever state the killed process left it.

### Published: v0.2.2beta5

- Commit `710d96d` (+ a notes-only follow-up) on `lock-gate`, tag `v0.2.2beta5` ->
  https://github.com/ilyatbn/sleepless/releases/tag/v0.2.2beta5 (pre-release, not Latest).
- CI run 15: version / linux x86_64 / linux aarch64 / windows / macos / release all
  completed success. Note: the first push of the tag produced **no run at all** (the tag was
  on the remote and the workflow file had the tag trigger; nothing appeared for 12 minutes).
  Deleting and re-pushing the tag created run 15 immediately - worth remembering as the fix
  for a silently dropped tag trigger.
- Checked in the shipped universal binary (x86_64 + arm64), not in the source:
  `Sleepless 0.2.2-beta.5`; `defaultCenter` present and `distributedNotificationCenter`
  **absent**; all the `Screen-lock watch: …` breadcrumbs; `Objective-C exception reporting
  installed.`; `SleeplessLockWatcher`; `SLEEPLESS_NO_SCREEN_LOCK_WATCH`;
  `Making one black window per display`; `Stage: the black screen.`; `Showing the pointer
  again.`; the gate's "no password is enrolled" line; and the imported symbols
  `objc_setExceptionPreprocessor`, `objc_setUncaughtExceptionHandler` and
  `objc2_exception_helper_0_1_try_catch` (so the exception guards are linked in).
- Size cost of `panic = "unwind"` (as expected, and worth knowing): `sleepless.exe` 274 KB ->
  306 KB, the Linux x86_64 tarball 1.28 MB -> 1.49 MB, the macOS universal CLI 1.13 MB ->
  1.22 MB.

### Fourth and fifth passes

After the 21 rows above were fixed, two more passes were run over the changed code:

- the fourth found only stale/incorrect comments (a doc comment claiming
  `raise_above_black_out` tracked "the alert we're currently showing", and a duplicated
  NSAlert-return paragraph), and one wrong **record in these notes**: the beta4 audit table
  asserted a `+ (NSDistributedNotificationCenter *)distributedNotificationCenter;`
  declaration that exists nowhere. It is now struck through rather than quietly edited,
  because how that claim came to be written down is itself the lesson.
- the fifth pass re-checked the areas the fixes touched - `store.rs` (no panicking paths:
  `from_hex` bounds its pairs, the registry read clamps through the error check, the config
  file is written 0600 via a temp + rename), `lock.rs` (`Secret` zeroes on drop,
  `enroll_passkey` refuses without a password, `remove()` clears both sections so a
  passkey-without-password state is not reachable through the CLI), the gate call sites on
  all three platforms, the selector spelling used by `define_class!` matching the one passed
  to `addObserver:selector:name:object:` (both `sleeplessSessionLockChanged:`), the watcher
  registration order in `run()`, and the absence of any remaining `NSApplication::run()`
  path. Nothing further was found.

### Things this review could not settle without a Mac

- Whether `+defaultCenter` on a process started by the tray app behaves (it is documented;
  the crash trace proves the *class* is present and messages reach it).
- Whether the distributed `com.apple.screenIsLocked` subscription actually fires for
  `Ctrl+Cmd+Q` on the user's macOS version - the beta5 log will now say which observer
  subscribed, and the new `SLEEPLESS_NO_SCREEN_LOCK_WATCH=1` switch isolates it.
- Whether `key window: true` appears in the timer line (activation of a background process is
  a policy the OS can refuse).
- Whether `NSDistributedNotificationCenter` delivers to a selector observer on the run loop we
  pump by hand (we drain events with `nextEventMatchingMask:`, not `NSApp run`).
- The Windows and Linux changes are compile-verified and code-reviewed only; the registry
  `RegDeleteValueW` path and the logind poller need the real machines.

## beta6: the enrolment dialogs were invisible (2026-10-21)

beta5 got the black screen back. The next report: "Set a password to come back" put a Dock
icon up and showed no dialog anywhere, while stealing focus from Terminal; Escape eventually
answered with `alert response 1001 -> not accepted`, and the `Cancelled:` alert *was* visible.
The log had the frame on screen and the level right, and `key window` flipping true/false
dozens of times across two minutes.

### The mechanism

Two things, both ours:

1. **`NSAlert`'s window is an `NSPanel`, and `NSPanel` defaults to `hidesOnDeactivate = YES`.**
   So every time the system deactivated us - which it does freely, because we are a
   background process the user keeps tabbing away from - the dialog hid itself. That is
   "on screen, right level, right frame, not visible".
2. **`keep_prompt_on_top` asked for activation twice a second, using macOS 14's cooperative
   `-activate`.** Cooperative activation is only a *request*: the system granted it, then
   gave focus back to Terminal, then granted it again. The `key window: true/false` flap in
   the log *is* that fight, and it is exactly the focus stealing the user reported. The timer
   also ran for ordinary dialogs, where there is nothing to keep above anything.

The policy flip to `Regular` in `show_modal` is where the unexpected Dock icon came from;
beta3 didn't have it, and the same dialog was visible and typeable there.

### What beta6 does

- `setHidesOnDeactivate(false)` on every dialog window this app shows - and explicitly on
  the black windows too, where the default happened to be the right one already. Without
  this, "the screen went black and then came back" and "the dialog isn't there" are both one
  flag away.
- No activation-policy change for enrolment or any ordinary dialog: beta3's behaviour, no
  Dock icon. `Regular` is used in exactly two places, each commented: the lock gate (a
  non-active accessory app cannot own the key window, and at the gate the screen is ours
  anyway) and the Touch ID prompt (Apple's panel needs an active app, and an accessory
  process cannot become active).
- The keep-on-top timer runs **only** for the lock gate (`show_modal(alert, mtm,
  at_the_lock_screen)`). For every other dialog: activate once, show, and leave the user's
  focus alone.
- Inside the timer: if the box still has key, do nothing at all. Only when it lost key does
  it re-raise and take key back, and re-activation is rate limited to once every two seconds
  (and the counter starts so the first retry is two seconds in, after the single activation
  in `show_modal` has had its chance).
- `activate_app` asks once, three ways: the deprecated `activateIgnoringOtherApps:` (still
  honoured before 14), `NSRunningApplication::currentApplication().activateWithOptions(
  ActivateAllWindows)` (only that option - the `ActivateIgnoringOtherApps` *option* is
  documented as having no effect since macOS 14), and 14's cooperative `-activate` if
  `respondsToSelector:` says it's there.
- The tray app, which is the app the user actually clicked, hands activation over to the
  child before it blocks on it: `run_cli` now spawns and then
  `yieldActivationToApplication:` for the child's pid (bounded 1 s search for the
  `NSRunningApplication`, logged either way, skipped entirely on a macOS that doesn't have
  the selector).
- Diagnostics for the next report: `hidesOnDeactivate`, `isVisible`, `occlusionState` and
  `NSApp.isActive` in the "Dialog … shown" line, and the same set from inside the modal loop
  on the first timer tick.

### Findings from reviewing this diff (second and third passes)

| # | What | Fix |
| --- | --- | --- |
| 1 | Two of the new probes were hand-written `msg_send![…, respondsToSelector: …]` - the exact pattern that caused the last two crashes | replaced with the typed `NSObjectProtocol::respondsToSelector` (needs `objc2-foundation` in the tray crate); hand-written Objective-C messages in the whole branch are down to three, all in the table below |
| 2 | `PROMPT_LAST_ACTIVATE` started at `u32::MAX - 10`, and `tick.saturating_sub(last)` on a small tick is 0 - the timer would never have re-activated a lost box | start at 0; first retry two seconds in |
| 3 | The black windows relied on `NSWindow`'s default (not hiding on deactivate) while the dialogs needed the opposite; an easy thing to break later | `setHidesOnDeactivate(false)` spelled out on both |
| 4 | `hand_over_activation` cast a `u32` pid to `i32` directly | `i32::try_from(pid).unwrap_or(-1)` |
| 5 | `run_cli` used `.output()`, so there was no pid before the child ran | `spawn` + `wait_with_output`, which also keeps the existing stdout/stderr logging identical |
| 6 | `activateWithOptions:` needed the crate's `libc` feature for `runningApplicationWithProcessIdentifier:` | feature recorded in `gui/Cargo.toml` with the reason |

### Hand-written Objective-C after beta6 (the whole branch)

| Selector | Receiver | Verified against | ok |
| --- | --- | --- | --- |
| `+defaultCenter` | NSDistributedNotificationCenter | Apple docs for the class (its only two class accessors) | yes |
| `+new` | SleeplessLockWatcher (our `define_class!` subclass of NSObject) | NSObject `+new`; `msg_send!` converts the `id` result to `Retained` | yes |
| `sleeplessSessionLockChanged:` | same class | declared in the same file by `#[unsafe(method(sleeplessSessionLockChanged:))]`; names match character for character | yes |

Everything else, including `respondsToSelector:`, `activate`, `activateWithOptions:`,
`yieldActivationToApplication:`, `setHidesOnDeactivate:`, `hidesOnDeactivate`,
`occlusionState`, `isVisible`, `isActive`, `currentApplication`,
`runningApplicationWithProcessIdentifier:`, `runModal`, `window`, the NSAlert/NSWindow/
NSApplication/NSRunLoop/NSTimer set and the three LAContext methods, is a generated binding:
the compiler resolves it against a signature taken from Apple's headers, so a wrong name or
return type fails the build rather than crashing on someone's machine. That is the difference
between a typed call and a hand-written `msg_send!`, and it is why the last three are the
only ones left.

### Still needs the machine

- Whether one activation plus `hidesOnDeactivate = false` is enough for the enrolment dialog
  to stay visible while the user works in Terminal - the new log line answers it.
- Whether the tray's `yieldActivationToApplication:` finds the child at all (a bare CLI binary
  may not appear as an `NSRunningApplication`); the log says which happened.
- Whether the gate box still wins key against the black windows with the timer now idle when
  key is held.
- `occlusionState` semantics on the user's display setup.

## beta6 (republished): the prompt presenter is deleted, not tuned

The enrolment dialogs were invisible and the lock prompt was a stack of windows
fighting over focus. Both had the same root: this app kept trying to show UI from
a process the system had no reason to activate.

Root causes, stated plainly:

1. Enrolment dialogs were shown by the CLI child (`sleepless --set-password`)
   that the tray app spawns. Since macOS 14 activation is cooperative: a
   background child of a GUI app does not get focus while the user is typing
   somewhere else. Activation-policy flips, `activate` twice a second and window
   levels above the menu bar were all attempts to override that, and all of them
   produced the same symptom - a panel with a correct frame, on screen, invisible,
   with the user's focus dragged back every other tick.
2. The lock prompt was an NSAlert layered over borderless black windows. A
   borderless NSWindow answers NO to `canBecomeKeyWindow`, so the black windows
   could never hold the keyboard; that is why a separate panel existed at all, and
   why it then had to fight to stay key and visible.

### What the code looks like now

**A. Enrolment UI lives in the tray app (`sleepless-gui`), in process.**
`gui/src/enrol.rs`:
- the user just clicked this app's menu, so it is the one app that is allowed to
  activate: `activateIgnoringOtherApps` (pre-14) plus `-activate` behind
  `respondsToSelector:`, asked **once** per dialog;
- a plain `NSAlert` with an `NSSecureTextField` accessory view, the alert window's
  `initialFirstResponder` set to the field, `runModal`; twice, for the confirm;
- no timers, no window levels, no activation-policy change; the tray app stays an
  accessory (`LSUIElement`) app, which can still activate when the user interacts
  with it;
- the password reaches the CLI through a pipe: `sleepless --set-password --stdin`
  reads one line from stdin - never argv (visible in the process list), never the
  environment (readable by any process);
- Touch ID enrolment is `LAContext.evaluatePolicy:localizedReason:reply:` from the
  tray process too (it has the bundle and the activation), waited for with the
  same bounded wait; when it succeeds the CLI is asked to record it with
  `sleepless --set-passkey --confirmed`, which writes the store without asking
  again. Off macOS `--confirmed` refuses, because there is no passkey there to
  confirm.
- the CLI keeps a terminal path (`stty -echo`) for `sleepless --set-password` run
  by hand or over ssh, and shows **no AppKit dialog at all** any more:
  `prompt_secret_interactive`, `ask_yes_no` and `report` are terminal/stderr.

**B. The lock prompt lives in the black window, in the session process.**
`src/macos.rs`:
- `define_class!` `SleeplessBlackWindow` (`#[unsafe(super(NSWindow))]`) overriding
  `canBecomeKeyWindow` / `canBecomeMainWindow` to YES - the black window itself
  takes the keyboard;
- the question, the `NSSecureTextField` and a status line are subviews of the
  window on the display the pointer is on (`pick_box_window`, unit-tested for
  beside / left-of / above layouts and the shared-edge case); the other displays
  stay plain black;
- one `activate_app` request when the box goes up; `makeKeyAndOrderFront` +
  `makeFirstResponder:`; if the field does **not** take focus the box is declared
  unbuildable and the session ends with the display handed back - a box nobody can
  type into is a trap, not a lock screen;
- Return / keypad-Enter / Escape come from one **local** event monitor
  (`addLocalMonitorForEventsMatchingMask:handler:`): the block returns nil to
  swallow the key, otherwise the event unchanged. Escape clears the field and
  never unlocks;
- the value is read with `-validateEditing` then `-stringValue`: while an edit is
  in progress a shared field editor is the real first responder and the control's
  `stringValue` is the last *committed* value, so reading it straight would have
  handed back an empty box however much was typed. Escape uses `-abortEditing`
  (throws the in-progress edit away). Neither moves focus, so the caret stays in
  the box for the next attempt;
- `lock::unlock_loop` still drives the attempts and the back-off: its `ask`
  closure sets the status line and then `wait_for_box`, which pumps until the
  monitor reports Enter or Escape, with no timeout (standing at a locked screen is
  allowed) and a 20-second breadcrumb saying the box is still up and whether the
  window is still key;
- Touch ID at the gate is evaluated from this process **after** the box appears,
  so if the sensor can't help, the focused field is simply still there. The
  pointer stays visible for the whole gate.

**C. Deleted**: `show_modal`, `secret_alert`, `secret_from`, `accepted`,
`prompt_secret`, `prompt_secret_locked`, `keep_prompt_on_top` + its timer and
thread-locals, `stop_keeping_prompt_on_top`, `raise_above_black_out` for prompts,
`centre_on_cursor_display`, the activation-policy flipping in the dialog path, and
the tray app's `hand_over_activation` (it only existed to help a child show
dialogs - the child shows none now). Kept: the session-lock watchers, caffeinate,
the restore logic, the logging and the crash reporting.

### Findings from the review passes over this change

| # | Finding | Fix |
|---|---|---|
| 1 | `stringValue` during an open edit returns the committed value, not what was typed - the gate would have submitted an empty box every time | `-validateEditing` before reading, `-abortEditing` for Escape |
| 2 | No `activate_app` call on the password-only path: the window was key but the app was not active, so keystrokes never arrived | one activation request right after the box is built |
| 3 | `passkey_unlock` re-hid the pointer after a failed Touch ID attempt, leaving the password box with a hidden cursor | the cursor stays visible for the whole gate |
| 4 | The CLI asked "replace the existing password?" **before** the `--stdin` branch; with no terminal that returns "no", so re-enrolment from the menu would have silently done nothing | `--stdin` is handled first, before any question |
| 5 | Modal dialogs and the Touch ID wait ran inside `APP.with_borrow_mut`, so a menu event arriving during them would re-enter the borrow and panic | enrolment runs with no borrow held; only the menu refresh takes it afterwards |
| 6 | `pump_events_until` only drained the event queue; `com.apple.screenIsLocked` arrives via `performSelectorOnMainThread:` on a run-loop port, so the lock watchdog could be deaf while the gate was up | every pass also gives the run loop a slice (`-runMode:beforeDate:`) |
| 7 | `src/macos.rs`'s `#[cfg(test)] mod tests` still called `accepted()`, deleted in the same commit - invisible because `cargo check` never compiles test code and `cargo test --workspace` on Linux never compiles `src/macos.rs` | replaced with a real test of the pointer→display choice; the release workflow now runs the unit tests on all three platforms (macOS leg also compiles them with `catch-objc-exceptions` on) |
| 8 | The typed password was copied into a `Secret`, leaving the original `String` to be freed un-zeroised | `Secret::take(String)` (used by the box and by `--stdin`), plus a test |
| 9 | `--stdin`'s early return skipped the passkey offer without saying why, and the `--confirmed` path could record a passkey on a platform that has none | logged; `passkey_supported()` gates `--confirmed` on every platform |
| 10 | Hand-written `+alloc` / `-initWithContentRect:...` on a runtime-defined subclass | kept (no generated constructor exists for a `define_class!` type), result *checked* with a downcast, nil-init handled, wrapped in `try_objc`, and creating the black window is a hard failure rather than a silent fallback to a non-key window |

### ObjC calls in the new code, verified against the vendored bindings

| selector | class | verified against | ok |
|---|---|---|---|
| `canBecomeKeyWindow` | NSWindow | objc2-app-kit `NSWindow.rs:1165` `-> bool` | ✓ (override signature matches) |
| `canBecomeMainWindow` | NSWindow | `NSWindow.rs:1169` `-> bool` | ✓ |
| `+alloc` | NSObject | runtime; `msg_send!` to `SleeplessBlackWindow::class()` | ✓ (hand-written: a runtime class has no generated constructor) |
| `-initWithContentRect:styleMask:backing:defer:` | NSWindow | `NSWindow.rs:570` (`Allocated<Self>`, `NSRect`, `NSWindowStyleMask`, `NSBackingStoreType`, `bool`) | ✓ (hand-written, args taken from that declaration, nil-checked + downcast) |
| `-contentView` | NSWindow | `NSWindow.rs:761` `-> Option<Retained<NSView>>` | ✓ |
| `-frame` | NSWindow / NSView | `NSView.rs:407` `-> NSRect` | ✓ |
| `-addSubview:` | NSView | `NSView.rs:293` | ✓ |
| `-setFrame:` | NSView | `NSView.rs:412` | ✓ |
| `+new` (NSSecureTextField / NSTextField) | NSControl family | `NSTextField.rs:427` `(mtm)` | ✓ |
| `-setEditable:` | NSTextField | `NSTextField.rs:218` | ✓ |
| `-setPlaceholderString:` | NSTextField | `NSTextField.rs:142` `Option<&NSString>` | ✓ |
| `-setBezeled:` / `-setDrawsBackground:` / `-setSelectable:` / `-setTextColor:` | NSTextField | `NSTextField.rs:209/178/227/191` | ✓ |
| `-stringValue` / `-setStringValue:` / `-setFont:` | NSControl | `NSControl.rs:206/213/344` | ✓ |
| `-validateEditing` | NSControl | `NSControl.rs:442` | ✓ |
| `-abortEditing` | NSControl | `NSControl.rs:438` `-> bool` | ✓ |
| `+addLocalMonitorForEventsMatchingMask:handler:` | NSEvent | `NSEvent.rs:1178` (`NSEventMask`, `&DynBlock<dyn Fn(NonNull<NSEvent>) -> *mut NSEvent>`) | ✓ |
| `-keyCode` | NSEvent | `NSEvent.rs:865` `-> c_ushort` | ✓ |
| `+removeMonitor:` | NSEvent | `NSEvent.rs:1188` | ✓ |
| `+mouseLocation` | NSEvent | `NSEvent.rs:1142` `-> NSPoint` | ✓ |
| `-setInitialFirstResponder:` | NSWindow | `NSWindow.rs:1795` `Option<&NSView>` | ✓ |
| `-makeKeyAndOrderFront:` | NSWindow | `NSWindow.rs:1092` `Option<&AnyObject>` | ✓ |
| `-makeFirstResponder:` | NSWindow | `NSWindow.rs:952` `Option<&NSResponder> -> bool` | ✓ |
| `-isKeyWindow` | NSWindow | `NSWindow.rs:1157` | ✓ |
| `-setTitle:` | NSWindow | `NSWindow.rs:606` | ✓ |
| `+whiteColor` | NSColor | `NSColor.rs:366` | ✓ |
| `+systemFontOfSize:` | NSFont | `NSFont.rs:131` (`CGFloat` - needs the crate's `objc2-core-foundation` feature) | ✓ |
| `-nextEventMatchingMask:untilDate:inMode:dequeue:` | NSApplication | `NSApplication.rs:908` | ✓ |
| `-sendEvent:` | NSApplication | `NSApplication.rs:893` | ✓ |
| `-activate` | NSApplication | `NSApplication.rs:571` (probed with `respondsToSelector:` - macOS 14+) | ✓ |
| `-activateIgnoringOtherApps:` | NSApplication | `NSApplication.rs:555` (deprecated) | ✓ |
| `-activationPolicy` / `-setActivationPolicy:` | NSApplication | `NSApplication.rs:743/754` | ✓ |
| `+currentApplication` / `-activateWithOptions:` | NSRunningApplication | `NSRunningApplication.rs:260/224` | ✓ |
| `+distantPast` / `+dateWithTimeIntervalSinceNow:` | NSDate | `NSDate.rs:212/189` | ✓ |
| `+currentRunLoop` / `-runMode:beforeDate:` | NSRunLoop | `NSRunLoop.rs:38/116` (`&NSRunLoopMode` - needs `NSObjCRuntime`) | ✓ |
| `-respondsToSelector:` | protocol NSObject | `NSObject.rs:196` (trait `NSObjectProtocol`) | ✓ |
| `+new` / `-window` / `-setMessageText:` / `-setInformativeText:` / `-addButtonWithTitle:` / `-setAccessoryView:` / `-runModal` / `-setAlertStyle:` | NSAlert | `NSAlert.rs:302/289/90/103/141/221/233/163` | ✓ (tray app only) |
| `-canEvaluatePolicy:error:` / `-evaluatePolicy:localizedReason:reply:` / `-invalidate` / `+new` | LAContext | objc2-local-authentication `LAContext.rs:236/303/323/572` | ✓ (reply block returns void) |

### Verification for this round

- `cargo check --tests --workspace` clean (0 warnings) for `x86_64-unknown-linux-gnu`,
  `x86_64-pc-windows-gnu`, `aarch64-apple-darwin`, `x86_64-apple-darwin`, and the
  `catch-objc-exceptions` feature build for `aarch64-apple-darwin`.
- `timeout 60 cargo test --workspace`: 18 passed, 1 ignored (`timing`), +1 in the gui crate.
- CLI plumbing run end to end on Linux against a scratch `SLEEPLESS_CONFIG_DIR`
  (no display involved): `--set-password --stdin` enrols (exit 0), replaces an
  existing password (exit 0), rejects an empty line (exit 1), `--set-passkey
  --confirmed` refuses off-macOS (exit 1), `--has-password` / `--clear-password`
  behave, and the store holds no plaintext.
- Still device-only: whether the box is visible *and* typeable with Terminal in
  front, whether `com.apple.screenIsLocked` fires for Ctrl+Cmd+Q on this macOS
  version, and whether Touch ID at the gate behaves with our window key.

### Release record: v0.2.2beta6 (pre-release), tag at `a79ae40`

https://github.com/ilyatbn/sleepless/releases/tag/v0.2.2beta6 - the tag was
deleted and re-pointed twice: the first beta6 (run 16, `f2b2f2c`) built the
superseded presenter, and the code that replaced it is what the release now
carries (`softprops/action-gh-release` creates *or updates* the release, and
`target_commitish` is now `a79ae40`).

Getting CI green took four runs, and every failure was a real one:

| Run | Outcome | Why |
|---|---|---|
| 18 | failure | the two new test legs failed: the multi-display assert in `pick_box_window` contradicted the (correct) edge rule, and on Windows the credential tests were reading/writing the live registry key because `SLEEPLESS_CONFIG_DIR` means nothing there |
| 19-22 | failure ×4 | same causes at `2aee87d`/`a9b7354`; the macOS leg went green with the edge-rule fix |
| 23 | failure | builds and tests all green (Windows included); only *Create / update release* failed - run 24 was writing the same release at the same moment |
| 24 | **success** | version, linux ×2, windows, macos, release |

Two CI hazards worth remembering: a tag push sometimes produces **no run at all**
(the remote ref moves and nothing is scheduled - delete, wait ~200s, re-push);
and two runs for the same tag **race on the release step**, so a failure there
next to a green run is that race, not a build problem.

Shipped binaries checked with `strings` (universal macOS build, both slices):

- present in `sleepless`: `SleeplessBlackWindow`, `canBecomeKeyWindow`,
  `Enter your password to come back`, `Password box: pointer at`,
  `validateEditing`, `abortEditing`, `Escape does not unlock Sleepless`,
  `Password arrived on stdin`, `--stdin`, `--confirmed`, `Sleepless 0.2.2-beta.6`;
- present in `sleepless-gui`: `password dialog`, `field focused`,
  `Touch ID prompt requested`, `Sleepless - new password` - the dialogs moved here;
- **absent from both**, proving the old presenter really is gone:
  `Lock prompt in its modal loop`, `hidesOnDeactivate`,
  `yielded activation to pid`, `Still holding the prompt`;
- the Linux and Windows binaries carry the same `0.2.2-beta.6`.


### Device test sheet for this build (v0.2.2beta6, tag at `2aee87d`)

Enrolment, from the menu bar:

1. Menu bar icon -> **Set a password to come back…**. Expect: a dialog *from Sleepless
   GUI* with "Choose a:", a secure field already focused (type immediately, no clicking),
   OK/Cancel; then a second one to confirm. No Dock icon appears at any point.
2. Wrong-then-right: cancel the second dialog -> "Cancelled: no password was set."
   Mismatched entries -> "The two entries didn't match, so nothing was saved."
3. After a successful pair: "Coming back from the black screen now needs this password."
   then "Also allow Touch ID to come back?" -> Yes -> Apple's Touch ID panel appears
   (this time from the tray app, which the user clicked, so it should simply work).
4. Re-run the menu item with a password already set -> "You already have a password to
   come back. Replace it?" first.
5. Terminal path still works: `sleepless --set-password` in Terminal asks with echo off;
   over ssh it also asks in the terminal. `--set-password --stdin` reads one line.

Coming back:

6. Eyes icon -> black screens. Move the mouse: the display the pointer is on now shows
   "Enter your password to come back", a secure field (focused, caret visible), and a
   status line. The pointer is visible. Other displays stay plain black.
7. Type the wrong password, Return -> "Wrong password, try again" + back-off; type the
   right one -> the screen comes back and the session ends.
8. Escape clears the field and does not unlock.
9. Two displays: put the pointer on the second one before coming back - the box must be
   on that display.
10. At the black screen *and* at the password box: **Ctrl+Cmd+Q** -> Sleepless exits
    immediately and the OS lock screen appears; the display is not left black.
11. `kill <pid>` of the session process (or **Quit Sleepless** in the tray) is still the
    supported bypass.

Log lines worth sending back (`~/Library/Logs/Sleepless/sleepless.log`, run with
`--debug` for the full picture):

- `Password box: pointer at (x, y); displays #0 …#1 …; using #N.` - which display was
  chosen and what the frames were.
- `Password box: window key true, field is first responder: true.` - if either is false,
  that is the bug.
- `Password box: Return pressed, N characters committed.` - N should be the length typed;
  `N = 0` when something was typed means the commit didn't happen.
- `Password box: still waiting for a password after 20s (window key: true).` - proves the
  box was up and the pump alive.
- `Password box: the field did not take keyboard focus.` / `No password box could be
  shown; ending the session rather than holding the screen.` - the escape hatch: the
  display is handed back instead of being held.
- `Asking for activation once (…)` - once per gate, not repeatedly.
- `Screen-lock watch: …` breadcrumbs, and `Session was taken over…` when Ctrl+Cmd+Q was
  pressed.
- Tray app (`~/Library/Logs/Sleepless/sleepless-gui.log`): `password dialog …: shown,
  field focused: true`, `password dialog …: N characters typed`, `Touch ID prompt
  requested; waiting up to 45s`, `Touch ID answered: confirmed`, `credential command
  ["--set-password", "--stdin"]: exit Some(0)`.
- Any `Objective-C exception thrown:` or `PANIC at` line.


## beta7: the four findings from an independent review of the beta6 design

The design (enrolment in the tray app, the box inside the black window) stood up
to the review; four real defects were found in it, all fixed here.

### 1. SERIOUS — the keystrokes could go to another app

`show_password_box` makes the window key and the field the first responder, and
then this process asks for activation once. Under cooperative activation
(macOS 14+) that request can be **declined**. The field is still "first responder"
of its window - and the log line said so - but the window belongs to an inactive
app, so everything typed goes to whatever app *is* frontmost. For anyone testing
this from Terminal, that means the password is typed into a shell prompt, and
Return runs it as a command and keeps it in the shell's history.

What the code does now:

- `app_is_active(mtm)` (`-[NSApplication isActive]`) is the only accepted evidence
  that input can arrive. `field is first responder: true` is kept in the log but
  is no longer treated as enough.
- `activate_and_wait` requests activation, pumps for up to 600ms (activation is
  asynchronous), and if the app is still not active requests **once** more - then
  stops: a poll loop is what made beta5's key window flap.
- While the app is not active, the box says `Click this screen to type your
  password` and the prompt message is not painted over it. A click on one of our
  own windows always activates our app; cooperative activation cannot decline
  that. `wait_for_box` re-checks `isActive` every pump slice (≤40ms), logs the
  transition (`The keyboard belongs to Sleepless again.` / `The keyboard does NOT
  belong to Sleepless: anything typed goes to another app.`), and on the way back
  in, re-focuses the field and restores the question.
- The same request/check happens right after black-out, so the session is the
  active app before the user ever comes back - the keypress that wakes the
  display goes to the frontmost app too, and it shouldn't be a shell. Logged as
  `Black screen: this app is the one the keyboard belongs to: true|false.`
- Touch ID is **skipped** while the keyboard isn't ours: Apple's panel may never
  appear for an inactive app, and the wait for it is bounded at 45s - the user
  would stare at "click this screen" for 45 seconds instead of clicking.
- The tray app's dialogs log the same fact (`activation requested; this app is the
  active app: true`), so an enrolment dialog that can't be typed into is visible
  in the log.

### 2. The pointer counter was inverted, so the pointer was never hidden

`hide_cursor` guarded with `if !CURSOR_HIDDEN.swap(true) { return }`: on the
first call the previous value is `false`, so it returned **without hiding**, and
every black screen in every beta so far has had a visible pointer. `show_cursor`
had the mirror bug. The transition is now a pure function -
`cursor_action(hidden, want_hide) -> (new state, act)` - with both directions and
a walk over a repeated sequence pinned by a test (`the_pointer_counter_acts_once_per_change`),
because AppKit counts these calls and an unbalanced pair leaves the pointer wrong
for the whole session.

### 3. The back-off penalty slept on the main thread

`lock::unlock_loop` called `std::thread::sleep` for a penalty of up to 16s. On
macOS that happens on the main thread, so for the whole penalty: no drawing, no
keyboard, and no `com.apple.screenIsLocked` - which is delivered on the main run
loop - so Ctrl+Cmd+Q did nothing until the penalty expired. That is precisely the
escape hatch failing when it is most needed.

- `unlock_loop_with_wait` hands the pause to the platform instead, and the pause
  says *why*: `Pause::Penalty(Duration)` (worth announcing) or `Pause::Brief`
  (a breath so a prompt that can't be answered can't spin the loop). Windows and
  Linux keep `unlock_loop`, which sleeps both.
- macOS waits penalties out with `wait_pumped`: pumping AppKit, aborting if the
  session is taken over, and counting the remaining seconds down in the box
  (`Too many attempts - wait 8s`).
- Escape and other dismissals are no longer guesses. `ask` returns
  `lock::Ask::{Guess, Dismissed}`; only `Guess` reaches `check_password` and feeds
  the back-off. Mashing Escape used to build up the penalty the next honest
  attempt then paid for. Three tests pin this: `dismissing_the_prompt_costs_no_back_off`,
  `wrong_guesses_hand_the_back_off_to_the_platform`, `a_cut_short_wait_ends_the_gate`.

### 4. The Escape notice was overwritten the instant it was shown

`unlock_loop` reset the message and the next `ask` called `set_box_status` with
it, so "Escape does not unlock Sleepless" was painted over immediately. The
Escape notice now sets a flag and writes only the status line; the gate's next
pass leaves it standing for that round.

### Found while re-reviewing these fixes

| Finding | Fix |
|---|---|
| The rejected guess sat in the box through the whole penalty - visible over the user's shoulder, and in the way of the next attempt (`set_box_status` cleared it only when the next round started) | the Return handler clears the box as soon as the value is committed; the next round puts the caret back |
| `activate_and_wait` waited 600ms for activation *before* requesting it on the gate path | requests, waits, then requests once more |
| A dismissed prompt must not spin the loop, but must not look like a penalty either | `Pause::Brief`, pumped on macOS without a countdown message |

### Audit additions

| selector | class | verified against | ok |
|---|---|---|---|
| `-isActive` | NSApplication | objc2-app-kit `NSApplication.rs` (`pub fn isActive(&self) -> bool`) | ✓ |
| `-validateEditing` | NSControl | `NSControl.rs:442` | ✓ |
| `-abortEditing` | NSControl | `NSControl.rs:438` `-> bool` | ✓ |
| `-makeFirstResponder:` | NSWindow | `NSWindow.rs:952` `Option<&NSResponder> -> bool` | ✓ |

### Verification for beta7

`cargo check --tests --workspace` clean (0 warnings) for linux, windows-gnu, both
darwin targets, and the `catch-objc-exceptions` build; `timeout 60 cargo test
--workspace` → 21 passed (+3 gate tests), 1 ignored, +1 gui. The two
macOS-only tests (`the_password_box_follows_the_pointer`,
`the_pointer_counter_acts_once_per_change`) run on the workflow's macOS leg.


## Sources

- https://learn.microsoft.com/windows/win32/api/winreg/nf-winreg-regcreatekeyexw
- https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-createwindowexw
- https://learn.microsoft.com/windows/win32/controls/edit-controls
- https://developer.apple.com/documentation/uikit/uitextfield (macOS counterpart: NSTextField/NSSecureTextField)
- https://developer.apple.com/documentation/localauthentication/lacontext
- https://learn.microsoft.com/windows/win32/api/webauthn/nf-webauthn-webauthnmakecredential
- OWASP password storage guidance (PBKDF2-HMAC-SHA256 work factor):
  https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html
- https://developer.apple.com/documentation/appkit/nsworkspace/notificationcenter
- https://developer.apple.com/documentation/appkit/nsworkspacesessiondidresignactiveconnection
- https://developer.apple.com/documentation/foundation/nsnotificationcenteraddobserverblock
- https://learn.microsoft.com/windows/win32/api/wtsapi32/nf-wtsapi32-wtsregistersessionnotification
- https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-msgwaitformultipleobjectsex
- https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-peekmessagew
- https://www.freedesktop.org/software/systemd/man/latest/loginctl.html
- https://specifications.freedesktop.org/idle-inhibit-spec/latest/
- https://developer.apple.com/documentation/localauthentication/lacontext/1514176-evaluatepolicy
- https://developer.apple.com/documentation/foundation/nsnotificationcenter/addobserverforname-object-queue-usingblock
- https://developer.apple.com/documentation/appkit/nsdistributednotificationcenter
- https://docs.rs/objc2-local-authentication/latest/objc2_local_authentication/struct.LAContext.html
- https://docs.rs/objc2/latest/objc2/exception/fn.catch.html
- https://objc2.objcs.io/objc2/ (define_class!, msg_send! Retained conversion)
- https://developer.apple.com/documentation/appkit/nsevent/addlocalmonitorforevents(matching:handler:) (return the event unmodified, or nil to stop it being dispatched)
- https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/TextEditing/Tasks/FieldEditor.html (the shared field editor *is* the first responder; ending an edit synchronises the cell; `validateEditing` / `abortEditing` on NSControl)
- https://developer.apple.com/documentation/appkit/nsapplication/activate()
- https://developer.apple.com/documentation/foundation/distributednotificationcenter (class methods +defaultCenter, +notificationCenterForType:; inherits from NSNotificationCenter)
- https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/CocoaFundamentals/CommunicatingWithObjects/CommunicateWithObjects.html
- https://apple.stackexchange.com/questions/159195/run-a-program-script-when-the-screen-is-locked-or-unlocked
- https://github.com/apple-oss-distributions/objc4/blob/objc4-818.2/runtime/objc-exception.h
- local: ~/.cargo/registry/src/*/objc2-{app-kit,foundation,local-authentication}/src/generated/*.rs and objc2-0.6.5/src/{exception.rs,ffi/exception.rs}
