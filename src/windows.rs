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
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
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

/// Bounds a character count returned by a Win32 text getter to the buffer that
/// was handed to it.
///
/// `GetDlgItemTextW` documents 0 on failure but can report more characters than
/// it actually copied when the buffer is too small; clamping is what stops that
/// slicing past the end of the buffer. A panic there would end the process, and
/// on the lock screen that means the desktop uncovering itself with the password
/// never asked - the one outcome this feature exists to prevent.
fn utf16_prefix(buf: &[u16], len: u32) -> &[u16] {
    let len = (len as usize).min(buf.len());
    &buf[..len]
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

// ---------------------------------------------------------------------
// Asking for the credential
//
// A small popup window with a password box. It isn't a real dialog (no
// resource template), but IsDialogMessageW gives the message loop what a
// dialog would: Tab moves between the boxes, Return presses the default
// button (which is how "Enter" submits the password) and Escape cancels.
// ---------------------------------------------------------------------

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::lock::{self, Secret};
use crate::store;
use windows_sys::Win32::Graphics::Gdi::{GetStockObject, DEFAULT_GUI_FONT};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetDlgItemTextW, GetMessageW,
    GetSystemMetrics, IsDialogMessageW, LoadCursorW, MessageBoxW, PostQuitMessage,
    RegisterClassExW, SendDlgItemMessageW, SetForegroundWindow, TranslateMessage, BS_DEFPUSHBUTTON,
    BS_PUSHBUTTON, ES_AUTOHSCROLL, ES_PASSWORD, IDCANCEL, IDC_ARROW, IDOK, IDYES,
    MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_YESNO, MSG, SM_CXSCREEN, SM_CYSCREEN, WM_CLOSE,
    WM_COMMAND, WM_DESTROY, WM_SETFONT, WNDCLASSEXW, WS_BORDER, WS_CAPTION, WS_CHILD,
    WS_EX_CONTROLPARENT, WS_POPUP, WS_VISIBLE,
};

/// Window class for our password prompt; registered on first use.
static PROMPT_CLASS: AtomicBool = AtomicBool::new(false);

// Control ids. IDOK / IDCANCEL are the values IsDialogMessageW uses for
// Return and Escape, so the buttons are deliberately given those ids.
const IDC_EDIT: i32 = 0x100;
const PROMPT_CLASS_NAME: &str = "SleeplessPrompt";

