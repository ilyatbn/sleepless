// Linux backend
//
//   1. `systemd-inhibit --mode=block --what=sleep:idle` (plus
//      `handle-lid-switch` with --asroot, run as root) holds a logind
//      inhibitor lock. The command it wraps just waits for our pid to
//      exit, so the lock is released the moment we do.
//   2. Turn the display off with whatever the session supports:
//        - GNOME (X11 or Wayland): Mutter's DisplayConfig PowerSaveMode
//        - KDE Plasma (X11 or Wayland): kscreen-doctor --dpms
//        - any other X11 session: xset dpms force off
//   3. Wait for keyboard/mouse activity, turn the display back on, exit.

use std::env;
use std::thread;
use std::time::Duration;

use crate::unix::{
    self, fail, has_command, run_ok, run_output, spawn, spawn_as_root, wait_for_our_exit_sh,
};

pub use crate::unix::{debug_log, init_logging};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Backend {
    Gnome,
    Kde,
    X11,
    None,
}

fn detect_backend() -> Backend {
    let desktop = env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let session = env::var("XDG_SESSION_TYPE")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let wayland = session == "wayland" || env::var_os("WAYLAND_DISPLAY").is_some();
    log!("XDG_CURRENT_DESKTOP={desktop:?} XDG_SESSION_TYPE={session:?} wayland={wayland}");

    if desktop.contains("gnome") && has_command("gdbus") {
        return Backend::Gnome;
    }
    if desktop.contains("kde") && has_command("kscreen-doctor") {
        return Backend::Kde;
    }
    // On Wayland, xset only talks to XWayland and can't touch the real
    // outputs, so it's only an option on a genuine X11 session.
    if !wayland && env::var_os("DISPLAY").is_some() && has_command("xset") {
        return Backend::X11;
    }
    Backend::None
}

// ---------------------------------------------------------------------
// D-Bus via the command line (gdbus ships with GLib, busctl with systemd)
// ---------------------------------------------------------------------

fn dbus_call(dest: &str, path: &str, iface: &str, method: &str) -> Option<String> {
    if has_command("gdbus") {
        let m = format!("{iface}.{method}");
        run_output(
            "gdbus",
            &[
                "call",
                "--session",
                "--dest",
                dest,
                "--object-path",
                path,
                "--method",
                &m,
            ],
        )
    } else {
        run_output("busctl", &["--user", "call", dest, path, iface, method])
    }
}

/// First unsigned integer in a D-Bus reply like "(uint64 1234,)" / "u 12".
fn first_number(reply: &str) -> Option<f64> {
    // Drop gdbus' type tags first so their digits aren't mistaken for the value.
    let reply = reply.replace("uint64", "").replace("uint32", "");
    reply
        .split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
}

// ---------------------------------------------------------------------
// GNOME
// ---------------------------------------------------------------------

const MUTTER_DC: &str = "org.gnome.Mutter.DisplayConfig";
const MUTTER_DC_PATH: &str = "/org/gnome/Mutter/DisplayConfig";

fn gnome_power_save(mode: i32) -> bool {
    // 0 = on, 3 = off
    let value = format!("<int32 {mode}>");
    run_ok(
        "gdbus",
        &[
            "call",
            "--session",
            "--dest",
            MUTTER_DC,
            "--object-path",
            MUTTER_DC_PATH,
            "--method",
            "org.freedesktop.DBus.Properties.Set",
            MUTTER_DC,
            "PowerSaveMode",
            &value,
        ],
    )
}

fn gnome_idle() -> Option<f64> {
    let reply = dbus_call(
        "org.gnome.Mutter.IdleMonitor",
        "/org/gnome/Mutter/IdleMonitor/Core",
        "org.gnome.Mutter.IdleMonitor",
        "GetIdletime",
    )?;
    first_number(&reply)
}

// ---------------------------------------------------------------------
// KDE / generic freedesktop idle time
// ---------------------------------------------------------------------

fn freedesktop_idle() -> Option<f64> {
    let reply = dbus_call(
        "org.freedesktop.ScreenSaver",
        "/org/freedesktop/ScreenSaver",
        "org.freedesktop.ScreenSaver",
        "GetSessionIdleTime",
    )?;
    first_number(&reply)
}

// ---------------------------------------------------------------------
// X11
// ---------------------------------------------------------------------

fn xset_query() -> Option<String> {
    run_output("xset", &["q"])
}

fn x11_monitor_on() -> Option<bool> {
    xset_query().map(|q| q.contains("Monitor is On"))
}

// ---------------------------------------------------------------------

