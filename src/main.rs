// Claude Usage Widget
//
// A lightweight Windows system-tray widget that shows an at-a-glance view of
// the current Claude Code subscription usage (5-hour session window and
// 7-day weekly window), by calling the same undocumented
// `api.anthropic.com/api/oauth/usage` endpoint that Claude Code's own
// statusline uses, with the OAuth token Claude Code already cached locally.
//
// No telemetry, no network calls to anything other than api.anthropic.com,
// no files written other than reading the local credentials file and
// (optionally) the HKCU Run registry key for the "Start with Windows" toggle
// and the app's own HKCU settings key for the floating usage panel.
#![windows_subsystem = "windows"]

mod icon;
mod notify;
mod panel;
mod registry;
mod single_instance;
mod usage;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::Foundation::HWND;

use panel::PanelMode;
use usage::TrayState;

// The poll interval is now a user-configurable, live-adjustable setting (see
// the "Poll interval" tray submenu and `registry::{poll_interval_secs,
// set_poll_interval_secs}`), not a fixed constant -- both the 20s and 30s
// defaults tried earlier turned out to be too aggressive for this
// undocumented endpoint's own rate limit. `registry::MIN_POLL_INTERVAL_SECS`
// (1 minute) is an explicit hard floor: "no need to spam Anthropic".
const POLL_INTERVAL_PRESETS_SECS: [(u64, &str); 4] =
    [(60, "1 minute"), (120, "2 minutes"), (300, "5 minutes"), (600, "10 minutes")];

/// Ceiling for the exponential backoff below: even after a long streak of
/// consecutive failures (429s, network errors, etc.), never wait longer
/// than this between polling attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);

const BAR_WIDTH: usize = 10;

/// Utilization percentage (for either window) at/above which a one-time
/// threshold notification fires.
const NOTIFY_THRESHOLD: u32 = 90;

/// Ignore repeat tray-icon-interaction refreshes within this long of the
/// last one, so repeatedly hovering doesn't hammer the endpoint. Note this
/// is purely a "don't spam pings into the channel" debounce; the actual
/// decision of whether a ping is honored (e.g. suppressed during a backoff
/// cooldown) lives solely in the worker loop in `spawn_worker`.
const TRAY_REFRESH_DEBOUNCE: Duration = Duration::from_secs(3);

/// Position (within `tray_menu`) the extra-usage line is inserted at when
/// it's shown: right after the two core lines, before the separator.
const EXTRA_USAGE_MENU_POSITION: usize = 2;

enum UserEvent {
    Tray(TrayIconEvent),
    Menu(MenuEvent),
    Usage(TrayState),
}

