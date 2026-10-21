// The credential that guards coming back from the black screen.
//
// Only a salted PBKDF2-HMAC-SHA256 hash is ever stored - never the password
// itself, and nothing that could be turned back into it. Everything here is
// plain, platform-independent code (the bytes live in src/store.rs), which is
// what makes it unit-testable on any machine.
//
// A passkey (macOS Touch ID / Windows Hello) is *additional*: it is offered
// after the password, it is checked by the platform, and if it isn't available
// the password path is completely unaffected.

use std::io;

use crate::store;

/// Store section holding the password.
const SECTION: &str = "password";
/// Store section holding the passkey handle.
const PASSKEY_SECTION: &str = "passkey";

/// OWASP's current work factor for PBKDF2-HMAC-SHA256 (see the notes file).
/// A single check costs a few hundred milliseconds, which is what makes the
/// back-off below short: guessing is already slow by itself.
pub const ITERATIONS: u32 = 600_000;

const SALT_BYTES: usize = 16;
const HASH_BYTES: usize = 32;
/// A short password is allowed (it's a deterrent, not a policy), but worth
/// pointing out once.
pub const WEAK_PASSWORD_LEN: usize = 8;

/// A string that wipes itself when it goes out of scope, so the typed
/// password doesn't linger in the process's memory.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: &str) -> Self {
        Secret(s.to_string())
    }

    /// Takes a String the caller is done with, instead of copying it: used where
    /// the plaintext already sits in a buffer that would otherwise be freed
    /// without being overwritten.
    pub fn take(s: String) -> Self {
        Secret(s)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.chars().count()
    }

    /// The only way to look at it. Call sites should keep the borrow short.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // Every byte becomes NUL, which keeps the buffer the same length and
        // still valid UTF-8 - that's what makes this unsafe call sound.
        unsafe {
            for byte in self.0.as_mut_str().as_bytes_mut() {
                *byte = 0;
            }
        }
    }
}

// ---------------------------------------------------------------------
// Encoding helpers
// ---------------------------------------------------------------------

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    let bytes = text.as_bytes();
    (0..bytes.len())
        .step_by(2)
        .map(|i| {
            let (hi, lo) = (bytes[i] as char, bytes[i + 1] as char);
            let hi = hi.to_digit(16)?;
            let lo = lo.to_digit(16)?;
            Some(((hi << 4) | lo) as u8)
        })
        .collect()
}

fn derive(password: &str, salt: &[u8], iterations: u32) -> [u8; HASH_BYTES] {
    let mut out = [0u8; HASH_BYTES];
    pbkdf2::pbkdf2_hmac::<pbkdf2::sha2::Sha256>(password.as_bytes(), salt, iterations, &mut out);
    out
}

/// Compares without leaking where the first difference is.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------
// The password
// ---------------------------------------------------------------------

/// True when a password (or passkey) is enrolled, i.e. coming back from the
/// black screen will ask for something.
pub fn is_enrolled() -> bool {
    store::get(SECTION, "hash").is_some() || store::get(PASSKEY_SECTION, "kind").is_some()
}

/// Enrolled password, if any.
pub fn has_password() -> bool {
    store::get(SECTION, "hash").is_some()
}

/// Enrolled passkey kind ("touch-id" / "webauthn"), if any.
pub fn passkey_kind() -> Option<String> {
    if has_password() {
        store::get(PASSKEY_SECTION, "kind")
    } else {
        // A passkey is only ever an addition to a password.
        None
    }
}

pub fn has_passkey() -> bool {
    passkey_kind().is_some()
}

/// Replaces any password that's already set.
pub fn enroll_password(password: &Secret) -> io::Result<()> {
    let mut salt = [0u8; SALT_BYTES];
    getrandom::fill(&mut salt)
        .map_err(|e| io::Error::other(format!("could not get random bytes: {e}")))?;
    let hash = derive(password.expose(), &salt, ITERATIONS);
    store::set(SECTION, "kdf", "pbkdf2-sha256")?;
    store::set(SECTION, "iterations", &ITERATIONS.to_string())?;
    store::set(SECTION, "salt", &to_hex(&salt))?;
    store::set(SECTION, "hash", &to_hex(&hash))?;
    Ok(())
}

