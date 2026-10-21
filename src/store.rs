// A very small per-user key/value store, used for the unlock credential
// (src/lock.rs) and for the Windows power-settings restore point
// (src/windows.rs).
//
//   Windows:  REG_SZ values under HKCU\Software\Sleepless\<section>
//   macOS:    ~/Library/Application Support/Sleepless/sleepless.conf
//   Linux:    $XDG_CONFIG_HOME/sleepless/sleepless.conf (created 0600)
//
// Values are always short, single-line strings (hex, decimal numbers), so the
// file backend is plain `section.key=value` lines and the registry backend is
// one sub-key per section - which also makes "delete a whole section" a single
// call on both.
//
// $SLEEPLESS_CONFIG_DIR replaces the unix config directory (the unit tests use
// it, and it's handy for trying a build without touching your real settings).

use std::io;

/// Reads one value. None if the section or the key isn't there.
pub fn get(section: &str, key: &str) -> Option<String> {
    backend::get(section, key)
}

/// Writes one value, creating the section if needed.
pub fn set(section: &str, key: &str, value: &str) -> io::Result<()> {
    backend::set(section, key, value)
}

/// Removes a whole section (no error if it wasn't there).
pub fn delete(section: &str) -> io::Result<()> {
    backend::delete(section)
}

/// Removes one value, leaving the rest of the section alone. Only the
/// Windows restore point needs this: the file backend rewrites the whole
/// file on every change.
#[cfg(windows)]
pub fn delete_value(section: &str, key: &str) {
    backend::delete_value(section, key)
}

/// Where the sections live, for error messages.
pub fn location() -> String {
    backend::location()
}

// ---------------------------------------------------------------------
// Windows: the registry
// ---------------------------------------------------------------------

#[cfg(windows)]
mod backend {
    use std::io;
    use std::ptr;

    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW,
        RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    // Our own corner of the registry.
    const SLEEPLESS_ROOT: &str = "Software\\Sleepless";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn path(section: &str) -> String {
        format!("{SLEEPLESS_ROOT}\\{section}")
    }

    pub fn location() -> String {
        format!("HKCU\\{SLEEPLESS_ROOT}")
    }

    fn open(section: &str, access: u32, create: bool) -> io::Result<HKEY> {
        let name = path(section);
        let sub = wide(&name);
        let mut hkey: HKEY = ptr::null_mut();
        let err = unsafe {
            if create {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    sub.as_ptr(),
                    0,
                    ptr::null(),
                    REG_OPTION_NON_VOLATILE,
                    access,
                    ptr::null(),
                    &mut hkey,
                    ptr::null_mut(),
                )
            } else {
                RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, access, &mut hkey)
            }
        };
        if err == 0 {
            Ok(hkey)
        } else {
            Err(io::Error::from_raw_os_error(err as i32))
        }
    }

    pub fn get(section: &str, key: &str) -> Option<String> {
        let hkey = open(section, KEY_READ, false).ok()?;
        let name = wide(key);
        // Big enough for anything we store (a passkey blob included).
        let mut buf = [0u8; 8192];
        let mut len: u32 = buf.len() as u32;
        let mut kind: u32 = 0;
        let err = unsafe {
            RegQueryValueExW(
                hkey,
                name.as_ptr(),
                ptr::null(),
                &mut kind,
                buf.as_mut_ptr(),
                &mut len,
            )
        };
        unsafe { RegCloseKey(hkey) };
        if err != 0 || kind != REG_SZ {
            return None;
        }
        let units = (len as usize / 2).saturating_sub(1);
        let slice = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u16, units) };
        Some(String::from_utf16_lossy(slice))
    }

    pub fn set(section: &str, key: &str, value: &str) -> io::Result<()> {
        let hkey = open(section, KEY_WRITE, true)?;
        let name = wide(key);
        let data = wide(value);
        let err = unsafe {
            RegSetValueExW(
                hkey,
                name.as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                (data.len() * 2) as u32,
            )
        };
        unsafe { RegCloseKey(hkey) };
        if err == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(err as i32))
        }
    }

    pub fn delete(section: &str) -> io::Result<()> {
        let name = path(section);
        let sub = wide(&name);
        let err = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, sub.as_ptr()) };
        // ERROR_FILE_NOT_FOUND: already gone, which is what was asked for.
        if err == 0 || err == 2 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(err as i32))
        }
    }

    pub fn delete_value(section: &str, key: &str) {
        if let Ok(hkey) = open(section, KEY_WRITE, false) {
            let name = wide(key);
            unsafe { RegDeleteValueW(hkey, name.as_ptr()) };
            unsafe { RegCloseKey(hkey) };
        }
    }
}

// ---------------------------------------------------------------------
// macOS / Linux: one settings file
// ---------------------------------------------------------------------

#[cfg(unix)]
mod backend {
    use std::fs;
    use std::io::{self, Write};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;

    fn home() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }

    fn config_dir() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("SLEEPLESS_CONFIG_DIR") {
            if !dir.as_os_str().is_empty() {
                return Some(PathBuf::from(dir));
            }
        }
        if cfg!(target_os = "macos") {
            return Some(home()?.join("Library/Application Support/Sleepless"));
        }
        let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => home()?.join(".config"),
        };
        Some(base.join("sleepless"))
    }

    fn file() -> io::Result<PathBuf> {
        let dir = config_dir().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "could not find the config directory",
            )
        })?;
        fs::create_dir_all(&dir)?;
        Ok(dir.join("sleepless.conf"))
    }

    pub fn location() -> String {
        config_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|| "(no config directory)".to_string())
    }

    fn read_all() -> Vec<(String, String)> {
        let Ok(text) = file().map(|p| fs::read_to_string(p).unwrap_or_default()) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| {
                let (key, value) = line.split_once('=')?;
                let key = key.trim();
                if key.is_empty() || key.starts_with('#') {
                    return None;
                }
                Some((key.to_string(), value.to_string()))
            })
            .collect()
    }

    fn write_all(entries: &[(String, String)]) -> io::Result<()> {
        let path = file()?;
        // A private file: it holds the password hash, and on unix anyone in the
        // group can otherwise read it.
        let mut tmp = path.clone();
        tmp.set_extension("conf.new");
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        for (key, value) in entries {
            writeln!(out, "{key}={value}")?;
        }
        out.flush()?;
        drop(out);
        fs::rename(&tmp, &path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn get(section: &str, key: &str) -> Option<String> {
        read_all()
            .into_iter()
            .find(|(k, _)| k == &format!("{section}.{key}"))
            .map(|(_, v)| v)
    }

    pub fn set(section: &str, key: &str, value: &str) -> io::Result<()> {
        // Values must stay on one line and must not look like a comment.
        let value: String = value.chars().filter(|c| *c != '\n' && *c != '\r').collect();
        let name = format!("{section}.{key}");
        let mut entries = read_all();
        match entries.iter_mut().find(|(k, _)| *k == name) {
            Some(entry) => entry.1 = value,
            None => entries.push((name, value)),
        }
        write_all(&entries)
    }

    pub fn delete(section: &str) -> io::Result<()> {
        let prefix = format!("{section}.");
        let entries: Vec<_> = read_all()
            .into_iter()
            .filter(|(k, _)| !k.starts_with(&prefix))
            .collect();
        write_all(&entries)
    }
}