fn main() {
    // Must happen before any tray icon is created or worker thread spawned:
    // if another copy of the widget is already running, exit immediately
    // rather than creating a duplicate tray icon / registry entry.
    let _single_instance_guard = match single_instance::acquire() {
        Ok(guard) => guard,
        Err(()) => {
            eprintln!("[claude-usage-widget] another instance is already running; exiting.");
            std::process::exit(0);
        }
    };

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();

    let tray_proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = tray_proxy.send_event(UserEvent::Tray(event));
    }));

    let menu_proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event));
    }));

    // Shared with the worker thread so changing the "Poll interval" menu
    // selection takes effect on the next wait cycle without a restart. The
    // registry read here (not just the compile-time preset list) is what
    // enforces the floor even if a stale/hand-edited value is smaller.
    let poll_interval_secs = Arc::new(AtomicU64::new(registry::poll_interval_secs()));

    let worker_proxy = event_loop.create_proxy();
    let refresh_tx = spawn_worker(worker_proxy, Arc::clone(&poll_interval_secs));

    // Menu items that need their text/state updated as usage data comes in.
    let session_item = MenuItem::new("Session  loading...", false, None);
    let weekly_item = MenuItem::new("Weekly   loading...", false, None);
    // Extra-usage/credit-balance line: created up front but only inserted
    // into the menu (see `extra_item_shown` below) when the account has it
    // enabled, so the common case doesn't show an empty/irrelevant line.
    let extra_item = MenuItem::new("Extra usage  loading...", false, None);
    let mut extra_item_shown = false;
    let refresh_item = MenuItem::new("Refresh now", true, None);
    let startup_item = CheckMenuItem::new(
        "Start with Windows",
        true,
        registry::is_startup_enabled(),
        None,
    );

    // Floating usage panel (see `panel.rs`): off by default on first run,
    // restored to whatever the user last chose otherwise.
    let panel_visible_initial = registry::is_panel_visible();
    let panel_mode_initial = registry::panel_mode_str()
        .and_then(|s| PanelMode::from_registry_str(&s))
        .unwrap_or(PanelMode::Both);

    let panel_visible_item = CheckMenuItem::new("Show panel", true, panel_visible_initial, None);
    let panel_mode_both =
        CheckMenuItem::new("Both", true, panel_mode_initial == PanelMode::Both, None);
    let panel_mode_five_hour = CheckMenuItem::new(
        "5-hour only",
        true,
        panel_mode_initial == PanelMode::FiveHourOnly,
        None,
    );
    let panel_mode_weekly = CheckMenuItem::new(
        "Weekly only",
        true,
        panel_mode_initial == PanelMode::WeeklyOnly,
        None,
    );
    let panel_mode_rotating = CheckMenuItem::new(
        "Rotating",
        true,
        panel_mode_initial == PanelMode::Rotating,
        None,
    );
    // Grouped so mutual exclusivity (muda's `CheckMenuItem` has no built-in
    // radio-group concept) can be enforced by hand in `select_panel_mode`.
    let panel_mode_items: Vec<(CheckMenuItem, PanelMode)> = vec![
        (panel_mode_both.clone(), PanelMode::Both),
        (panel_mode_five_hour.clone(), PanelMode::FiveHourOnly),
        (panel_mode_weekly.clone(), PanelMode::WeeklyOnly),
        (panel_mode_rotating.clone(), PanelMode::Rotating),
    ];
    let panel_submenu = Submenu::with_items(
        "Usage panel",
        true,
        &[
            &panel_visible_item,
            &PredefinedMenuItem::separator(),
            &panel_mode_both,
            &panel_mode_five_hour,
            &panel_mode_weekly,
            &panel_mode_rotating,
        ],
    )
    .expect("failed to build usage panel submenu");

    // "Poll interval" submenu: mutually-exclusive presets, same hand-rolled
    // radio-group pattern as the panel-mode items above (`muda`'s
    // `CheckMenuItem` has no built-in radio concept). 1 minute is the
    // floor -- there's deliberately no way to pick anything shorter.
    let current_interval_secs = poll_interval_secs.load(Ordering::Relaxed);
    let poll_interval_items: Vec<(CheckMenuItem, u64)> = POLL_INTERVAL_PRESETS_SECS
        .iter()
        .map(|&(secs, label)| {
            (
                CheckMenuItem::new(label, true, secs == current_interval_secs, None),
                secs,
            )
        })
        .collect();
    let poll_interval_submenu_entries: Vec<&dyn tray_icon::menu::IsMenuItem> = poll_interval_items
        .iter()
        .map(|(item, _)| item as &dyn tray_icon::menu::IsMenuItem)
        .collect();
    let poll_interval_submenu =
        Submenu::with_items("Poll interval", true, &poll_interval_submenu_entries)
            .expect("failed to build poll interval submenu");

    let quit_item = MenuItem::new("Quit", true, None);

    let tray_menu = Menu::new();
    let _ = tray_menu.append_items(&[
        &session_item,
        &weekly_item,
        &PredefinedMenuItem::separator(),
        &refresh_item,
        &startup_item,
        &panel_submenu,
        &poll_interval_submenu,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ]);

    let mut tray_icon: Option<tray_icon::TrayIcon> = None;
    let mut panel_hwnd: HWND = std::ptr::null_mut();

    // Edge-triggered threshold-notification state: true once we've already
    // notified for the current crossing, reset back to false once
    // utilization drops back below the threshold so a later re-crossing can
    // notify again.
    let mut notified_five_hour = false;
    let mut notified_seven_day = false;

    // Debounce state for refreshes triggered by tray icon hover/click.
    let mut last_tray_triggered_refresh: Option<Instant> = None;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => {
                let icon = icon::render(icon::GRAY, None);
                tray_icon = Some(
                    TrayIconBuilder::new()
                        .with_icon(icon)
                        .with_tooltip("Claude usage: loading...")
                        .with_menu(Box::new(tray_menu.clone()))
                        .build()
                        .expect("failed to create tray icon"),
                );

                // Best-effort: pin our icon to "always show" instead of
                // leaving it behind the overflow chevron. Runs on its own
                // thread with retries since Windows registers the
                // NotifyIconSettings entry asynchronously; never blocks
                // startup and never panics on failure.
                registry::promote_tray_icon_async();

                // Create the (initially hidden, unless persisted otherwise)
                // floating usage panel window. Created once here and only
                // shown/hidden afterwards -- see `panel.rs`.
                panel_hwnd = panel::create_window();
                panel::set_mode(panel_hwnd, panel_mode_initial);
                panel::set_visible(panel_hwnd, panel_visible_initial);
            }

            Event::UserEvent(UserEvent::Usage(state)) => {
                if let Some(tray) = tray_icon.as_ref() {
                    let _ = tray.set_icon(Some(icon::render(
                        icon::color_for(&state),
                        state.max_utilization(),
                    )));
                    let _ = tray.set_tooltip(Some(state.tooltip()));
                }
                let (session_text, weekly_text) = state.menu_lines(BAR_WIDTH);
                session_item.set_text(session_text);
                weekly_item.set_text(weekly_text);

                panel::update_data(panel_hwnd, &state);

                match state.extra_usage_line(BAR_WIDTH) {
                    Some(line) => {
                        extra_item.set_text(line);
                        if !extra_item_shown {
                            let _ =
                                tray_menu.insert(&extra_item, EXTRA_USAGE_MENU_POSITION);
                            extra_item_shown = true;
                        }
                    }
                    None => {
                        if extra_item_shown {
                            let _ = tray_menu.remove(&extra_item);
                            extra_item_shown = false;
                        }
                    }
                }

                if let Some((five_hour, seven_day)) = state.windows() {
                    if crossed_threshold(&mut notified_five_hour, five_hour.pct) {
                        notify::show_balloon(
                            "Claude usage".to_string(),
                            format!(
                                "Session at {}%, resets in {}",
                                five_hour.pct, five_hour.resets_label
                            ),
                        );
                    }
                    if crossed_threshold(&mut notified_seven_day, seven_day.pct) {
                        notify::show_balloon(
                            "Claude usage".to_string(),
                            format!(
                                "Weekly at {}%, resets {}",
                                seven_day.pct, seven_day.resets_label
                            ),
                        );
                    }
                }
            }

            Event::UserEvent(UserEvent::Tray(event)) => {
                // Refresh immediately on any real interaction (hover-enter
                // or click) rather than waiting for the poll timer, so the
                // numbers the user is about to look at aren't stale. Move
                // events (continuous mouse movement while already hovering)
                // and Leave are deliberately not treated as interactions.
                let is_interaction = matches!(
                    event,
                    TrayIconEvent::Enter { .. }
                        | TrayIconEvent::Click { .. }
                        | TrayIconEvent::DoubleClick { .. }
                );
                if is_interaction {
                    let now = Instant::now();
                    let should_refresh = match last_tray_triggered_refresh {
                        Some(last) => now.duration_since(last) >= TRAY_REFRESH_DEBOUNCE,
                        None => true,
                    };
                    if should_refresh {
                        last_tray_triggered_refresh = Some(now);
                        let _ = refresh_tx.send(());
                    }
                }
            }

            Event::UserEvent(UserEvent::Menu(event)) => {
                if event.id == refresh_item.id() {
                    let _ = refresh_tx.send(());
                } else if event.id == startup_item.id() {
                    // muda already toggled the visual checked state before
                    // delivering this event; make the registry match it.
                    let desired = startup_item.is_checked();
                    if let Err(e) = registry::set_startup_enabled(desired) {
                        eprintln!(
                            "[claude-usage-widget] failed to update startup registry entry: {e}"
                        );
                        // Reflect the real state back if the write failed.
                        startup_item.set_checked(!desired);
                    }
                } else if event.id == panel_visible_item.id() {
                    let desired = panel_visible_item.is_checked();
                    if let Err(e) = registry::set_panel_visible(desired) {
                        eprintln!(
                            "[claude-usage-widget] failed to persist usage panel visibility: {e}"
                        );
                    }
                    panel::set_visible(panel_hwnd, desired);
                } else if let Some((_, mode)) = panel_mode_items
                    .iter()
                    .find(|(item, _)| event.id == item.id())
                {
                    select_panel_mode(&panel_mode_items, panel_hwnd, *mode);
                } else if let Some((_, secs)) = poll_interval_items
                    .iter()
                    .find(|(item, _)| event.id == item.id())
                {
                    select_poll_interval(&poll_interval_items, &poll_interval_secs, *secs);
                } else if event.id == quit_item.id() {
                    tray_icon.take();
                    *control_flow = ControlFlow::Exit;
                }
            }

            _ => {}
        }
    });
}

