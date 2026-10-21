// One log file that both Sleepless binaries append to, so a report from
// someone's machine is a single file to send:
//
//   Windows: %LOCALAPPDATA%\Sleepless\sleepless.log
//   macOS:   ~/Library/Logs/Sleepless/sleepless.log
//   Linux:   $XDG_STATE_HOME/sleepless/sleepless.log  (~/.local/state/...)
//
// It's written whether or not --debug was passed: a session started from the
// tray / menu bar has no console, and "it said cancelled" is impossible to
// diagnose without something on disk. Secrets never go in here - only which
// prompt was shown, what the user pressed, and exit codes.
//
// $SLEEPLESS_LOG overrides the path (the tests use that, and it lets a build
// be tried without touching the real log).

use std::any::Any;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Start the file over once it passes this, so it can't grow without bound.
const MAX_BYTES: u64 = 512 * 1024;

struct Sink {
    path: PathBuf,
    file: std::fs::File,
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);
static BYTES: AtomicU64 = AtomicU64::new(0);
static WARNED: AtomicBool = AtomicBool::new(false);

/// Where the log lives.
pub fn path() -> PathBuf {
    if let Some(override_) = std::env::var_os("SLEEPLESS_LOG") {
        if !override_.as_os_str().is_empty() {
            return PathBuf::from(override_);
        }
    }
    if cfg!(windows) {
        // LOCALAPPDATA is where logs belong; APPDATA is the usual fallback and
        // USERPROFILE the last one.
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
            .or_else(home)
            .unwrap_or_default();
        return base.join("Sleepless").join("sleepless.log");
    }
    if cfg!(target_os = "macos") {
        return home()
            .unwrap_or_default()
            .join("Library/Logs/Sleepless/sleepless.log");
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().unwrap_or_default().join(".local/state"));
    state.join("sleepless").join("sleepless.log")
}

#[cfg(unix)]
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[cfg(windows)]
fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

/// Appends one timestamped line. A logging failure is never fatal: it's
/// mentioned once on stderr and then left alone.
/// Formats what a panic was about: where it happened and what it said. The
/// payload is a `&str` for `panic!("literal")` and a `String` for anything
/// formatted, so both are handled.
pub fn panic_message(payload: &(dyn Any + Send), where_: Option<&std::panic::Location>) -> String {
    let what = match payload.downcast_ref::<&str>() {
        Some(s) => (*s).to_string(),
        None => payload
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_else(|| "no message".to_string()),
    };
    let where_ = match where_ {
        Some(l) => format!("{}:{}", l.file(), l.line()),
        None => "unknown place".to_string(),
    };
    format!("PANIC at {where_}: {what}")
}

/// A panic in the release build (which is `panic = "abort"`) otherwise leaves
/// the log ending mid-sentence, which reads like a killed process rather than
/// a bug. This writes the panic to the log file before the abort, going
/// straight to the file instead of through the usual sink - the panic may have
/// happened while that sink's lock was held.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let line = panic_message(info.payload(), info.location());
        let target = path();
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&target) {
            let _ = writeln!(
                file,
                "{} {line} - aborting the run.",
                stamp(SystemTime::now())
            );
        }
        eprintln!("Sleepless: {line}");
    }));
}

pub fn note(msg: &str) {
    let target = path();
    let line = format!("{} {msg}\n", stamp(SystemTime::now()));

    let mut guard = SINK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let reopen = match guard.as_ref() {
        None => true,
        Some(sink) => sink.path != target,
    };
    if reopen {
        if let Some(sink) = guard.as_ref() {
            let _ = sink.file.sync_all();
        }
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let existing = fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
        BYTES.store(existing, Ordering::Relaxed);
        // Over the cap: start over, but say so in the file that survives.
        let truncate = existing > MAX_BYTES;
        let opened = OpenOptions::new()
            .create(true)
            .write(true)
            .append(!truncate)
            .truncate(truncate)
            .open(&target);
        match opened {
            Ok(mut file) => {
                if truncate {
                    let _ = writeln!(
                        file,
                        "--- log truncated at {} to start over ---",
                        stamp(SystemTime::now())
                    );
                }
                *guard = Some(Sink {
                    path: target.clone(),
                    file,
                });
            }
            Err(e) => {
                if !WARNED.swap(true, Ordering::Relaxed) {
                    eprintln!("Sleepless: could not write the log at {target:?}: {e}");
                }
                return;
            }
        }
    }

    if let Some(sink) = guard.as_mut() {
        let written = sink
            .file
            .write_all(line.as_bytes())
            .and_then(|_| sink.file.flush());
        match written {
            Ok(()) => {
                if BYTES.fetch_add(line.len() as u64, Ordering::Relaxed) > MAX_BYTES {
                    // Next note reopens (and, being over the cap, truncates).
                    *guard = None;
                }
            }
            // The file may have been moved or deleted underneath us.
            Err(_) => *guard = None,
        }
    }
}

