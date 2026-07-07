// Renders the tray icon in-memory as an RGBA ring, color-coded by usage.
// No external .ico asset is needed.

use crate::usage::TrayState;

pub const GREEN: [u8; 3] = [45, 164, 78];
pub const AMBER: [u8; 3] = [222, 163, 30];
pub const RED: [u8; 3] = [216, 59, 59];
pub const GRAY: [u8; 3] = [140, 140, 140];

const SIZE: u32 = 32;

/// Picks the icon color from the highest of the two utilization percentages
/// (green < 50%, amber 50-80%, red > 80%, gray when data is unavailable).
pub fn color_for(state: &TrayState) -> [u8; 3] {
    match state.max_utilization() {
        None => GRAY,
        Some(pct) if pct > 80 => RED,
        Some(pct) if pct >= 50 => AMBER,
        Some(_) => GREEN,
    }
}

/// Renders a filled circle of the given color into a square RGBA buffer and
/// wraps it as a `tray_icon::Icon`.
pub fn render(color: [u8; 3]) -> tray_icon::Icon {
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];

    let center = (SIZE as f32 - 1.0) / 2.0;
    let radius = SIZE as f32 / 2.0 - 2.0;

    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();

            // One-pixel anti-aliased edge so the circle doesn't look jagged
            // at the small sizes Windows renders tray icons at.
            let coverage = (radius + 0.5 - dist).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }

            let idx = ((y * SIZE + x) * 4) as usize;
            rgba[idx] = color[0];
            rgba[idx + 1] = color[1];
            rgba[idx + 2] = color[2];
            rgba[idx + 3] = (coverage * 255.0) as u8;
        }
    }

    tray_icon::Icon::from_rgba(rgba, SIZE, SIZE).expect("icon buffer has valid dimensions")
}
