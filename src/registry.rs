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

// Our own settings key -- distinct from the two Windows-owned keys above --
// for the floating usage panel's persisted visibility/mode (see `panel.rs`)
// and the configurable poll interval below.
const APP_SETTINGS_KEY_PATH: &str = r"Software\ClaudeUsageWidget";
const PANEL_VISIBLE_VALUE: &str = "PanelVisible";
const PANEL_MODE_VALUE: &str = "PanelMode";
const POLL_INTERVAL_VALUE: &str = "PollIntervalSecs";
const PANEL_OPACITY_VALUE: &str = "PanelOpacityPct";
const PANEL_POS_X_VALUE: &str = "PanelPosX";
const PANEL_POS_Y_VALUE: &str = "PanelPosY";

/// Opacity the floating panel starts at when nothing has been chosen yet:
/// visible at a glance, but see-through enough to sit over other windows
/// without hiding what's underneath.
pub const DEFAULT_PANEL_OPACITY_PCT: u32 = 70;

/// Floor for panel opacity. Below roughly this point the panel is too faint
/// to read, and -- worse -- a value of 0 would make it completely invisible
/// while still being "shown", i.e. indistinguishable from the panel being
/// broken. Enforced here rather than only in the menu so a hand-edited
/// registry value can't produce an unusable, hard-to-diagnose window.
pub const MIN_PANEL_OPACITY_PCT: u32 = 20;

/// Hard floor for the poll interval: "no need to spam Anthropic" was an
/// explicit requirement, not just a sensible default, so this is enforced
/// here (not just in the menu's list of offered choices) in case the
/// registry value is ever edited by hand or by an older/newer version.
pub const MIN_POLL_INTERVAL_SECS: u32 = 60;

/// Default for a fresh install (no registry value yet). Raised from the
/// original 60s: live evidence on 2026-07-08 showed HTTP 429 recurring
/// frequently even from pure 60s solo polling with zero manual refreshes or
/// hover interaction involved -- this undocumented endpoint's real-world
/// rate limit, especially after a day of heavy testing against the same
/// account, is evidently stricter than "once a minute" in practice. 5
/// minutes is a meaningfully safer out-of-the-box default; 1 minute is still
/// selectable from the "Poll interval" menu for anyone who wants it.
const DEFAULT_POLL_INTERVAL_SECS: u32 = 300;

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

/// Whether the floating usage panel (`panel.rs`) should be shown, per the
/// last choice made from the tray menu. Defaults to `false` (off) when the
/// value has never been set -- first run should not pop up a window the
/// user didn't ask for.
pub fn is_panel_visible() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    hkcu.open_subkey_with_flags(APP_SETTINGS_KEY_PATH, KEY_READ)
        .and_then(|key| key.get_value::<u32, _>(PANEL_VISIBLE_VALUE))
        .map(|v| v != 0)
        .unwrap_or(false)
}

/// Persists the floating usage panel's shown/hidden state.
pub fn set_panel_visible(visible: bool) -> std::io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _disposition) = hkcu.create_subkey(APP_SETTINGS_KEY_PATH)?;
    key.set_value(PANEL_VISIBLE_VALUE, &(visible as u32))
}

/// The raw persisted panel display-mode string (see
/// `panel::PanelMode::{as_registry_str, from_registry_str}`), or `None` if
/// never set (caller should default to "both").
pub fn panel_mode_str() -> Option<String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    hkcu.open_subkey_with_flags(APP_SETTINGS_KEY_PATH, KEY_READ)
        .and_then(|key| key.get_value::<String, _>(PANEL_MODE_VALUE))
        .ok()
}

/// Persists the floating usage panel's selected display mode.
pub fn set_panel_mode_str(mode: &str) -> std::io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _disposition) = hkcu.create_subkey(APP_SETTINGS_KEY_PATH)?;
    key.set_value(PANEL_MODE_VALUE, &mode.to_string())
}

