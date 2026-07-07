// Minimal Win32 balloon/toast notifications via `Shell_NotifyIconW`,
// independent of the `tray-icon` crate (which has no notification API of
// its own). Deliberately not reusing the main tray icon's own (hWnd, uID)
// pair to show the balloon "in place" -- that pair is a private
// implementation detail of the tray-icon crate (an internal atomic counter),
// not part of its public API, so we don't build on something that could
// silently break on a crate upgrade.
//
// Trade-off: because this registers its own short-lived notification-icon
// entry rather than reusing the app's permanent one, a second tray icon can
// flash briefly (a few seconds) alongside the main one while the balloon is
// showing. Given how rarely this fires (once per 90%-threshold crossing),
// that's an acceptable trade for staying decoupled from crate internals
// instead of guessing at them.

use std::time::Duration;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIIF_INFO, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, LoadIconW, RegisterClassW, CW_USEDEFAULT,
    IDI_INFORMATION, WNDCLASSW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

const CLASS_NAME: &str = "ClaudeUsageWidgetNotifyWindow";

/// How long to keep the ephemeral notification icon registered before
/// removing it again; long enough for Windows to show (and, in the common
/// case, auto-dismiss) the balloon/toast.
const BALLOON_LIFETIME: Duration = Duration::from_secs(8);

fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Copies as much of `s` (UTF-16, null-terminated) as fits into `dest`,
/// truncating rather than overflowing if it's longer than the fixed-size
/// buffer Windows gives balloon text.
fn copy_wide_into(dest: &mut [u16], s: &str) {
    let wide = wide_null(s);
    let n = wide.len().min(dest.len());
    dest[..n].copy_from_slice(&wide[..n]);
    if n == dest.len() && n > 0 {
        // Ensure null-termination even when truncated.
        dest[n - 1] = 0;
    }
}

/// Shows a Windows balloon/toast notification with the given title and
/// message. Runs on its own background thread and never panics; any failure
/// is logged to stderr and otherwise ignored (a missed notification isn't
/// worth crashing the widget over).
pub fn show_balloon(title: String, message: String) {
    std::thread::spawn(move || unsafe {
        let class_name = wide_null(CLASS_NAME);
        let hinstance = GetModuleHandleW(std::ptr::null());

        let wnd_class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            lpszClassName: class_name.as_ptr(),
            hInstance: hinstance,
            ..std::mem::zeroed()
        };
        // Ignore the result: if a previous notification already registered
        // this class in this process, RegisterClassW just fails harmlessly
        // and the existing registration is reused.
        RegisterClassW(&wnd_class);

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name.as_ptr(),
            std::ptr::null(),
            WS_OVERLAPPED,
            CW_USEDEFAULT,
            0,
            CW_USEDEFAULT,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            eprintln!(
                "[claude-usage-widget] could not create notification window: {}",
                std::io::Error::last_os_error()
            );
            return;
        }

        let hicon = LoadIconW(std::ptr::null_mut(), IDI_INFORMATION);

        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_INFO,
            hIcon: hicon,
            dwInfoFlags: NIIF_INFO,
            ..std::mem::zeroed()
        };
        copy_wide_into(&mut nid.szInfo, &message);
        copy_wide_into(&mut nid.szInfoTitle, &title);

        if Shell_NotifyIconW(NIM_ADD, &nid) == 0 {
            eprintln!(
                "[claude-usage-widget] could not show balloon notification: {}",
                std::io::Error::last_os_error()
            );
            DestroyWindow(hwnd);
            return;
        }

        std::thread::sleep(BALLOON_LIFETIME);

        Shell_NotifyIconW(NIM_DELETE, &nid);
        DestroyWindow(hwnd);
    });
}
