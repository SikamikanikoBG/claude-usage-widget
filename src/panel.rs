// Optional small always-on-top floating panel showing the same "five_hour"
// and "seven_day" usage windows as the tray icon/menu, but as real drawn
// progress bars rather than text. Off by default (see `registry.rs`); the
// user opts in via the tray menu's "Usage panel" submenu, and the choice
// (shown/hidden + display mode) is remembered across restarts.
//
// Built as a plain always-on-top popup window using raw Win32 calls via
// `windows-sys`, following the same "no extra windowing crate" pattern
// `notify.rs` already established for its balloon-notification window. This
// is deliberately NOT a Win32 AppBar (`SHAppBarMessage`) -- that reserves
// screen real estate globally and is a much bigger, riskier UX change than
// the "small floating window in a corner" that was asked for.
//
// The window is created once at startup and only ever shown/hidden via
// `ShowWindow` afterwards (never destroyed/recreated while the app runs),
// so there's no lifetime juggling between the tray menu toggling visibility
// and the paint/timer logic below. Its wndproc is a plain `extern "system"`
// function with no closure captures (standard Win32 constraint), so the
// data it paints lives in a process-wide `Mutex<PanelState>` updated by
// `update_data`/`set_mode`/`set_visible` from the main thread and read back
// in `WM_PAINT`.

use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::{COLORREF, HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, FrameRect, GetStockObject,
    InvalidateRect, SelectObject, SetBkMode, SetTextColor, TextOutW, DEFAULT_GUI_FONT, HDC,
    PAINTSTRUCT, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetClientRect, KillTimer, RegisterClassW, SetTimer,
    SetWindowPos, ShowWindow, SystemParametersInfoW, SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_NOZORDER,
    SW_HIDE, SW_SHOW, WM_ERASEBKGND, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

use crate::usage::TrayState;

const CLASS_NAME: &str = "ClaudeUsageWidgetPanel";
const PANEL_WIDTH: i32 = 260;
/// Height of a single "label + progress bar" row. The window's total height
/// is `rows * ROW_HEIGHT`, sized to the current mode (see `rows_for_mode`) so
/// each row keeps the same proportions whether one, two or three are shown.
const ROW_HEIGHT: i32 = 45;
/// Most rows any mode shows at once (Both: Session + Weekly + Projected).
/// Used only as the window's initial height at creation, before the real
/// mode is applied.
const MAX_ROWS: i32 = 3;
const CORNER_MARGIN: i32 = 12;
const ROTATE_TIMER_ID: usize = 1;
const ROTATE_INTERVAL_MS: u32 = 2000;
/// Number of distinct single-window views the `Rotating` mode cycles through:
/// Session, then Weekly, then Projected.
const ROTATE_VIEWS: u8 = 3;

/// The four display modes selectable from the tray menu's "Usage panel"
/// submenu.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanelMode {
    Both,
    FiveHourOnly,
    WeeklyOnly,
    Rotating,
}

impl PanelMode {
    /// Stable string used to persist this mode to the registry; deliberately
    /// not `Debug`/derived so a future rename of the enum variant doesn't
    /// silently change what's stored.
    pub fn as_registry_str(self) -> &'static str {
        match self {
            PanelMode::Both => "both",
            PanelMode::FiveHourOnly => "five_hour",
            PanelMode::WeeklyOnly => "weekly",
            PanelMode::Rotating => "rotating",
        }
    }

    pub fn from_registry_str(s: &str) -> Option<Self> {
        match s {
            "both" => Some(PanelMode::Both),
            "five_hour" => Some(PanelMode::FiveHourOnly),
            "weekly" => Some(PanelMode::WeeklyOnly),
            "rotating" => Some(PanelMode::Rotating),
            _ => None,
        }
    }

    /// How many bar rows this mode draws at once, so the window can be sized
    /// to fit them exactly. `Both` shows all three (Session, Weekly,
    /// Projected); `WeeklyOnly` pairs Weekly with its Projected extrapolation;
    /// `Rotating` and `FiveHourOnly` show a single row.
    fn row_count(self) -> i32 {
        match self {
            PanelMode::Both => 3,
            PanelMode::WeeklyOnly => 2,
            PanelMode::FiveHourOnly => 1,
            PanelMode::Rotating => 1,
        }
    }
}

