# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
