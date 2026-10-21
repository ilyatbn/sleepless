// Turning the platform's own prompts into a stored credential:
// `sleepless --set-password` and friends. The prompts themselves live in the
// platform backends (a dialog on Windows, the terminal on macOS, an askpass
// tool or the terminal on Linux); everything decided here is shared.
//
// Every step is logged (src/logging.rs) because the usual way to reach these
// commands is the tray / menu bar, where nothing is printed. What's typed is
// never logged - only how long it was.

use crate::lock::{self, Secret};
use crate::platform;
use crate::store;
use crate::Command;
use std::io;

/// Runs one of the credential commands and returns the process exit code.
pub fn run(command: Command, opts: &crate::Options) -> i32 {
    match command {
        Command::SetPassword => set_password(opts),
        Command::ClearPassword => clear_password(),
        // Quiet: the tray app only looks at the exit code.
        Command::HasPassword => i32::from(!lock::is_enrolled()),
        Command::SetPasskey => set_passkey(opts),
        Command::ClearPasskey => clear_passkey(),
        Command::Run => 0,
    }
}

/// One line from stdin, for `--stdin`: the menu-bar app asks the user and hands
/// the answer over a pipe. Never argv (visible in the process list) and never an
/// environment variable (visible in another process's environment).
fn read_stdin_line() -> Option<Secret> {
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        // Trimmed in place and then moved, rather than sliced into a second
        // copy: the buffer that held the password is the one that gets zeroised.
        Ok(_) => {
            while line.ends_with(['\n', '\r']) {
                line.pop();
            }
            Some(Secret::take(line))
        }
    }
}

fn set_password(opts: &crate::Options) -> i32 {
    log!(
        "Enrolment started; credential store: {}.",
        store::location()
    );
    // `--stdin` is handled before anything is asked. Whoever used it has already
    // done the asking - the menu-bar app asks twice and confirms the replacement
    // itself - and on macOS this process could ask nothing at all anyway: a
    // background child cannot take focus from the app the user is typing in.
    if opts.stdin {
        let Some(password) = read_stdin_line() else {
            log!("--stdin was asked for but nothing came in on stdin.");
            platform::report("Nothing arrived on stdin, so no password was set.");
            return 1;
        };
        if password.is_empty() {
            log!("An empty password arrived on stdin.");
            platform::report("That password was empty, so nothing was saved.");
            return 1;
        }
        log!("Password arrived on stdin: {} characters.", password.len());
        return match lock::enroll_password(&password) {
            Ok(()) => {
                log!("Password enrolled in {}.", store::location());
                platform::report(
                    "Coming back from the black screen now needs this password.\n\
                     (Only a salted hash is stored, never the password itself.)",
                );
                // The passkey offer belongs to whoever asked, since that is the
                // process the system will let show a Touch ID prompt in.
                log!("Not offering a passkey from this process; the caller does that.");
                0
            }
            Err(e) => {
                log!("Could not write the credential: {e}");
                platform::report(&format!("Could not save the password: {e}"));
                1
            }
        };
    }

    if lock::has_password() {
        log!("A password is already enrolled; asking whether to replace it.");
        if !platform::ask_yes_no(
            "Sleepless",
            "You already have a password to come back. Replace it?",
        ) {
            log!("Replacement declined.");
            platform::report("Nothing changed.");
            return 0;
        }
    }
    log!("Asking for the new password in this platform's own way.");
    let Some(first) =
        platform::prompt_secret_interactive("Sleepless - new password", "Choose a password:")
    else {
        log!("No password came back from the prompt (cancelled, or no way to ask).");
        platform::report("Cancelled: no password was set.");
        return 1;
    };
    if first.is_empty() {
        log!("The prompt returned an empty password.");
        platform::report("That password was empty, so nothing was saved.");
        return 1;
    }
    log!("First entry got: {} characters.", first.len());

    let again = if first.len() < lock::WEAK_PASSWORD_LEN {
        "That's short. Type it again to confirm:"
    } else {
        "Type it again to confirm:"
    };
    log!("Asking for the confirmation.");
    let Some(second) = platform::prompt_secret_interactive("Sleepless - new password", again)
    else {
        log!("Confirmation prompt cancelled.");
        platform::report("Cancelled: no password was set.");
        return 1;
    };
    if first != second {
        log!(
            "The two entries differ ({} vs {} characters).",
            first.len(),
            second.len()
        );
        platform::report("The two entries didn't match, so nothing was saved.");
        return 1;
    }

    match lock::enroll_password(&first) {
        Ok(()) => {
            log!("Password enrolled in {}.", store::location());
            platform::report(
                "Coming back from the black screen now needs this password.\n\
                 (Only a salted hash is stored, never the password itself.)",
            );
            offer_passkey();
            0
        }
        Err(e) => {
            log!("Could not write the credential: {e}");
            platform::report(&format!("Could not save the password: {e}"));
            1
        }
    }
}

