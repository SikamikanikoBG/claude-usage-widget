// Renders the tray icon in-memory as an RGBA ring, color-coded by usage,
// with the highest current utilization percentage drawn as text on top of
// it. No external .ico asset and no font-rendering crate are needed: digits
// are drawn from a small hardcoded 5x7 pixel font (see `DIGIT_FONT` below),
// bolded via bit-level dilation and given a high-contrast outline (see
// `draw_percentage`) so they stay legible at real tray-icon size regardless
// of the badge color underneath.

use crate::usage::TrayState;

pub const GREEN: [u8; 3] = [45, 164, 78];
pub const AMBER: [u8; 3] = [222, 163, 30];
pub const RED: [u8; 3] = [216, 59, 59];
pub const GRAY: [u8; 3] = [140, 140, 140];

// Bumped from 64 (which itself was bumped from the original 32) so the
// bolded, outlined glyphs below have enough headroom to stay proportionally
// thick; Windows downscales this to whatever the actual tray DPI needs, same
// as it always has.
const SIZE: u32 = 96;

/// Picks the icon color from the highest of the two utilization percentages
/// (green < 50%, amber 50-80%, red > 80%, gray when data is unavailable).
pub fn color_for(state: &TrayState) -> [u8; 3] {
    match state.max_utilization() {
        None => GRAY,
        Some(pct) => color_for_pct(pct),
    }
}

/// Same green/amber/red thresholds as [`color_for`], but for a single raw
/// percentage rather than a `TrayState`. Factored out so the floating usage
/// panel (`panel.rs`) can color its per-window bars identically to the tray
/// icon without duplicating the thresholds.
pub fn color_for_pct(pct: u32) -> [u8; 3] {
    if pct > 80 {
        RED
    } else if pct >= 50 {
        AMBER
    } else {
        GREEN
    }
}

/// Renders a filled circle of the given color into a square RGBA buffer,
/// with `pct` (when present) drawn as centered digit text on top, and wraps
/// it as a `tray_icon::Icon`. `pct` is `None` for the "unavailable" state,
/// which keeps today's plain-color-no-digits look rather than guessing.
pub fn render(color: [u8; 3], pct: Option<u32>) -> tray_icon::Icon {
    let rgba = build_rgba(color, pct);
    tray_icon::Icon::from_rgba(rgba, SIZE, SIZE).expect("icon buffer has valid dimensions")
}

/// The actual pixel-buffer logic behind [`render`], factored out so tests
/// can inspect the raw RGBA bytes directly (`tray_icon::Icon` doesn't expose
/// its buffer back out) rather than only being able to build an opaque
/// platform icon handle.
fn build_rgba(color: [u8; 3], pct: Option<u32>) -> Vec<u8> {
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
        draw_percentage(&mut rgba, pct, color);
    }

    rgba
}

/// Perceptual-ish luminance (standard broadcast weights) used only to pick
/// black-vs-white text for readability; not trying to be color-accurate.
fn luminance(c: [u8; 3]) -> f32 {
    0.299 * c[0] as f32 + 0.587 * c[1] as f32 + 0.114 * c[2] as f32
}

/// Picks (fill, outline) colors for the percentage text against the given
/// badge background: whichever of black/white contrasts better with `bg`
/// is the fill, and the other is the outline drawn one ring around it. This
/// way the digits stay readable regardless of which badge color (green/
/// amber/red/gray) they land on, rather than relying on a single contrast
/// pick alone.
fn text_colors_for(bg: [u8; 3]) -> ([u8; 3], [u8; 3]) {
    if luminance(bg) > 140.0 {
        ([0, 0, 0], [255, 255, 255])
    } else {
        ([255, 255, 255], [0, 0, 0])
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
        1 => 9,
        2 => 6,
        _ => 4,
    }
}

/// Outline thickness in pixels, proportional to `scale` rather than a flat
/// constant: an outline that's fine at the large 1-digit scale would bleed
/// two adjacent 2-3 digit glyphs into an illegible blob if reused as-is (an
/// earlier version of this function did exactly that -- verified by
/// rendering + downscaling actual previews, see the icon preview dump test
/// below). Capped at 3px so it never gets thick enough to swallow the
/// glyph's own strokes at any digit count.
fn outline_px_for_scale(scale: u32) -> i32 {
    ((scale / 3).max(1) as i32).min(3)
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

/// Grows a boolean `size` x `size` mask by `radius` pixels in every
/// direction (Chebyshev/box dilation): a pixel is "on" in the output if any
/// pixel within `radius` of it is "on" in the input. Used to build the
/// outline ring around the (already bolded) glyph mask.
fn dilate_mask(mask: &[bool], size: i32, radius: i32) -> Vec<bool> {
    let mut out = vec![false; mask.len()];
    for y in 0..size {
        for x in 0..size {
            let mut on = false;
            'search: for dy in -radius..=radius {
                let ny = y + dy;
                if ny < 0 || ny >= size {
                    continue;
                }
                for dx in -radius..=radius {
                    let nx = x + dx;
                    if nx < 0 || nx >= size {
                        continue;
                    }
                    if mask[(ny * size + nx) as usize] {
                        on = true;
                        break 'search;
                    }
                }
            }
            out[(y * size + x) as usize] = on;
        }
    }
    out
}

