// Pixel office: every Claude Code session on this machine as a pixel-art
// character in a little office, in the browser.
//
// The office is pixel-agents' standalone server
// (https://github.com/pixel-agents-hq/pixel-agents, MIT), pinned to
// `PIXEL_AGENTS_VERSION` and run through `npx` the first time the menu item is
// clicked -- from the user's home folder, bound to 127.0.0.1 on a fixed port.
// The widget only starts it, opens it in the default browser and stops it on
// Quit. pixel-agents itself reads the session transcripts under
// `~/.claude/projects`; if you approve it in its own Settings it also installs
// Claude Code hooks into `~/.claude/settings.json` for live events. Nothing
// here touches either file.
//
// Needs Node.js 20+ (`npx`) on PATH. The first start downloads the package
// into the npm cache, so it takes a few seconds; later starts are quick.

use std::io::{BufRead, BufReader, Read};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::notify;

/// The pixel-agents release the widget starts. Pinned, so an upstream release
/// is something to opt into rather than something that happens to you.
pub const PIXEL_AGENTS_VERSION: &str = "1.4.1";

/// Fixed rather than OS-assigned, so a server left running by a widget that
/// crashed is found (and reused) instead of a second one being started.
pub const PORT: u16 = 47615;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// What pixel-agents prints once it is listening, followed by the URL (which
/// carries the session token that lets the browser approve the hook install).
const READY_MARKER: &str = "running at ";

#[derive(Default)]
struct State {
    /// The server this widget started; `None` when it isn't running (or was
    /// found already running, which this widget does not own).
    child: Option<Child>,
    /// The tokened URL the running child printed.
    url: Option<String>,
    /// A start is in progress (npx may still be downloading).
    starting: bool,
}

#[derive(Clone, Default)]
pub struct Office {
    state: Arc<Mutex<State>>,
}

impl Office {
    /// Opens the office in the default browser, starting the server first
    /// when it isn't running. Never blocks the caller (the tray event loop).
    pub fn open(&self) {
        let state = Arc::clone(&self.state);
        std::thread::spawn(move || open_blocking(&state));
    }

    /// Stops the server this widget started. A server it only found running
    /// (started by hand, or by an earlier widget) is left alone.
    pub fn stop(&self) {
        let mut st = lock(&self.state);
        st.url = None;
        if let Some(child) = st.child.take() {
            kill_tree(child);
        }
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn open_blocking(state: &Mutex<State>) {
    {
        let mut st = lock(state);
        if st.starting {
            // A second click while the first start is still under way.
            return;
        }
        let alive = st
            .child
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)));
        if alive {
            if let Some(url) = st.url.clone() {
                drop(st);
                open_url(&url);
                return;
            }
        } else {
            st.child = None;
            st.url = None;
            if server_answers() {
                drop(st);
                // Not ours, so no token: the office shows, only the hooks
                // approval in its Settings is refused.
                open_url(&base_url());
                return;
            }
        }
        st.starting = true;
    }

    notify::show_balloon(
        "Pixel office".to_string(),
        "Starting the pixel office. The first start downloads it, so give it a few seconds."
            .to_string(),
    );
    let url = match spawn() {
        Ok(mut child) => {
            let stdout = child.stdout.take();
            if let Some(stderr) = child.stderr.take() {
                drain_to_log(stderr);
            }
            lock(state).child = Some(child);
            stdout.and_then(read_url)
        }
        Err(e) => {
            eprintln!("[claude-usage-widget] office: could not start npx: {e}");
            None
        }
    };

    let mut st = lock(state);
    st.starting = false;
    match url {
        Some(url) => {
            st.url = Some(url.clone());
            drop(st);
            open_url(&url);
        }
        None => {
            if let Some(child) = st.child.take() {
                kill_tree(child);
            }
            drop(st);
            notify::show_balloon(
                "Pixel office".to_string(),
                "Could not start the pixel office. It needs Node.js 20+ (npx) on PATH; \
                 the widget log has the details."
                    .to_string(),
            );
        }
    }
}

fn spawn() -> std::io::Result<Child> {
    // `npx` is a .cmd shim on Windows, which CreateProcess can't run directly.
    Command::new("cmd")
        .args([
            "/c",
            "npx",
            "--yes",
            &format!("pixel-agents@{PIXEL_AGENTS_VERSION}"),
            "--port",
            &PORT.to_string(),
        ])
        .current_dir(home_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
}

/// Reads the server's output until it says where it is listening, and keeps
/// draining it afterwards (a full pipe would stall the server). `None` when
/// the output ends first: the server exited.
fn read_url(stdout: impl Read + Send + 'static) -> Option<String> {
    let mut lines = BufReader::new(stdout).lines();
    for line in lines.by_ref() {
        let Ok(line) = line else { break };
        if let Some(url) = parse_ready_line(&line) {
            eprintln!("[claude-usage-widget] office: listening on {}", base_url());
            std::thread::spawn(move || lines.for_each(drop));
            return Some(url);
        }
    }
    None
}

/// The URL from pixel-agents' "server running at <url>" line.
pub fn parse_ready_line(line: &str) -> Option<String> {
    let rest = &line[line.find(READY_MARKER)? + READY_MARKER.len()..];
    let url = rest.split_whitespace().next()?;
    url.starts_with("http://").then(|| url.to_string())
}

fn drain_to_log(stderr: impl Read + Send + 'static) {
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[claude-usage-widget] office: {line}");
        }
    });
}

fn base_url() -> String {
    format!("http://127.0.0.1:{PORT}/")
}

fn server_answers() -> bool {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .and_then(|client| client.get(format!("{}api/health", base_url())).send())
        .is_ok_and(|response| response.status().is_success())
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Opens `url` in the default browser. `url.dll` rather than `cmd /c start`,
/// so nothing in the URL is ever read by a shell.
fn open_url(url: &str) {
    let result = Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    if let Err(e) = result {
        eprintln!("[claude-usage-widget] office: could not open the browser: {e}");
    }
}

/// `cmd` -> `npx` -> `node`: killing only `cmd` would leave the server
/// running, so take the whole tree.
fn kill_tree(mut child: Child) {
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &child.id().to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_url_in_the_ready_line() {
        assert_eq!(
            parse_ready_line("  Pixel Agents server running at http://127.0.0.1:47615/?token=abc123"),
            Some("http://127.0.0.1:47615/?token=abc123".to_string())
        );
    }

    #[test]
    fn ignores_every_other_line() {
        assert_eq!(parse_ready_line("[Pixel Agents] Server: listening on 127.0.0.1:47615"), None);
        assert_eq!(parse_ready_line("running at "), None);
        assert_eq!(parse_ready_line("running at ftp://nope"), None);
    }
}