/// Edge-triggered threshold check: returns `true` (and marks `already` as
/// notified) exactly once per crossing from below `NOTIFY_THRESHOLD` to
/// at/above it. Resets `already` back to false once `pct` drops below the
/// threshold again, so a later re-crossing can notify again.
fn crossed_threshold(already_notified: &mut bool, pct: u32) -> bool {
    if pct >= NOTIFY_THRESHOLD {
        if !*already_notified {
            *already_notified = true;
            return true;
        }
    } else {
        *already_notified = false;
    }
    false
}

/// Enforces mutual exclusion across the four "Usage panel" mode checkboxes
/// (`muda`'s `CheckMenuItem` has no built-in radio-group concept), persists
/// the choice to the registry, and applies it to the live panel window.
fn select_panel_mode(items: &[(CheckMenuItem, PanelMode)], panel_hwnd: HWND, selected: PanelMode) {
    for (item, mode) in items {
        item.set_checked(*mode == selected);
    }
    if let Err(e) = registry::set_panel_mode_str(selected.as_registry_str()) {
        eprintln!("[claude-usage-widget] failed to persist usage panel mode: {e}");
    }
    panel::set_mode(panel_hwnd, selected);
}

/// Enforces mutual exclusion across the "Poll interval" preset checkboxes,
/// persists the choice (clamped to the 1-minute floor by
/// `registry::set_poll_interval_secs` regardless of what's passed in), and
/// updates the shared value the worker thread reads on its next wait cycle
/// -- no restart needed for the new interval to take effect.
fn select_poll_interval(
    items: &[(CheckMenuItem, u64)],
    poll_interval_secs: &Arc<AtomicU64>,
    selected_secs: u64,
) {
    for (item, secs) in items {
        item.set_checked(*secs == selected_secs);
    }
    if let Err(e) = registry::set_poll_interval_secs(selected_secs) {
        eprintln!("[claude-usage-widget] failed to persist poll interval: {e}");
    }
    poll_interval_secs.store(selected_secs, Ordering::Relaxed);
}