#[derive(Clone)]
struct BarInfo {
    /// The percentage shown in the label. For the projection bar this may
    /// exceed 100 (e.g. 123%); the drawn fill is clamped to 100 in
    /// `draw_bar_row`, the label is not.
    pct: u32,
    color: [u8; 3],
    /// Trailing text after the percentage, e.g. `resets Wed 18:00` for a
    /// usage window or `over` / `under` for the projection.
    detail: String,
}

enum PanelData {
    /// Nothing fetched yet (briefly, right after startup).
    Loading,
    Ok {
        five_hour: BarInfo,
        seven_day: BarInfo,
        /// Run-rate extrapolation of where weekly utilization lands at reset;
        /// the third bar, matching the tray tooltip's/menu's third line.
        projected: BarInfo,
    },
    /// Carries the same short reason label the tray tooltip/menu show (e.g.
    /// "rate-limited, retrying" vs "sign-in needed"), so the panel doesn't
    /// show a vaguer message than the tray icon does for the same failure.
    Unavailable(&'static str),
}

struct PanelState {
    data: PanelData,
    mode: PanelMode,
    visible: bool,
    /// Only meaningful in `PanelMode::Rotating`: which of the `ROTATE_VIEWS`
    /// single-window views (0 = Session, 1 = Weekly, 2 = Projected) is
    /// currently shown; advanced by the `WM_TIMER` handler every
    /// `ROTATE_INTERVAL_MS`.
    rotate_index: u8,
}

static PANEL_STATE: OnceLock<Mutex<PanelState>> = OnceLock::new();

fn state() -> &'static Mutex<PanelState> {
    PANEL_STATE.get_or_init(|| {
        Mutex::new(PanelState {
            data: PanelData::Loading,
            mode: PanelMode::Both,
            visible: false,
            rotate_index: 0,
        })
    })
}

fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// Reads the primary monitor's work area (screen bounds minus the taskbar)
/// via `SPI_GETWORKAREA`. Falls back to a conservative 1920x1080-ish guess
/// if the call somehow fails, rather than a zeroed rect that would pin the
/// panel at the top-left corner.
fn work_area() -> RECT {
    unsafe {
        let mut rect: RECT = std::mem::zeroed();
        let ok = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            &mut rect as *mut RECT as *mut core::ffi::c_void,
            0,
        );
        if ok == 0 {
            rect = RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040,
            };
        }
        rect
    }
}

