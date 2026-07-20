# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.7.0]

### Added

- **CPU temperature in the tray.** A second tray icon showing the current CPU
  temperature in degrees Celsius, alongside the usage one — green below 70 °C,
  amber from 70 °C, red from 85 °C. It updates every 5 seconds, and it's a
  separate icon on purpose: a menu line needs a click and the panel needs a
  window, but a tray icon is a number that's just always on screen. (The usage
  icon can't share it — there's no legible way to fit two numbers into 16
  physical pixels.)
  - **The two icons are different shapes: usage stays round, temperature is
    square.** They share the green/amber/red scale, so colour can't tell them
    apart — two green circles side by side are ambiguous exactly when you're
    trying to read them quickly. Shape is the one channel that still works at
    16 physical pixels. The temperature badge keeps its square outline even in
    the gray "unavailable" state, so it stays identifiable when the number
    isn't there.
  - Toggle it with **Show CPU temperature** in the right-click menu. It's on
    by default and the choice is remembered across restarts.
  - No administrator rights needed. The usual way to read CPU temperature on
    Windows — the `MSAcpi_ThermalZoneTemperature` WMI class — requires
    elevation, and third-party sensor tools additionally need a kernel driver.
    This reads the `Thermal Zone Information` performance counters instead,
    which are available to a normal user, so the widget stays unelevated.
  - Machines whose firmware exposes no usable thermal zone (this includes most
    VMs) show a gray icon rather than a wrong number, the same way the usage
    icon already handles missing data.

### Fixed

- **Two-digit numbers in the tray icon were unreadable.** The digit sizes were
  chosen to fit the icon's square canvas, but the badge drawn on that canvas
  is a *circle* — so the corners of the top and bottom segment bars fell
  outside it, where they were painted in the digit colour onto a transparent
  background. Against a dark taskbar those overhanging pieces are invisible,
  leaving only the parts inside the circle and turning a number like `60` into
  a blob. Digits are now sized to fit the circle. They're slightly smaller and
  considerably easier to read, since nothing is being silently cut off any
  more. Confirmed the way this project's icon changes always have to be — on a
  screenshot of the real tray, not a local preview.
- **Only the first tray icon was promoted out of the "hidden icons" overflow.**
  Windows keeps one `NotifyIconSettings` entry per icon, and the promotion
  logic stopped at the first match for this exe. With a second icon that would
  have left the temperature hidden behind the chevron — which, for a widget
  whose entire job is being glanceable, defeats the point.

## [0.6.0]

### Added

- **Panel opacity.** The floating usage panel can now be made see-through, so
  it can sit on top of your editor without covering up what's underneath.
  Pick **30% / 50% / 70% / 85% / 100%** from the new **Opacity** submenu under
  **Usage panel**; the default is **70%**. The change applies immediately,
  while the menu is still open, and is remembered across restarts.
- **The panel is movable.** Drag it anywhere on screen by clicking anywhere on
  it — the whole surface is the grab handle, since there's no title bar to
  grab. Where you drop it is remembered across restarts.
  - A new **Reset position** item puts it back in the default bottom-right
    corner.
  - A saved position is validated against the current screen layout on
    start-up: if the panel was parked on a monitor that's since been
    unplugged (or the resolution shrank), it returns to the default corner
    instead of faithfully restoring itself somewhere you can't see or reach.
- **A log file**, at `%LOCALAPPDATA%\ClaudeUsageWidget\widget.log`. The widget
  has always logged its poll heartbeat, backoff decisions and window errors —
  but it's built as a GUI app, so it has no console, and every one of those
  lines was being written to an invalid handle and silently discarded in the
  released binary. All of that diagnostic detail only ever reached people
  running the exe from a terminal, i.e. not the person actually hitting a bug.
  Now it goes to a file (rotated at 1 MB), which is the first place to look
  when something misbehaves and the thing to attach to a bug report. No tokens
  or personal data — percentages, timings and window coordinates.

### Fixed