// Filled in by the window procedure while the prompt's message loop runs.
thread_local! {
    static PROMPT_ANSWER: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn edit_text(parent: HWND) -> String {
    let mut buf = [0u16; 512];
    let len = unsafe { GetDlgItemTextW(parent, IDC_EDIT, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(utf16_prefix(&buf, len))
}

unsafe extern "system" fn prompt_proc(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize {
    match msg {
        WM_COMMAND => {
            let id = (wparam & 0xFFFF) as i32;
            if id == IDOK || id == IDCANCEL {
                if id == IDOK {
                    PROMPT_ANSWER.with(|answer| *answer.borrow_mut() = Some(edit_text(hwnd)));
                }
                PostQuitMessage(0);
                return 0;
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        // The close box, Alt+F4: same as pressing Cancel.
        WM_CLOSE => {
            PostQuitMessage(0);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn register_prompt_class() {
    if PROMPT_CLASS.load(Ordering::SeqCst) {
        return;
    }
    let class_name = wide(PROMPT_CLASS_NAME);
    let cursor = unsafe { LoadCursorW(ptr::null_mut(), IDC_ARROW) };
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: 0,
        lpfnWndProc: Some(prompt_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: ptr::null_mut(),
        hIcon: ptr::null_mut(),
        hCursor: cursor,
        // (COLOR_WINDOW + 1): the plain window background, as Win32 wants the
        // brush passed as a pseudo-handle in this field.
        hbrBackground: (6isize) as _,
        lpszMenuName: ptr::null(),
        lpszClassName: class_name.as_ptr(),
        hIconSm: ptr::null_mut(),
    };
    if unsafe { RegisterClassExW(&wc) } != 0 {
        PROMPT_CLASS.store(true, Ordering::SeqCst);
    }
}

/// Child control helper: create it, then give it the system font.
unsafe fn add_control(
    parent: HWND,
    class: &str,
    text: &str,
    style: u32,
    id: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> HWND {
    let class_w = wide(class);
    let text_w = wide(text);
    let hwnd = CreateWindowExW(
        0,
        class_w.as_ptr(),
        text_w.as_ptr(),
        style | WS_CHILD | WS_VISIBLE,
        x,
        y,
        w,
        h,
        parent,
        id as *mut c_void,
        ptr::null_mut(),
        ptr::null(),
    );
    if !hwnd.is_null() {
        let font = GetStockObject(DEFAULT_GUI_FONT) as usize;
        SendDlgItemMessageW(parent, id, WM_SETFONT, font, 1);
    }
    hwnd
}

/// Asks for a secret. None when the user cancelled.
pub fn prompt_secret(title: &str, message: &str) -> Option<Secret> {
    unsafe {
        register_prompt_class();
        PROMPT_ANSWER.with(|answer| *answer.borrow_mut() = None);

        let title_w = wide(title);
        let width: i32 = 420;
        let height: i32 = 170;
        let x = (GetSystemMetrics(SM_CXSCREEN) - width) / 2;
        let y = (GetSystemMetrics(SM_CYSCREEN) - height) / 2;

        let parent = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            wide(PROMPT_CLASS_NAME).as_ptr(),
            title_w.as_ptr(),
            WS_POPUP | WS_CAPTION | WS_VISIBLE,
            x,
            y,
            width,
            height,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null(),
        );
        if parent.is_null() {
            return None;
        }

        add_control(parent, "STATIC", message, 0, 0, 16, 16, width - 32, 24);
        let edit = add_control(
            parent,
            "EDIT",
            "",
            WS_BORDER | (ES_AUTOHSCROLL | ES_PASSWORD) as u32,
            IDC_EDIT,
            16,
            48,
            width - 32,
            28,
        );
        add_control(
            parent,
            "BUTTON",
            "OK",
            BS_DEFPUSHBUTTON as u32,
            IDOK,
            width - 190,
            height - 90,
            80,
            30,
        );
        add_control(
            parent,
            "BUTTON",
            "Cancel",
            BS_PUSHBUTTON as u32,
            IDCANCEL,
            width - 100,
            height - 90,
            80,
            30,
        );

        SetFocus(edit);
        SetForegroundWindow(parent);

        // Message loop for this window only, so the tray / other windows of
        // the process are left alone.
        let mut msg: MSG = std::mem::zeroed();
        loop {
            let got = GetMessageW(&mut msg, ptr::null_mut(), 0, 0);
            if got <= 0 {
                break;
            }
            if IsDialogMessageW(parent, &mut msg) != 0 {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let answer = PROMPT_ANSWER.with(|answer| answer.borrow_mut().take());
        DestroyWindow(parent);
        answer.map(|text| Secret::new(text.trim_end()))
    }
}

/// One-line result message for the credential commands.
pub fn report(message: &str) {
    log!("Report to the user: {}", message.replace('\n', " "));
    notify("Sleepless", message);
}

/// Enrolment asks through the same dialog: there's no terminal path here.
pub fn prompt_secret_interactive(title: &str, message: &str) -> Option<Secret> {
    prompt_secret(title, message)
}

/// No platform passkey on Windows: the only local option is the WebAuthn C
/// API, which means parsing the attestation/assertion blobs and verifying an
/// ECDSA signature by hand - none of which can be checked anywhere but the
/// device, so it isn't shipped. The password carries the lock screen alone.
pub fn passkey_supported() -> Option<&'static str> {
    None
}

pub fn passkey_enroll() -> Result<&'static str, String> {
    Err("Sleepless has no passkey support on Windows yet, so use the password".to_string())
}

// ---------------------------------------------------------------------
// The lock screen
//
// With a credential enrolled, a black topmost window covers the whole desktop
// (every monitor) *before* the blank screensaver is started. The screensaver
// still does its usual job, but the first keypress that ends it now reveals
// this window instead of the desktop, so nothing flashes into view. The
// password box only appears once we're actually locked; while the saver is up
// the window is just black.
//
// It deliberately isn't a security boundary: the process can always be killed
// (that's what the power settings restore point in src/store.rs is for), and
// the OS lock screen stays the real one.
// ---------------------------------------------------------------------

use std::cell::Cell;

use windows_sys::Win32::Foundation::{POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontIndirectW, CreateSolidBrush, DeleteObject, EndPaint, GetMonitorInfoW,
    GetTextExtentPoint32W, InvalidateRect, MonitorFromPoint, SelectObject, SetBkMode, SetTextColor,
    TextOutW, HDC, HFONT, LOGFONTW, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
    TRANSPARENT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetCursorPos, GetDlgItem, GetForegroundWindow, KillTimer, SetTimer,
    SetWindowPos, SetWindowTextW, ShowWindow, ES_CENTER, HWND_TOPMOST, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SW_HIDE, SW_SHOW, WM_PAINT, WM_TIMER, WS_EX_TOOLWINDOW,
};

const LOCK_CLASS_NAME: &str = "SleeplessLock";
/// The password box on the lock screen.
const IDC_LOCK_EDIT: i32 = 0x200;
/// Id for the timer that keeps the lock screen on top.
const LOCK_TIMER: usize = 0x5c0c;
const LOCK_TIMER_MS: u32 = 200;
/// COLORREF for the text drawn on the black overlay.
const RGB_WHITE: u32 = 0x00ff_ffff;

/// True while the password prompt is up.
static LOCKED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// The line drawn above the password box: why we're asking.
    static LOCK_STATUS: RefCell<String> = const { RefCell::new(String::new()) };
    /// Fonts for it, created with the overlay and freed with it.
    static LOCK_FONTS: RefCell<Option<(HFONT, HFONT)>> = const { RefCell::new(None) };
    /// Set by the window procedure when the user presses Enter / Unlock.
    static LOCK_SUBMITTED: Cell<bool> = const { Cell::new(false) };
}

fn make_font(height: i32) -> HFONT {
    let mut face = [0u16; 32];
    for (slot, unit) in face
        .iter_mut()
        .zip("Segoe UI".encode_utf16().chain(std::iter::once(0)))
    {
        *slot = unit;
    }
    let mut logfont: LOGFONTW = unsafe { std::mem::zeroed() };
    logfont.lfHeight = height;
    logfont.lfWeight = 400; // FW_NORMAL
    logfont.lfCharSet = 0; // DEFAULT_CHARSET
    logfont.lfFaceName = face;
    unsafe { CreateFontIndirectW(&logfont) }
}

/// Draws `text` in white, horizontally centred on the window's client area.
unsafe fn draw_centered(hdc: HDC, hwnd: HWND, font: HFONT, text: &str, y: i32) {
    let mut bounds: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut bounds);
    let units = wide(text);
    let len = units.len().saturating_sub(1) as i32;
    let mut size = SIZE { cx: 0, cy: 0 };
    GetTextExtentPoint32W(hdc, units.as_ptr(), len, &mut size);
    SelectObject(hdc, font);
    SetTextColor(hdc, RGB_WHITE);
    SetBkMode(hdc, TRANSPARENT as i32);
    TextOutW(hdc, (bounds.right - size.cx) / 2, y, units.as_ptr(), len);
}

unsafe extern "system" fn lock_proc(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            if LOCKED.load(Ordering::SeqCst) {
                let status = LOCK_STATUS.with(|s| s.borrow().clone());
                let fonts = LOCK_FONTS.with(|fonts| *fonts.borrow());
                if let Some((title_font, text_font)) = fonts {
                    draw_centered(hdc, hwnd, title_font, "Sleepless", 70);
                    draw_centered(hdc, hwnd, text_font, &status, 120);
                }
            }
            EndPaint(hwnd, &ps);
            0
        }
        WM_COMMAND => {
            // Only the default button (and therefore Enter) gets here. Escape
            // arrives as IDCANCEL and falls through to the default handler,
            // because dismissing the prompt must not be a way past it.
            if (wparam & 0xFFFF) as i32 == IDOK {
                LOCK_SUBMITTED.with(|submitted| submitted.set(true));
                PostQuitMessage(0);
            }
            0
        }
        WM_CLOSE => {
            if LOCKED.load(Ordering::SeqCst) {
                0 // Alt+F4 is not an escape route either
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_TIMER => {
            if LOCKED.load(Ordering::SeqCst) {
                keep_on_top(hwnd);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Keeps the lock screen above whatever the desktop tries to put in front of
/// it: the taskbar, a notification, an app stealing the foreground.
unsafe fn keep_on_top(hwnd: HWND) {
    SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
    if GetForegroundWindow() != hwnd {
        SetForegroundWindow(hwnd);
    }
}

fn register_lock_class() -> bool {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.load(Ordering::SeqCst) {
        return true;
    }
    let class_name = wide(LOCK_CLASS_NAME);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: 0,
        lpfnWndProc: Some(lock_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: ptr::null_mut(),
        hIcon: ptr::null_mut(),
        hCursor: unsafe { LoadCursorW(ptr::null_mut(), IDC_ARROW) },
        // Solid black background: shows nothing but black, which on an OLED
        // means the pixels are off. Created once for the class's lifetime.
        hbrBackground: unsafe { CreateSolidBrush(0) } as _,
        lpszMenuName: ptr::null(),
        lpszClassName: class_name.as_ptr(),
        hIconSm: ptr::null_mut(),
    };
    let ok = unsafe { RegisterClassExW(&wc) } != 0;
    if ok {
        DONE.store(true, Ordering::SeqCst);
    }
    ok
}

/// The black overlay over the whole virtual desktop, with the password box
/// and Unlock button hidden until they're needed.
fn lock_overlay_new() -> Option<HWND> {
    if !register_lock_class() {
        log!("Could not register the lock screen window class.");
        return None;
    }
    unsafe {
        LOCK_FONTS.with(|fonts| *fonts.borrow_mut() = Some((make_font(-30), make_font(-20))));

        let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if width <= 0 || height <= 0 {
            log!("Could not measure the desktop; no lock screen window.");
            return None;
        }

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            wide(LOCK_CLASS_NAME).as_ptr(),
            wide("Sleepless").as_ptr(),
            WS_POPUP | WS_VISIBLE,
            left,
            top,
            width,
            height,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null(),
        );
        if hwnd.is_null() {
            log!("Could not create the lock screen window.");
            return None;
        }
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );

        // The prompt goes on the monitor the cursor is on.
        let mut point: POINT = std::mem::zeroed();
        GetCursorPos(&mut point);
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let on_screen = GetMonitorInfoW(monitor, &mut info) != 0;
        let (centre, monitor_width) = if on_screen {
            (
                (info.rcMonitor.left + info.rcMonitor.right) / 2 - left,
                info.rcMonitor.right - info.rcMonitor.left,
            )
        } else {
            (width / 2, width)
        };

        add_hidden_control(
            hwnd,
            "EDIT",
            "",
            WS_BORDER | (ES_AUTOHSCROLL | ES_PASSWORD | ES_CENTER) as u32,
            IDC_LOCK_EDIT,
            centre - 140,
            170,
            280,
            32,
        );
        add_hidden_control(
            hwnd,
            "BUTTON",
            "Unlock",
            BS_DEFPUSHBUTTON as u32,
            IDOK,
            centre - 60,
            214,
            120,
            34,
        );
        log!(
            "Lock screen window ready over {width}x{height} at ({left},{top}); \
             prompt centred at x={centre} on a {monitor_width}px display."
        );
        Some(hwnd)
    }
}

/// Like add_control, but the control stays hidden until the prompt is shown.
unsafe fn add_hidden_control(
    parent: HWND,
    class: &str,
    text: &str,
    style: u32,
    id: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> HWND {
    let hwnd = add_control(parent, class, text, style, id, x, y, w, h);
    if !hwnd.is_null() {
        ShowWindow(hwnd, SW_HIDE);
    }
    hwnd
}

/// Shows the password box and waits for it: pumps messages until Enter (or
/// Unlock) and hands back what was typed. None if the window went away under
/// us, which lock::unlock_loop treats as "still locked".
unsafe fn lock_overlay_ask(hwnd: HWND, status: &str) -> Option<Secret> {
    LOCK_STATUS.with(|s| *s.borrow_mut() = status.to_string());
    LOCK_SUBMITTED.with(|submitted| submitted.set(false));
    LOCKED.store(true, Ordering::SeqCst);

    let edit = GetDlgItem(hwnd, IDC_LOCK_EDIT);
    for id in [IDC_LOCK_EDIT, IDOK] {
        let child = GetDlgItem(hwnd, id);
        if !child.is_null() {
            ShowWindow(child, SW_SHOW);
        }
    }
    InvalidateRect(hwnd, ptr::null(), 1);
    SetFocus(edit);
    keep_on_top(hwnd);
    SetTimer(hwnd, LOCK_TIMER, LOCK_TIMER_MS, None);

    let mut msg: MSG = std::mem::zeroed();
    // IsDialogMessageW is what turns Enter into a press of the default
    // button here; there's no dialog resource behind this window.
    while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
        if IsDialogMessageW(hwnd, &mut msg) != 0 {
            continue;
        }
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    KillTimer(hwnd, LOCK_TIMER);

    let submitted = LOCK_SUBMITTED.with(|submitted| submitted.get());
    if !submitted || edit.is_null() {
        return None;
    }
    let mut buf = [0u16; 512];
    let len = GetDlgItemTextW(hwnd, IDC_LOCK_EDIT, buf.as_mut_ptr(), buf.len() as i32);
    let text = String::from_utf16_lossy(utf16_prefix(&buf, len));
    // Never leave the attempt that just failed in the box.
    SetWindowTextW(edit, wide("").as_ptr());
    Some(Secret::new(text.trim_end()))
}

unsafe fn lock_overlay_free(hwnd: HWND) {
    LOCKED.store(false, Ordering::SeqCst);
    ShowWindow(hwnd, SW_HIDE);
    DestroyWindow(hwnd);
    LOCK_FONTS.with(|fonts| {
        if let Some((title, text)) = fonts.borrow_mut().take() {
            DeleteObject(title as _);
            DeleteObject(text as _);
        }
    });
}

// ---------------------------------------------------------------------
// The power-settings restore point
//
// Windows is the one platform where what Sleepless changes lives in the
// system until it puts it back: if the process is killed - which is allowed,
// even documented, as the way out of the lock screen - the timeouts would
// otherwise stay on "Never" forever. So the values are written to the
// registry before they're overwritten and deleted once they're back, and a
// run that finds a leftover restore point puts the settings right first.
// ---------------------------------------------------------------------

const RESTORE_SECTION: &str = "restore";

#[derive(Clone, Copy)]
struct RestorePoint {
    scheme: GUID,
    sleep: (u32, u32),
    video: (u32, u32),
    lid: Option<(u32, u32)>,
}

fn guid_to_string(g: &GUID) -> String {
    let d = &g.data4;
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1, g.data2, g.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
    )
}

fn guid_from_string(text: &str) -> Option<GUID> {
    let text = text.trim().trim_matches('{').trim_matches('}');
    let mut parts = text.split('-');
    let data1 = u32::from_str_radix(parts.next()?, 16).ok()?;
    let data2 = u16::from_str_radix(parts.next()?, 16).ok()?;
    let data3 = u16::from_str_radix(parts.next()?, 16).ok()?;
    let mut digits = String::new();
    digits.push_str(parts.next()?);
    digits.push_str(parts.next()?);
    if digits.len() != 16 || parts.next().is_some() {
        return None;
    }
    let bytes = lock::from_hex(&digits)?;
    let mut data4 = [0u8; 8];
    data4.copy_from_slice(&bytes);
    Some(GUID {
        data1,
        data2,
        data3,
        data4,
    })
}

fn save_restore_point(point: &RestorePoint) -> std::io::Result<()> {
    store::set(RESTORE_SECTION, "scheme", &guid_to_string(&point.scheme))?;
    store::set(RESTORE_SECTION, "sleep", &values(&point.sleep))?;
    store::set(RESTORE_SECTION, "video", &values(&point.video))?;
    if let Some(lid) = point.lid {
        store::set(RESTORE_SECTION, "lid", &values(&lid))?;
    } else {
        store::delete_value(RESTORE_SECTION, "lid");
    }
    Ok(())
}

fn values(pair: &(u32, u32)) -> String {
    format!("{},{}", pair.0, pair.1)
}

fn read_values(section: &str, key: &str) -> Option<(u32, u32)> {
    let text = store::get(section, key)?;
    let (ac, dc) = text.split_once(',')?;
    Some((ac.trim().parse().ok()?, dc.trim().parse().ok()?))
}

fn load_restore_point() -> Option<RestorePoint> {
    let scheme = guid_from_string(&store::get(RESTORE_SECTION, "scheme")?)?;
    Some(RestorePoint {
        scheme,
        sleep: read_values(RESTORE_SECTION, "sleep")?,
        video: read_values(RESTORE_SECTION, "video")?,
        lid: read_values(RESTORE_SECTION, "lid"),
    })
}

fn apply_restore_point(point: &RestorePoint) {
    write_index(
        &point.scheme,
        &GUID_SLEEP_SUBGROUP,
        &GUID_STANDBY_IDLE,
        point.sleep.0,
        point.sleep.1,
    );
    write_index(
        &point.scheme,
        &GUID_VIDEO_SUBGROUP,
        &GUID_VIDEO_POWERDOWN_TIMEOUT,
        point.video.0,
        point.video.1,
    );
    if let Some((ac, dc)) = point.lid {
        write_index(
            &point.scheme,
            &GUID_SYSTEM_BUTTON_SUBGROUP,
            &GUID_LIDSWITCH_ACTION,
            ac,
            dc,
        );
    }
}

/// Puts the power settings back if an earlier run was killed mid-session.
fn heal_stale_restore_point() {
    let Some(point) = load_restore_point() else {
        return;
    };
    log!("Found a restore point from a run that was killed - putting the power settings back first: sleep {:?}, screen {:?}", point.sleep, point.video);
    apply_restore_point(&point);
    if let Err(e) = store::delete(RESTORE_SECTION) {
        log!("Could not remove the restore point: {e}");
    }
}

pub fn notify(title: &str, message: &str) {
    unsafe {
        MessageBoxW(
            ptr::null_mut(),
            wide(message).as_ptr(),
            wide(title).as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

pub fn ask_yes_no(title: &str, message: &str) -> bool {
    unsafe {
        MessageBoxW(
            ptr::null_mut(),
            wide(message).as_ptr(),
            wide(title).as_ptr(),
            MB_YESNO | MB_ICONQUESTION,
        ) == IDYES
    }
}

// ---------------------------------------------------------------------
// The OS taking the session over
//
// When Windows locks the workstation (Win+L, the lock button, a screensaver
// lock policy) Sleepless gets out of the way: the run ends through its normal
// path, so the power settings are put back and the black overlay comes down.
// Locking the machine must always be a way out of Sleepless, including if one
// of our own prompts ever gets stuck.
//
// The notification arrives as a window message, so a message-only watch
// window is created for the whole run (whether or not a password is
// enrolled), and the waiting loops sleep through MsgWaitForMultipleObjectsEx
// so they can see it.
// ---------------------------------------------------------------------

use windows_sys::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MsgWaitForMultipleObjectsEx, PeekMessageW, HWND_MESSAGE, PM_REMOVE, QS_ALLINPUT,
    WM_QUERYENDSESSION, WM_WTSSESSION_CHANGE, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};

const WATCH_CLASS_NAME: &str = "SleeplessWatch";

static SESSION_TAKEN: AtomicBool = AtomicBool::new(false);

fn session_was_taken() -> bool {
    SESSION_TAKEN.load(Ordering::SeqCst)
}

unsafe extern "system" fn watch_proc(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize {
    match msg {
        WM_WTSSESSION_CHANGE => {
            match wparam as u32 {
                WTS_SESSION_LOCK => {
                    log!("Windows locked the session: Sleepless is getting out of the way.");
                    SESSION_TAKEN.store(true, Ordering::SeqCst);
                    // Ends whatever message loop is waiting on a prompt.
                    PostQuitMessage(0);
                }
                WTS_SESSION_UNLOCK => log!("Windows reports the session unlocked again."),
                other => log!("Windows session change: {other}"),
            }
            0
        }
        // Logoff or shutdown: allow it. Anything left undone is picked up by
        // the restore point on the next run.
        WM_QUERYENDSESSION => {
            log!("Logoff or shutdown requested: allowing it.");
            1
        }
        // Every registration needs a matching unregistration before the window
        // goes away, or a later notification is delivered to a dead HWND.
        WM_DESTROY => {
            let _ = WTSUnRegisterSessionNotification(hwnd);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Creates the message-only watch window and subscribes it to session
/// changes. Failure is only worth a log line: the restore point still covers
/// the power settings if we're killed.
fn start_session_watch() {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    let class_name = wide(WATCH_CLASS_NAME);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: 0,
        lpfnWndProc: Some(watch_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: ptr::null_mut(),
        hIcon: ptr::null_mut(),
        hCursor: ptr::null_mut(),
        hbrBackground: ptr::null_mut(),
        lpszMenuName: ptr::null(),
        lpszClassName: class_name.as_ptr(),
        hIconSm: ptr::null_mut(),
    };
    if unsafe { RegisterClassExW(&wc) } == 0 {
        log!("Could not register the session watch window class.");
        return;
    }
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            wide("Sleepless").as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null(),
        )
    };
    if hwnd.is_null() {
        log!("Could not create the session watch window.");
        return;
    }
    let registered = unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) };
    log!("Session watch window ready; WTSRegisterSessionNotification -> {registered}.");
}

/// Runs whatever messages this thread has waiting (session changes, and while
/// locked the lock screen's own messages).
unsafe fn pump_messages() {
    let mut msg: MSG = std::mem::zeroed();
    while unsafe { PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Sleeps up to `ms` but wakes for messages, so a session lock is noticed
/// even while we're just waiting for the screensaver.
fn interruptible_sleep(ms: u32) {
    unsafe {
        MsgWaitForMultipleObjectsEx(0, ptr::null(), ms, QS_ALLINPUT, 0);
        pump_messages();
    }
}

/// Nothing to install here: a Win32 failure returns an error code rather than
/// throwing, so the panic hook in src/logging.rs is the whole story.
pub fn install_crash_reporting() {}

pub fn run(opts: crate::Options) {
    // If an earlier run was killed (which is the documented way out of the
    // lock screen), its power settings are still changed. Put them back first.
    heal_stale_restore_point();

    // Watch for Windows taking the session over: locking the workstation has
    // to end this run, prompt or no prompt.
    start_session_watch();

    // --- 1. Save current sleep / screen-off timeouts, then disable them ---
    log!("Reading the active power scheme...");
    let scheme = get_active_scheme();

    let (sleep_ac, sleep_dc) = read_index(&scheme, &GUID_SLEEP_SUBGROUP, &GUID_STANDBY_IDLE);
    let (video_ac, video_dc) =
        read_index(&scheme, &GUID_VIDEO_SUBGROUP, &GUID_VIDEO_POWERDOWN_TIMEOUT);
    log!("Current sleep timeout:      AC={sleep_ac}s  DC={sleep_dc}s");
    log!("Current screen-off timeout: AC={video_ac}s  DC={video_dc}s");

    let lid = opts.asroot.then(|| {
        let (lid_ac, lid_dc) =
            read_index(&scheme, &GUID_SYSTEM_BUTTON_SUBGROUP, &GUID_LIDSWITCH_ACTION);
        log!("Current lid close action: AC={lid_ac} DC={lid_dc} (0=nothing 1=sleep 2=hibernate 3=shut down)");
        (lid_ac, lid_dc)
    });

    // Write down everything we're about to change, so a killed process
    // doesn't leave the timeouts on "Never" for good.
    let point = RestorePoint {
        scheme,
        sleep: (sleep_ac, sleep_dc),
        video: (video_ac, video_dc),
        lid,
    };
    if let Err(e) = save_restore_point(&point) {
        log!(
            "Could not save the restore point in {}: {e}",
            store::location()
        );
    }

    log!("Setting sleep and screen-off timeouts to Never...");
    write_index(&scheme, &GUID_SLEEP_SUBGROUP, &GUID_STANDBY_IDLE, 0, 0);
    write_index(
        &scheme,
        &GUID_VIDEO_SUBGROUP,
        &GUID_VIDEO_POWERDOWN_TIMEOUT,
        0,
        0,
    );
    if lid.is_some() {
        log!("Setting lid close action to Do nothing...");
        write_index(
            &scheme,
            &GUID_SYSTEM_BUTTON_SUBGROUP,
            &GUID_LIDSWITCH_ACTION,
            0,
            0,
        );
    }

    // --- 2. Black the desktop, then start the blank screensaver ---
    // The overlay goes up first so the keypress that ends the screensaver
    // reveals this instead of the desktop.
    let overlay = if lock::is_enrolled() {
        log!("A password is enrolled: putting the lock screen up first.");
        lock_overlay_new()
    } else {
        None
    };

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
    while !screensaver_is_running() && waited_ms < 5000 && !session_was_taken() {
        interruptible_sleep(100);
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
    while screensaver_is_running() && !session_was_taken() {
        interruptible_sleep(1000);
        ticks += 1;
        if ticks % 30 == 0 {
            log!("...still waiting ({ticks}s elapsed)");
        }
    }
    if session_was_taken() {
        log!("The session was locked while the screensaver was up.");
    }
    log!("Screensaver has stopped.");

    // --- 3.5 The password gate ---
    // The overlay has been covering the desktop the whole time, so nothing
    // was revealed by the screensaver ending; it comes down by itself once
    // the password is right.
    if lock::is_enrolled() {
        log!("Asking for the password before the session ends.");
        match overlay {
            Some(overlay) => {
                let unlocked = lock::unlock_loop(
                    |message| unsafe { lock_overlay_ask(overlay, message) },
                    session_was_taken,
                );
                unsafe { lock_overlay_free(overlay) };
                if unlocked {
                    log!("Password accepted; the lock screen comes down.");
                } else {
                    log!("Gate ended without a password: the session was taken over.");
                }
            }
            // The overlay couldn't be created (or the credential appeared
            // while we were running): fall back to the ordinary password
            // dialog rather than letting the session end unasked.
            None => {
                log!("No lock screen window; asking through the password dialog instead.");
                let unlocked = lock::unlock_loop(
                    |message| prompt_secret("Sleepless", message),
                    session_was_taken,
                );
                if unlocked {
                    log!("Password accepted.");
                } else {
                    log!("Gate ended without a password: the session was taken over.");
                }
            }
        }
    }

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
        match &original_screensaver {
            Some(path) => write_string_value(hkey, "SCRNSAVE.EXE", path),
            // There was no screensaver chosen before us, so "restoring" means
            // removing the one we pointed at - leaving it would quietly enable
            // the blank saver for the user for good.
            None => {
                let name_w = wide("SCRNSAVE.EXE");
                let result = unsafe { RegDeleteValueW(hkey, name_w.as_ptr()) };
                log!("No screensaver was set before: removed SCRNSAVE.EXE (result {result}).");
            }
        }
        unsafe { RegCloseKey(hkey) };
    }

    // Everything is back the way it was, so the safety net isn't needed.
    if let Err(e) = store::delete(RESTORE_SECTION) {
        log!("Could not remove the restore point: {e}");
    }
}
