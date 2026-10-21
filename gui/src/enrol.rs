// The credential prompts, on macOS, in *this* process.
//
// They used to live in the command-line child the menu starts, and that can't
// work: since macOS 14 activation is cooperative, and the system does not hand
// focus to a background child process while the user is typing in Terminal. Every
// workaround for that (activation-policy flips, `activate` twice a second, window
// levels above the menu bar) was tried across three betas, and what it produced
// was a dialog that was on screen at the right frame and invisible, with the
// user's focus being dragged back every other tick.
//
// The menu-bar app is different: the user has just clicked its icon, so it is
// allowed to become active, and its dialogs behave like any other app's. So the
// asking happens here, and the answer is passed to the CLI over a pipe
// (`--set-password --stdin`) - never in argv, which the process list shows, and
// never in the environment, which other processes can read.
//
// Everything here is plain AppKit: an NSAlert with an NSSecureTextField as its
// accessory view, its initial first responder set to that field. No timers, no
// levels, no policy changes.

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::sel;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSEventMask,
        NSSecureTextField, NSWindow,
    };
    use objc2_foundation::NSObjectProtocol;
    use objc2_foundation::{
        NSDate, NSDefaultRunLoopMode, NSError, NSPoint, NSRect, NSRunLoop, NSSize, NSString,
    };

    use crate::log;

    /// Where the Touch ID answer lands: 0 = still waiting, 1 = yes, 2 = no.
    /// The reply block is documented as running "on a private queue internal to
    /// the framework", with no guarantee about thread, so the answer has to be
    /// something thread-agnostic - and it may arrive after we gave up waiting,
    /// which a late write to a counter can't confuse.
    static TOUCH_ID_ANSWER: AtomicU8 = AtomicU8::new(0);

    /// How long the system prompt may stay unanswered. Enrolment has to end,
    /// fail or time out; it must never hang.
    const TOUCH_ID_TIMEOUT: Duration = Duration::from_secs(45);

    /// Runs an Objective-C call, turning an exception into a logged failure
    /// instead of an abort. Gated on the crate's `catch-objc-exceptions` feature
    /// for the same reason as the CLI: catching needs an Objective-C compiler for
    /// the target, so only the macOS build turns it on.
    unsafe fn try_objc<T>(f: impl FnOnce() -> T) -> Result<T, String> {
        #[cfg(feature = "catch-objc-exceptions")]
        return objc2::exception::catch(std::panic::AssertUnwindSafe(f)).map_err(|exception| {
            match exception {
                Some(exception) => format!("{exception}"),
                None => "an exception with no information".to_string(),
            }
        });

        #[cfg(not(feature = "catch-objc-exceptions"))]
        return Ok(f());
    }

    /// Asks once for this app to be active. It usually already is - the user has
    /// just clicked its menu - but a status-bar item does not necessarily
    /// activate the app that owns it, and a dialog can only take keyboard focus
    /// in an active app.
    ///
    /// `-activate` is macOS 14+ and these bindings don't encode that, so it is
    /// probed first: sending a method the system doesn't have is an
    /// unrecognized-selector exception.
    fn activate_once(app: &NSApplication) {
        #[allow(deprecated)] // the pre-14 way, still honoured there
        app.activateIgnoringOtherApps(true);
        if app.respondsToSelector(sel!(activate)) {
            app.activate();
        }
        // Not a promise: activation is a request since macOS 14. The dialog is
        // still drawn either way, and clicking it always activates this app -
        // but a secure field the user can't type in is the failure this whole
        // design is trying to avoid, so record whether it went our way.
        log::note(&format!(
            "activation requested; this app is the active app: {}",
            app.isActive()
        ));
    }

    /// A password held briefly in this process and overwritten on the way out.
    /// Only a salted hash ever reaches the store; this is about the plaintext
    /// sitting in the tray app's memory while the CLI child is asked to save it.
    pub struct Secret(String);

    impl Secret {
        pub fn text(&self) -> &str {
            &self.0
        }
    }

    impl Drop for Secret {
        fn drop(&mut self) {
            // Every byte becomes NUL: same length, still valid UTF-8, which is
            // what makes mutating a String's buffer this way sound.
            unsafe {
                for byte in self.0.as_mut_str().as_bytes_mut() {
                    *byte = 0;
                }
            }
        }
    }

    /// One question, one secure field, OK/Cancel. None when cancelled.
    pub fn ask_secret(title: &str, question: &str) -> Option<Secret> {
        let Some(mtm) = MainThreadMarker::new() else {
            log::note("not on the main thread; cannot show a password dialog");
            return None;
        };
        let app = NSApplication::sharedApplication(mtm);
        activate_once(&app);

        // Built and run inside try_objc: if Foundation refuses something here,
        // enrolment fails with a line in the log instead of taking the tray app
        // down with it.
        let built = unsafe {
            try_objc(|| {
                let alert = NSAlert::new(mtm);
                alert.setAlertStyle(NSAlertStyle::Warning);
                alert.setMessageText(&NSString::from_str(question));
                // First button is the default, so Return answers it.
                alert.addButtonWithTitle(&NSString::from_str("OK"));
                alert.addButtonWithTitle(&NSString::from_str("Cancel"));

                let field = NSSecureTextField::new(mtm);
                field.setFrame(NSRect::new(
                    NSPoint::new(0.0, 0.0),
                    NSSize::new(280.0, 24.0),
                ));
                alert.setAccessoryView(Some(&field));

                let window: Retained<NSWindow> = alert.window();
                window.setTitle(&NSString::from_str(title));
                window.setInitialFirstResponder(Some(&field));
                let focused = window.makeFirstResponder(Some(&field));

                log::note(&format!(
                    "password dialog {title:?}: shown, field focused: {focused}"
                ));
                let answer = alert.runModal();
                (answer, field.stringValue().to_string())
            })
        };
        let (answer, text) = match built {
            Ok(pair) => pair,
            Err(why) => {
                log::note(&format!(
                    "password dialog {title:?} could not be shown: {why}"
                ));
                return None;
            }
        };
        // NSAlert numbers its buttons from NSAlertFirstButtonReturn (1000) by
        // the order they were added - never NSModalResponseOK (1). That mistake
        // once made every dialog look cancelled.
        if answer == NSAlertFirstButtonReturn {
            log::note(&format!(
                "password dialog {title:?}: {} characters typed",
                text.chars().count()
            ));
            Some(Secret(text))
        } else {
            log::note(&format!(
                "password dialog {title:?}: cancelled (response {answer})"
            ));
            None
        }
    }

    /// Yes / No.
    pub fn ask_yes_no(title: &str, question: &str) -> bool {
        let Some(mtm) = MainThreadMarker::new() else {
            return false;
        };
        let app = NSApplication::sharedApplication(mtm);
        activate_once(&app);
        let built = unsafe {
            try_objc(|| {
                let alert = NSAlert::new(mtm);
                alert.setAlertStyle(NSAlertStyle::Informational);
                alert.setMessageText(&NSString::from_str(question));
                alert.addButtonWithTitle(&NSString::from_str("Yes"));
                alert.addButtonWithTitle(&NSString::from_str("No"));
                alert.runModal()
            })
        };
        let answer = match built {
            Ok(answer) => answer,
            Err(why) => {
                log::note(&format!(
                    "confirmation dialog {title:?} could not be shown: {why}"
                ));
                return false;
            }
        };
        log::note(&format!("confirmation dialog {title:?}: response {answer}"));
        answer == NSAlertFirstButtonReturn
    }

    /// A one-line result message, in the app the user is talking to.
    pub fn notify(title: &str, message: &str) {
        log::note(&format!(
            "report to the user: {}",
            message.replace('\n', " ")
        ));
        let Some(mtm) = MainThreadMarker::new() else {
            eprintln!("Sleepless: {message}");
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        activate_once(&app);
        let shown = unsafe {
            try_objc(|| {
                let alert = NSAlert::new(mtm);
                alert.setMessageText(&NSString::from_str(title));
                alert.setInformativeText(&NSString::from_str(message));
                alert.addButtonWithTitle(&NSString::from_str("OK"));
                alert.runModal();
                true
            })
        };
        if !shown.unwrap_or(false) {
            // No dialog: the log line above is the only record the user gets.
            eprintln!("Sleepless: {message}");
        }
    }

    /// The passkey this machine can offer, without asking the user anything.
    pub fn passkey_label() -> Option<&'static str> {
        use objc2_local_authentication::LAContext;
        let ctx = unsafe { LAContext::new() };
        let policy = objc2_local_authentication::LAPolicy::DeviceOwnerAuthenticationWithBiometrics;
        let probe = unsafe { try_objc(|| ctx.canEvaluatePolicy_error(policy)) };
        match match probe {
            // A throw here means the framework wouldn't talk to us: that's the
            // same as "no Touch ID", and enrolment stops at the password.
            Ok(inner) => inner,
            Err(why) => {
                log::note(&format!("Touch ID could not be probed: {why}"));
                return None;
            }
        } {
            Ok(()) => {
                log::note("Touch ID is available");
                Some("Touch ID")
            }
            Err(error) => {
                log::note(&format!(
                    "Touch ID is not available: code {}, {}: {}",
                    error.code(),
                    error.domain(),
                    error.localizedDescription()
                ));
                None
            }
        }
    }

    /// Asks the system to confirm who the user is. Returns whether it did.
    ///
    /// The call returns immediately and the answer arrives in the reply block;
    /// the panel is presented by this process, so waiting means keeping AppKit's
    /// event queue running - the same thing `NSApplication::run` does.
    pub fn confirm_passkey(reason: &str) -> bool {
        use objc2_local_authentication::LAContext;
        let Some(mtm) = MainThreadMarker::new() else {
            return false;
        };
        let ctx = unsafe { LAContext::new() };
        let policy = objc2_local_authentication::LAPolicy::DeviceOwnerAuthenticationWithBiometrics;
        let probe = unsafe { try_objc(|| ctx.canEvaluatePolicy_error(policy)) };
        match probe {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                log::note("Touch ID cannot be evaluated; not showing the prompt");
                return false;
            }
            Err(why) => {
                log::note(&format!("Touch ID could not be probed: {why}"));
                return false;
            }
        }
        activate_once(&NSApplication::sharedApplication(mtm));

        TOUCH_ID_ANSWER.store(0, Ordering::SeqCst);
        let answer = RcBlock::new(|ok: objc2::runtime::Bool, error: *mut NSError| {
            if ok.as_bool() {
                log::note("Touch ID answered: confirmed");
                TOUCH_ID_ANSWER.store(1, Ordering::SeqCst);
                return;
            }
            let why = match unsafe { Retained::retain(error) } {
                Some(error) => format!(
                    "code {}, {}: {}",
                    error.code(),
                    error.domain(),
                    error.localizedDescription()
                ),
                None => "no error information".to_string(),
            };
            log::note(&format!("Touch ID answered: not confirmed ({why})"));
            TOUCH_ID_ANSWER.store(2, Ordering::SeqCst);
        });
        let why = NSString::from_str(reason);
        // No return value: -evaluatePolicy:localizedReason:reply: returns void,
        // and reading a BOOL out of it once made this give up in the same second
        // the panel appeared.
        let requested =
            unsafe { try_objc(|| ctx.evaluatePolicy_localizedReason_reply(policy, &why, &answer)) };
        if let Err(why) = requested {
            log::note(&format!("Touch ID prompt could not be requested: {why}"));
            return false;
        }
        log::note(&format!(
            "Touch ID prompt requested; waiting up to {}s",
            TOUCH_ID_TIMEOUT.as_secs()
        ));

        let app = NSApplication::sharedApplication(mtm);
        let deadline = Instant::now() + TOUCH_ID_TIMEOUT;
        while Instant::now() < deadline {
            match TOUCH_ID_ANSWER.load(Ordering::SeqCst) {
                1 => return true,
                2 => return false,
                _ => {}
            }
            // Drain what's queued, then let the run loop have a turn. The panel
            // itself needs the events dispatched; everything else this process
            // is waiting on (timers, mach ports, the tray's own machinery)
            // arrives through the run loop.
            while let Some(event) = unsafe {
                app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    NSEventMask::Any,
                    Some(&NSDate::distantPast()),
                    NSDefaultRunLoopMode,
                    true,
                )
            } {
                app.sendEvent(&event);
            }
            let slice = NSDate::dateWithTimeIntervalSinceNow(0.04);
            // Unsafe only because the run-loop mode is a global NSString the
            // compiler can't check; AppKit guarantees it is set.
            unsafe { NSRunLoop::currentRunLoop().runMode_beforeDate(NSDefaultRunLoopMode, &slice) };
        }
        // Close the system prompt rather than leaving it on screen.
        let _ = unsafe { try_objc(|| ctx.invalidate()) };
        log::note("Touch ID never answered in time");
        false
    }
}

