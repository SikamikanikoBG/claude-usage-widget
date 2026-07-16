// Sends the widget's diagnostics to a real file instead of nowhere.
//
// This app is built with `#![windows_subsystem = "windows"]`, which means the
// process gets no console -- so every `eprintln!` in this codebase (the poll
// heartbeat, the backoff decisions, the panel's window-creation errors) was
// being written to an invalid handle and silently discarded in the shipped
// binary. All of that careful logging only ever existed for whoever ran the
// exe from a terminal during development, which is precisely NOT the person
// hitting a bug on their own machine.
//
// Rather than convert every call site to a logging macro, this redirects the
// process's stderr *handle* to a log file: Rust's `std::io::Stderr` resolves
// `GetStdHandle(STD_ERROR_HANDLE)` on each write rather than caching it at
// startup, so pointing that handle at a file makes every existing (and
// future) `eprintln!` land in the log with no other code changes.
//
// Deliberately dependency-free and best-effort: if anything here fails, the
// widget carries on exactly as before (silently), because losing logs must
// never be a reason for the app itself not to run.

use std::fs::OpenOptions;
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;

use windows_sys::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE};

/// Rotate once the log passes this size, so a long-running widget can't grow
/// a log file without bound. One previous generation is kept -- enough to
/// cover "it broke, then I restarted it before grabbing the log".
const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// `%LOCALAPPDATA%\ClaudeUsageWidget\widget.log`.
fn log_path() -> Option<PathBuf> {
    let local_app_data = std::env::var_os("LOCALAPPDATA")?;
    let mut path = PathBuf::from(local_app_data);
    path.push("ClaudeUsageWidget");
    path.push("widget.log");
    Some(path)
}

/// Points the process's stderr at the log file. Call once, first thing in
/// `main`, before anything that might want to log.
///
/// Returns the log's path on success, so the caller can tell the user where
/// to look; `None` means logging stayed disabled and the app should carry on
/// regardless.
pub fn init() -> Option<PathBuf> {
    let path = log_path()?;
    std::fs::create_dir_all(path.parent()?).ok()?;

    // Rotate before opening, so the size check is against the completed
    // previous run rather than a file we're already appending to.
    if std::fs::metadata(&path)
        .map(|meta| meta.len() > MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;

    // SAFETY: `file`'s handle is valid here, and `std::mem::forget` below
    // keeps it that way for the rest of the process's life -- closing the
    // `File` would leave `STD_ERROR_HANDLE` dangling, which every subsequent
    // `eprintln!` would then write through.
    unsafe {
        if SetStdHandle(STD_ERROR_HANDLE, file.as_raw_handle() as _) == 0 {
            return None;
        }
    }
    std::mem::forget(file);

    Some(path)
}