- **The tooltip showed `Pr` instead of the projected weekly line.** The
  three-line tooltip added in 0.5.0 was 101 characters long, and Windows
  truncates a tray icon's tooltip at **63** — so it was chopped mid-word,
  leaving the new projection rendered as a bare `Pr`.
  - The 63-character limit is the *legacy* one. Windows honours 128 for
    callers that opt into the Shell v5 behaviour by sending a correct `cbSize`
    and/or `NIM_SETVERSION`; the `tray-icon` crate does neither (it builds
    `NOTIFYICONDATAW` with `..std::mem::zeroed()`, leaving `cbSize` at 0), so
    the shell applies the old limit. Nothing warns about this — the crate
    copies up to 128 characters and the API returns success; the truncation
    happens silently inside the shell.
  - Fixed by rewriting all three lines to fit the real budget
    (`Session 12% 3h40m` / `Weekly 54% Wed 18:00` / `Projected 71% under`),
    with tests that fail the build if the worst case ever stops fitting.
  - The **"unavailable" tooltips were over the limit too**, which was worse
    than cosmetic: the sign-in message got cut mid-word, eating the "run
    `claude` once to refresh" instruction — the only actionable part of it.
    All four now fit, instruction intact.
- **The floating panel never appeared on some Windows 10 machines** while
  working normally on Windows 11. Windows ignores the show/hide argument of a
  process's *first* `ShowWindow` call, substituting whatever the launching
  program put in its `STARTUPINFO` — so whether the panel's first "please
  show" was honoured or silently swapped for "hide" depended on what started
  the widget (Explorer, the Run key, a shortcut). The panel now deliberately
  spends that unpredictable first call on a no-op at creation, and shows
  itself via `SetWindowPos(HWND_TOPMOST, …, SWP_SHOWWINDOW)`, which isn't
  subject to the substitution and also re-asserts always-on-top — a panel
  stuck behind every other window looks exactly like one that never opened.
- **A repaint storm that could wedge the whole widget.** If `BeginPaint` ever
  failed, the panel's paint handler returned without the matching `EndPaint`,
  leaving the update region unvalidated — so Windows would immediately post
  another `WM_PAINT`, forever, hanging the message loop and taking the tray
  icon down with it.
- The panel now repaints correctly when it resizes between display modes
  (`CS_HREDRAW | CS_VREDRAW`); previously a grown panel could keep showing the
  previous, shorter layout until something else happened to invalidate it.
- The panel no longer steals focus from what you're typing in when it appears.

## [0.5.0]

### Added

- **Projected weekly utilization.** Alongside the live 5-hour and 7-day
  percentages, the widget now extrapolates where your weekly usage is *headed*
  by reset. It measures how far you are through the current 7-day window
  (derived from the reset timestamp), divides your current utilization by that
  elapsed fraction, and shows the run-rate result: e.g. `83%` means you're on
  pace to finish the week comfortably under the limit ("under"), `123%` means
  you're on pace to blow through it and should slow down ("over"). This is the
  number to watch when deciding how carefully to spend the days left before
  reset.
  - It appears in three places, matching the existing two-window layout:
    - a **third tooltip line** on hover (`Projected weekly: 83% of limit
      (under)`),
    - a **third right-click menu line** (`Projected [███░░░░░░░] 83% (under)`),
    - a **third progress bar** in the floating usage panel. The panel now
      resizes to fit: "Both" is three bars tall, "Weekly only" pairs the
      weekly bar with its projection, and "Rotating" cycles Session → Weekly
      → Projected.
  - The projection is deliberately not clamped to 100% (over-limit is the
    whole point), but the first hour of a fresh window is floored so a tiny
    early burst can't extrapolate to an absurd, flapping thousands-of-percent
    figure; once an hour has elapsed it's the exact linear run-rate.

## [0.4.1]

### Changed

- Visual design pass on the seven-segment digits introduced in 0.4.0:
  legible but "doesn't look professional" per real-world feedback on a
  fresh screenshot. Segments are now rounded (not raw rectangles) with
  soft anti-aliased edges (the same analytic-AA technique already used for
  the outer badge circle) instead of hard pixel cutoffs, and inset from
  each other by a small gap so they read as distinct segments rather than
  a fused block. Badge colors also swapped from fully-saturated primary
  hues to flatter, more refined tones closer to iOS/macOS system status
  colors (systemGreen/systemOrange/systemRed/systemGray), which read as
  "status indicator" rather than "hazard sign."

