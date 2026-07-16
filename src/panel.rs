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
    CreateWindowExW, DefWindowProcW, GetClientRect, GetSystemMetrics, GetWindowRect,
    IsWindowVisible, KillTimer, RegisterClassW, SetLayeredWindowAttributes, SetTimer, SetWindowPos,
    ShowWindow,
    SystemParametersInfoW, CS_HREDRAW, CS_VREDRAW, HTCAPTION, HTCLIENT, HWND_TOPMOST, LWA_ALPHA,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SPI_GETWORKAREA,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE,
    SW_SHOWNOACTIVATE, WM_ERASEBKGND, WM_EXITSIZEMOVE, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_PAINT,
    WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::registry;
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
/// How much of the panel must remain within the virtual screen for a saved
/// position to be reused on the next start. Guards the "panel was dragged
/// onto a second monitor that is no longer connected" case: without this,
/// the panel would faithfully restore itself to coordinates the user cannot
/// see or reach, which is indistinguishable from it being broken.
const MIN_VISIBLE_PX: i32 = 40;
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
    /// Current opacity, 20-100. Applied via `SetLayeredWindowAttributes`.
    opacity_pct: u32,
    /// Where the user last dragged the panel to, or `None` to keep it corner
    /// anchored. Mirrors `registry::panel_position` -- cached here so the
    /// resize path doesn't hit the registry on every mode change.
    custom_pos: Option<(i32, i32)>,
}

static PANEL_STATE: OnceLock<Mutex<PanelState>> = OnceLock::new();