/// Resizes and repositions the panel to fit `mode`'s row count, keeping it
/// anchored in the bottom-right corner of the work area (above the taskbar).
/// The window grows/shrinks between one and three rows as the user switches
/// modes -- e.g. `Both` is now three rows tall to fit the added Projected
/// bar, while `Rotating` stays a single compact row. A null `hwnd` (window
/// creation failed) is a no-op, like every other function here.
fn position_and_size(hwnd: HWND, mode: PanelMode) {
    if hwnd.is_null() {
        return;
    }
    let height = mode.row_count().max(1) * ROW_HEIGHT;
    let area = work_area();
    let x = area.right - PANEL_WIDTH - CORNER_MARGIN;
    let y = area.bottom - height - CORNER_MARGIN;
    unsafe {
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            x,
            y,
            PANEL_WIDTH,
            height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Enables/disables the rotate timer to match whether the panel is both
/// visible and in `Rotating` mode, and schedules a repaint either way.
fn sync_timer(hwnd: HWND) {
    let should_run = {
        let Ok(s) = state().lock() else { return };
        s.visible && s.mode == PanelMode::Rotating
    };
    unsafe {
        if should_run {
            SetTimer(hwnd, ROTATE_TIMER_ID, ROTATE_INTERVAL_MS, None);
        } else {
            KillTimer(hwnd, ROTATE_TIMER_ID);
        }
        InvalidateRect(hwnd, std::ptr::null(), 0);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize {
    match msg {
        // We repaint the whole client area every time in WM_PAINT, so
        // skip the default background erase to avoid a visible flicker.
        WM_ERASEBKGND => 1,
        WM_TIMER => {
            if wparam == ROTATE_TIMER_ID {
                if let Ok(mut s) = state().lock() {
                    s.rotate_index = (s.rotate_index + 1) % ROTATE_VIEWS;
                }
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            0
        }
        WM_PAINT => {
            paint(hwnd);
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn draw_text(hdc: HDC, x: i32, y: i32, text: &str, color: COLORREF) {
    let wide = wide_null(text);
    unsafe {
        SetTextColor(hdc, color);
        TextOutW(hdc, x, y, wide.as_ptr(), (wide.len() - 1) as i32);
    }
}

/// Draws one "label + percentage + reset time" line and its progress bar
/// within the vertical slice `[top, top + row_h)` of the client area.
fn draw_bar_row(hdc: HDC, client: &RECT, top: i32, row_h: i32, label: &str, bar: &BarInfo) {
    const PAD: i32 = 10;
    const LABEL_H: i32 = 18;

    let label_text = format!("{label}  {}%  {}", bar.pct, bar.detail);
    draw_text(hdc, client.left + PAD, top + 4, &label_text, rgb(230, 230, 230));

    let bar_rect = RECT {
        left: client.left + PAD,
        top: top + 4 + LABEL_H,
        right: client.right - PAD,
        bottom: (top + row_h - 8).max(top + 4 + LABEL_H + 6),
    };

    unsafe {
        let track = CreateSolidBrush(rgb(60, 60, 60));
        FillRect(hdc, &bar_rect, track);
        DeleteObject(track as _);

        let width = (bar_rect.right - bar_rect.left).max(0);
        let filled_w = (width as f32 * (bar.pct.min(100) as f32 / 100.0)).round() as i32;
        if filled_w > 0 {
            let fill_rect = RECT {
                right: bar_rect.left + filled_w,
                ..bar_rect
            };
            let fill_brush = CreateSolidBrush(rgb(bar.color[0], bar.color[1], bar.color[2]));
            FillRect(hdc, &fill_rect, fill_brush);
            DeleteObject(fill_brush as _);
        }

        let border = CreateSolidBrush(rgb(90, 90, 90));
        FrameRect(hdc, &bar_rect, border);
        DeleteObject(border as _);
    }
}

fn paint(hwnd: HWND) {
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.is_null() {
            return;
        }

        let mut client: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut client);

        let bg_brush = CreateSolidBrush(rgb(24, 24, 24));
        FillRect(hdc, &client, bg_brush);
        DeleteObject(bg_brush as _);

        SetBkMode(hdc, TRANSPARENT as i32);
        let font = GetStockObject(DEFAULT_GUI_FONT);
        SelectObject(hdc, font);

        let Ok(s) = state().lock() else {
            EndPaint(hwnd, &ps);
            return;
        };

        match &s.data {
            PanelData::Loading => {
                draw_text(hdc, client.left + 10, client.top + 10, "Claude usage: loading...", rgb(200, 200, 200));
            }
            PanelData::Unavailable(label) => {
                draw_text(
                    hdc,
                    client.left + 10,
                    client.top + 10,
                    &format!("Claude usage: {label}"),
                    rgb(200, 200, 200),
                );
            }
            PanelData::Ok { five_hour, seven_day, projected } => {
                let rows: Vec<(&str, &BarInfo)> = match s.mode {
                    PanelMode::Both => vec![
                        ("Session", five_hour),
                        ("Weekly", seven_day),
                        ("Projected", projected),
                    ],
                    PanelMode::FiveHourOnly => vec![("Session", five_hour)],
                    PanelMode::WeeklyOnly => {
                        vec![("Weekly", seven_day), ("Projected", projected)]
                    }
                    PanelMode::Rotating => match s.rotate_index % ROTATE_VIEWS {
                        0 => vec![("Session", five_hour)],
                        1 => vec![("Weekly", seven_day)],
                        _ => vec![("Projected", projected)],
                    },
                };

                let row_count = rows.len().max(1) as i32;
                let row_h = (client.bottom - client.top) / row_count;
                for (i, (label, bar)) in rows.iter().enumerate() {
                    let top = client.top + i as i32 * row_h;
                    draw_bar_row(hdc, &client, top, row_h, label, bar);
                }
            }
        }

        drop(s);
        EndPaint(hwnd, &ps);
    }
}

/// Creates the (initially hidden) panel window, positioned in the bottom
/// right corner of the primary monitor's work area (i.e. above the
/// taskbar). Returns a null `HWND` -- checked by every other function here
/// -- if window creation fails for some reason; that's logged but never
/// fatal to the rest of the app.
pub fn create_window() -> HWND {
    unsafe {
        let class_name = wide_null(CLASS_NAME);
        let hinstance = GetModuleHandleW(std::ptr::null());

        let wnd_class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            lpszClassName: class_name.as_ptr(),
            hInstance: hinstance,
            ..std::mem::zeroed()
        };
        // Ignore the result: if this class was already registered (should
        // not normally happen, `create_window` is only called once), the
        // existing registration is reused.
        RegisterClassW(&wnd_class);

        // Initial geometry uses the tallest layout (MAX_ROWS); `set_mode` --
        // always called right after `create_window` in `main.rs` -- resizes
        // it to the actually-selected mode via `position_and_size`.
        let initial_height = MAX_ROWS * ROW_HEIGHT;
        let area = work_area();
        let x = area.right - PANEL_WIDTH - CORNER_MARGIN;
        let y = area.bottom - initial_height - CORNER_MARGIN;

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class_name.as_ptr(),
            std::ptr::null(),
            WS_POPUP,
            x,
            y,
            PANEL_WIDTH,
            initial_height,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        );

        if hwnd.is_null() {
            eprintln!(
                "[claude-usage-widget] could not create usage panel window: {}",
                std::io::Error::last_os_error()
            );
        }

        hwnd
    }
}

/// Shows or hides the panel, starting/stopping the rotate timer to match.
pub fn set_visible(hwnd: HWND, visible: bool) {
    if hwnd.is_null() {
        return;
    }
    if let Ok(mut s) = state().lock() {
        s.visible = visible;
    }
    unsafe {
        ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
    }
    sync_timer(hwnd);
}

/// Switches the panel's display mode, starting/stopping the rotate timer to
/// match and resetting rotation back to the five-hour side.
pub fn set_mode(hwnd: HWND, mode: PanelMode) {
    eprintln!("[claude-usage-widget] panel: set_mode({mode:?}), hwnd_is_null={}", hwnd.is_null());
    if let Ok(mut s) = state().lock() {
        s.mode = mode;
        s.rotate_index = 0;
    } else {
        eprintln!("[claude-usage-widget] panel: set_mode could not lock state (poisoned mutex?)");
    }
    if !hwnd.is_null() {
        // Resize to the new mode's row count before syncing the timer/repaint
        // so the window is already the right height when it next paints.
        position_and_size(hwnd, mode);
        sync_timer(hwnd);
    }
}

/// Feeds freshly fetched usage data into the panel and schedules a repaint.
/// Called from the same `UserEvent::Usage` handler that already drives the
/// tray icon/menu, so the panel never needs its own polling.
pub fn update_data(hwnd: HWND, tray_state: &TrayState) {
    let data = match tray_state.windows() {
        Some((five_hour, seven_day)) => {
            // Projection shares the same `Ok` state the windows came from, so
            // this is always `Some` here; fall back defensively just in case.
            let projected = tray_state.weekly_projection();
            let projected_bar = match projected {
                Some(p) => BarInfo {
                    pct: p.projected_pct,
                    // Color by the projected value (clamped for the color
                    // thresholds), so an over-limit projection reads red.
                    color: crate::icon::color_for_pct(p.projected_pct.min(100)),
                    detail: p.tag().to_string(),
                },
                None => BarInfo {
                    pct: 0,
                    color: crate::icon::color_for_pct(0),
                    detail: "--".to_string(),
                },
            };
            PanelData::Ok {
                five_hour: BarInfo {
                    pct: five_hour.pct,
                    color: crate::icon::color_for_pct(five_hour.pct),
                    detail: format!("resets {}", five_hour.resets_label),
                },
                seven_day: BarInfo {
                    pct: seven_day.pct,
                    color: crate::icon::color_for_pct(seven_day.pct),
                    detail: format!("resets {}", seven_day.resets_label),
                },
                projected: projected_bar,
            }
        }
        None => PanelData::Unavailable(tray_state.unavailable_short_label().unwrap_or("unavailable")),
    };

    if let Ok(mut s) = state().lock() {
        s.data = data;
    }

    if !hwnd.is_null() {
        unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;

    #[test]
    fn panel_mode_registry_round_trips() {
        for mode in [
            PanelMode::Both,
            PanelMode::FiveHourOnly,
            PanelMode::WeeklyOnly,
            PanelMode::Rotating,
        ] {
            let s = mode.as_registry_str();
            assert_eq!(PanelMode::from_registry_str(s), Some(mode));
        }
        assert_eq!(PanelMode::from_registry_str("bogus"), None);
    }

    #[test]
    fn work_area_is_non_degenerate() {
        let area = work_area();
        assert!(area.right > area.left);
        assert!(area.bottom > area.top);
    }

    /// End-to-end smoke test of the actual window: creates the real Win32
    /// window, drives it through hidden -> visible -> hidden and through all
    /// four display modes (including `Rotating`, which starts/stops a real
    /// timer), and feeds it both `TrayState` variants -- verifying the
    /// state-transition and window-creation/positioning logic this module
    /// exposes to `main.rs` actually works, without needing a human to click
    /// the tray menu.
    #[test]
    fn create_show_hide_and_cycle_modes() {
        let hwnd = create_window();
        assert!(!hwnd.is_null(), "panel window creation failed");

        // Hidden by default.
        assert_eq!(unsafe { IsWindowVisible(hwnd) }, 0);

        set_visible(hwnd, true);
        assert_ne!(unsafe { IsWindowVisible(hwnd) }, 0);

        for mode in [
            PanelMode::Both,
            PanelMode::FiveHourOnly,
            PanelMode::WeeklyOnly,
            PanelMode::Rotating,
        ] {
            set_mode(hwnd, mode);
            assert_eq!(state().lock().unwrap().mode, mode);
        }

        update_data(
            hwnd,
            &TrayState::Ok {
                five_hour_pct: 42,
                five_hour_resets: chrono::Utc::now(),
                seven_day_pct: 7,
                seven_day_resets: chrono::Utc::now(),
                extra_usage: None,
            },
        );
        assert!(matches!(
            state().lock().unwrap().data,
            PanelData::Ok { .. }
        ));

        update_data(
            hwnd,
            &TrayState::Unavailable {
                detail: "test".to_string(),
                reason: crate::usage::UnavailableReason::RateLimited,
            },
        );
        assert!(matches!(
            state().lock().unwrap().data,
            PanelData::Unavailable(_)
        ));

        set_visible(hwnd, false);
        assert_eq!(unsafe { IsWindowVisible(hwnd) }, 0);
    }
}
