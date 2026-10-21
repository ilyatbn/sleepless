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
- [ ] tag `v0.2.2beta1` pushed -> the workflow builds all three platforms and
      publishes the pre-release.
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

## Sources

- https://learn.microsoft.com/windows/win32/api/winreg/nf-winreg-regcreatekeyexw
- https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-createwindowexw
- https://learn.microsoft.com/windows/win32/controls/edit-controls
- https://developer.apple.com/documentation/uikit/uitextfield (macOS counterpart: NSTextField/NSSecureTextField)
- https://developer.apple.com/documentation/localauthentication/lacontext
- https://learn.microsoft.com/windows/win32/api/webauthn/nf-webauthn-webauthnmakecredential
- OWASP password storage guidance (PBKDF2-HMAC-SHA256 work factor):
  https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html