fn state() -> &'static Mutex<PanelState> {
    PANEL_STATE.get_or_init(|| {
        Mutex::new(PanelState {
            data: PanelData::Loading,
            mode: PanelMode::Both,
            visible: false,
            rotate_index: 0,
            // Unlike `mode`/`visible` (which `main.rs` passes in so it can
            // also tick the matching menu items), these two are read straight
            // from the registry: the window needs them at creation time, and
            // `custom_pos` isn't a menu-driven setting at all -- it's owned by
            // the drag handler below.
            opacity_pct: registry::panel_opacity_pct(),
            custom_pos: registry::panel_position(),
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

/// Whether a panel rectangle overlaps the virtual screen (the union of all
/// monitors) by at least [`MIN_VISIBLE_PX`] on both axes -- i.e. whether the
/// user could actually see and grab it there.
fn is_reachable(x: i32, y: i32, width: i32, height: i32) -> bool {
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        // A zeroed virtual screen means the metrics call gave us nothing
        // useful; don't let that alone condemn a saved position.
        if vw <= 0 || vh <= 0 {
            return true;
        }
        let overlap_x = (x + width).min(vx + vw) - x.max(vx);
        let overlap_y = (y + height).min(vy + vh) - y.max(vy);
        overlap_x >= MIN_VISIBLE_PX && overlap_y >= MIN_VISIBLE_PX
    }
}

/// The panel's default home: the bottom-right corner of the work area, i.e.
/// above the taskbar.
fn default_corner(height: i32) -> (i32, i32) {
    let area = work_area();
    (
        area.right - PANEL_WIDTH - CORNER_MARGIN,
        area.bottom - height - CORNER_MARGIN,
    )
}

/// Resolves a saved position (or the lack of one) to where the panel should
/// actually sit. Split out from [`panel_origin`] as a plain function of its
/// inputs so the "unplugged monitor" fallback can be unit tested without
/// poking the shared state or the registry.
fn origin_for(saved: Option<(i32, i32)>, height: i32) -> (i32, i32) {
    match saved {
        Some((x, y)) if is_reachable(x, y, PANEL_WIDTH, height) => (x, y),
        Some((x, y)) => {
            eprintln!(
                "[claude-usage-widget] panel: saved position ({x},{y}) is off-screen; using the default corner"
            );
            default_corner(height)
        }
        None => default_corner(height),
    }
}

/// Where the panel should sit: wherever the user last dragged it, falling
/// back to the default corner when it has never been moved -- or when the
/// saved spot is no longer reachable (e.g. it was parked on a monitor that
/// has since been unplugged, or the resolution shrank).
fn panel_origin(height: i32) -> (i32, i32) {
    let saved = state().lock().ok().and_then(|s| s.custom_pos);
    origin_for(saved, height)
}

/// Resizes the panel to fit `mode`'s row count and puts it where
/// [`panel_origin`] says it belongs. The window grows/shrinks between one and
/// three rows as the user switches modes -- e.g. `Both` is three rows tall to
/// fit the Projected bar, while `Rotating` stays a single compact row. A null
/// `hwnd` (window creation failed) is a no-op, like every other function here.
fn position_and_size(hwnd: HWND, mode: PanelMode) {
    if hwnd.is_null() {
        return;
    }
    let height = mode.row_count().max(1) * ROW_HEIGHT;
    let (x, y) = panel_origin(height);
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

/// Applies `pct` opacity to the layered panel window.
///
/// This is not optional plumbing: a window created with `WS_EX_LAYERED` is
/// not composited at all until either this or `UpdateLayeredWindow` has been
/// called once, so skipping it would leave the panel permanently invisible
/// rather than merely opaque.
fn apply_opacity(hwnd: HWND, pct: u32) {
    if hwnd.is_null() {
        return;
    }
    let pct = pct.clamp(registry::MIN_PANEL_OPACITY_PCT, 100);
    let alpha = ((pct as f32 / 100.0) * 255.0).round().clamp(0.0, 255.0) as u8;
    unsafe {
        if SetLayeredWindowAttributes(hwnd, 0, alpha, LWA_ALPHA) == 0 {
            eprintln!(
                "[claude-usage-widget] panel: could not set opacity to {pct}%: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// Records the position the user just dragged the panel to, both in memory
/// and in the registry so it survives a restart.
fn save_position(hwnd: HWND) {
    if hwnd.is_null() {
        return;
    }
    let mut rect: RECT = unsafe { std::mem::zeroed() };
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        eprintln!(
            "[claude-usage-widget] panel: could not read window rect after a drag: {}",
            std::io::Error::last_os_error()
        );
        return;
    }

    if let Ok(mut s) = state().lock() {
        s.custom_pos = Some((rect.left, rect.top));
    }
    if let Err(e) = registry::set_panel_position(rect.left, rect.top) {
        eprintln!("[claude-usage-widget] failed to persist usage panel position: {e}");
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
        // Makes the whole panel draggable. The window is a borderless
        // WS_POPUP with no title bar to grab, so we tell the hit test that
        // every point in the client area *is* the caption: DefWindowProcW
        // then runs its standard move loop on WM_NCLBUTTONDOWN, giving us
        // drag-anywhere for free rather than hand-rolling mouse capture and
        // WM_MOUSEMOVE arithmetic.
        WM_NCHITTEST => {
            let hit = unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            if hit == HTCLIENT as isize {
                HTCAPTION as isize
            } else {
                hit
            }
        }
        // Claiming HTCAPTION above also opts us into the rest of the
        // caption's default behaviour -- including "double-click to
        // maximize/restore". A 260px status panel has no business filling the
        // screen, and its layout isn't written for it, so swallow the
        // double-click rather than letting DefWindowProcW act on it.
        WM_NCLBUTTONDBLCLK => 0,
        // Fired once when the move loop above finishes. Saving here rather
        // than on every WM_MOVE means one registry write per drag instead of
        // one per mouse-move message.
        WM_EXITSIZEMOVE => {
            save_position(hwnd);
            0
        }
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
            // Still pair the EndPaint: bailing out without one leaves the
            // update region unvalidated, so Windows immediately posts another
            // WM_PAINT and we spin on it forever, wedging the message loop
            // (and with it the tray icon) instead of just skipping a frame.
            EndPaint(hwnd, &ps);
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
            // Repaint the whole window whenever it changes size. The panel
            // resizes every time the display mode changes (one to three
            // rows), and without these the newly exposed area isn't added to
            // the update region, so a grown panel can keep showing the old,
            // shorter layout until something else happens to invalidate it.
            style: CS_HREDRAW | CS_VREDRAW,
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
        let (x, y) = panel_origin(initial_height);

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
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
            return hwnd;
        }

        // Must happen before the window is ever shown: WS_EX_LAYERED windows
        // aren't composited until their alpha has been set once.
        let opacity = state().lock().map(|s| s.opacity_pct).unwrap_or(registry::DEFAULT_PANEL_OPACITY_PCT);
        apply_opacity(hwnd, opacity);

        // Deliberately burn the process's first ShowWindow call on a no-op
        // hide. Windows documents that the *first* ShowWindow of a process
        // ignores the nCmdShow you pass and substitutes the wShowWindow from
        // the STARTUPINFO its launcher supplied -- so whether our first
        // "please show" is honoured or silently swapped for SW_HIDE depends
        // on who started us (Explorer, the Run key, a shortcut, a debugger).
        // Spending that unpredictable first call here means every later
        // show/hide means exactly what it says.
        ShowWindow(hwnd, SW_HIDE);

        eprintln!(
            "[claude-usage-widget] panel: window created at ({x},{y}) {PANEL_WIDTH}x{initial_height}, opacity {opacity}%"
        );

        hwnd
    }
}

/// Shows or hides the panel, starting/stopping the rotate timer to match.
///
/// Showing goes through `SetWindowPos(HWND_TOPMOST, .., SWP_SHOWWINDOW)`
/// rather than `ShowWindow` alone. Two reasons, both learned from the panel
/// failing to appear at all on some Windows 10 machines while working on
/// Windows 11:
///
///  - `SWP_SHOWWINDOW` isn't subject to the first-call nCmdShow substitution
///    described in `create_window`, so it shows the window even if we somehow
///    still owe Windows that first call.
///  - It re-asserts topmost. A `WS_EX_TOPMOST` window can quietly lose that
///    band (another app forcing itself foreground, an Explorer restart), and
///    a panel that is technically visible but stuck behind every other window
///    looks exactly like a panel that never opened.
///
/// `SW_SHOWNOACTIVATE` (not `SW_SHOW`) keeps the panel from stealing focus
/// from whatever you're typing in when it appears.
pub fn set_visible(hwnd: HWND, visible: bool) {
    if hwnd.is_null() {
        eprintln!("[claude-usage-widget] panel: set_visible({visible}) ignored, no window");
        return;
    }
    if let Ok(mut s) = state().lock() {
        s.visible = visible;
    }
    unsafe {
        if visible {
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            InvalidateRect(hwnd, std::ptr::null(), 0);
        } else {
            ShowWindow(hwnd, SW_HIDE);
        }
    }
    sync_timer(hwnd);
    log_visibility(hwnd, visible);
}

/// Logs where the panel actually ended up after a show/hide, as opposed to
/// where we asked for it to be.
///
/// This exists because "the panel doesn't appear" is otherwise impossible to
/// diagnose remotely -- created-but-off-screen, created-but-transparent,
/// never-created and shown-but-behind-everything all look identical from the
/// outside. With `log.rs` now routing stderr to a file, these lines tell the
/// difference on a machine we can't attach a debugger to.
fn log_visibility(hwnd: HWND, requested: bool) {
    unsafe {
        let mut rect: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut rect) == 0 {
            eprintln!("[claude-usage-widget] panel: visible={requested}, but window rect is unreadable");
            return;
        }
        let actually_visible = IsWindowVisible(hwnd) != 0;
        eprintln!(
            "[claude-usage-widget] panel: visible requested={requested} actual={actually_visible} rect=({},{})-({},{})",
            rect.left, rect.top, rect.right, rect.bottom
        );
    }
}

/// Changes the panel's opacity and repaints. Persisting is the caller's job
/// (`main.rs`), matching how `set_mode`/`set_visible` are wired.
pub fn set_opacity(hwnd: HWND, pct: u32) {
    if let Ok(mut s) = state().lock() {
        s.opacity_pct = pct;
    }
    apply_opacity(hwnd, pct);
}

/// Forgets the dragged-to position and sends the panel back to its default
/// bottom-right corner.
pub fn reset_position(hwnd: HWND) {
    let mode = {
        let Ok(mut s) = state().lock() else {
            return;
        };
        s.custom_pos = None;
        s.mode
    };
    if let Err(e) = registry::clear_panel_position() {
        eprintln!("[claude-usage-widget] failed to clear the saved usage panel position: {e}");
    }
    position_and_size(hwnd, mode);
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

    /// Height of the tallest layout, used by the position tests below.
    fn full_height() -> i32 {
        MAX_ROWS * ROW_HEIGHT
    }

    #[test]
    fn the_default_corner_is_reachable() {
        let (x, y) = default_corner(full_height());
        assert!(is_reachable(x, y, PANEL_WIDTH, full_height()));
    }

    #[test]
    fn no_saved_position_uses_the_default_corner() {
        assert_eq!(origin_for(None, full_height()), default_corner(full_height()));
    }

    #[test]
    fn a_reachable_saved_position_is_honored() {
        // Near the top-left of the primary monitor: on-screen on any machine
        // this test can plausibly run on.
        assert_eq!(origin_for(Some((100, 100)), full_height()), (100, 100));
    }

    /// The panel must not restore itself to coordinates the user can't see or
    /// grab -- e.g. it was dragged onto a second monitor that has since been
    /// unplugged. That would be indistinguishable from the panel being broken.
    #[test]
    fn an_unreachable_saved_position_falls_back_to_the_corner() {
        let far_away = Some((-30_000, -30_000));
        assert_eq!(
            origin_for(far_away, full_height()),
            default_corner(full_height())
        );
        assert!(!is_reachable(-30_000, -30_000, PANEL_WIDTH, full_height()));
    }

    /// A position only barely peeking onto the screen is treated as
    /// unreachable: there'd be nothing left to grab and drag back.
    #[test]
    fn a_barely_visible_saved_position_falls_back_to_the_corner() {
        let area = work_area();
        let sliver = Some((area.right - (MIN_VISIBLE_PX / 2), 100));
        assert_eq!(
            origin_for(sliver, full_height()),
            default_corner(full_height())
        );
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

        // The window must really carry WS_EX_LAYERED: without it this call
        // fails, `apply_opacity` only logs, and the opacity setting would
        // silently do nothing. Assert against the live window rather than
        // trusting the style flags we passed to CreateWindowExW.
        assert_ne!(
            unsafe { SetLayeredWindowAttributes(hwnd, 0, 178, LWA_ALPHA) },
            0,
            "panel window is not layered, so opacity cannot work: {}",
            std::io::Error::last_os_error()
        );

        for pct in [30, 70, 100] {
            set_opacity(hwnd, pct);
            assert_eq!(state().lock().unwrap().opacity_pct, pct);
        }

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
