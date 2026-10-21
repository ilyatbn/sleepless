// Turning the platform's own prompts into a stored credential:
// `sleepless --set-password` and friends. The prompts themselves live in the
// platform backends (a dialog on Windows, an NSAlert on macOS, an askpass
// tool or the terminal on Linux); everything decided here is shared.

use crate::lock;
use crate::platform;
use crate::store;
use crate::Command;

/// Runs one of the credential commands and returns the process exit code.
pub fn run(command: Command) -> i32 {
    match command {
        Command::SetPassword => set_password(),
        Command::ClearPassword => clear_password(),
        // Quiet: the tray app only looks at the exit code.
        Command::HasPassword => i32::from(!lock::is_enrolled()),
        Command::SetPasskey => set_passkey(),
        Command::ClearPasskey => clear_passkey(),
        Command::Run => 0,
    }
}

fn set_password() -> i32 {
    if lock::has_password()
        && !platform::ask_yes_no(
            "Sleepless",
            "You already have a password to come back. Replace it?",
        )
    {
        platform::report("Nothing changed.");
        return 0;
    }

    let Some(first) =
        platform::prompt_secret_interactive("Sleepless - new password", "Choose a password:")
    else {
        platform::report("Cancelled: no password was set.");
        return 1;
    };
    if first.is_empty() {
        platform::report("That password was empty, so nothing was saved.");
        return 1;
    }

    let again = if first.len() < lock::WEAK_PASSWORD_LEN {
        "That's short. Type it again to confirm:"
    } else {
        "Type it again to confirm:"
    };
    let Some(second) = platform::prompt_secret_interactive("Sleepless - new password", again)
    else {
        platform::report("Cancelled: no password was set.");
        return 1;
    };
    if first != second {
        platform::report("The two entries didn't match, so nothing was saved.");
        return 1;
    }

    match lock::enroll_password(&first) {
        Ok(()) => {
            log!("Password enrolled in {}", store::location());
            platform::report(
                "Coming back from the black screen now needs this password.\n\
                 (Only a salted hash is stored, never the password itself.)",
            );
            offer_passkey();
            0
        }
        Err(e) => {
            platform::report(&format!("Could not save the password: {e}"));
            1
        }
    }
}

fn clear_password() -> i32 {
    if !lock::is_enrolled() {
        platform::report("No password is set.");
        return 0;
    }
    match lock::remove() {
        Ok(()) => {
            log!("Password removed from {}", store::location());
            platform::report("Coming back from the black screen is free again.");
            0
        }
        Err(e) => {
            platform::report(&format!("Could not remove the password: {e}"));
            1
        }
    }
}

/// A passkey is an addition, never a replacement: the password is what makes
/// the gate work, so this is offered once that exists.
fn offer_passkey() {
    let Some(label) = platform::passkey_supported() else {
        return;
    };
    if lock::has_passkey()
        || !platform::ask_yes_no("Sleepless", &format!("Also allow {label} to come back?"))
    {
        return;
    }
    match platform::passkey_enroll().and_then(|kind| {
        lock::enroll_passkey(kind)
            .map(|()| kind)
            .map_err(|e| e.to_string())
    }) {
        Ok(kind) => {
            log!("Passkey ({kind}) enrolled in {}", store::location());
            platform::report(&format!("{label} can now unlock the screen as well."));
        }
        Err(lock_or_platform_error) => {
            // The password is set either way, so this only reports what didn't
            // happen; it never undoes the enrolment above.
            platform::report(&format!("No passkey: {lock_or_platform_error}."));
        }
    }
}

fn set_passkey() -> i32 {
    if !lock::has_password() {
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
            log!("Passkey ({kind}) enrolled in {}", store::location());
            platform::report("Coming back can now use the passkey as well as the password.");
            0
        }
        Err(e) => {
            platform::report(&format!("No passkey: {e}."));
            1
        }
    }
}

fn clear_passkey() -> i32 {
    if !lock::has_passkey() {
        platform::report("No passkey is enrolled.");
        return 0;
    }
    match lock::remove_passkey() {
        Ok(()) => {
            platform::report("Coming back now needs the password only.");
            0
        }
        Err(e) => {
            platform::report(&format!("Could not remove the passkey: {e}"));
            1
        }
    }
}
