# Sleepless Build and Debug Notes

## Project Overview
- Repository: `c:\Users\ilya\code\sleepless`
- Main source: `src/main.rs`
- Build system: Rust / Cargo
- Target platform: Windows
- Dependencies:
  - `windows-sys = { version = "0.59", features = ["Win32_Foundation", "Win32_System_Console", "Win32_System_Power", "Win32_System_Registry", "Win32_UI_WindowsAndMessaging"] }`

## Key build command
- `cargo check`
- `cargo build --release`

## GitHub Actions failure encountered
- Error: `E0277: *mut c_void cannot be shared between threads safely`
- Error: `E0277: *mut c_void cannot be sent between threads safely`
- Error: `E0308: mismatched types` for `WriteConsoleW`

## Root cause
- `HANDLE` in `windows-sys` is a raw pointer type (`*mut c_void`)
- `static` values require types that implement `Sync`
- `OnceLock<HANDLE>` therefore fails to compile
- `WriteConsoleW` expects `*const u16`, but the code was passing `*const c_void`

## Fix applied
1. Use `OnceLock<isize>` for `CONSOLE_OUT` instead of `OnceLock<HANDLE>`
2. Store the console handle as an integer:
   - `CONSOLE_OUT.set(handle as isize)`
3. Recover the handle before writing:
   - `let handle: HANDLE = addr as HANDLE;`
4. Use `text.as_ptr()` directly for `WriteConsoleW` so the pointer type is `*const u16`

## Relevant code locations
- `src/main.rs`
  - `static CONSOLE_OUT: OnceLock<isize> = OnceLock::new();`
  - `init_console()` stores `handle as isize`
  - `debug_log()` casts the stored `isize` back to `HANDLE`
  - `WriteConsoleW(handle, text.as_ptr(), text.len() as u32, &mut written, ptr::null_mut());`

## Notes for next time
- If the build fails in GitHub Actions, check whether the failing source matches the latest commit.
- On Windows-specific Rust code, prefer storing raw handles as integer-sized values in statics when thread-safety traits are required.
- For Windows API calls in Rust, verify the pointer argument types from the generated `windows-sys` signatures.

## Recommended checklist before opening a PR
- `cargo check`
- `cargo fmt`
- Confirm `src/main.rs` contains the updated `CONSOLE_OUT` handling
- Make sure GitHub Actions target is Windows if the crate uses `windows-sys` APIs
