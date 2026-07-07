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
// (optionally) the HKCU Run registry key for the "Start with Windows" toggle.
#![windows_subsystem = "windows"]

mod icon;
mod notify;
mod registry;
mod single_instance;
mod usage;

use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIconBuilder, TrayIconEvent};

use usage::TrayState;

// Shortened from the original 60s: the most common complaint was stale
// numbers requiring a manual "Refresh now" click. 20s is still a courteous
// polling interval for an aggregate-percentage endpoint like this one.
const REFRESH_INTERVAL: Duration = Duration::from_secs(20);
const BAR_WIDTH: usize = 10;

/// Utilization percentage (for either window) at/above which a one-time
/// threshold notification fires.
const NOTIFY_THRESHOLD: u32 = 90;

/// Ignore repeat tray-icon-interaction refreshes within this long of the
/// last one, so repeatedly hovering doesn't hammer the endpoint.
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

    let worker_proxy = event_loop.create_proxy();
    let refresh_tx = spawn_worker(worker_proxy);

    // Menu items that need their text/state updated as usage data comes in.
    let session_item = MenuItem::new("Session  loading...", false, None);
    let weekly_item = MenuItem::new("Weekly   loading...", false, None);
    // Extra-usage/credit-balance line: created up front but only inserted
    // into the menu (see `extra_item_shown` below) when the account has it
    // enabled, so the common case doesn't show an empty/irrelevant line.
    let extra_item = MenuItem::new("Extra usage  loading...", false, None);
    let mut extra_item_shown = false;
    let refresh_item = MenuItem::new("Refresh now", true, None);
    let startup_item = tray_icon::menu::CheckMenuItem::new(
        "Start with Windows",
        true,
        registry::is_startup_enabled(),
        None,
    );
    let quit_item = MenuItem::new("Quit", true, None);

    let tray_menu = Menu::new();
    let _ = tray_menu.append_items(&[
        &session_item,
        &weekly_item,
        &PredefinedMenuItem::separator(),
        &refresh_item,
        &startup_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ]);

    let mut tray_icon: Option<tray_icon::TrayIcon> = None;

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

/// Spawns the background thread that polls the usage endpoint every
/// [`REFRESH_INTERVAL`], and returns a sender that can be used to trigger an
/// immediate re-fetch (used by "Refresh now" and by tray icon interaction).
fn spawn_worker(proxy: EventLoopProxy<UserEvent>) -> Sender<()> {
    let (tx, rx) = mpsc::channel::<()>();

    thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                eprintln!("[claude-usage-widget] failed to build HTTP client: {e}");
                let _ = proxy.send_event(UserEvent::Usage(TrayState::Unavailable(
                    "could not initialize HTTP client".to_string(),
                )));
                return;
            }
        };

        loop {
            let state = usage::fetch_state(&client);
            if proxy.send_event(UserEvent::Usage(state)).is_err() {
                // The event loop is gone (app is shutting down); stop polling.
                break;
            }

            match rx.recv_timeout(REFRESH_INTERVAL) {
                // Either a manual/interaction-triggered refresh ping, or the
                // timer elapsed: loop around and fetch again.
                Ok(()) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    tx
}
