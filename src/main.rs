// Claude Usage Widget
//
// A lightweight Windows system-tray widget that shows an at-a-glance view of
// the current Claude Code subscription usage (5-hour session window and
// 7-day weekly window), by calling the same undocumented
// `api.anthropic.com/api/oauth/usage` endpoint that Claude Code's own
// statusline uses, with the OAuth token Claude Code already cached locally.
//
// No telemetry and no network calls to anything other than api.anthropic.com.
// Locally it reads the credentials file, writes a diagnostic log under
// `%LOCALAPPDATA%\ClaudeUsageWidget` (see `log.rs`), and touches
// (optionally) the HKCU Run registry key for the "Start with Windows" toggle
// and the app's own HKCU settings key for the floating usage panel.
#![windows_subsystem = "windows"]

mod cpu_temp;
mod icon;
mod log;
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

/// Opacity choices offered for the floating panel. A tray app has no settings
/// window to hang a slider off, so the setting is a short list of presets in
/// the menu -- the same hand-rolled radio-group pattern the panel-mode and
/// poll-interval submenus already use. The default (see
/// `registry::DEFAULT_PANEL_OPACITY_PCT`) is deliberately one of these, so
/// the submenu always opens with exactly one item ticked.
const PANEL_OPACITY_PRESETS_PCT: [(u32, &str); 5] =
    [(30, "30%"), (50, "50%"), (70, "70% (default)"), (85, "85%"), (100, "100%")];

/// Ceiling for the RATE-LIMITED backoff schedule: even after a long streak
/// of consecutive 429s, never wait longer than this between attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// Base and ceiling for the CONNECTIVITY backoff schedule (network errors,
/// unparseable responses, token problems -- anything that isn't a 429).
/// Live evidence: after the machine sat idle for a few hours, a handful of
/// requests failed with connection/decode errors (stale keep-alive
/// connections after sleep/wake is the leading theory) and self-healed
/// within a few attempts -- but at the rate-limit backoff's schedule, each
/// of those attempts was minutes apart, making a hiccup that actually
/// resolves in seconds look like the widget being broken for a long
/// stretch. These aren't rate-limit signals, so they get a much faster,
/// separate retry schedule instead of borrowing the cautious one meant for
/// 429s.
const CONNECTIVITY_BACKOFF_BASE: Duration = Duration::from_secs(5);
const CONNECTIVITY_MAX_BACKOFF: Duration = Duration::from_secs(60);

const BAR_WIDTH: usize = 10;

/// Utilization percentage (for either window) at/above which a one-time
/// threshold notification fires.
const NOTIFY_THRESHOLD: u32 = 90;

/// Position (within `tray_menu`) the extra-usage line is inserted at when
/// it's shown: right after the three core lines (session, weekly, projected),
/// before the separator.
const EXTRA_USAGE_MENU_POSITION: usize = 3;

/// How often the CPU temperature is sampled.
///
/// Deliberately much faster than the usage poll interval, and deliberately
/// not user-configurable. The reason the usage poll has a 1-minute floor and
/// a configurable interval is that it hits an undocumented, rate-limited
/// remote endpoint; none of that applies here. This reads a local Windows
/// performance counter, so it costs nothing and touches no network. A
/// temperature that updates once every 5 minutes would be useless for the
/// stated purpose -- noticing the machine getting hot at a glance.
const CPU_TEMP_POLL_INTERVAL: Duration = Duration::from_secs(5);

enum UserEvent {
    Menu(MenuEvent),
    Usage(TrayState),
    /// Latest CPU temperature in whole degrees Celsius, or `None` when this
    /// machine exposes no usable thermal sensor / the sample failed.
    CpuTemp(Option<u32>),
}