/// Seconds-since-epoch -> "2026-10-21 12:34:56Z" in UTC, without a date crate.
fn stamp(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let within = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z",
        within / 3600,
        (within % 3600) / 60,
        within % 60
    )
}

/// Days since 1970-01-01 to a calendar date (Howard Hinnant's algorithm, the
/// one behind C++20's days_from_civil).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // day of era: 0..=146096
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_panic_names_the_place_and_the_reason() {
        let exploded = std::panic::catch_unwind(|| panic!("the credential store is gone"));
        let payload = exploded.unwrap_err();
        let message = panic_message(payload.as_ref(), None);
        assert_eq!(
            message,
            "PANIC at unknown place: the credential store is gone"
        );

        // A formatted panic arrives as a String instead of a &'static str.
        let name = "caffeinate";
        let exploded = std::panic::catch_unwind(move || panic!("could not start {name}"));
        let payload = exploded.unwrap_err();
        assert_eq!(
            panic_message(payload.as_ref(), None),
            "PANIC at unknown place: could not start caffeinate"
        );
    }

    use super::*;
    use std::time::Duration;

    /// The file tests share the static sink and the SLEEPLESS_LOG variable, so
    /// they take turns (the same reason src/lock.rs has one).
    static TEST_LOG: Mutex<()> = Mutex::new(());

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn stamps_are_utc_and_readable() {
        assert_eq!(stamp(at(0)), "1970-01-01 00:00:00Z");
        assert_eq!(stamp(at(1)), "1970-01-01 00:00:01Z");
        // 2024-01-01 is day 19723 after the epoch: 54 years plus 13 leap days.
        assert_eq!(stamp(at(19_723 * 86_400)), "2024-01-01 00:00:00Z");
        assert_eq!(stamp(at(19_722 * 86_400 + 86_399)), "2023-12-31 23:59:59Z");
        // A leap day, which sits in the March-based year the algorithm uses.
        assert_eq!(stamp(at(11_016 * 86_400)), "2000-02-29 00:00:00Z");
        assert_eq!(stamp(at(11_017 * 86_400)), "2000-03-01 00:00:00Z");
        assert_eq!(
            stamp(at(17_501 * 86_400 + 12 * 3600 + 34 * 60 + 56)),
            "2017-12-01 12:34:56Z"
        );
    }

    #[test]
    fn calendar_dates_stay_in_range() {
        for days in [-40_000, 0, 59, 60, 365, 366, 11_016, 19_723, 47_482] {
            let (year, month, day) = civil_from_days(days);
            assert!((1859..=2101).contains(&year), "year {year} out of range");
            assert!((1..=12).contains(&month), "month {month} out of range");
            assert!((1..=31).contains(&day), "day {day} out of range");
        }
    }

    #[test]
    fn notes_land_in_the_file_path_names() {
        let _turn = TEST_LOG.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("sleepless-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Deliberately nested: the parents have to be created on the way.
        let file = dir.join("nested/dir/test.log");
        std::env::set_var("SLEEPLESS_LOG", &file);
        *SINK.lock().unwrap() = None;

        note("hello from the test");
        note("second line");

        let text = std::fs::read_to_string(&file).expect("the log file was created");
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2, "got {text:?}");
        assert!(lines[0].contains("hello from the test"));
        assert!(lines[1].contains("second line"));
        assert!(lines[0].starts_with("20"), "line should start with a date");
        assert!(lines[0].contains("Z "), "date should be marked UTC");

        std::env::remove_var("SLEEPLESS_LOG");
        *SINK.lock().unwrap() = None;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_giant_log_starts_over_instead_of_growing() {
        let _turn = TEST_LOG.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("sleepless-log-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("test.log");
        fs::create_dir_all(&dir).unwrap();
        // Pretend a previous run filled it up.
        fs::write(&file, vec![b'x'; (MAX_BYTES + 10) as usize]).unwrap();
        std::env::set_var("SLEEPLESS_LOG", &file);
        *SINK.lock().unwrap() = None;

        note("after the cap");

        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.len() < 1024,
            "file should have been restarted: {} bytes",
            text.len()
        );
        assert!(text.contains("truncated"));
        assert!(text.contains("after the cap"));

        std::env::remove_var("SLEEPLESS_LOG");
        *SINK.lock().unwrap() = None;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