/// Records that the platform passkey (Touch ID, and later whatever else the
/// OS verifies for us) may unlock the session. The verification itself is the
/// platform's job, so nothing here is secret - the id is only a marker that
/// lets a future implementation match a credential to this user.
pub fn enroll_passkey(kind: &str) -> io::Result<()> {
    if !has_password() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "set a password before adding a passkey",
        ));
    }
    let mut id = [0u8; 16];
    getrandom::fill(&mut id)
        .map_err(|e| io::Error::other(format!("could not get random bytes: {e}")))?;
    store::set(PASSKEY_SECTION, "kind", kind)?;
    store::set(PASSKEY_SECTION, "id", &to_hex(&id))?;
    Ok(())
}

/// Removes both the password and the passkey.
pub fn remove() -> io::Result<()> {
    store::delete(SECTION)?;
    store::delete(PASSKEY_SECTION)
}

pub fn remove_passkey() -> io::Result<()> {
    store::delete(PASSKEY_SECTION)
}

pub fn check_password(password: &Secret) -> bool {
    let Some(stored) = store::get(SECTION, "hash") else {
        return false;
    };
    let Some(salt_hex) = store::get(SECTION, "salt") else {
        return false;
    };
    let Some(salt) = from_hex(&salt_hex) else {
        return false;
    };
    let Some(wanted) = from_hex(&stored) else {
        return false;
    };
    // Never let a damaged record lower the work factor below what we ship.
    let iterations = store::get(SECTION, "iterations")
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|i| *i >= 1_000)
        .unwrap_or(ITERATIONS);
    if wanted.len() != HASH_BYTES || salt.is_empty() {
        return false;
    }
    let got = derive(password.expose(), &salt, iterations);
    constant_time_eq(&got, &wanted)
}

// ---------------------------------------------------------------------
// Slowing down guesses
// ---------------------------------------------------------------------

/// Counts failures in one session and turns that into a pause. The password
/// check already takes a while, so this is mainly about not letting someone
/// hammer the box.
#[derive(Default)]
pub struct Backoff {
    failures: u32,
}

impl Backoff {
    /// How long to wait before accepting another attempt.
    pub fn penalty(&self) -> std::time::Duration {
        match self.failures {
            0 => std::time::Duration::ZERO,
            n => std::time::Duration::from_millis(500u64 << n.min(5)),
        }
    }

    pub fn failure(&mut self) -> u32 {
        self.failures += 1;
        self.failures
    }

    /// How many guesses have been made so far.
    pub fn attempts(&self) -> u32 {
        self.failures
    }
}

/// The ask-until-it's-right loop every lock screen uses: `ask` shows whatever
/// prompt the platform has (a black overlay on Windows and macOS, an askpass
/// dialog on Linux) and returns None when it was dismissed.
///
/// Cancelling a prompt is not an escape: it counts as a failed attempt and
/// asks again. Passkeys are checked by the platform in front of this, so they
/// never appear here.
///
/// `cancelled` is consulted before every attempt and ends the loop with
/// `false`: the OS taking the session over (its own lock screen, a fast user
/// switch) has to be able to interrupt a gate whose prompt is wedged. Returns
/// whether the password was accepted.
/// What the prompt came back with.
pub enum Ask {
    /// Something was submitted - possibly nothing at all. This is a guess: it is
    /// checked, and a wrong one feeds the back-off.
    Guess(Secret),
    /// The prompt was cleared or abandoned (Escape, a cancelled dialog). Never a
    /// way through, and deliberately not a guess either: mashing Escape must not
    /// build up a penalty the user then pays for typing the right password.
    Dismissed,
}

impl Ask {
    /// Adapter for the prompts that answer with an `Option` - the Windows dialog
    /// and the Linux askpass. Whatever came back, even an empty string, was
    /// submitted; only "no answer at all" is a dismissal. (The macOS box says
    /// which key was pressed instead, so it doesn't need this.)
    #[cfg(any(windows, target_os = "linux"))]
    pub fn from_prompt(answer: Option<Secret>) -> Ask {
        match answer {
            Some(secret) => Ask::Guess(secret),
            None => Ask::Dismissed,
        }
    }
}