fn main() {
    // First thing of all: give the rest of this file's `eprintln!`s somewhere
    // to go. This binary has no console (`windows_subsystem = "windows"`), so
    // until this runs every diagnostic in the process is written to an
    // invalid handle and dropped on the floor.
    let log_path = log::init();

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

    // Written after the single-instance check so the log isn't cluttered by
    // start-up attempts that immediately exit. Marks the boundary between
    // runs in an appended log file.
    eprintln!(
        "[claude-usage-widget] ---- starting v{} on {} ----",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    );
    match &log_path {
        Some(path) => eprintln!("[claude-usage-widget] logging to {}", path.display()),
        None => eprintln!("[claude-usage-widget] no log file (this line goes nowhere)"),
    }

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();

    // Tray icon hover/click events are deliberately NOT wired up to trigger
    // anything: an earlier version used them to force an immediate refresh,
    // which turned out to be the actual cause of a real production bug --
    // casually hovering the icon a few times fires extra requests on top of
    // the timer, and those were what tripped this endpoint's rate limit in
    // practice. "Refresh now" in the menu is the only way to force a check;
    // hovering just shows the last fetched data.
    //
    // A handler is still registered here (as a true no-op) rather than left
    // unset entirely. Leaving it unset was tried live and reproduced a real
    // freeze: the widget logged one poll cycle, then went completely silent
    // for 30+ minutes with the tray icon stuck gray, even though the usage
    // endpoint was confirmed reachable again the whole time -- something
    // about not registering a handler at all wedges the GUI thread after
    // the first OS-delivered tray event, in a way that then makes the
    // worker thread's `proxy.send_event` calls silently do nothing forever.
    // (Checked one specific theory -- the crate's internal fallback channel
    // filling up and blocking the Windows message pump -- and ruled it out:
    // that channel is unbounded, so it can't block. The exact mechanism is
    // still unconfirmed; what's confirmed is that this exact plumbing
    // (handler always registered, whether or not it does anything) is what
    // ran stable for 30+ minutes in 0.2.0/0.3.0 before this file removed it,
    // so restoring it is the safe fix rather than a fully understood one.)
    TrayIconEvent::set_event_handler(Some(|_event| {}));

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

    spawn_cpu_temp_worker(event_loop.create_proxy());

    // Menu items that need their text/state updated as usage data comes in.
    let session_item = MenuItem::new("Session  loading...", false, None);
    let weekly_item = MenuItem::new("Weekly   loading...", false, None);
    // The run-rate projection of where weekly utilization lands at reset --
    // the third "at a glance" line, mirroring the tooltip's third line and
    // the panel's third bar. Always present (unlike the optional extra-usage
    // line), so the weekly reason line above it never gets an orphaned
    // duplicate when data is unavailable.
    let projected_item = MenuItem::new("Projected  loading...", false, None);
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

    // Second tray icon showing CPU temperature (see `cpu_temp.rs`). It's a
    // separate icon rather than another line in this menu or another bar in
    // the panel because the whole point is glanceability: a menu line needs a
    // click, the panel needs a window, but a tray icon is a number that's
    // just always on screen next to the usage one. It can't share the usage
    // icon -- that one is already showing two digits of its own, and there is
    // no legible way to fit two numbers into 16 physical pixels.
    let cpu_temp_visible_initial = registry::is_cpu_temp_visible();
    let cpu_temp_item =
        CheckMenuItem::new("Show CPU temperature", true, cpu_temp_visible_initial, None);

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
    // "Opacity" presets, same hand-rolled radio-group pattern as the mode
    // items above.
    let panel_opacity_initial = registry::panel_opacity_pct();
    let panel_opacity_items: Vec<(CheckMenuItem, u32)> = PANEL_OPACITY_PRESETS_PCT
        .iter()
        .map(|&(pct, label)| {
            (
                CheckMenuItem::new(label, true, pct == panel_opacity_initial, None),
                pct,
            )
        })
        .collect();
    let panel_opacity_entries: Vec<&dyn tray_icon::menu::IsMenuItem> = panel_opacity_items
        .iter()
        .map(|(item, _)| item as &dyn tray_icon::menu::IsMenuItem)
        .collect();
    let panel_opacity_submenu = Submenu::with_items("Opacity", true, &panel_opacity_entries)
        .expect("failed to build panel opacity submenu");

    // The escape hatch for the drag feature: if the panel ends up somewhere
    // awkward (or on a monitor that's since been unplugged), this puts it
    // back in the bottom-right corner without touching the registry by hand.
    let panel_reset_pos_item = MenuItem::new("Reset position", true, None);

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
            &PredefinedMenuItem::separator(),
            &panel_opacity_submenu,
            &panel_reset_pos_item,
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
        &projected_item,
        &PredefinedMenuItem::separator(),
        &refresh_item,
        &startup_item,
        &cpu_temp_item,
        &panel_submenu,
        &poll_interval_submenu,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ]);

    let mut tray_icon: Option<tray_icon::TrayIcon> = None;
    // Present only while the CPU-temperature icon is enabled: dropping the
    // `TrayIcon` is what removes it from the tray, and rebuilding it is what
    // puts it back, so the `Option` is the on/off state rather than a
    // separate flag that could drift out of sync with reality.
    let mut temp_tray: Option<tray_icon::TrayIcon> = None;
    // Last temperature seen, kept so that toggling the icon back on can
    // render the current reading immediately instead of showing an
    // unavailable-gray icon until the next sample comes in.
    let mut last_temp_c: Option<u32> = None;
    let mut panel_hwnd: HWND = std::ptr::null_mut();

    // Edge-triggered threshold-notification state: true once we've already
    // notified for the current crossing, reset back to false once
    // utilization drops back below the threshold so a later re-crossing can
    // notify again.
    let mut notified_five_hour = false;
    let mut notified_seven_day = false;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => {
                let icon = icon::render(icon::GRAY, None, icon::BadgeShape::Circle);
                tray_icon = Some(
                    TrayIconBuilder::new()
                        .with_icon(icon)
                        .with_tooltip("Claude usage: loading...")
                        .with_menu(Box::new(tray_menu.clone()))
                        .build()
                        .expect("failed to create tray icon"),
                );

                if cpu_temp_visible_initial {
                    temp_tray = build_temp_tray(&tray_menu, last_temp_c);
                }

                // Best-effort: pin our icon(s) to "always show" instead of
                // leaving them behind the overflow chevron. Runs on its own
                // thread with retries since Windows registers the
                // NotifyIconSettings entry asynchronously; never blocks
                // startup and never panics on failure. The count matters --
                // Windows keeps one entry per icon, and promoting only the
                // first would leave the temperature icon hidden.
                registry::promote_tray_icon_async(if cpu_temp_visible_initial { 2 } else { 1 });

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
                        icon::BadgeShape::Circle,
                    )));
                    let _ = tray.set_tooltip(Some(state.tooltip()));
                }
                let (session_text, weekly_text) = state.menu_lines(BAR_WIDTH);
                session_item.set_text(session_text);
                weekly_item.set_text(weekly_text);
                projected_item.set_text(
                    state
                        .projected_menu_line(BAR_WIDTH)
                        .unwrap_or_else(|| "Projected  unavailable".to_string()),
                );

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

            Event::UserEvent(UserEvent::CpuTemp(celsius)) => {
                last_temp_c = celsius;
                if let Some(tray) = temp_tray.as_ref() {
                    apply_temp_to_tray(tray, celsius);
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
                } else if event.id == cpu_temp_item.id() {
                    // muda has already flipped the visual checkmark; make the
                    // tray and the registry agree with it.
                    let desired = cpu_temp_item.is_checked();
                    if let Err(e) = registry::set_cpu_temp_visible(desired) {
                        eprintln!(
                            "[claude-usage-widget] failed to persist CPU temperature visibility: {e}"
                        );
                    }
                    if desired {
                        temp_tray = build_temp_tray(&tray_menu, last_temp_c);
                        // Only worth re-running when an icon was just added;
                        // the entry for a newly registered icon won't exist
                        // until Windows gets around to writing it.
                        registry::promote_tray_icon_async(2);
                    } else {
                        // Dropping the TrayIcon is what removes it from the
                        // tray -- there's no explicit hide call.
                        temp_tray.take();
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
                } else if let Some((_, pct)) = panel_opacity_items
                    .iter()
                    .find(|(item, _)| event.id == item.id())
                {
                    select_panel_opacity(&panel_opacity_items, panel_hwnd, *pct);
                } else if event.id == panel_reset_pos_item.id() {
                    panel::reset_position(panel_hwnd);
                } else if let Some((_, secs)) = poll_interval_items
                    .iter()
                    .find(|(item, _)| event.id == item.id())
                {
                    select_poll_interval(&poll_interval_items, &poll_interval_secs, *secs);
                } else if event.id == quit_item.id() {
                    tray_icon.take();
                    temp_tray.take();
                    *control_flow = ControlFlow::Exit;
                } else {
                    eprintln!("[claude-usage-widget] menu: unmatched event id {:?}", event.id);
                }
            }

            _ => {}
        }
    });
}

