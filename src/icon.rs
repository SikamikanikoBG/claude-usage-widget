// Renders the tray icon in-memory as an RGBA ring, color-coded by usage,
// with the highest current utilization percentage drawn as text on top of
// it. No external .ico asset and no font-rendering crate are needed: digits
// are drawn from a small hardcoded 5x7 pixel font (see `DIGIT_FONT` below).

use crate::usage::TrayState;

pub const GREEN: [u8; 3] = [45, 164, 78];
pub const AMBER: [u8; 3] = [222, 163, 30];
pub const RED: [u8; 3] = [216, 59, 59];
pub const GRAY: [u8; 3] = [140, 140, 140];

// Bumped from the original 32x32 so 1-3 digit percentages stay legible;
// Windows downscales this to whatever the actual tray DPI needs, same as it
// did before.
const SIZE: u32 = 64;

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

/// Renders a filled circle of the given color into a square RGBA buffer,
/// with `pct` (when present) drawn as centered digit text on top, and wraps
/// it as a `tray_icon::Icon`. `pct` is `None` for the "unavailable" state,
/// which keeps today's plain-color-no-digits look rather than guessing.
pub fn render(color: [u8; 3], pct: Option<u32>) -> tray_icon::Icon {
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

    if let Some(pct) = pct {
        draw_percentage(&mut rgba, pct, text_color_for(color));
    }

    tray_icon::Icon::from_rgba(rgba, SIZE, SIZE).expect("icon buffer has valid dimensions")
}

/// Perceptual-ish luminance (standard broadcast weights) used only to pick
/// black-vs-white text for readability; not trying to be color-accurate.
fn luminance(c: [u8; 3]) -> f32 {
    0.299 * c[0] as f32 + 0.587 * c[1] as f32 + 0.114 * c[2] as f32
}

/// Checked against all four badge colors (green ~119, amber ~166, red ~106,
/// gray 140): white wins on green/red, black wins on amber/gray, so we pick
/// per-background rather than hardcoding a single color for all of them.
fn text_color_for(bg: [u8; 3]) -> [u8; 3] {
    if luminance(bg) > 140.0 {
        [0, 0, 0]
    } else {
        [255, 255, 255]
    }
}

/// 5 wide x 7 tall bitmap font for digits 0-9, one row per byte using the
/// low 5 bits (bit 4 = leftmost pixel, bit 0 = rightmost pixel).
const DIGIT_FONT: [[u8; 7]; 10] = [
    // 0
    [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
    // 1
    [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
    // 2
    [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
    // 3
    [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110],
    // 4
    [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
    // 5
    [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
    // 6
    [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
    // 7
    [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
    // 8
    [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
    // 9
    [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
];

const FONT_W: u32 = 5;
const FONT_H: u32 = 7;

/// Bigger digits when there are fewer of them, so a single-digit percentage
/// (the common case) isn't tiny just to leave room for a 3-digit "100" that
/// only happens at the very cap.
fn scale_for_digit_count(len: usize) -> u32 {
    match len {
        1 => 7,
        2 => 5,
        _ => 3,
    }
}

fn digits_of(pct: u32) -> Vec<u8> {
    let pct = pct.min(999); // defensive; usage.rs already clamps to 0..=100
    if pct == 0 {
        return vec![0];
    }
    let mut digits = Vec::new();
    let mut n = pct;
    while n > 0 {
        digits.push((n % 10) as u8);
        n /= 10;
    }
    digits.reverse();
    digits
}

/// Blits the percentage as 1-3 digits, centered, onto the RGBA buffer.
fn draw_percentage(rgba: &mut [u8], pct: u32, color: [u8; 3]) {
    let digits = digits_of(pct);
    let scale = scale_for_digit_count(digits.len());
    let spacing = scale; // one font-pixel worth of gap between digits

    let glyph_w = FONT_W * scale;
    let glyph_h = FONT_H * scale;
    let total_w = digits.len() as u32 * glyph_w + (digits.len() as u32 - 1) * spacing;

    let start_x = (SIZE as i64 - total_w as i64) / 2;
    let start_y = (SIZE as i64 - glyph_h as i64) / 2;

    for (i, &digit) in digits.iter().enumerate() {
        let glyph = DIGIT_FONT[digit as usize];
        let digit_x0 = start_x + i as i64 * (glyph_w + spacing) as i64;

        for row in 0..FONT_H {
            let bits = glyph[row as usize];
            for col in 0..FONT_W {
                if bits & (1 << (FONT_W - 1 - col)) == 0 {
                    continue;
                }
                // Blit this font pixel as a scale x scale block.
                for sy in 0..scale {
                    for sx in 0..scale {
                        let x = digit_x0 + (col * scale + sx) as i64;
                        let y = start_y + (row * scale + sy) as i64;
                        if x < 0 || y < 0 || x >= SIZE as i64 || y >= SIZE as i64 {
                            continue;
                        }
                        let idx = ((y as u32 * SIZE + x as u32) * 4) as usize;
                        rgba[idx] = color[0];
                        rgba[idx + 1] = color[1];
                        rgba[idx + 2] = color[2];
                        rgba[idx + 3] = 255;
                    }
                }
            }
        }
    }
}