/// Why the gate is pausing, so a platform with a screen can tell the two apart:
/// a back-off penalty is worth announcing, a short breath after a dismissed
/// prompt is not - and labelling one as the other tells the user they've been
/// guessing when they haven't.
#[derive(Debug, PartialEq, Eq)]
pub enum Pause {
    /// The user has guessed wrong too often; this long, and worth showing.
    Penalty(std::time::Duration),
    /// A brief breath so that a prompt which can't be answered (no askpass, a
    /// wedged dialog) can't spin the loop at full speed. Not a penalty.
    Brief(std::time::Duration),
}

/// The gate, with the platform's own prompt and the platform's own sleep.
pub fn unlock_loop(ask: impl FnMut(&str) -> Ask, cancelled: impl FnMut() -> bool) -> bool {
    unlock_loop_with_wait(ask, cancelled, |pause| {
        let how_long = match pause {
            Pause::Penalty(how_long) | Pause::Brief(how_long) => how_long,
        };
        std::thread::sleep(how_long);
        true
    })
}

/// The gate, with the wait handed to the caller.
///
/// `wait` sleeps out a pause and returns false if it was cut short because the
/// session was taken over. A prompt that lives on the **main thread** must not
/// sleep there: on macOS that freezes AppKit for the whole penalty - no drawing,
/// no keyboard input, and no `com.apple.screenIsLocked` notification, which is
/// delivered on the main run loop - so the OS lock would not be noticed until the
/// penalty ran out. Those platforms pass a pumping wait instead.
pub fn unlock_loop_with_wait(
    mut ask: impl FnMut(&str) -> Ask,
    mut cancelled: impl FnMut() -> bool,
    mut wait: impl FnMut(Pause) -> bool,
) -> bool {
    let mut backoff = Backoff::default();
    let mut message = "Enter your password to come back";
    loop {
        // Nothing to check: with a passkey enrolled but no password (only
        // reachable by editing the store, or by clearing the password while the
        // screen is black) asking forever would trap the user behind a prompt
        // that can never be satisfied. Ending the gate ends the session, which
        // is the safe direction: nothing is let through, and nothing is stuck.
        if !has_password() {
            log!(
                "The gate is armed but no password is enrolled, so there is nothing to check: \
                 ending the session instead of asking forever."
            );
            return false;
        }
        // The OS taking the session over must always get us out of here, even
        // if our own prompt is wedged: that's the documented safety valve.
        if cancelled() {
            log!(
                "Lock screen giving up after {} attempt(s): the session was taken over.",
                backoff.attempts()
            );
            return false;
        }
        match ask(message) {
            Ask::Guess(secret) => {
                let penalty = backoff.penalty();
                if !penalty.is_zero() {
                    log!("Too many guesses: waiting {penalty:?} before the next attempt.");
                    if !wait(Pause::Penalty(penalty)) {
                        log!(
                            "Session taken over during the back-off wait: ending the gate                              ({} attempt(s) so far).",
                            backoff.attempts()
                        );
                        return false;
                    }
                }
                if check_password(&secret) {
                    log!(
                        "Password accepted ({} failed attempt(s)).",
                        backoff.attempts()
                    );
                    return true;
                }
                backoff.failure();
                message = "Wrong password, try again";
            }
            Ask::Dismissed => {
                // Not a guess, so no back-off - but a fixed short pause, so a
                // prompt that can't be answered (no askpass, a wedged dialog)
                // can't spin the loop at full speed.
                log!("The prompt was dismissed; not counted as a guess.");
                if !wait(Pause::Brief(std::time::Duration::from_millis(400))) {
                    log!("Session taken over while the prompt was dismissed.");
                    return false;
                }
                message = "Enter your password to come back";
            }
        }
    }
}