fn start_inhibitor(opts: crate::Options) {
    let Some(inhibit) = ["systemd-inhibit", "elogind-inhibit"]
        .into_iter()
        .find(|c| has_command(c))
    else {
        if opts.asroot {
            fail("--asroot needs systemd-inhibit (or elogind-inhibit) to block the lid switch");
        }
        eprintln!("Sleepless: systemd-inhibit not found; the display will turn off but sleep isn't blocked");
        return;
    };
    let what = if opts.asroot {
        "sleep:idle:handle-lid-switch"
    } else {
        "sleep:idle"
    };
    let argv = [
        inhibit.to_string(),
        format!("--what={what}"),
        "--who=Sleepless".to_string(),
        "--why=Display is off, keeping the machine awake".to_string(),
        "--mode=block".to_string(),
        "/bin/sh".to_string(),
        "-c".to_string(),
        wait_for_our_exit_sh(),
    ];
    let mut child = if opts.asroot {
        spawn_as_root(&argv)
    } else {
        spawn(&argv)
    }
    .unwrap_or_else(|e| fail(&format!("could not start {inhibit}: {e}")));

    // Confirm the lock is actually held (and give a pkexec password dialog
    // time to be answered).
    let timeout = Duration::from_secs(if opts.asroot { 120 } else { 5 });
    let held = unix::wait_until(&mut child, timeout, || {
        run_output(inhibit, &["--list", "--no-pager"]).is_some_and(|l| l.contains("Sleepless"))
    });
    if !held {
        let msg = format!("{inhibit} did not take the inhibitor lock");
        if opts.asroot {
            fail(&format!("{msg} (password prompt cancelled?)"));
        }
        eprintln!("Sleepless: {msg}; the display will turn off but sleep isn't blocked");
        return;
    }
    log!("Inhibitor lock held ({what}); released automatically when Sleepless exits.");
}

pub fn run(opts: crate::Options) {
    if unix::is_root() && env::var_os("SUDO_USER").is_some() {
        eprintln!(
            "Sleepless: running under sudo usually can't reach your desktop session to turn the display off.\n\
             Run it as your normal user instead; --asroot asks for the password itself."
        );
    }

    // --- 1. Keep the system awake (and ignore the lid with --asroot) ---
    start_inhibitor(opts);

    // --- 2. Display off ---
    let backend = detect_backend();
    log!("Display backend: {backend:?}");
    let x11_dpms_was_enabled =
        backend == Backend::X11 && xset_query().is_some_and(|q| q.contains("DPMS is Enabled"));

    let turned_off = match backend {
        Backend::Gnome => gnome_power_save(3),
        Backend::Kde => run_ok("kscreen-doctor", &["--dpms", "off"]),
        Backend::X11 => run_ok("xset", &["+dpms"]) && run_ok("xset", &["dpms", "force", "off"]),
        Backend::None => false,
    };

    // --- 3. Wait for the user to come back ---
    let returned = match backend {
        _ if !turned_off => false,
        Backend::Gnome => unix::wait_for_activity(gnome_idle),
        Backend::Kde => unix::wait_for_activity(freedesktop_idle),
        Backend::X11 => {
            // The X server turns DPMS back on by itself on input, the
            // same way the Windows screensaver ends.
            let mut waited = 0;
            while x11_monitor_on() == Some(true) && waited < 50 {
                thread::sleep(Duration::from_millis(100));
                waited += 1;
            }
            log!("Waiting for the monitor to come back on (move the mouse / press a key)...");
            while x11_monitor_on() == Some(false) {
                thread::sleep(Duration::from_secs(1));
            }
            true
        }
        Backend::None => false,
    };

    if !returned {
        // Couldn't turn the screen off / watch for input: still useful as
        // a "keep awake" until the user stops it.
        eprintln!(
            "Sleepless: couldn't control the display on this desktop ({backend:?}).\n\
             Keeping the machine awake until you press Enter or Ctrl+C."
        );
        let mut line = String::new();
        if unix::has_tty() {
            let _ = std::io::stdin().read_line(&mut line);
        } else {
            loop {
                thread::sleep(Duration::from_secs(3600));
            }
        }
    }

    // --- 3.5 Password gate ---
    // The display is back and the desktop is visible: hold the session open
    // until the password is typed. See the notes in unlock_gate().
    if returned && lock::is_enrolled() {
        unlock_gate();
    }

    // --- 4. Restore ---
    match backend {
        Backend::Gnome if turned_off => {
            gnome_power_save(0);
        }
        Backend::Kde if turned_off => {
            run_ok("kscreen-doctor", &["--dpms", "on"]);
        }
        Backend::X11 if !x11_dpms_was_enabled => {
            run_ok("xset", &["-dpms"]);
        }
        _ => {}
    }
    log!("Display restored - releasing the inhibitor lock.");
}

/// Asks for the password until it's right (the shared loop lives in
/// src/lock.rs).
///
/// On Linux this is a prompt, not a covered screen: the compositor decides
/// when DPMS comes back, so the desktop is already visible by the time we can
/// ask. It's a deterrent against whoever is sitting at the keyboard, and
/// cancelling keeps you locked out rather than letting you through. Killing
/// Sleepless is always a way out (the inhibitor lock is held by a helper that
/// goes away with the process), which is deliberate: the OS lock screen is the
/// thing to use when a real boundary is needed.
fn unlock_gate() {
    log!("A password is enrolled: asking before restoring the power settings.");
    lock::unlock_loop(|message| prompt_secret("Sleepless", message));
    log!("Password accepted - restoring the power settings.");
}