/// The floating panel's opacity in percent, clamped to
/// `[MIN_PANEL_OPACITY_PCT, 100]` whatever is stored, defaulting to
/// [`DEFAULT_PANEL_OPACITY_PCT`] when never set.
pub fn panel_opacity_pct() -> u32 {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    hkcu.open_subkey_with_flags(APP_SETTINGS_KEY_PATH, KEY_READ)
        .and_then(|key| key.get_value::<u32, _>(PANEL_OPACITY_VALUE))
        .map(|pct| pct.clamp(MIN_PANEL_OPACITY_PCT, 100))
        .unwrap_or(DEFAULT_PANEL_OPACITY_PCT)
}

/// Persists the floating panel's opacity, clamping to the readable range
/// regardless of what the caller asks for.
pub fn set_panel_opacity_pct(pct: u32) -> std::io::Result<()> {
    let pct = pct.clamp(MIN_PANEL_OPACITY_PCT, 100);
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _disposition) = hkcu.create_subkey(APP_SETTINGS_KEY_PATH)?;
    key.set_value(PANEL_OPACITY_VALUE, &pct)
}

/// The floating panel's last dragged-to top-left position, or `None` if the
/// user has never moved it (in which case the panel anchors itself to the
/// bottom-right corner -- see `panel::panel_origin`).
///
/// Coordinates are screen coordinates and are legitimately negative on a
/// multi-monitor setup where a second display sits left of / above the
/// primary one. The registry has no signed-DWORD type, so they round-trip
/// through `u32` bit-for-bit via `as` casts rather than being stored as text.
pub fn panel_position() -> Option<(i32, i32)> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey_with_flags(APP_SETTINGS_KEY_PATH, KEY_READ)
        .ok()?;
    let x = key.get_value::<u32, _>(PANEL_POS_X_VALUE).ok()? as i32;
    let y = key.get_value::<u32, _>(PANEL_POS_Y_VALUE).ok()? as i32;
    Some((x, y))
}

/// Persists the position the user dragged the floating panel to.
pub fn set_panel_position(x: i32, y: i32) -> std::io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _disposition) = hkcu.create_subkey(APP_SETTINGS_KEY_PATH)?;
    key.set_value(PANEL_POS_X_VALUE, &(x as u32))?;
    key.set_value(PANEL_POS_Y_VALUE, &(y as u32))
}

/// Forgets any dragged-to position, sending the panel back to its default
/// bottom-right corner. Backs the "Reset position" menu item, which is the
/// escape hatch if the panel ever ends up somewhere unreachable.
pub fn clear_panel_position() -> std::io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(key) = hkcu.open_subkey_with_flags(APP_SETTINGS_KEY_PATH, KEY_WRITE) else {
        // No settings key at all means there's no saved position to clear.
        return Ok(());
    };
    for value in [PANEL_POS_X_VALUE, PANEL_POS_Y_VALUE] {
        match key.delete_value(value) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The poll interval in seconds, clamped to [`MIN_POLL_INTERVAL_SECS`] no
/// matter what's stored (defends against a hand-edited or stale-version
/// registry value below the floor), defaulting to
/// `DEFAULT_POLL_INTERVAL_SECS` when never set.
pub fn poll_interval_secs() -> u64 {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let raw = hkcu
        .open_subkey_with_flags(APP_SETTINGS_KEY_PATH, KEY_READ)
        .and_then(|key| key.get_value::<u32, _>(POLL_INTERVAL_VALUE))
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECS);
    raw.max(MIN_POLL_INTERVAL_SECS) as u64
}

/// Persists the poll interval, clamping below to [`MIN_POLL_INTERVAL_SECS`]
/// regardless of what the caller asks for.
pub fn set_poll_interval_secs(secs: u64) -> std::io::Result<()> {
    let secs = (secs.min(u32::MAX as u64) as u32).max(MIN_POLL_INTERVAL_SECS);
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _disposition) = hkcu.create_subkey(APP_SETTINGS_KEY_PATH)?;
    key.set_value(POLL_INTERVAL_VALUE, &secs)
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
