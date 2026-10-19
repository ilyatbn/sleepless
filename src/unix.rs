// Helpers shared by the macOS and Linux backends.
//
// Everything here shells out to the standard system tools (caffeinate,
// pmset, systemd-inhibit, gdbus, xset...) instead of linking against
// platform libraries, so the binary has no runtime dependencies beyond
// what the OS already ships.
//
// Restoring state is delegated to small helper processes that watch our
// pid and undo their change as soon as we exit - including when we're
// killed or crash - so nothing is ever left behind.

use std::env;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

extern "C" {
    fn geteuid() -> u32;
    fn isatty(fd: i32) -> i32;
}

pub fn is_root() -> bool {
    unsafe { geteuid() == 0 }
}

pub fn has_tty() -> bool {
    unsafe { isatty(0) == 1 }
}

pub fn debug_log(msg: &str) {
    eprintln!("[sleepless] {msg}");
}

pub fn init_logging(_debug: bool) {}

/// Prints an error and exits. Always shown, debug mode or not.
pub fn fail(msg: &str) -> ! {
    eprintln!("Sleepless: {msg}");
    std::process::exit(1);
}

pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

pub fn has_command(name: &str) -> bool {
    find_in_path(name).is_some()
}

/// Runs a command to completion, capturing its output. Returns None if it
/// couldn't be started or exited with a failure status.
pub fn run_output(program: &str, args: &[&str]) -> Option<String> {
    log!("$ {program} {}", args.join(" "));
    let out: Output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| log!("  could not run {program}: {e}"))
        .ok()?;
    if !out.status.success() {
        log!(
            "  {program} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Runs a command to completion, returning whether it succeeded.
pub fn run_ok(program: &str, args: &[&str]) -> bool {
    run_output(program, args).is_some()
}

/// Shell snippet that blocks for as long as this process is alive.
pub fn wait_for_our_exit_sh() -> String {
    format!(
        "while kill -0 {} 2>/dev/null; do sleep 1; done",
        std::process::id()
    )
}

fn quiet_unless_debug(cmd: &mut Command) {
    if !crate::DEBUG.load(std::sync::atomic::Ordering::Relaxed) {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
}

/// Starts a long-running helper as the current user.
pub fn spawn(argv: &[String]) -> std::io::Result<Child> {
    log!("Spawning: {}", argv.join(" "));
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).stdin(Stdio::null());
    quiet_unless_debug(&mut cmd);
    cmd.spawn()
}

/// Starts a long-running helper as root, asking for a password if we
/// aren't root already:
///   - from a terminal: sudo (password prompt in the terminal)
///   - macOS, no terminal (e.g. launched as an .app): the standard
///     "Sleepless wants to make changes" admin dialog via osascript
///   - Linux, no terminal: pkexec (graphical polkit prompt)
pub fn spawn_as_root(argv: &[String]) -> std::io::Result<Child> {
    if is_root() {
        return spawn(argv);
    }
    if has_tty() && has_command("sudo") {
        // Authenticate up-front in the foreground so the password prompt
        // isn't fighting a background process for the terminal.
        log!("Asking for your password via sudo...");
        let status = Command::new("sudo")
            .args(["-v", "-p", "Sleepless needs your password for --asroot: "])
            .status()?;
        if !status.success() {
            fail("sudo authentication failed; --asroot needs administrator rights");
        }
        let mut full = vec!["sudo".to_string(), "-n".to_string(), "--".to_string()];
        full.extend_from_slice(argv);
        return spawn(&full);
    }
    if cfg!(target_os = "macos") {
        let script = argv
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        let apple = format!(
            "do shell script \"{}\" with prompt \"Sleepless needs administrator rights to keep running with the lid closed.\" with administrator privileges",
            script.replace('\\', "\\\\").replace('"', "\\\"")
        );
        return spawn(&["osascript".to_string(), "-e".to_string(), apple]);
    }
    if has_command("pkexec") {
        let mut full = vec!["pkexec".to_string()];
        full.extend_from_slice(argv);
        return spawn(&full);
    }
    fail("--asroot needs root: run it from a terminal (for sudo) or install pkexec");
}

pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_=./:,@%+".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Polls `check` every 250ms until it returns true or `timeout` passes.
/// Bails out early if `child` dies, since then it'll never come true.
pub fn wait_until(child: &mut Child, timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if check() {
            return true;
        }
        if let Ok(Some(status)) = child.try_wait() {
            log!("Helper exited early ({status})");
            return check();
        }
        thread::sleep(Duration::from_millis(250));
    }
    false
}

/// Blocks until the user comes back, judged by an idle-time counter that
/// resets on any keyboard/mouse input. The unit doesn't matter (seconds,
/// milliseconds...) - only that it grows while idle and drops on input.
/// Returns false if the counter can't be read at all.
pub fn wait_for_activity(mut idle: impl FnMut() -> Option<f64>) -> bool {
    // Let the keypress / click that launched us settle first.
    thread::sleep(Duration::from_millis(1500));
    let Some(mut prev) = idle() else {
        return false;
    };
    log!("Waiting for keyboard / mouse activity (idle counter at {prev})...");
    let mut ticks = 0u32;
    loop {
        thread::sleep(Duration::from_secs(1));
        ticks += 1;
        if ticks.is_multiple_of(30) {
            log!("...still waiting ({ticks}s elapsed)");
        }
        match idle() {
            Some(now) if now < prev => {
                log!("Activity detected (idle counter dropped from {prev} to {now}).");
                return true;
            }
            Some(now) => prev = now,
            None => log!("Could not read the idle counter this time, retrying."),
        }
    }
}