/// Builds the CPU-temperature tray icon, sharing the main tray menu so a
/// right-click on either icon opens the same menu (including the toggle that
/// turns this one back off -- without the shared menu, hiding it would be a
/// one-way trip unless the user knew to right-click the *other* icon).
///
/// Returns `None` if the icon couldn't be created, which is logged and then
/// treated as "the feature is simply not showing" rather than being fatal:
/// the usage icon, the panel and the menu all keep working regardless.
fn build_temp_tray(menu: &Menu, celsius: Option<u32>) -> Option<tray_icon::TrayIcon> {
    let built = TrayIconBuilder::new()
        .with_icon(temp_icon(celsius))
        .with_tooltip(temp_tooltip(celsius))
        .with_menu(Box::new(menu.clone()))
        .build();

    match built {
        Ok(tray) => Some(tray),
        Err(e) => {
            eprintln!("[claude-usage-widget] failed to create the CPU temperature tray icon: {e}");
            None
        }
    }
}

/// Pushes a new reading onto an existing temperature tray icon.
fn apply_temp_to_tray(tray: &tray_icon::TrayIcon, celsius: Option<u32>) {
    let _ = tray.set_icon(Some(temp_icon(celsius)));
    let _ = tray.set_tooltip(Some(temp_tooltip(celsius)));
}

