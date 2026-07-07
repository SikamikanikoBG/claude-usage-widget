// "Start with Windows" toggle, backed by the per-user
// HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run key.
// This is the standard, no-admin-required way for a user-scope tray app to
// launch at logon; it does not touch any other part of the registry.
//
// Also: best-effort self-promotion of the tray icon out of the overflow
// ("hidden icons") area, backed by the per-user
// HKEY_CURRENT_USER\Control Panel\NotifyIconSettings\<key> entries Windows
// maintains, one per app that has ever registered a notification icon.

use std::time::Duration;

use winreg::enums::*;
use winreg::RegKey;

const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE_NAME: &str = "ClaudeUsageWidget";

const NOTIFY_ICON_SETTINGS_PATH: &str = r"Control Panel\NotifyIconSettings";
const PROMOTE_MAX_ATTEMPTS: u32 = 10;
const PROMOTE_RETRY_DELAY: Duration = Duration::from_millis(200);

/// Returns the quoted path to the currently running executable, used both to
/// populate the registry and to check whether an existing entry matches us.
fn current_exe_value() -> std::io::Result<String> {
    let exe = std::env::current_exe()?;
    Ok(format!("\"{}\"", exe.display()))
}

/// Whether a Run entry pointing at this exe currently exists.
pub fn is_startup_enabled() -> bool {
    let Ok(expected) = current_exe_value() else {
        return false;
    };

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(run_key) = hkcu.open_subkey_with_flags(RUN_KEY_PATH, KEY_READ) else {
        return false;
    };

    run_key
        .get_value::<String, _>(RUN_VALUE_NAME)
        .map(|existing| existing == expected)
        .unwrap_or(false)
}

/// Adds or removes the Run entry so the widget does (or doesn't) launch at
/// Windows sign-in.
pub fn set_startup_enabled(enable: bool) -> std::io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    if enable {
        let value = current_exe_value()?;
        let (run_key, _disposition) = hkcu.create_subkey(RUN_KEY_PATH)?;
        run_key.set_value(RUN_VALUE_NAME, &value)?;
    } else if let Ok(run_key) = hkcu.open_subkey_with_flags(RUN_KEY_PATH, KEY_WRITE) {
        match run_key.delete_value(RUN_VALUE_NAME) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

/// Outcome of a single attempt to find and promote this exe's
/// `NotifyIconSettings` subkey.
enum PromoteOutcome {
    Set,
    AlreadySet,
    NotFound,
}

/// Spawns a background thread that finds this exe's `NotifyIconSettings`
/// subkey and sets `IsPromoted = 1` (DWORD) on it, which is what "always
/// show this icon in the taskbar" toggles when done through Settings. This
/// is purely a nice-to-have: it never panics and never blocks the caller
/// (call it right after the tray icon is created; it does its own waiting).
///
/// Windows creates/updates the matching subkey asynchronously relative to
/// `Shell_NotifyIcon` registering the icon, so the entry may not exist yet
/// the first time we look — hence the retry loop. Note that even after we
/// set `IsPromoted`, Explorer may not visibly move the icon out of the
/// overflow area until it re-reads this key (e.g. on the icon's next
/// register, or on an Explorer restart) on some Windows builds; that's a
/// Windows-side timing quirk, not a bug in this logic.
pub fn promote_tray_icon_async() {
    std::thread::spawn(|| {
        for attempt in 1..=PROMOTE_MAX_ATTEMPTS {
            match try_promote_tray_icon() {
                Ok(PromoteOutcome::Set) => {
                    eprintln!(
                        "[claude-usage-widget] promoted tray icon to always-show in NotifyIconSettings"
                    );
                    return;
                }
                Ok(PromoteOutcome::AlreadySet) => {
                    eprintln!(
                        "[claude-usage-widget] tray icon was already set to always-show"
                    );
                    return;
                }
                Ok(PromoteOutcome::NotFound) => {
                    if attempt == PROMOTE_MAX_ATTEMPTS {
                        eprintln!(
                            "[claude-usage-widget] gave up looking for our NotifyIconSettings entry after {attempt} attempts"
                        );
                        return;
                    }
                    std::thread::sleep(PROMOTE_RETRY_DELAY);
                }
                Err(e) => {
                    eprintln!("[claude-usage-widget] could not promote tray icon: {e}");
                    return;
                }
            }
        }
    });
}

/// One lookup attempt: enumerate `NotifyIconSettings` subkeys, find the one
/// whose `ExecutablePath` matches our own exe, and set `IsPromoted` on it if
/// it isn't already 1.
fn try_promote_tray_icon() -> std::io::Result<PromoteOutcome> {
    let current_exe = std::env::current_exe()?.canonicalize()?;
    let current_exe = current_exe.to_string_lossy().to_lowercase();

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let settings_key = match hkcu.open_subkey(NOTIFY_ICON_SETTINGS_PATH) {
        Ok(key) => key,
        Err(_) => return Ok(PromoteOutcome::NotFound),
    };

    for subkey_name in settings_key.enum_keys().flatten() {
        let subkey = match settings_key
            .open_subkey_with_flags(&subkey_name, KEY_READ | KEY_SET_VALUE)
        {
            Ok(key) => key,
            Err(_) => continue,
        };

        let exe_path: String = match subkey.get_value("ExecutablePath") {
            Ok(value) => value,
            Err(_) => continue,
        };

        let candidate = match std::path::Path::new(&exe_path).canonicalize() {
            Ok(path) => path.to_string_lossy().to_lowercase(),
            Err(_) => continue,
        };

        if candidate != current_exe {
            continue;
        }

        let already_promoted = subkey
            .get_value::<u32, _>("IsPromoted")
            .map(|v| v == 1)
            .unwrap_or(false);
        if already_promoted {
            return Ok(PromoteOutcome::AlreadySet);
        }

        subkey.set_value("IsPromoted", &1u32)?;
        return Ok(PromoteOutcome::Set);
    }

    Ok(PromoteOutcome::NotFound)
}
