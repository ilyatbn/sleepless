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
  - `gui/` - the tray / menu bar app (separate crate, `sleepless-gui`).
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
  higher-ranked), `RcBlock::new` needs the target type written out, `NSDefaultRunLoopMode`
  is an extern static (unsafe to name), and `LAContext` needs
  `#[link(name = "LocalAuthentication", kind = "framework")]` before
  `AnyClass::get("LAContext")` finds it at all.
- Tests: the store location is an environment variable, so the test helper takes a process
 -wide lock. Without it, two tests switch the directory under each other and an "ask until
  correct" loop never terminates - which looks like a hung test suite, not a broken test.
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