/// Blits the percentage as 1-3 digits, centered, onto the RGBA buffer: first
/// as a bolded (dilated) ink mask, then an outline ring dilated further out
/// from that, drawn outline-first so the ink fill ends up on top. `bg` is
/// the badge color the icon was already filled with, used to pick a fill/
/// outline color pair that contrasts with it.
fn draw_percentage(rgba: &mut [u8], pct: u32, bg: [u8; 3]) {
    let digits = digits_of(pct);
    let scale = scale_for_digit_count(digits.len());
    let outline_px = outline_px_for_scale(scale);
    // Leave enough of a gap between digit cells that two adjacent glyphs'
    // outline rings can never touch (which would fuse them into one blob).
    let spacing = scale + outline_px as u32 * 2;

    let glyph_w = FONT_W * scale;
    let glyph_h = FONT_H * scale;
    let total_w = digits.len() as u32 * glyph_w + (digits.len() as u32 - 1) * spacing;

    let start_x = (SIZE as i64 - total_w as i64) / 2;
    let start_y = (SIZE as i64 - glyph_h as i64) / 2;

    let mut ink = vec![false; (SIZE * SIZE) as usize];

    for (i, &digit) in digits.iter().enumerate() {
        let glyph = DIGIT_FONT[digit as usize];
        let digit_x0 = start_x + i as i64 * (glyph_w + spacing) as i64;

        for row in 0..FONT_H {
            let bits = glyph[row as usize];
            for col in 0..FONT_W {
                if bits & (1 << (FONT_W - 1 - col)) == 0 {
                    continue;
                }
                // Blit this font pixel as a scale x scale block. The
                // boldness here comes entirely from the outline ring drawn
                // around this ink mask below, not from thickening the ink
                // itself -- dilating the strokes *and* outlining them both
                // was tried and over-thickened badly enough that adjacent
                // digits fused into a blob at real tray size (see the
                // preview-dump test at the bottom of this file).
                for sy in 0..scale {
                    for sx in 0..scale {
                        let x = digit_x0 + (col * scale + sx) as i64;
                        let y = start_y + (row * scale + sy) as i64;
                        if x < 0 || y < 0 || x >= SIZE as i64 || y >= SIZE as i64 {
                            continue;
                        }
                        ink[(y as u32 * SIZE + x as u32) as usize] = true;
                    }
                }
            }
        }
    }

    let outline = dilate_mask(&ink, SIZE as i32, outline_px);
    let (fill_color, outline_color) = text_colors_for(bg);

    // Outline first (drawn everywhere the dilated ring covers, including
    // under the ink), then the ink fill on top -- leaving only the ring
    // around the glyph visible in the outline color.
    for (idx, &on) in outline.iter().enumerate() {
        if !on {
            continue;
        }
        let px = idx * 4;
        rgba[px] = outline_color[0];
        rgba[px + 1] = outline_color[1];
        rgba[px + 2] = outline_color[2];
        rgba[px + 3] = 255;
    }
    for (idx, &on) in ink.iter().enumerate() {
        if !on {
            continue;
        }
        let px = idx * 4;
        rgba[px] = fill_color[0];
        rgba[px + 1] = fill_color[1];
        rgba[px + 2] = fill_color[2];
        rgba[px + 3] = 255;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dilate_mask_only_ever_adds_pixels() {
        // A single lit pixel in the middle of a 9x9 mask, dilated by 2,
        // must still cover that original pixel plus strictly more area
        // around it -- dilation should only ever add coverage, never
        // remove or shift it.
        let size = 9;
        let mut mask = vec![false; (size * size) as usize];
        mask[(4 * size + 4) as usize] = true;

        let dilated = dilate_mask(&mask, size, 2);
        assert!(dilated[(4 * size + 4) as usize], "original pixel must survive dilation");
        let before: usize = mask.iter().filter(|&&on| on).count();
        let after: usize = dilated.iter().filter(|&&on| on).count();
        assert!(after > before, "dilation should grow coverage");
    }

    #[test]
    fn outline_stays_bounded_across_digit_counts() {
        // Regression guard for the over-bolding bug this file's history
        // hit: an early version dilated the font glyph itself *and* drew a
        // flat 4px outline, which fused adjacent 2-3 digit glyphs into a
        // solid blob once rendered and downscaled to real tray size (see
        // the preview-dump test below). The outline must stay small
        // relative to `scale` no matter the digit count.
        for len in 1..=3 {
            let scale = scale_for_digit_count(len);
            let outline = outline_px_for_scale(scale);
            assert!(outline >= 1, "outline must stay visible");
            assert!(
                (outline as u32) * 2 < scale,
                "outline ({outline}px) is too large relative to scale ({scale}px) for {len}-digit glyphs and would bleed into neighboring digits"
            );
        }
    }

    /// Not a pass/fail assertion -- dumps a few representative renders
    /// (raw RGBA + dimensions) to the OS temp dir so they can be converted
    /// to PNG and eyeballed. Run explicitly; not part of the default test
    /// suite's job of catching regressions.
    #[test]
    fn dump_previews_for_manual_review() {
        let dir = std::env::temp_dir().join("claude-usage-widget-icon-previews");
        let _ = std::fs::create_dir_all(&dir);

        let cases: [(&str, [u8; 3], u32); 5] = [
            ("amber_42", AMBER, 42),
            ("red_87", RED, 87),
            ("green_7", GREEN, 7),
            ("amber_100", AMBER, 100),
            ("red_5", RED, 5),
        ];

        for (name, color, pct) in cases {
            let rgba = build_rgba(color, Some(pct));
            let path = dir.join(format!("{name}.rgba"));
            let mut bytes = Vec::with_capacity(rgba.len() + 8);
            bytes.extend_from_slice(&SIZE.to_le_bytes());
            bytes.extend_from_slice(&SIZE.to_le_bytes());
            bytes.extend_from_slice(&rgba);
            std::fs::write(&path, &bytes).expect("failed to write preview dump");
            println!("wrote preview: {}", path.display());
        }
    }
}
