// Sleepless
//
// Turns the display off while keeping the machine awake. Once you come
// back (move the mouse / press a key), it puts everything back the way it
// was and exits.
//
// With a password enrolled (--set-password), coming back asks for it first.
// Sleepless checks it itself, never through the OS lock screen, so the VPN
// and the rest of the session stays up.
//
// Usage:
//   sleepless [--debug] [--asroot]
//
//   --debug           log everything it does (Windows: to the calling cmd
//                     window, macOS/Linux: to stderr). `/debug` also works
//                     on Windows.
//   --asroot          also keep running with the lid closed: nothing
//                     disconnects and the machine doesn't suspend until you
//                     come back. macOS/Linux ask for your password (sudo /
//                     admin prompt) for the small root helper this needs.
//   --set-password    require a password to come back
//   --clear-password  stop requiring one
//   --has-password    exit code only: 0 if one is enrolled, 1 if not
//
// Platform specifics live in windows.rs, macos.rs and linux.rs.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::env;
use std::sync::atomic::{AtomicBool, Ordering};

static DEBUG: AtomicBool = AtomicBool::new(false);

macro_rules! log {
    ($($arg:tt)*) => {{
        // Always into the log file (src/logging.rs), and into the console when
        // --debug was passed: a session started from the tray / menu bar has no
        // console at all, so a file is the only way a report can be more useful
        // than a repro request.
        let message = format!($($arg)*);
        $crate::logging::note(&message);
        if $crate::DEBUG.load(std::sync::atomic::Ordering::Relaxed) {
            $crate::platform::debug_log(&message);
        }
    }};
}

#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;

#[cfg(unix)]
mod unix;

mod enroll;
mod lock;
mod logging;
mod store;

/// What we were asked to do. Everything except `Run` is about the unlock
/// credential: it finishes without touching the display at all.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Command {
    #[default]
    Run,
    SetPassword,
    ClearPassword,
    HasPassword,
    SetPasskey,
    ClearPasskey,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub debug: bool,
    pub asroot: bool,
    pub command: Command,
}

const USAGE: &str = "\
Usage: sleepless [--debug] [--asroot]

Turns the display off while keeping the machine awake, then restores
everything once you come back (move the mouse / press a key).

  --debug           log everything that happens
  --asroot          also keep running with the lid closed (asks for your
                    password on macOS/Linux)
  --set-password    require a password to come back (asks for it twice)
  --clear-password  stop requiring a password to come back
  --has-password    nothing printed: exit 0 if a password is set, 1 if not
  --set-passkey     also allow Touch ID / Windows Hello to come back
                    (macOS only for now; the password is still required first)
  --clear-passkey   stop accepting the passkey
  --help            show this message";

fn parse_args() -> Options {
    let mut opts = Options::default();
    for arg in env::args().skip(1) {
        // Accept --flag, -flag, /flag (Windows habit) and the em dash that
        // some keyboards / chat apps auto-substitute for "--".
        let name = arg
            .trim_start_matches(['-', '/', '\u{2014}', '\u{2013}'])
            .to_ascii_lowercase();
        match name.as_str() {
            "debug" => opts.debug = true,
            "asroot" => opts.asroot = true,
            "set-password" | "password" | "passwd" | "enroll" => {
                opts.command = Command::SetPassword
            }
            "clear-password" | "remove-password" | "no-password" | "unenroll" => {
                opts.command = Command::ClearPassword
            }
            "has-password" => opts.command = Command::HasPassword,
            "set-passkey" | "passkey" | "touch-id" | "hello" => opts.command = Command::SetPasskey,
            "clear-passkey" | "remove-passkey" => opts.command = Command::ClearPasskey,
            "h" | "help" | "?" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            // macOS passes -psn_0_12345 when an .app is launched by Finder
            // on older releases; ignore it rather than bailing out.
            _ if name.starts_with("psn_") => {}
            _ => {
                eprintln!("Sleepless: unknown argument '{arg}'\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    opts
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let opts = parse_args();
    platform::init_logging(opts.debug);
    DEBUG.store(opts.debug, Ordering::Relaxed);
    log!(
        "Sleepless {} starting (pid {}), args {args:?}, options: {opts:?}; logging to {:?}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        logging::path()
    );
    let code = match opts.command {
        Command::Run => {
            platform::run(opts);
            log!("Done. Exiting.");
            0
        }
        command => {
            let code = enroll::run(command);
            log!("Credential command {command:?} finished with exit code {code}");
            code
        }
    };
    log!("Sleepless exiting with code {code}");
    std::process::exit(code);
}
