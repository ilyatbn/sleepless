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

Hand-written `msg_send!` - three, all documented, all with the declaration checked against
the Apple headers / the objc2 generated bindings:

| Call | Apple declaration | Return | Notes |
| --- | --- | --- | --- |
| `[NSDistributedNotificationCenter distributedNotificationCenter]` | `+ (NSDistributedNotificationCenter *)distributedNotificationCenter;` | object pointer, taken with `Retained::retain` | no arguments; only hand-written because objc2-app-kit 0.3 has no bindings for this class |
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
- https://developer.apple.com/documentation/appkit/nsapplication/activate()
