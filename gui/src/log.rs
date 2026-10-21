// Logging for the tray app, into the same file `sleepless` uses, so one report
// shows both the menu click and what the CLI did with it:
//
//   Windows: %LOCALAPPDATA%\Sleepless\sleepless.log
//   macOS:   ~/Library/Logs/Sleepless/sleepless.log
//   Linux:   $XDG_STATE_HOME/sleepless/sleepless.log  (~/.local/state/...)
//
// Kept deliberately small and independent of the CLI crate: a menu-bar app
// that can't write its log must still work.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static LOG: Mutex<Option<OpenFile>> = Mutex::new(None);

struct OpenFile {
    path: PathBuf,
    file: std::fs::File,
    bytes: u64,
}

/// Where the log goes. $SLEEPLESS_LOG overrides it.
pub fn path() -> PathBuf {
    if let Some(override_) = std::env::var_os("SLEEPLESS_LOG") {
        if !override_.as_os_str().is_empty() {
            return PathBuf::from(override_);
        }
    }
    if cfg!(windows) {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
            .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
            .unwrap_or_default();
        return base.join("Sleepless").join("sleepless.log");
    }
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_default();
    if cfg!(target_os = "macos") {
        return home.join("Library/Logs/Sleepless/sleepless.log");
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"));
    state.join("sleepless").join("sleepless.log")
}

/// Writes the panic to the log before the process aborts, so a crash in a
/// `panic = "abort"` release build leaves a last line naming what died. Goes
/// straight to the file: the panic may have happened while the log's own lock
/// was held.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let what = match info.payload().downcast_ref::<&str>() {
            Some(s) => (*s).to_string(),
            None => info
                .payload()
                .downcast_ref::<String>()
                .cloned()
                .unwrap_or_else(|| "no message".to_string()),
        };
        let where_ = match info.location() {
            Some(l) => format!("{}:{}", l.file(), l.line()),
            None => "unknown place".to_string(),
        };
        let line = format!("PANIC at {where_}: {what}");
        let target = path();
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&target) {
            let _ = writeln!(file, "{} {line} - aborting.", stamp());
        }
        eprintln!("Sleepless: {line}");
    }));
}

pub fn note(msg: &str) {
    let target = path();
    let mut guard = match LOG.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    // Reopen if this is the first line, the path moved, or the file got too
    // big (512 KiB, same as the CLI's own limit).
    let stale = match guard.as_ref() {
        None => true,
        Some(open) => open.path != target || open.bytes > 512 * 1024,
    };
    if stale {
        if let Some(old) = guard.as_ref() {
            let _ = old.file.sync_all();
        }
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let truncate = guard.as_ref().is_some_and(|old| {
            old.path == target && old.bytes > 512 * 1024 && old.file.sync_all().is_ok()
        });
        // write(true) matters for the truncating case; append keeps lines
        // from the CLI going to the same file intact.
        match OpenOptions::new()
            .create(true)
            .write(true)
            .append(!truncate)
            .truncate(truncate)
            .open(&target)
        {
            Ok(file) => {
                let mut open = OpenFile {
                    path: target.clone(),
                    file,
                    bytes: 0,
                };
                if truncate {
                    let _ = writeln!(
                        open.file,
                        "--- gui log truncated at {} to start over ---",
                        stamp()
                    );
                }
                *guard = Some(open);
            }
            Err(_) => return,
        }
    }

    if let Some(open) = guard.as_mut() {
        let line = format!("{} sleepless-gui {msg}\n", stamp());
        if open.file.write_all(line.as_bytes()).is_ok() {
            let _ = open.file.flush();
            open.bytes += line.len() as u64;
        }
    }
}

/// "2026-10-21 12:34:56Z" without pulling in a date crate (same as the CLI's).
fn stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let (year, month, day) = date_from_days(secs.div_euclid(86_400));
    let within = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z",
        within / 3600,
        (within % 3600) / 60,
        within % 60
    )
}

/// Days since 1970-01-01 to a calendar date; the same algorithm the CLI uses.
fn date_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Checks the calendar arithmetic these timestamps are built with. The CLI's
/// copy of the same function is cross-checked against Python's dates, and the
/// day numbers here come from that comparison.
#[cfg(test)]
mod tests {
    use super::date_from_days;

    #[test]
    fn day_numbers_map_to_the_expected_dates() {
        let named = |days: i64| {
            let (y, m, d) = date_from_days(days);
            format!("{y:04}-{m:02}-{d:02}")
        };
        for (days, expected) in [
            (0, "1970-01-01"),
            (19_723, "2024-01-01"),
            (19_722, "2023-12-31"),
            (11_016, "2000-02-29"),
            (11_017, "2000-03-01"),
            (-40_000, "1860-06-26"),
        ] {
            assert_eq!(named(days), expected, "day {days}");
        }
    }
}