## [0.4.0]

### Changed

- Redesigned the tray icon's percentage digits from a small pixel font
  (bolded via dilation, with a separate outline layer) to bold seven-segment
  digits, like a digital clock. A real screenshot of the tray icon showed
  the previous design rendering as an illegible blur at actual tray-icon
  size (16-24 physical pixels) -- badly enough that it read as a garbled
  face rather than a number -- even though it looked fine in this project's
  own downscaled preview tests. Two lessons from that: simulating the
  downscale with a different resize algorithm than whatever Windows Shell
  actually uses for notification icons gives false confidence, and
  outline-plus-fill adds a second set of parallel edges that blur together
  at tiny sizes. Seven-segment digits are bold rectangular blocks in one
  flat color with no separate outline -- the same design choice that makes
  them legible on low-resolution LED/LCD displays.

## [0.3.3]

### Fixed

- Connectivity hiccups (network errors, unparseable responses, token
  problems) no longer use the same long, cautious backoff schedule as an
  actual HTTP 429. Live evidence: after the machine sat idle for a few
  hours, a handful of requests failed with connection/decode errors (stale
  keep-alive connections surviving a sleep/wake cycle is the leading
  theory) and self-healed within a few attempts -- but at the rate-limit
  schedule's pace, each retry was minutes apart, making a hiccup that
  actually resolves in seconds look like the widget being broken for a
  long stretch. Only a real 429 gets the cautious 5-minute-based backoff
  now; everything else gets a fast 5s-to-60s schedule. The HTTP client is
  also rebuilt after a network-transport-level failure specifically,
  rather than continuing to reuse a connection that might be the actual
  cause.

## [0.3.2]

### Fixed

