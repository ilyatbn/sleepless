// Sleepless
//
// Turns the display off while keeping the machine awake. Once you come
// back (move the mouse / press a key), it puts everything back the way it
// was and exits.
//
// Usage:
//   sleepless [--debug] [--asroot] [--lock]
//
//   --debug    log everything it does (Windows: to the calling cmd window,
//              macOS/Linux: to stderr). `/debug` also works on Windows.
//   --asroot   also keep running with the lid closed: nothing disconnects
//              and the machine doesn't suspend until you come back.
//              macOS/Linux ask for your password (sudo / admin prompt)
//              for the small root helper this needs.
//   --lock     lock the session too: the black screen only goes away
//              after you enter your login password on the OS lock screen.
//
// Platform specifics live in windows.rs, macos.rs and linux.rs.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::env;
use std::sync::atomic::{AtomicBool, Ordering};

static DEBUG: AtomicBool = AtomicBool::new(false);

macro_rules! log {
    ($($arg:tt)*) => {{
        if $crate::DEBUG.load(std::sync::atomic::Ordering::Relaxed) {
            $crate::platform::debug_log(&format!($($arg)*));
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

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub debug: bool,
    pub asroot: bool,
    pub lock: bool,
}

const USAGE: &str = "\
Usage: sleepless [--debug] [--asroot] [--lock]

Turns the display off while keeping the machine awake, then restores
everything once you come back (move the mouse / press a key).

  --debug    log everything that happens
  --asroot   also keep running with the lid closed (asks for your password
             on macOS/Linux)
  --lock     require your login password before releasing the black screen
  --help     show this message";

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
            "lock" => opts.lock = true,
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
    let opts = parse_args();
    platform::init_logging(opts.debug);
    DEBUG.store(opts.debug, Ordering::Relaxed);
    log!(
        "Sleepless starting (pid {}), options: {opts:?}",
        std::process::id()
    );
    platform::run(opts);
    log!("Done. Exiting.");
}
