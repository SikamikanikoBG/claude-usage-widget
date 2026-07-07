// Single-instance guard: prevents two copies of the widget running at once
// (e.g. a manual double-click while "Start with Windows" also launches one),
// which would otherwise create duplicate tray icons and fight over the same
// `NotifyIconSettings` registry entry.
//
// Implemented as a named Win32 mutex held for the lifetime of the process.
// The mutex name is fixed and app-specific; the `Global\` prefix makes the
// check work across sessions (e.g. Remote Desktop / fast user switching),
// not just within one logon session.

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows_sys::Win32::System::Threading::CreateMutexW;

const MUTEX_NAME: &str = "Global\\ClaudeUsageWidget_SingleInstance";

/// Holds the mutex handle for the process lifetime. Keep this bound in
/// `main` for as long as the app runs; dropping it early releases the
/// mutex and would let a second instance slip in.
pub struct SingleInstanceGuard {
    handle: HANDLE,
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}

fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Attempts to acquire the single-instance guard.
///
/// - `Ok(guard)`: this is the only instance; keep `guard` alive for the
///   process lifetime.
/// - `Err(())`: another instance already holds the mutex; the caller should
///   exit immediately without creating any UI or spawning the worker thread.
///
/// If the mutex simply can't be created for some unrelated OS reason (rare),
/// this fails open (`Ok`, with a warning logged) rather than refusing to
/// start the whole app over a guard that couldn't be set up.
pub fn acquire() -> Result<SingleInstanceGuard, ()> {
    let name = wide_null(MUTEX_NAME);

    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };

    if handle.is_null() {
        eprintln!(
            "[claude-usage-widget] could not create single-instance mutex ({}); continuing without the guard",
            std::io::Error::last_os_error()
        );
        return Ok(SingleInstanceGuard {
            handle: std::ptr::null_mut(),
        });
    }

    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(handle) };
        return Err(());
    }

    Ok(SingleInstanceGuard { handle })
}