// ---------------------------------------------------------------------
// Tests (run on any host: `cargo test -p sleepless`)
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `take` moves the text rather than duplicating it, so the copy the caller
    /// was holding is the one that gets zeroised on drop.
    #[test]
    fn take_moves_the_string_without_copying_it() {
        let secret = Secret::take("hunter2".to_string());
        assert_eq!(secret.expose(), "hunter2");
        assert_eq!(secret.len(), 7);
        assert!(!secret.is_empty());
    }

    /// The store's location comes from a process-wide environment variable,
    /// so tests that touch it take turns.
    static TEST_STORE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct TempStore {
        _turn: std::sync::MutexGuard<'static, ()>,
    }

    impl TempStore {
        fn new(tag: &str) -> Self {
            let _turn = TEST_STORE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            #[cfg(windows)]
            {
                // On Windows the store is the registry, so a temporary directory
                // means nothing there: point it at a throwaway key named for this
                // process instead. Otherwise the tests would read whatever
                // credential the machine already has - and write over it.
                std::env::set_var(
                    "SLEEPLESS_REGISTRY_ROOT",
                    format!("Software\\Sleepless-test-{}-{tag}", std::process::id()),
                );
            }
            #[cfg(not(windows))]
            {
                let dir = std::env::temp_dir()
                    .join(format!("sleepless-test-{tag}-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&dir);
                std::env::set_var("SLEEPLESS_CONFIG_DIR", &dir);
            }
            TempStore { _turn }
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            #[cfg(windows)]
            {
                let _ = crate::store::delete(SECTION);
                std::env::remove_var("SLEEPLESS_REGISTRY_ROOT");
            }
            #[cfg(not(windows))]
            if let Some(dir) = std::env::var_os("SLEEPLESS_CONFIG_DIR") {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0u8, 1, 0x7f, 0x80, 0xff];
        let text = to_hex(&bytes);
        assert_eq!(text, "00017f80ff");
        assert_eq!(from_hex(&text).unwrap(), bytes);
        assert!(from_hex("abc").is_none());
        assert!(from_hex("zzzz").is_none());
    }

    #[test]
    fn constant_time_compare_matches_only_equal_values() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn enroll_then_check() {
        let _store = TempStore::new("enroll");
        let password = Secret::new("correct horse battery staple");
        assert!(!is_enrolled());
        enroll_password(&password).expect("enrolment writes to the store");
        assert!(is_enrolled());
        assert!(has_password());
        assert!(!has_passkey());
        assert!(check_password(&password));
        assert!(!check_password(&Secret::new(
            "Correct horse battery staple"
        )));
        assert!(!check_password(&Secret::new("")));
    }

    #[test]
    fn the_password_itself_is_never_stored() {
        let _store = TempStore::new("nosecret");
        let password = Secret::new("a-very-unlikely-string-to-appear-in-a-file");
        enroll_password(&password).unwrap();
        // What "not in the clear" means depends on where the store is: a file on
        // unix, registry values on Windows.
        #[cfg(unix)]
        {
            let text = std::fs::read_to_string(
                std::env::var("SLEEPLESS_CONFIG_DIR").unwrap() + "/sleepless.conf",
            )
            .unwrap();
            assert!(
                !text.contains(password.expose()),
                "the file held the password"
            );
            assert!(text.contains("password.hash="));
        }
        #[cfg(windows)]
        {
            let salt = store::get(SECTION, "salt").expect("a salt was stored");
            let hash = store::get(SECTION, "hash").expect("a hash was stored");
            assert!(!salt.contains(password.expose()), "the salt held it");
            assert!(!hash.contains(password.expose()), "the hash held it");
            assert_eq!(hash.len(), 64, "32 bytes of PBKDF2 output, in hex");
        }
        // Two records for the same password use different salts.
        let first = store::get(SECTION, "salt").unwrap();
        enroll_password(&password).unwrap();
        assert_ne!(first, store::get(SECTION, "salt").unwrap());
    }

    #[test]
    fn remove_clears_everything() {
        let _store = TempStore::new("remove");
        enroll_password(&Secret::new("hunter2hunter2")).unwrap();
        enroll_passkey("touch-id").unwrap();
        assert!(has_passkey());
        remove().unwrap();
        assert!(!is_enrolled());
        assert!(!has_password());
        assert!(!check_password(&Secret::new("hunter2hunter2")));
    }

    #[test]
    fn a_passkey_needs_a_password_first() {
        let _store = TempStore::new("passkey");
        assert!(enroll_passkey("touch-id").is_err());
        enroll_password(&Secret::new("somethinglongenough")).unwrap();
        enroll_passkey("touch-id").unwrap();
        assert_eq!(passkey_kind().as_deref(), Some("touch-id"));
        // A marker id is recorded, so a future platform credential
        // implementation has something to match against.
        let marker = from_hex(&store::get(PASSKEY_SECTION, "id").unwrap()).unwrap();
        assert_eq!(marker.len(), 16);
        remove_passkey().unwrap();
        assert!(!has_passkey());
        // The password still works on its own.
        assert!(check_password(&Secret::new("somethinglongenough")));
    }

    #[test]
    fn back_off_grows_with_failures() {
        let mut backoff = Backoff::default();
        assert_eq!(backoff.penalty(), std::time::Duration::ZERO);
        for expected in [1.0, 2.0, 4.0, 8.0, 16.0, 16.0] {
            backoff.failure();
            assert_eq!(backoff.penalty().as_secs_f64(), expected);
        }
    }

    /// A record with deliberately few rounds, so the loop tests stay quick.
    fn enroll_cheaply(password: &str) {
        let salt = [7u8; SALT_BYTES];
        let hash = derive(password, &salt, 1_000);
        store::set(SECTION, "kdf", "pbkdf2-sha256").unwrap();
        store::set(SECTION, "iterations", "1000").unwrap();
        store::set(SECTION, "salt", &to_hex(&salt)).unwrap();
        store::set(SECTION, "hash", &to_hex(&hash)).unwrap();
    }

    #[test]
    fn unlock_loop_holds_the_door_until_the_password_matches() {
        let _store = TempStore::new("unlockloop");
        enroll_cheaply("letmein");

        let mut answers = [
            Ask::Guess(Secret::new("wrong")),
            Ask::Guess(Secret::new("letmein")),
            Ask::Guess(Secret::new("never asked")),
        ]
        .into_iter();
        let mut messages = Vec::new();
        assert!(unlock_loop(
            |message| {
                messages.push(message.to_string());
                answers.next().unwrap()
            },
            || false,
        ));
        // The loop stopped at the right password and told the user why the
        // second prompt was different.
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], "Enter your password to come back");
        assert_eq!(messages[1], "Wrong password, try again");
    }

    /// A gate with nothing to check must end rather than ask forever - the
    /// difference between a stuck black screen and a session that lets go.
    #[test]
    fn the_gate_ends_when_no_password_is_enrolled() {
        let _store = TempStore::new("unlockempty");
        let asked = std::cell::Cell::new(0usize);
        let unlocked = unlock_loop(
            |_| {
                asked.set(asked.get() + 1);
                Ask::Guess(Secret::new("anything"))
            },
            || false,
        );
        assert!(!unlocked, "an unsatisfiable gate must not report success");
        assert_eq!(asked.get(), 0, "and it must not ask at all");
    }

    /// The OS taking the session over has to interrupt the gate even when the
    /// password has never been accepted - that's the way out of a stuck prompt.
    #[test]
    fn the_loop_stops_when_the_session_is_taken_over() {
        let _store = TempStore::new("unlockcancel");
        enroll_cheaply("letmein");

        // Shared between the two closures, so a Cell rather than a mutable
        // local (both closures would have to borrow it).
        let asked = std::cell::Cell::new(0usize);
        let unlocked = unlock_loop(
            |_| {
                asked.set(asked.get() + 1);
                Ask::Guess(Secret::new("wrong on purpose"))
            },
            || asked.get() >= 2,
        );
        assert!(!unlocked, "a cancelled gate must not report success");
        assert_eq!(asked.get(), 2, "should stop as soon as cancelled() says so");
    }

    #[test]
    fn a_dismissed_prompt_still_does_not_let_you_through() {
        let _store = TempStore::new("unlockdismiss");
        enroll_cheaply("letmein");

        let mut answers = [Ask::Dismissed, Ask::Guess(Secret::new("letmein"))].into_iter();
        let mut asked = 0;
        assert!(unlock_loop(
            |_| {
                asked += 1;
                answers.next().unwrap()
            },
            || false,
        ));
        assert_eq!(asked, 2);
    }

    /// Escape is not a guess, so it must never feed the back-off: someone
    /// clearing the box a few times should not then pay 16 seconds to type the
    /// right password. The pauses are the same fixed length, and none of them is
    /// a penalty.
    #[test]
    fn dismissing_the_prompt_costs_no_back_off() {
        let _store = TempStore::new("dismissbackoff");
        enroll_cheaply("letmein");

        let mut answers = [
            Ask::Dismissed,
            Ask::Dismissed,
            Ask::Dismissed,
            Ask::Guess(Secret::new("letmein")),
        ]
        .into_iter();
        let mut pauses = Vec::new();
        assert!(unlock_loop_with_wait(
            |_| answers.next().unwrap(),
            || false,
            |pause| {
                pauses.push(pause);
                true
            },
        ));
        assert_eq!(pauses.len(), 3, "three dismissals, three pauses");
        assert!(
            pauses
                .iter()
                .all(|p| *p == Pause::Brief(std::time::Duration::from_millis(400))),
            "every one of them a flat breath, and not one a penalty: {pauses:?}"
        );
    }

    /// A wrong guess is a guess, and the wait belongs to the platform: on macOS
    /// the penalty has to be pumped (AppKit keeps drawing, the session-lock
    /// notification still gets delivered), which is only possible if the loop
    /// hands the duration out instead of sleeping on its own thread.
    #[test]
    fn wrong_guesses_hand_the_back_off_to_the_platform() {
        let _store = TempStore::new("guessbackoff");
        enroll_cheaply("letmein");

        let mut answers = [
            Ask::Guess(Secret::new("nope")),
            Ask::Guess(Secret::new("nope again")),
            Ask::Guess(Secret::new("letmein")),
        ]
        .into_iter();
        let mut penalties = Vec::new();
        assert!(unlock_loop_with_wait(
            |_| answers.next().unwrap(),
            || false,
            |pause| {
                penalties.push(pause);
                true
            },
        ));
        assert_eq!(
            penalties,
            vec![
                Pause::Penalty(std::time::Duration::from_secs(1)),
                Pause::Penalty(std::time::Duration::from_secs(2)),
            ],
            "the second and third guesses wait 1s and 2s, as penalties, handed              to the caller to wait out however its UI needs to"
        );
    }

    /// A wait that reports the session was taken over ends the gate at once.
    #[test]
    fn a_cut_short_wait_ends_the_gate() {
        let _store = TempStore::new("cutshort");
        enroll_cheaply("letmein");

        let mut answers = [
            Ask::Guess(Secret::new("nope")),
            Ask::Guess(Secret::new("nope")),
            Ask::Guess(Secret::new("would never be asked")),
        ]
        .into_iter();
        let mut asked = 0;
        let unlocked = unlock_loop_with_wait(
            |_| {
                asked += 1;
                answers.next().unwrap()
            },
            || false,
            |_| false, // the platform says: cut short, the session is gone
        );
        assert!(
            !unlocked,
            "a gate whose penalty was cut short lets nobody in"
        );
        assert_eq!(asked, 2, "and it stops right there");
    }

    #[test]
    fn pbkdf2_matches_the_known_test_vector() {
        // From the pbkdf2 crate's own docs: 600 000 rounds of
        // PBKDF2-HMAC-SHA256("password", "salt").
        let out = derive("password", b"salt", 600_000);
        assert_eq!(
            to_hex(&out[..20]),
            "669cfe52482116fda1aa2cbe409b2f56c8e45637"
        );
    }

    /// Reports how long a single check takes on this machine; run it on the
    /// target hardware with `cargo test -- --ignored timing`.
    #[test]
    #[ignore]
    fn timing() {
        let start = std::time::Instant::now();
        let out = derive("password", b"salt", ITERATIONS);
        println!(
            "{ITERATIONS} rounds took {:?} ({})",
            start.elapsed(),
            &to_hex(&out[..8])
        );
    }
}
