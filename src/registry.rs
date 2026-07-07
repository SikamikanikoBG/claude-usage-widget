// "Start with Windows" toggle, backed by the per-user
// HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run key.
// This is the standard, no-admin-required way for a user-scope tray app to
// launch at logon; it does not touch any other part of the registry.

use winreg::enums::*;
use winreg::RegKey;

const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE_NAME: &str = "ClaudeUsageWidget";

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