/// Pure exponential-backoff calculation: `base * 2^consecutive_failures`,
/// capped at `cap`. Factored out of `spawn_worker` so it can be unit tested
/// without needing to force a real failure against the live endpoint.
/// `consecutive_failures == 0` yields `base` unchanged (i.e. no backoff yet).
fn backoff_duration(consecutive_failures: u32, base: Duration, cap: Duration) -> Duration {
    // Clamped well below 64 so `1u64 << exponent` can never overflow/panic,
    // no matter how long a failure streak runs; by the time the exponent
    // would matter, `.min(cap)` below has long since taken over anyway.
    let exponent = consecutive_failures.min(20);
    let multiplier = 1u64 << exponent;
    let computed = Duration::from_secs(base.as_secs().saturating_mul(multiplier));
    computed.min(cap)
}

/// The actual wait `spawn_worker` uses after a failure: [`backoff_duration`],
/// optionally *extended* (never shortened) by a server-sent `Retry-After`.
///
/// This distinction matters in practice: the live usage endpoint has been
/// observed sending `Retry-After: 0` while actively rate-limiting every
/// request. Trusting that value literally (as an earlier version of this
/// function did) turns the backoff into a tight retry loop -- the exact
/// failure mode this whole mechanism exists to prevent. So a `Retry-After`
/// only ever raises the wait above what our own exponential schedule would
/// already use, never lowers it.
fn effective_wait(
    consecutive_failures: u32,
    base: Duration,
    cap: Duration,
    retry_after_secs: Option<u64>,
) -> Duration {
    let computed = backoff_duration(consecutive_failures, base, cap);
    retry_after_secs
        .map(Duration::from_secs)
        .map(|server_wait| server_wait.max(computed))
        .unwrap_or(computed)
        .min(cap)
}

