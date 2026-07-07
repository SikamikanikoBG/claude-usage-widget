# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.2.0]

### Added

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
  shown are fresher without needing a manual refresh.

## [0.1.0]

### Added

- Initial release: system-tray widget showing Claude Code's 5-hour session
  and 7-day weekly usage windows.
- Color-coded, percentage-badged tray icon (green/amber/red/gray), rendered
  in memory.
- Tooltip with a two-line usage summary.
- Right-click menu with text progress bars for both usage windows,
  "Refresh now", a "Start with Windows" toggle, and "Quit".
- Self-promotion of the tray icon to the always-visible tray area via the
  `HKCU\Control Panel\NotifyIconSettings` registry entry.
- Polls `GET https://api.anthropic.com/api/oauth/usage` every 60 seconds
  using the OAuth token Claude Code caches locally; falls back to a neutral
  "unavailable" state on any failure instead of crashing.
