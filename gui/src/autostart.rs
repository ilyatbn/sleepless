// "Launch at login", using each platform's plain per-user mechanism:
//
//   Windows: a value under HKCU\Software\Microsoft\Windows\CurrentVersion\Run
//   macOS:   a LaunchAgent in ~/Library/LaunchAgents
//   Linux:   an XDG autostart entry in ~/.config/autostart
//
// None of these need admin rights, and the entry itself is the on/off
// state, so it also shows up (and can be removed) in the system's own
// startup / login items settings.

use std::env;
use std::io;
use std::path::PathBuf;

#[cfg(not(windows))]
use std::fs;

fn current_exe() -> io::Result<PathBuf> {
    env::current_exe()
}

/// $XDG_CONFIG_HOME, defaulting to ~/.config.
#[cfg(not(windows))]
pub fn xdg_config_home() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(env::var_os("HOME")?).join(".config")))
}

#[cfg(windows)]
pub fn xdg_config_home() -> Option<PathBuf> {
    None
}

// ---------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use std::io;
    use std::ptr;

    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
    };

    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const VALUE_NAME: &str = "Sleepless";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn check(err: u32) -> io::Result<()> {
        if err == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(err as i32))
        }
    }

    /// Runs `f` with the Run key open, closing it afterwards.
    fn with_run_key<T>(access: u32, f: impl FnOnce(HKEY) -> io::Result<T>) -> io::Result<T> {
        let subkey = wide(RUN_KEY);
        let mut hkey: HKEY = ptr::null_mut();
        check(unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, access, &mut hkey) })?;
        let result = f(hkey);
        unsafe { RegCloseKey(hkey) };
        result
    }

    pub fn is_enabled() -> bool {
        let name = wide(VALUE_NAME);
        with_run_key(KEY_QUERY_VALUE, |hkey| {
            check(unsafe {
                RegQueryValueExW(
                    hkey,
                    name.as_ptr(),
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            })
        })
        .is_ok()
    }

    pub fn set(enable: bool) -> io::Result<()> {
        let name = wide(VALUE_NAME);
        if !enable {
            return with_run_key(KEY_SET_VALUE, |hkey| {
                match unsafe { RegDeleteValueW(hkey, name.as_ptr()) } {
                    ERROR_FILE_NOT_FOUND => Ok(()),
                    err => check(err),
                }
            });
        }
        let command = wide(&format!("\"{}\"", super::current_exe()?.display()));
        with_run_key(KEY_SET_VALUE, |hkey| {
            check(unsafe {
                RegSetValueExW(
                    hkey,
                    name.as_ptr(),
                    0,
                    REG_SZ,
                    command.as_ptr() as *const u8,
                    (command.len() * 2) as u32,
                )
            })
        })
    }
}

// ---------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod imp {
    use std::path::{Path, PathBuf};
    use std::{env, io};

    use super::fs;

    const LABEL: &str = "io.github.ilyatbn.sleepless-gui";

    fn plist_path() -> Option<PathBuf> {
        let home = env::var_os("HOME")?;
        Some(Path::new(&home).join(format!("Library/LaunchAgents/{LABEL}.plist")))
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    pub fn is_enabled() -> bool {
        plist_path().is_some_and(|p| p.is_file())
    }

    pub fn set(enable: bool) -> io::Result<()> {
        let path = plist_path().ok_or_else(|| io::Error::other("HOME is not set"))?;
        if !enable {
            return match fs::remove_file(&path) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                other => other,
            };
        }
        // LABEL doubles as the app's bundle id, which lets System Settings >
        // Login Items show this entry as Sleepless rather than a bare
        // executable.
        let exe = super::current_exe()?;
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>AssociatedBundleIdentifiers</key>
    <string>{LABEL}</string>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#,
            exe = xml_escape(&exe.display().to_string())
        );
        fs::create_dir_all(
            path.parent()
                .ok_or_else(|| io::Error::other("no directory for the autostart file"))?,
        )?;
        fs::write(&path, plist)
    }
}

// ---------------------------------------------------------------------
// Linux (and other XDG desktops)
// ---------------------------------------------------------------------

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use std::io;
    use std::path::PathBuf;

    use super::fs;

    fn desktop_path() -> Option<PathBuf> {
        Some(super::xdg_config_home()?.join("autostart/sleepless-gui.desktop"))
    }

    /// Quotes an Exec= argument per the Desktop Entry spec.
    fn exec_quote(s: &str) -> String {
        let mut out = String::from("\"");
        for c in s.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        // The whole value is also subject to the spec's general string
        // escaping, where a backslash is itself escaped.
        out.replace('\\', "\\\\")
    }

    pub fn is_enabled() -> bool {
        desktop_path().is_some_and(|p| p.is_file())
    }

    pub fn set(enable: bool) -> io::Result<()> {
        let path = desktop_path().ok_or_else(|| io::Error::other("HOME is not set"))?;
        if !enable {
            return match fs::remove_file(&path) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                other => other,
            };
        }
        let exe = super::current_exe()?;
        let entry = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Sleepless\n\
             Comment=Tray icon to turn the display off while keeping the computer awake\n\
             Exec={}\n\
             Icon=sleepless\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n",
            exec_quote(&exe.display().to_string())
        );
        fs::create_dir_all(
            path.parent()
                .ok_or_else(|| io::Error::other("no directory for the autostart file"))?,
        )?;
        fs::write(&path, entry)
    }
}

pub use imp::{is_enabled, set};