#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
// Only `Secret` is actually referenced off macOS (the stdin plumbing keeps the
// same shape everywhere); the prompts themselves belong to the CLI child there,
// which can show its own dialogs without any activation fight.
mod imp {
    pub struct Secret(String);

    impl Secret {
        pub fn text(&self) -> &str {
            &self.0
        }
    }

    pub fn ask_secret(_title: &str, _question: &str) -> Option<Secret> {
        None
    }

    pub fn ask_yes_no(_title: &str, _question: &str) -> bool {
        false
    }

    pub fn notify(_title: &str, message: &str) {
        crate::log::note(&format!("report to the user: {message}"));
    }

    pub fn passkey_label() -> Option<&'static str> {
        None
    }

    pub fn confirm_passkey(_reason: &str) -> bool {
        false
    }
}

pub use imp::*;

#[cfg(test)]
mod tests {
    /// The rule that broke password enrolment once: an NSAlert's buttons answer
    /// with NSAlertFirstButtonReturn (1000) and up, never NSModalResponseOK (1).
    /// Kept here as well so the dialog code above can't drift back.
    #[cfg(target_os = "macos")]
    #[test]
    fn alert_buttons_are_numbered_from_a_thousand() {
        use objc2_app_kit::{
            NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSModalResponseOK,
        };
        assert_eq!(NSAlertFirstButtonReturn, 1000);
        assert_eq!(NSAlertSecondButtonReturn, 1001);
        assert_eq!(NSModalResponseOK, 1);
        assert_ne!(NSAlertFirstButtonReturn, NSModalResponseOK);
    }
}