fn clear_password() -> i32 {
    if !lock::is_enrolled() {
        log!("Nothing enrolled to remove.");
        platform::report("No password is set.");
        return 0;
    }
    match lock::remove() {
        Ok(()) => {
            log!("Password removed from {}.", store::location());
            platform::report("Coming back from the black screen is free again.");
            0
        }
        Err(e) => {
            log!("Could not remove the credential: {e}");
            platform::report(&format!("Could not remove the password: {e}"));
            1
        }
    }
}

/// A passkey is an addition, never a replacement: the password is what makes
/// the gate work, so this is offered once that exists.
fn offer_passkey() {
    let Some(label) = platform::passkey_supported() else {
        log!("No passkey available on this platform.");
        return;
    };
    if lock::has_passkey() {
        log!("A passkey is already enrolled; not offering one again.");
        return;
    }
    log!("Platform passkey available: {label}. Asking whether to add it.");
    if !platform::ask_yes_no("Sleepless", &format!("Also allow {label} to come back?")) {
        log!("Passkey declined by the user.");
        return;
    }
    match platform::passkey_enroll().and_then(|kind| {
        lock::enroll_passkey(kind)
            .map(|()| kind)
            .map_err(|e| e.to_string())
    }) {
        Ok(kind) => {
            log!("Passkey ({kind}) enrolled in {}.", store::location());
            platform::report(&format!("{label} can now unlock the screen as well."));
        }
        Err(reason) => {
            // The password is set either way, so this only reports what didn't
            // happen; it never undoes the enrolment above.
            log!("Passkey enrolment did not complete: {reason}");
            platform::report(&format!("No passkey: {reason}."));
        }
    }
}

fn set_passkey(opts: &crate::Options) -> i32 {
    // The menu-bar app can ask Touch ID where this process can't; when it has,
    // all that's left here is to record that it may be used.
    if opts.confirmed {
        if !lock::has_password() {
            log!("--confirmed passkey refused: no password enrolled yet.");
            return 1;
        }
        let Some(kind) = platform::passkey_supported() else {
            log!("--confirmed passkey refused: no passkey available on this platform.");
            return 1;
        };
        return match lock::enroll_passkey(kind) {
            Ok(()) => {
                log!(
                    "Passkey ({kind}) recorded in {} after the caller's confirmation.",
                    store::location()
                );
                0
            }
            Err(e) => {
                log!("Could not record the passkey: {e}");
                1
            }
        };
    }

    if !lock::has_password() {
        log!("Passkey refused: no password enrolled yet.");
        platform::report(
            "Set a password first: the passkey is an addition to it, \
             so there's always a way back if the sensor can't help.",
        );
        return 1;
    }
    match platform::passkey_enroll().and_then(|kind| {
        lock::enroll_passkey(kind)
            .map(|()| kind)
            .map_err(|e| e.to_string())
    }) {
        Ok(kind) => {
            log!("Passkey ({kind}) enrolled in {}.", store::location());
            platform::report("Coming back can now use the passkey as well as the password.");
            0
        }
        Err(reason) => {
            log!("Passkey enrolment did not complete: {reason}");
            platform::report(&format!("No passkey: {reason}."));
            1
        }
    }
}

fn clear_passkey() -> i32 {
    if !lock::has_passkey() {
        log!("No passkey enrolled to remove.");
        platform::report("No passkey is enrolled.");
        return 0;
    }
    match lock::remove_passkey() {
        Ok(()) => {
            log!("Passkey removed from {}.", store::location());
            platform::report("Coming back now needs the password only.");
            0
        }
        Err(e) => {
            log!("Could not remove the passkey: {e}");
            platform::report(&format!("Could not remove the passkey: {e}"));
            1
        }
    }
}
