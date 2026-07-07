# Claude Usage Widget

A tiny Windows system-tray widget that shows your [Claude Code](https://claude.com/claude-code)
subscription usage at a glance: how much of your rolling 5-hour session
window and 7-day weekly window you've used, and when each one resets.

No dashboard, no browser tab, no `/usage` command to remember - just a
colored dot in your system tray.

![platform](https://img.shields.io/badge/platform-Windows-blue)
![license](https://img.shields.io/badge/license-MIT-green)

## Install (no Rust required)

1. Grab the latest `claude-usage-widget.exe` from the [Releases page](https://github.com/SikamikanikoBG/claude-usage-widget/releases).
2. Run it. That's it - no installer, no admin rights, nothing else to set up.
3. Windows may show a SmartScreen warning ("Windows protected your PC") because
   the binary isn't code-signed. Click **More info -> Run anyway**. This is
   normal for small open-source tools - the source is right here if you want
   to check it or build it yourself instead.
4. First run: look in the system tray overflow (the `^` arrow near the clock) -
   new tray icons default to hidden on Windows. Drag it out (or enable it via
   *Settings -> Personalization -> Taskbar -> Other system tray icons*) to
   keep it always visible.

## What it looks like

- **Tray icon**: a small colored dot, color-coded by your highest current
  utilization across both windows:
  - Green: under 50%
  - Amber: 50-80%
  - Red: over 80%
  - Gray: usage data isn't available right now (see "Why gray?" below)
- **Tooltip** (hover): a two-line summary, e.g.
  ```
  Session: 12% (resets in 3h40m)
  Weekly: 54% (resets Wed 18:00)
  ```
- **Right-click menu**: the same two numbers as text progress bars, plus
  controls:
  ```
  Session  [██░░░░░░░░] 12%  resets 3h40m
  Weekly   [█████░░░░░] 54%  resets Wed 18:00
  ---------------------------------------------
  Refresh now
  ✓ Start with Windows
  ---------------------------------------------
  Quit
  ```
  If your account has "extra usage" (pay-as-you-go credits beyond the plan
  limit) enabled, a third informational line shows up right below the two
  above:
  ```
  Session  [██░░░░░░░░] 12%  resets 3h40m
  Weekly   [█████░░░░░] 54%  resets Wed 18:00
  Extra usage  [██░░░░░░░░] 12%  42.50/850.00 EUR
  ---------------------------------------------
  Refresh now
  ✓ Start with Windows
  ---------------------------------------------
  Quit
  ```
  It's left out entirely when extra usage isn't enabled on your account,
  which is the common case.
- **Threshold notification**: if either window's utilization crosses 90%
  (from below 90% up to 90% or higher), you'll get a one-time Windows
  notification, e.g. "Session at 92%, resets in 1h20m" - it won't repeat
  again for that same crossing, only once the number drops back below 90%
  and later crosses again.

It refreshes automatically every 20 seconds. "Refresh now" forces an
immediate re-check without waiting for the timer, and so does simply
hovering over or clicking the tray icon (debounced to at most once every 3
seconds so repeated hovering doesn't hammer the endpoint) - so the numbers
you're looking at are rarely more than a few seconds stale.

Only one copy of the widget runs at a time: if you double-click the exe
while it's already running (or "Start with Windows" launches it and you
also start it manually), the second copy notices, prints a message, and
exits immediately instead of creating a duplicate tray icon.

## Why gray? (and why this needs your credentials file)

Claude Code caches your OAuth session locally at
`%USERPROFILE%\.claude\.credentials.json` after you sign in with the `claude`
CLI. This widget reads the `accessToken` out of that file and calls Anthropic's
usage endpoint with it - the same way Claude Code's own statusline gets its
numbers.

This is an **undocumented** endpoint, and this widget has no way to refresh
an expired token on its own (there's no public refresh flow for it). So if
your cached token has expired, or the credentials file is missing, or the
network is down, the icon turns gray and the tooltip explains: run `claude`
once to refresh your session, and the widget will pick up the new token on
its next check - no restart needed.

## Privacy / what it talks to

This widget makes network requests to **`api.anthropic.com` only**, using
**your own already-cached token** - nothing else, no analytics, no telemetry,
no third-party service. It only reads one local file (your existing Claude
Code credentials cache) and only writes to one registry location, and only if
you turn on "Start with Windows":
`HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run`. It writes
no files of its own. The source is small enough to read end to end in a few
minutes - that's the point of it being open source.

## Building from source

Requirements:
- Rust (stable), MSVC toolchain (`x86_64-pc-windows-msvc`)
- Windows 10/11

```powershell
git clone https://github.com/SikamikanikoBG/claude-usage-widget.git
cd claude-usage-widget
cargo build --release
```

The binary is produced at `target\release\claude-usage-widget.exe`. It's a
single self-contained executable - copy it wherever you like and run it.

## Running

Just double-click `claude-usage-widget.exe`, or run it from a terminal.
It has no window - only a tray icon appears (look in the system tray overflow
arrow if you don't see it right away). You'll need to have signed in with
Claude Code at least once (`claude` CLI) so the credentials file exists.

To have it launch automatically at sign-in, right-click the tray icon and
check "Start with Windows".

## How it works, briefly

- A background thread polls `GET https://api.anthropic.com/api/oauth/usage`
  every 20 seconds (or immediately on "Refresh now", or on tray icon
  hover/click), re-reading the credentials file on every attempt so it picks
  up a refreshed token without needing a restart.
- A named Win32 mutex (`Global\ClaudeUsageWidget_SingleInstance`) guards
  against two copies running at once; a second launch detects the existing
  mutex and exits immediately, before creating any UI.
- The tray icon is rendered in memory (a simple anti-aliased filled circle)
  rather than shipped as a static asset file, so the color always matches the
  live data.
- The 90%-threshold notification is a plain Win32 balloon/toast shown via
  `Shell_NotifyIconW`, not a separate notification library.
- Built with [`tray-icon`](https://crates.io/crates/tray-icon) +
  [`tao`](https://crates.io/crates/tao) for a minimal tray/event-loop stack
  (deliberately not pulling in a full webview), `reqwest` (rustls, no
  OpenSSL dependency) for HTTP, `winreg` for the startup toggle, and
  `windows-sys` for the single-instance mutex and balloon notification.

## Limitations

- Windows only.
- Depends on an undocumented Anthropic endpoint that could change or move
  without notice; if it does, the widget will simply show "unavailable"
  until it's updated.
- No historical charts or trend graphs - it's intentionally just the
  at-a-glance tray view plus the one 90%-threshold notification.

## License

MIT - see [LICENSE](LICENSE).