/// The temperature icon itself: the number in degrees Celsius over a
/// threshold color, or -- when there's no reading -- a plain gray, digitless
/// badge, matching how the usage icon signals "unavailable" rather than
/// inventing a second idea of what missing data looks like.
///
/// Always the square badge, including in the unavailable case: the shape is
/// what identifies which icon is which, so it has to stay constant even when
/// the number doesn't. A gray circle and a gray square are still tellable
/// apart; two gray circles would not be.
fn temp_icon(celsius: Option<u32>) -> tray_icon::Icon {
    let shape = icon::BadgeShape::RoundedSquare;
    match celsius {
        Some(c) => icon::render(icon::color_for_temp_c(c), Some(c), shape),
        None => icon::render(icon::GRAY, None, shape),
    }
}

/// Tooltip for the temperature icon. Kept deliberately short: Windows
/// silently truncates tray tooltips at 63 characters (see the note in
/// `usage.rs` on the same constraint), and there is nothing to say here
/// beyond the number anyway.
fn temp_tooltip(celsius: Option<u32>) -> String {
    match celsius {
        Some(c) => format!("CPU temperature: {c}\u{00B0}C"),
        None => "CPU temperature: unavailable".to_string(),
    }
}

/// Spawns the background thread that samples the CPU temperature and pushes
/// it to the event loop every [`CPU_TEMP_POLL_INTERVAL`].
///
/// Runs unconditionally, even when the temperature icon is hidden. Sampling a
/// local performance counter is essentially free, and keeping the thread
/// running means toggling the icon on shows a live number immediately rather
/// than a gray circle for the first few seconds. If the machine has no usable
/// thermal sensor, this reports that once and then stops -- there's no point
/// re-checking a hardware capability every 5 seconds forever.
fn spawn_cpu_temp_worker(proxy: EventLoopProxy<UserEvent>) {
    thread::spawn(move || {
        let reader = match cpu_temp::CpuTempReader::new() {
            Some(reader) => reader,
            None => {
                let _ = proxy.send_event(UserEvent::CpuTemp(None));
                return;
            }
        };

        // Only logged when the value actually changes: at a 5-second
        // interval an unconditional heartbeat would be 17,000 lines a day and
        // would bury the poll/backoff lines this log exists for. The usage
        // worker logs every attempt for a specific reason (telling "wedged"
        // apart from "quietly fine" during a real incident) that doesn't
        // apply to a local counter which can't hang on a network call.
        let mut last_logged: Option<u32> = None;

        loop {
            let celsius = reader.read_celsius().map(|c| c.round().max(0.0) as u32);

            if celsius != last_logged {
                match celsius {
                    Some(c) => eprintln!("[claude-usage-widget] cpu temp: {c}C"),
                    None => eprintln!("[claude-usage-widget] cpu temp: sample unavailable"),
                }
                last_logged = celsius;
            }

            if proxy.send_event(UserEvent::CpuTemp(celsius)).is_err() {
                // The event loop is gone (app is shutting down).
                break;
            }

            thread::sleep(CPU_TEMP_POLL_INTERVAL);
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
    eprintln!("[claude-usage-widget] menu: panel mode selected -> {selected:?}");
    for (item, mode) in items {
        item.set_checked(*mode == selected);
    }
    if let Err(e) = registry::set_panel_mode_str(selected.as_registry_str()) {
        eprintln!("[claude-usage-widget] failed to persist usage panel mode: {e}");
    }
    panel::set_mode(panel_hwnd, selected);
}

/// Enforces mutual exclusion across the "Opacity" preset checkboxes,
/// persists the choice (clamped to a readable range by
/// `registry::set_panel_opacity_pct` regardless of what's passed in) and
/// applies it to the live panel window, so the change is visible while the
/// menu is still open.
fn select_panel_opacity(items: &[(CheckMenuItem, u32)], panel_hwnd: HWND, selected_pct: u32) {
    for (item, pct) in items {
        item.set_checked(*pct == selected_pct);
    }
    if let Err(e) = registry::set_panel_opacity_pct(selected_pct) {
        eprintln!("[claude-usage-widget] failed to persist usage panel opacity: {e}");
    }
    panel::set_opacity(panel_hwnd, selected_pct);
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
        fn build_client() -> reqwest::Result<reqwest::blocking::Client> {
            reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
        }

        let mut client = match build_client() {
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
            let unavailable_reason = outcome.state.unavailable_reason();

            // Logged on every attempt, success included: a silent-on-success
            // design once made a real production bug (the GUI thread wedging
            // after a tray-icon event) indistinguishable from "everything's
            // fine, quietly polling" for 30+ minutes. A one-line heartbeat
            // costs nothing and means "is this actually still running" is
            // never again a guessing game from the log alone.
            if let TrayState::Ok {
                five_hour_pct,
                seven_day_pct,
                ..
            } = &outcome.state
            {
                eprintln!(
                    "[claude-usage-widget] poll ok: session {five_hour_pct}%, weekly {seven_day_pct}%"
                );
            }

            if proxy.send_event(UserEvent::Usage(outcome.state)).is_err() {
                // The event loop is gone (app is shutting down); stop polling.
                eprintln!(
                    "[claude-usage-widget] event loop proxy is gone; stopping the poll worker"
                );
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

                // Only an actual 429 gets the long, cautious rate-limit
                // backoff. Everything else (network errors, unparseable
                // responses, token problems) is far more likely to be a
                // short-lived hiccup -- e.g. stale connections after the
                // machine sleeps/wakes -- and gets a much faster schedule so
                // it doesn't look like the widget is broken for minutes at a
                // time over something that resolves itself in seconds.
                let is_rate_limited =
                    unavailable_reason == Some(usage::UnavailableReason::RateLimited);
                let wait = if is_rate_limited {
                    effective_wait(
                        consecutive_failures,
                        base_interval,
                        MAX_BACKOFF,
                        outcome.retry_after_secs,
                    )
                } else {
                    effective_wait(
                        consecutive_failures,
                        CONNECTIVITY_BACKOFF_BASE,
                        CONNECTIVITY_MAX_BACKOFF,
                        None,
                    )
                };

                // A network-transport-level failure (as opposed to a bad
                // HTTP status) is the one case where the client itself might
                // be the problem -- e.g. reusing a keep-alive connection
                // that went stale across a sleep/wake cycle. Rebuilding it
                // is cheap and means the next attempt isn't handicapped by
                // whatever connection caused this failure.
                if unavailable_reason == Some(usage::UnavailableReason::NetworkError) {
                    match build_client() {
                        Ok(fresh) => {
                            client = fresh;
                            eprintln!(
                                "[claude-usage-widget] rebuilt HTTP client after a network error"
                            );
                        }
                        Err(e) => {
                            eprintln!("[claude-usage-widget] could not rebuild HTTP client: {e}");
                        }
                    }
                }

                eprintln!(
                    "[claude-usage-widget] entering backoff for {}s (consecutive failure #{consecutive_failures}, {})",
                    wait.as_secs(),
                    if is_rate_limited { "rate-limited" } else { "connectivity" }
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