- **0.3.1 has a real freeze bug -- upgrade past it.** In removing the
  hover-refresh trigger (below), 0.3.1 also removed the
  `TrayIconEvent::set_event_handler` registration entirely instead of
  replacing it with a no-op. Live evidence: the widget logged one poll
  cycle normally, then went completely silent for 30+ minutes with the
  tray icon stuck on its last state, even though the usage endpoint was
  confirmed reachable the whole time. The exact internal mechanism wasn't
  fully pinned down (one specific theory -- the crate's internal fallback
  event channel filling up and blocking the Windows message pump -- was
  checked and ruled out, since that channel is unbounded and can't block),
  but restoring the handler registration (now a genuine no-op rather than
  forwarding events anywhere) is the exact plumbing that ran stable for
  30+ minutes across 0.2.0 and 0.3.0, so that's what's restored here.
- Added a one-line heartbeat log for every successful poll (previously
  silent on success), so "is this actually still running" is never again
  a guessing game from the log alone -- this silence is exactly what made
  the 0.3.1 freeze hard to distinguish from healthy operation at first.
- Default poll interval raised from 60s to 5 minutes. Live evidence on
  2026-07-08 showed HTTP 429 recurring frequently even from pure passive
  60s polling with zero manual refreshes or hover interaction involved --
  this undocumented endpoint's real-world rate limit, especially after a
  day of heavy testing against the same account, is evidently stricter
  than "once a minute" in practice. 1 minute remains selectable from the
  "Poll interval" menu (the floor is unchanged), just no longer the
  out-of-the-box default.
- Added menu-dispatch diagnostic logging (which panel mode was selected,
  and a fallback log for any menu event that doesn't match a known item)
  to make future "the UI doesn't seem to respond" reports diagnosable
  without needing to reproduce live with a custom-instrumented build.

## [0.3.1]

### Fixed

- Removed the tray icon hover/click auto-refresh added in 0.2.0. Live
  evidence right after 0.3.0 shipped: the log showed a repeating
  success -> immediate 429 -> 120s backoff -> success -> immediate 429
  cycle, matching exactly "hover to check the icon a couple of times" -
  each hover fired an extra request on top of the timer (the 3-second
  debounce wasn't nearly enough headroom), and those extra requests were
  what tripped this endpoint's rate limit. Hovering/clicking the icon no
  longer triggers any network request; "Refresh now" in the menu is the
  only way to force an immediate check, same as the timer-driven poll.

## [0.3.0]

### Added

- Floating usage panel: an optional always-on-top window (off by default,
  toggled from the tray menu's "Usage panel" submenu) showing real drawn
  progress bars for both windows. Four display modes - Both, 5-hour only,
  Weekly only, Rotating (alternates every 2 seconds) - remembered across
  restarts.
- Configurable poll interval: a "Poll interval" tray submenu (1/2/5/10
  minutes). Changing it applies immediately, no restart needed. 1 minute is
  both the default and a hard floor enforced in code, not just in the menu's
  offered choices.
- Accurate "unavailable" messaging: the tray tooltip, menu lines, and
  floating panel now distinguish *why* data is unavailable - "sign-in
  needed" (bad/missing token, actually needs `claude` re-auth), "rate-limited,
  retrying" (temporary 429, self-heals), or "network error, retrying" -
  instead of one generic message that told users to re-authenticate even
  when the real cause was rate-limiting.

### Fixed

- Tray icon percentage digits were illegible at real tray-icon display size
  (thin single-pixel-wide strokes anti-aliased into mush once scaled down).
  Digits are now bolded with a proportional high-contrast outline; verified
  by rendering and downscaling real previews to 16/24/32px rather than just
  reading the code.
- HTTP 429 storm: polling at 20-30 seconds (introduced in 0.2.0 to fix
  staleness) was hitting this undocumented endpoint's own rate limit, which
  ironically made the widget show "unavailable" *more* than 0.1.0 did.
  Failures now back off exponentially (doubling the poll interval, capped at
  5 minutes) and respect a server-sent `Retry-After` header when it makes
  sense to (a literal `Retry-After: 0` observed live while actively
  rate-limited is treated as a floor, not something that can shrink the
  backoff below the computed schedule). A manual refresh (menu click or tray
  hover/click) is ignored while backing off, so user interaction can't make
  a rate-limit situation worse.

## [0.2.0]

### Added

- Self-promotion of the tray icon to the always-visible tray area via the
  `HKCU\Control Panel\NotifyIconSettings` registry entry, so it no longer
  sits hidden behind the overflow chevron by default.
- Percentage badge rendered directly on the tray icon (the higher of the two
  utilization percentages), so the number is visible without hovering.
- Single-instance guard: a named Win32 mutex prevents two copies of the
  widget running at once (e.g. a manual double-click while "Start with
  Windows" also launched one), which previously could create duplicate tray
  icons and duplicate `NotifyIconSettings` registry entries. A second launch
  now detects the existing instance, logs one line to stderr, and exits
  immediately.
- Extra usage / credit balance line: when an account has "extra usage"
  (pay-as-you-go credits beyond the plan limit) enabled, a third menu line
  shows the used/limit balance as a progress bar and currency amount. Left
  out entirely when extra usage isn't enabled, and never breaks parsing of
  the core session/weekly percentages if Anthropic changes or omits this
  field.
- Threshold notification: a one-time Windows balloon/toast notification
  fires when either the session or weekly utilization crosses 90% (edge
  triggered - fires once per crossing, resets once the number drops back
  below 90%).
- Tray icon hover/click now triggers an immediate refresh (debounced to at
  most once every 3 seconds), in addition to the timer and "Refresh now".

### Changed

- Refresh interval shortened from 60 seconds to 20 seconds so the numbers
  shown are fresher without needing a manual refresh. (Reverted in 0.3.0 -
  see above - in favor of a configurable interval with a 1-minute floor.)

## [0.1.0]

### Added

- Initial release: system-tray widget showing Claude Code's 5-hour session
  and 7-day weekly usage windows.
- Color-coded tray icon (green/amber/red/gray), rendered in memory as a
  plain filled circle (the percentage badge came in 0.2.0).
- Tooltip with a two-line usage summary.
- Right-click menu with text progress bars for both usage windows,
  "Refresh now", a "Start with Windows" toggle, and "Quit".
- Polls `GET https://api.anthropic.com/api/oauth/usage` every 60 seconds
  using the OAuth token Claude Code caches locally; falls back to a neutral
  "unavailable" state on any failure instead of crashing.