/// Spawns the background thread that polls the usage endpoint, and returns a
/// sender that can be used to request an immediate re-fetch (used by
/// "Refresh now" and by tray icon interaction).
///
/// Polls every `poll_interval_secs` (read fresh each cycle, so changing the
/// "Poll interval" menu selection takes effect on the next wait without a
/// restart; always clamped to `registry::MIN_POLL_INTERVAL_SECS`) while
/// healthy. On failure (429, other HTTP errors, and network errors are all
/// treated the same), backs off exponentially per [`backoff_duration`] --
/// respecting a `Retry-After` header when the failure was a 429 that sent
/// one -- up to [`MAX_BACKOFF`], and resets back to the base interval on the
/// next success.
///
/// This loop is the single source of truth for "are we allowed to fetch
/// right now": while backing off, a manual refresh ping (from "Refresh now"
/// or tray icon interaction) is deliberately swallowed rather than
/// shortcutting the wait, so user interaction can never defeat the backoff.
fn spawn_worker(proxy: EventLoopProxy<UserEvent>, poll_interval_secs: Arc<AtomicU64>) -> Sender<()> {
    let (tx, rx) = mpsc::channel::<()>();

    thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                eprintln!("[claude-usage-widget] failed to build HTTP client: {e}");
                let _ = proxy.send_event(UserEvent::Usage(TrayState::Unavailable {
                    detail: "could not initialize HTTP client".to_string(),
                    reason: usage::UnavailableReason::Other,
                }));
                return;
            }
        };

        let mut consecutive_failures: u32 = 0;

        loop {
            // Read fresh every cycle (not cached once) so a live "Poll
            // interval" menu change applies without restarting the widget.
            // The registry's own floor-clamping means this is never below
            // `registry::MIN_POLL_INTERVAL_SECS` even if something odd got
            // written there.
            let base_interval = Duration::from_secs(
                poll_interval_secs
                    .load(Ordering::Relaxed)
                    .max(registry::MIN_POLL_INTERVAL_SECS as u64),
            );

            let outcome = usage::fetch_state(&client);
            let succeeded = matches!(outcome.state, TrayState::Ok { .. });

            if proxy.send_event(UserEvent::Usage(outcome.state)).is_err() {
                // The event loop is gone (app is shutting down); stop polling.
                break;
            }

            let wait = if succeeded {
                if consecutive_failures > 0 {
                    eprintln!(
                        "[claude-usage-widget] usage endpoint recovered after {consecutive_failures} failed attempt(s); resuming {}s polling",
                        base_interval.as_secs()
                    );
                }
                consecutive_failures = 0;
                base_interval
            } else {
                consecutive_failures = consecutive_failures.saturating_add(1);
                let wait = effective_wait(
                    consecutive_failures,
                    base_interval,
                    MAX_BACKOFF,
                    outcome.retry_after_secs,
                );
                eprintln!(
                    "[claude-usage-widget] entering backoff for {}s (consecutive failure #{consecutive_failures})",
                    wait.as_secs()
                );
                wait
            };

            let backing_off = consecutive_failures > 0;
            let deadline = Instant::now() + wait;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match rx.recv_timeout(remaining) {
                    // A manual/interaction-triggered refresh ping: honor it
                    // immediately when healthy, but swallow it (keep
                    // waiting out the rest of the cooldown) while backing
                    // off, so hovering the tray icon or mashing "Refresh
                    // now" during a 429 backoff can't defeat the backoff.
                    Ok(()) => {
                        if backing_off {
                            continue;
                        }
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    });

    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_failures_is_the_base_interval() {
        let base = Duration::from_secs(30);
        let cap = Duration::from_secs(300);
        assert_eq!(backoff_duration(0, base, cap), base);
    }

    #[test]
    fn backoff_grows_exponentially_from_base() {
        let base = Duration::from_secs(30);
        let cap = Duration::from_secs(300);
        assert_eq!(backoff_duration(1, base, cap), Duration::from_secs(60));
        assert_eq!(backoff_duration(2, base, cap), Duration::from_secs(120));
        assert_eq!(backoff_duration(3, base, cap), Duration::from_secs(240));
    }

    #[test]
    fn backoff_is_capped_and_never_overflows() {
        let base = Duration::from_secs(30);
        let cap = Duration::from_secs(300);
        assert_eq!(backoff_duration(4, base, cap), cap); // 30 * 16 = 480 > 300
        assert_eq!(backoff_duration(10, base, cap), cap);
        assert_eq!(backoff_duration(1_000_000, base, cap), cap);
    }

    #[test]
    fn a_zero_or_tiny_retry_after_cannot_undercut_the_backoff_schedule() {
        // Regression test: the live endpoint has been observed sending
        // `Retry-After: 0` while actively 429-ing every request. Naively
        // trusting that value turns the backoff into a tight retry storm --
        // confirmed live during verification of this fix. A server-sent
        // Retry-After must only ever extend the wait, never shrink it below
        // what our own exponential schedule would already use.
        let base = Duration::from_secs(30);
        let cap = Duration::from_secs(300);
        assert_eq!(
            effective_wait(1, base, cap, Some(0)),
            backoff_duration(1, base, cap)
        );
        assert_eq!(
            effective_wait(3, base, cap, Some(1)),
            backoff_duration(3, base, cap)
        );
    }

    #[test]
    fn a_longer_retry_after_is_still_respected() {
        let base = Duration::from_secs(30);
        let cap = Duration::from_secs(300);
        // Server asks for more than our schedule would otherwise wait
        // (but still under the cap): honor it.
        assert_eq!(effective_wait(1, base, cap, Some(90)), Duration::from_secs(90));
        // Server asks for more than the cap: still capped.
        assert_eq!(effective_wait(1, base, cap, Some(10_000)), cap);
    }

    #[test]
    fn no_retry_after_uses_the_computed_backoff() {
        let base = Duration::from_secs(30);
        let cap = Duration::from_secs(300);
        assert_eq!(effective_wait(2, base, cap, None), backoff_duration(2, base, cap));
    }
}
