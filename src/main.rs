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
mod registry;
mod usage;

use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIconBuilder, TrayIconEvent};

use usage::TrayState;

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const BAR_WIDTH: usize = 10;

enum UserEvent {
    /// Tray icon hover/click events themselves aren't acted on today (the
    /// OS-native context menu already handles clicks), but we still forward
    /// them through so the event loop wakes up promptly if that changes.
    Tray(#[allow(dead_code)] TrayIconEvent),
    Menu(MenuEvent),
    Usage(TrayState),
}

fn main() {
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

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => {
                let icon = icon::render(icon::GRAY);
                tray_icon = Some(
                    TrayIconBuilder::new()
                        .with_icon(icon)
                        .with_tooltip("Claude usage: loading...")
                        .with_menu(Box::new(tray_menu.clone()))
                        .build()
                        .expect("failed to create tray icon"),
                );
            }

            Event::UserEvent(UserEvent::Usage(state)) => {
                if let Some(tray) = tray_icon.as_ref() {
                    let _ = tray.set_icon(Some(icon::render(icon::color_for(&state))));
                    let _ = tray.set_tooltip(Some(state.tooltip()));
                }
                let (session_text, weekly_text) = state.menu_lines(BAR_WIDTH);
                session_item.set_text(session_text);
                weekly_item.set_text(weekly_text);
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

/// Spawns the background thread that polls the usage endpoint every
/// [`REFRESH_INTERVAL`], and returns a sender that can be used to trigger an
/// immediate re-fetch (used by the "Refresh now" menu item).
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
                // Either a manual "Refresh now" ping, or the timer elapsed:
                // loop around and fetch again.
                Ok(()) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    tx
}
