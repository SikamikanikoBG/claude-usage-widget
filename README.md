# Claude Usage Widget

A tiny Windows system-tray widget that shows your [Claude Code](https://claude.com/claude-code)
subscription usage at a glance: how much of your rolling 5-hour session
window and 7-day weekly window you've used, and when each one resets.

No dashboard, no browser tab, no `/usage` command to remember - just a
colored, badged dot in your system tray, plus an optional floating panel
with real progress bars if you want more than a glance.

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

- **Tray icon**: a small colored dot with your highest current utilization
  printed right on it (bold, outlined so it stays legible on any of the
  colors below) - no hovering needed to see the number:
  - Green: under 50%
  - Amber: 50-80%
  - Red: over 80%
  - Gray, no number: usage data isn't available right now (see "Why gray?"
    below for what that actually means)
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
- **Floating usage panel** (optional, off by default): a small always-on-top
  window in the bottom-right corner with real drawn progress bars for both
  windows. Turn it on from the tray menu's **Usage panel** submenu, which
  also lets you pick what it shows: **Both** (stacked), **5-hour only**,
  **Weekly only**, or **Rotating** (alternates every 2 seconds). Your choice
  is remembered across restarts.
- **Poll interval** (tray menu submenu): how often it checks, from **1
  minute** (the default, and the floor - this app will never poll faster
  than that, on purpose) up to **10 minutes**. Changing it applies
  immediately, no restart needed.

"Refresh now" forces an immediate re-check without waiting for the timer -
unless the widget is currently backing off after a failed request (see
"Why gray?" below), in which case it's deliberately ignored until the
backoff clears, so mashing "Refresh now" can't make a rate-limit situation
worse. Hovering or clicking the tray icon itself does **not** trigger a
network request - it just shows whatever was last fetched. (An earlier
version did refresh on hover, which turned out to be a real bug: casually
checking the icon a few times fires extra requests on top of the timer,
and that's what was actually tripping the rate limit described below.)

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

The icon turns gray whenever a fetch fails, but the tooltip/menu text tells
you *why*, because the fix is different depending on the cause:

- **"sign-in needed"** - the credentials file is missing, unparseable, or the
  server rejected the token outright (HTTP 401/403). This is the one case
  that actually needs you to do something: run `claude` once to refresh your
  session, and the widget picks up the new token on its next check.
- **"rate-limited, retrying"** - this undocumented endpoint has its own rate
  limit, separate from your actual usage quota. Polling too fast (or several
  tools/test runs hitting it around the same time) can trigger a temporary
  HTTP 429. The widget backs off automatically (starting at your configured
  poll interval and doubling on repeated failures, up to 5 minutes) and
  recovers on its own - no action needed, and manual refreshes are ignored
  during the backoff so they can't make it worse.
- **"network error, retrying"** / **"unavailable, retrying"** - a connectivity
  hiccup or an unexpected response shape; also self-recovers on the next poll.

This is an **undocumented** endpoint with no public token-refresh flow, so
"sign-in needed" is the only case this widget can't fix by itself.

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
  at your configured interval (1-10 minutes, 1 minute floor and default),
  re-reading the credentials file on every attempt so it picks up a refreshed
  token without needing a restart.
- On failure, it backs off exponentially (doubling from the poll interval, up
  to a 5-minute cap) rather than retrying at a fixed rate, and respects a
  server-sent `Retry-After` header when there's a sane one to respect - this
  is what makes the "rate-limited, retrying" state self-heal instead of
  turning a temporary 429 into a retry storm.
- A named Win32 mutex (`Global\ClaudeUsageWidget_SingleInstance`) guards
  against two copies running at once; a second launch detects the existing
  mutex and exits immediately, before creating any UI.
- The tray icon is rendered in memory (a bolded, outlined digit font blitted
  onto an anti-aliased filled circle) rather than shipped as a static asset
  file, so the color and number always match the live data.
- The floating usage panel is a plain always-on-top Win32 popup window,
  positioned from the primary monitor's work area (so it sits above the
  taskbar, not under it), drawn with raw GDI calls - no extra windowing/UI
  crate.
- The 90%-threshold notification is a plain Win32 balloon/toast shown via
  `Shell_NotifyIconW`, not a separate notification library.
- Built with [`tray-icon`](https://crates.io/crates/tray-icon) +
  [`tao`](https://crates.io/crates/tao) for a minimal tray/event-loop stack
  (deliberately not pulling in a full webview), `reqwest` (rustls, no
  OpenSSL dependency) for HTTP, `winreg` for settings, and `windows-sys` for
  the single-instance mutex, balloon notification, and floating panel window.

## Limitations

- Windows only.
- Depends on an undocumented Anthropic endpoint that could change or move
  without notice; if it does, the widget will simply show "unavailable,
  retrying" until it's updated.
- No historical charts or trend graphs - it's intentionally just the
  at-a-glance tray view, the optional floating panel, and the one
  90%-threshold notification.

## License

MIT - see [LICENSE](LICENSE).
