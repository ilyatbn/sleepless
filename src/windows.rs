// Windows backend
//
// Temporarily disables sleep + screen-off timeouts and shows the blank
// screensaver. Once you come back and dismiss the screensaver, it puts
// your original power settings back and exits.
//
// No admin rights required: changing the timeouts of the *active* power
// scheme does not require elevation (that's why the Windows power slider
// doesn't prompt for UAC either). The same goes for --asroot, which only
// adds the "When I close the lid" action to the settings it changes.

use std::env;
use std::ffi::c_void;
use std::ptr;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{HANDLE, HLOCAL, HWND};
use windows_sys::Win32::System::Console::{
    AllocConsole, AttachConsole, GetStdHandle, WriteConsoleW, ATTACH_PARENT_PROCESS,
    STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Power::{
    PowerGetActiveScheme, PowerReadACValueIndex, PowerReadDCValueIndex, PowerSetActiveScheme,
    PowerWriteACValueIndex, PowerWriteDCValueIndex,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_READ, KEY_WRITE, REG_SZ,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SendMessageW, SystemParametersInfoW, HWND_BROADCAST, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE,
    SPI_GETSCREENSAVEACTIVE, SPI_GETSCREENSAVERRUNNING, SPI_SETSCREENSAVEACTIVE, WM_SYSCOMMAND,
};

// SC_SCREENSAVE isn't exposed by windows-sys (it lives in the SC_* window
// menu command range, not the SPI_* table), so it's declared here directly.
// Value is stable ABI, documented in winuser.h.
const SC_SCREENSAVE: usize = 0xF140;

// ---------------------------------------------------------------------
// Power-setting GUIDs. These identify the "Sleep" and "Display" timeout
// sliders in Windows' power plan UI. They're stable, documented constants
// (winnt.h / powrprof.h) but aren't bundled as constants in windows-sys,
// so they're reproduced here from their published GUID values.
// ---------------------------------------------------------------------
const GUID_SLEEP_SUBGROUP: GUID = GUID {
    data1: 0x238C_9FA8,
    data2: 0x0AAD,
    data3: 0x41ED,
    data4: [0x83, 0xF4, 0x97, 0xBE, 0x24, 0x2C, 0x8F, 0x20],
};
const GUID_STANDBY_IDLE: GUID = GUID {
    data1: 0x29F6_C1DB,
    data2: 0x86DA,
    data3: 0x48C5,
    data4: [0x9F, 0xDB, 0xF2, 0xB6, 0x7B, 0x1F, 0x44, 0xDA],
};
const GUID_VIDEO_SUBGROUP: GUID = GUID {
    data1: 0x7516_B95F,
    data2: 0xF776,
    data3: 0x4464,
    data4: [0x8C, 0x53, 0x06, 0x16, 0x7F, 0x40, 0xCC, 0x99],
};
const GUID_VIDEO_POWERDOWN_TIMEOUT: GUID = GUID {
    data1: 0x3C0B_C021,
    data2: 0xC8A8,
    data3: 0x4E07,
    data4: [0xA9, 0x73, 0x6B, 0x14, 0xCB, 0xCB, 0x2B, 0x7E],
};
// "Power buttons and lid" > "Lid close action" (0 = Do nothing), used by
// --asroot.
const GUID_SYSTEM_BUTTON_SUBGROUP: GUID = GUID {
    data1: 0x4F97_1E89,
    data2: 0xEEBD,
    data3: 0x4455,
    data4: [0xA8, 0xDE, 0x9E, 0x59, 0x04, 0x0E, 0x73, 0x47],
};
const GUID_LIDSWITCH_ACTION: GUID = GUID {
    data1: 0x5CA8_3367,
    data2: 0x6E45,
    data3: 0x459F,
    data4: [0xA2, 0x7B, 0x47, 0x6B, 0x1D, 0x01, 0xC9, 0x36],
};

// HANDLE is a raw pointer (*mut c_void), which isn't Send/Sync, so it can't
// live in a static directly. Stash it as a plain pointer-sized integer
// instead and cast back to HANDLE only at the point of use.
static CONSOLE_OUT: OnceLock<isize> = OnceLock::new();

pub fn debug_log(msg: &str) {
    let Some(&addr) = CONSOLE_OUT.get() else {
        return;
    };
    if addr == 0 {
        return;
    }
    let handle: HANDLE = addr as HANDLE;
    let text: Vec<u16> = format!("{msg}\r\n").encode_utf16().collect();
    let mut written: u32 = 0;
    unsafe {
        WriteConsoleW(
            handle,
            text.as_ptr(),
            text.len() as u32,
            &mut written,
            ptr::null_mut(),
        );
    }
}

/// Wires up console output when /debug is passed. Reuses the console of
/// whoever launched us (cmd.exe) if there is one; otherwise opens a fresh
/// one so /debug still shows something even when double-clicked.
pub fn init_logging(debug: bool) {
    if !debug {
        return;
    }
    unsafe {
        let attached = AttachConsole(ATTACH_PARENT_PROCESS) != 0;
        if !attached {
            AllocConsole();
        }
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let _ = CONSOLE_OUT.set(handle as isize);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------------
// Power plan helpers
// ---------------------------------------------------------------------

fn get_active_scheme() -> GUID {
    unsafe {
        let mut guid_ptr: *mut GUID = ptr::null_mut();
        let err = PowerGetActiveScheme(ptr::null_mut(), &mut guid_ptr);
        if err != 0 || guid_ptr.is_null() {
            log!("PowerGetActiveScheme failed (error {err})");
            eprintln!("Sleepless: could not read the active power scheme (error {err})");
            std::process::exit(1);
        }
        let guid = *guid_ptr;
        windows_sys::Win32::Foundation::LocalFree(guid_ptr as HLOCAL);
        guid
    }
}

fn read_index(scheme: &GUID, subgroup: &GUID, setting: &GUID) -> (u32, u32) {
    let mut ac: u32 = 0;
    let mut dc: u32 = 0;
    unsafe {
        PowerReadACValueIndex(ptr::null_mut(), scheme, subgroup, setting, &mut ac);
        PowerReadDCValueIndex(ptr::null_mut(), scheme, subgroup, setting, &mut dc);
    }
    (ac, dc)
}

fn write_index(scheme: &GUID, subgroup: &GUID, setting: &GUID, ac: u32, dc: u32) {
    unsafe {
        PowerWriteACValueIndex(ptr::null_mut(), scheme, subgroup, setting, ac);
        PowerWriteDCValueIndex(ptr::null_mut(), scheme, subgroup, setting, dc);
        // Re-applying the (same) active scheme is what makes the new
        // index values actually take effect immediately.
        PowerSetActiveScheme(ptr::null_mut(), scheme);
    }
}

// ---------------------------------------------------------------------
// Registry helpers (HKCU\Control Panel\Desktop\SCRNSAVE.EXE)
// ---------------------------------------------------------------------

fn open_desktop_key(access: u32) -> Option<HKEY> {
    let subkey = wide("Control Panel\\Desktop");
    let mut hkey: HKEY = ptr::null_mut();
    let result = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, access, &mut hkey) };
    if result == 0 {
        Some(hkey)
    } else {
        None
    }
}

fn read_string_value(hkey: HKEY, name: &str) -> Option<String> {
    let name_w = wide(name);
    let mut buf = [0u8; 4096];
    let mut buf_len: u32 = buf.len() as u32;
    let mut value_type: u32 = 0;
    let result = unsafe {
        RegQueryValueExW(
            hkey,
            name_w.as_ptr(),
            ptr::null(),
            &mut value_type,
            buf.as_mut_ptr(),
            &mut buf_len,
        )
    };
    if result != 0 || value_type != REG_SZ {
        return None;
    }
    let u16_len = (buf_len as usize) / 2;
    let slice: &[u16] = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u16, u16_len) };
    Some(
        String::from_utf16_lossy(slice)
            .trim_end_matches('\0')
            .to_string(),
    )
}