// ---------------------------------------------------------------------
// Prompts: where there's a terminal attached we talk there, because that's
// what someone running `sleepless --set-password` from a shell expects.
// Without one (started from the tray or the desktop) we use an askpass
// dialog. Shelling out to zenity / kdialog / stty keeps the binary free of
// GUI library dependencies, same as everywhere else in this backend.
// ---------------------------------------------------------------------

use crate::lock::{self, Secret};
use std::io::{self, Write};
use std::process::Command;

#[derive(Clone, Copy, PartialEq)]
enum Prompt {
    Zenity,
    Kdialog,
    Terminal,
}

fn has_desktop_session() -> bool {
    env::var_os("DISPLAY").is_some() || env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Where the *lock screen* should ask: a dialog, since the display has just
/// come back and nobody is necessarily in a terminal.
fn prompt_for_gate() -> Prompt {
    if has_desktop_session() && has_command("zenity") {
        Prompt::Zenity
    } else if has_desktop_session() && has_command("kdialog") {
        Prompt::Kdialog
    } else {
        Prompt::Terminal
    }
}

/// Where an interactive command (--set-password and friends) should ask.
fn prompt_for_command() -> Prompt {
    if unix::has_tty() {
        Prompt::Terminal
    } else {
        prompt_for_gate()
    }
}

/// Asks for a secret, echoing nothing. None if the user cancelled.
/// Used by the lock screen.
pub fn prompt_secret(title: &str, message: &str) -> Option<Secret> {
    ask_secret(prompt_for_gate(), title, message)
}

/// Same, but in the terminal when there is one. Used by the enrolment
/// commands.
pub fn prompt_secret_interactive(title: &str, message: &str) -> Option<Secret> {
    ask_secret(prompt_for_command(), title, message)
}

fn ask_secret(where_: Prompt, title: &str, message: &str) -> Option<Secret> {
    match where_ {
        Prompt::Zenity => run_output(
            "zenity",
            &[
                "--password",
                &format!("--title={title}"),
                &format!("--text={message}"),
            ],
        )
        .map(|out| Secret::new(out.trim_end_matches(['\n', '\r']))),
        Prompt::Kdialog => run_output("kdialog", &["--title", title, "--password", message])
            .map(|out| Secret::new(out.trim_end_matches(['\n', '\r']))),
        Prompt::Terminal => ask_in_terminal(message),
    }
}

fn ask_in_terminal(message: &str) -> Option<Secret> {
    if !unix::has_tty() {
        eprintln!("Sleepless: no terminal to ask for a password in.");
        return None;
    }
    // Turn the terminal's echo off through stty, so the password doesn't
    // appear on screen, then put it back however it was.
    let echo_off = Command::new("stty")
        .args(["-echo"])
        .status()
        .is_ok_and(|s| s.success());
    eprint!("{message} ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    if echo_off {
        let _ = Command::new("stty").args(["echo"]).status();
    }
    eprintln!();
    match read {
        Ok(n) if n > 0 => Some(Secret::new(line.trim_end_matches(['\n', '\r']))),
        _ => None,
    }
}

/// One-line result message for the credential commands.
pub fn report(message: &str) {
    if unix::has_tty() {
        for line in message.lines() {
            println!("Sleepless: {line}");
        }
    } else {
        notify("Sleepless", message);
    }
}

pub fn notify(title: &str, message: &str) {
    match prompt_for_command() {
        Prompt::Zenity => {
            let _ = run_output(
                "zenity",
                &[
                    "--info",
                    &format!("--title={title}"),
                    &format!("--text={message}"),
                ],
            );
        }
        Prompt::Kdialog => {
            let _ = run_output("kdialog", &["--title", title, "--passive-popup", message]);
        }
        Prompt::Terminal => {
            for line in message.lines() {
                println!("Sleepless: {line}");
            }
        }
    }
}

pub fn ask_yes_no(title: &str, message: &str) -> bool {
    match prompt_for_command() {
        Prompt::Zenity => run_output(
            "zenity",
            &[
                "--question",
                &format!("--title={title}"),
                &format!("--text={message}"),
            ],
        )
        .is_some(),
        Prompt::Kdialog => run_output("kdialog", &["--title", title, "--yesno", message]).is_some(),
        Prompt::Terminal => {
            if !unix::has_tty() {
                return false;
            }
            eprint!("{message} [y/N] ");
            let _ = io::stderr().flush();
            let mut answer = String::new();
            let _ = std::io::stdin().read_line(&mut answer);
            matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        }
    }
}

/// No passkey on Linux: there's no portable biometric prompt that isn't the
/// desktop's own lock screen, so the password is the whole credential.
pub fn passkey_supported() -> Option<&'static str> {
    None
}

pub fn passkey_enroll() -> Result<&'static str, String> {
    Err("Sleepless has no passkey support on Linux, so use the password".to_string())
}
