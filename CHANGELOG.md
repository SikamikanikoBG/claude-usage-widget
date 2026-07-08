# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