fn write_string_value(hkey: HKEY, name: &str, value: &str) {
    let name_w = wide(name);
    let value_w = wide(value);
    let bytes = (value_w.len() * 2) as u32;
    unsafe {
        RegSetValueExW(
            hkey,
            name_w.as_ptr(),
            0,
            REG_SZ,
            value_w.as_ptr() as *const u8,
            bytes,
        );
    }
}

// ---------------------------------------------------------------------
// Screensaver helpers
// ---------------------------------------------------------------------

fn screensaver_is_running() -> bool {
    let mut running: i32 = 0;
    unsafe {
        SystemParametersInfoW(
            SPI_GETSCREENSAVERRUNNING,
            0,
            &mut running as *mut _ as *mut c_void,
            0,
        );
    }
    running != 0
}

pub fn run(opts: crate::Options) {
    // --- 1. Save current sleep / screen-off timeouts, then disable them ---
    log!("Reading the active power scheme...");
    let scheme = get_active_scheme();

    let (sleep_ac, sleep_dc) = read_index(&scheme, &GUID_SLEEP_SUBGROUP, &GUID_STANDBY_IDLE);
    let (video_ac, video_dc) =
        read_index(&scheme, &GUID_VIDEO_SUBGROUP, &GUID_VIDEO_POWERDOWN_TIMEOUT);
    log!("Current sleep timeout:      AC={sleep_ac}s  DC={sleep_dc}s");
    log!("Current screen-off timeout: AC={video_ac}s  DC={video_dc}s");

    log!("Setting sleep and screen-off timeouts to Never...");
    write_index(&scheme, &GUID_SLEEP_SUBGROUP, &GUID_STANDBY_IDLE, 0, 0);
    write_index(
        &scheme,
        &GUID_VIDEO_SUBGROUP,
        &GUID_VIDEO_POWERDOWN_TIMEOUT,
        0,
        0,
    );

    let lid = opts.asroot.then(|| {
        let (lid_ac, lid_dc) = read_index(&scheme, &GUID_SYSTEM_BUTTON_SUBGROUP, &GUID_LIDSWITCH_ACTION);
        log!("Current lid close action: AC={lid_ac} DC={lid_dc} (0=nothing 1=sleep 2=hibernate 3=shut down)");
        log!("Setting lid close action to Do nothing...");
        write_index(&scheme, &GUID_SYSTEM_BUTTON_SUBGROUP, &GUID_LIDSWITCH_ACTION, 0, 0);
        (lid_ac, lid_dc)
    });

    // --- 2. Start the blank screensaver ---
    log!("Saving current screensaver executable...");
    let original_screensaver = open_desktop_key(KEY_READ).and_then(|hkey| {
        let val = read_string_value(hkey, "SCRNSAVE.EXE");
        unsafe { RegCloseKey(hkey) };
        val
    });
    log!("Previous SCRNSAVE.EXE value: {:?}", original_screensaver);

    let mut screensaver_was_active: i32 = 0;
    unsafe {
        SystemParametersInfoW(
            SPI_GETSCREENSAVEACTIVE,
            0,
            &mut screensaver_was_active as *mut _ as *mut c_void,
            0,
        );
    }
    log!(
        "Screensaver was previously active: {}",
        screensaver_was_active != 0
    );

    let blank_saver = env::var("WINDIR")
        .map(|w| format!("{w}\\System32\\scrnsave.scr"))
        .unwrap_or_else(|_| "C:\\Windows\\System32\\scrnsave.scr".to_string());
    log!("Pointing SCRNSAVE.EXE at {blank_saver}");

    if let Some(hkey) = open_desktop_key(KEY_WRITE) {
        write_string_value(hkey, "SCRNSAVE.EXE", &blank_saver);
        unsafe { RegCloseKey(hkey) };
    } else {
        log!("Could not open Control Panel\\Desktop for writing.");
    }

    unsafe {
        SystemParametersInfoW(
            SPI_SETSCREENSAVEACTIVE,
            1,
            ptr::null_mut(),
            SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
        );
    }

    log!("Triggering the screensaver...");
    unsafe {
        SendMessageW(HWND_BROADCAST as HWND, WM_SYSCOMMAND, SC_SCREENSAVE, 0);
    }

    // Give it a moment to actually start before we start watching for it
    // to stop - otherwise we'd race SPI_GETSCREENSAVERRUNNING and see
    // "not running" before it ever begins.
    log!("Waiting for the screensaver to report as running...");
    let mut waited_ms = 0u32;
    while !screensaver_is_running() && waited_ms < 5000 {
        thread::sleep(Duration::from_millis(100));
        waited_ms += 100;
    }
    if screensaver_is_running() {
        log!("Screensaver is running.");
    } else {
        log!("Screensaver did not report as running after 5s - continuing anyway.");
    }

    // --- 3. Wait for the screensaver to end, then revert everything ---
    log!("Waiting for the screensaver to stop (move the mouse / press a key)...");
    let mut ticks = 0u32;
    while screensaver_is_running() {
        thread::sleep(Duration::from_secs(1));
        ticks += 1;
        if ticks % 30 == 0 {
            log!("...still waiting ({ticks}s elapsed)");
        }
    }
    log!("Screensaver has stopped.");

    log!("Restoring original power settings...");
    write_index(
        &scheme,
        &GUID_SLEEP_SUBGROUP,
        &GUID_STANDBY_IDLE,
        sleep_ac,
        sleep_dc,
    );
    write_index(
        &scheme,
        &GUID_VIDEO_SUBGROUP,
        &GUID_VIDEO_POWERDOWN_TIMEOUT,
        video_ac,
        video_dc,
    );
    log!("Power settings restored (sleep AC={sleep_ac}s DC={sleep_dc}s, screen AC={video_ac}s DC={video_dc}s)");
    if let Some((lid_ac, lid_dc)) = lid {
        write_index(
            &scheme,
            &GUID_SYSTEM_BUTTON_SUBGROUP,
            &GUID_LIDSWITCH_ACTION,
            lid_ac,
            lid_dc,
        );
        log!("Lid close action restored (AC={lid_ac} DC={lid_dc})");
    }

    log!("Restoring screensaver state...");
    unsafe {
        SystemParametersInfoW(
            SPI_SETSCREENSAVEACTIVE,
            screensaver_was_active as u32,
            ptr::null_mut(),
            SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
        );
    }
    if let Some(hkey) = open_desktop_key(KEY_WRITE) {
        if let Some(path) = &original_screensaver {
            write_string_value(hkey, "SCRNSAVE.EXE", path);
        }
        unsafe { RegCloseKey(hkey) };
    }
}
