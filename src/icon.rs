// Renders the tray icon in-memory as an RGBA ring, color-coded by usage,
// with the highest current utilization percentage drawn as digits on top of
// it. No external .ico asset and no font-rendering crate are needed.
//
// Digits are drawn as bold seven-segment shapes (like a digital clock),
// not a small pixel font -- this is a deliberate redesign after a real
// screenshot showed the previous dilated-glyph-plus-outline approach
// rendering as an illegible blur at actual tray-icon size (16-24 physical
// pixels), even though it looked fine in this crate's own downscaled
// preview tests. Two things changed to fix that:
//   1. Simulating the downscale myself (via a different resize algorithm
//      than whatever Windows Shell actually uses for notification icons)
//      gave false confidence -- the real rendering was worse than the
//      simulated one. Ground truth has to come from an actual screenshot
//      of the real tray icon, not a local preview.
//   2. Outline-plus-fill adds a second set of parallel edges that blur
//      together at tiny sizes, which is likely exactly what made digits
//      look like a blob (described as looking like a face) rather than
//      numbers. Seven-segment digits are bold rectangular blocks with a
//      single edge each -- the same design choice that makes them legible
//      on low-resolution LED/LCD displays -- and are drawn in one flat
//      color with no separate outline layer.

use crate::usage::TrayState;

// Tuned to read as "refined status indicator" rather than "hazard sign" --
// closer to the flat system-status colors iOS/macOS use (systemGreen/
// systemOrange/systemRed/systemGray) than fully-saturated primary colors,
// which is part of what made the earlier badge look more like a warning
// label than an app icon.
pub const GREEN: [u8; 3] = [48, 209, 88];
pub const AMBER: [u8; 3] = [255, 159, 10];
pub const RED: [u8; 3] = [255, 69, 58];
pub const GRAY: [u8; 3] = [142, 142, 147];

// Bumped from 64 (which itself was bumped from the original 32) so the
// bolded, outlined glyphs below have enough headroom to stay proportionally
// thick; Windows downscales this to whatever the actual tray DPI needs, same
// as it always has.
const SIZE: u32 = 96;

/// Which silhouette a badge is drawn as.
///
/// The two tray icons have to be tellable apart at a glance, and colour can't
/// do it -- both use the same green/amber/red scale, so a green circle next to
/// a green circle is exactly the confusing case. Shape is the one channel
/// that stays readable at 16 physical pixels and doesn't collide with the
/// status colour: the round badge is Claude usage, the square one is CPU
/// temperature.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BadgeShape {
    /// Claude usage. The original badge; unchanged so the app's existing
    /// icon doesn't shift identity under people who already know it.
    Circle,
    /// CPU temperature.
    RoundedSquare,
}

/// Radius of the filled badge circle on the `SIZE` canvas.
const BADGE_RADIUS: f32 = SIZE as f32 / 2.0 - 2.0;

/// Half-extent of the square badge, matching the circle's radius so both
/// icons occupy the same footprint in the tray and neither looks larger than
/// the other.
const SQUARE_HALF: f32 = SIZE as f32 / 2.0 - 2.0;

/// Corner rounding on the square badge. Enough to read as deliberately
/// rounded (in the same family as the circle, rather than a hard-edged box
/// that looks like a different app's icon), but not so much that it starts
/// reading as a circle again at tray size -- which would defeat the whole
/// point of using shape as the distinguishing signal.
const SQUARE_CORNER_RADIUS: f32 = 21.0;

/// The radius digits must stay within on a *circular* badge. Slightly inside
/// [`BADGE_RADIUS`] so a digit never runs right up against the circle's
/// anti-aliased edge.
///
/// This constraint exists because of a real, screenshot-confirmed rendering
/// bug: the digit boxes were originally sized to fit the square *canvas*,
/// which is not the same thing as fitting the *circle* drawn on it. A
/// two-digit number at the old size put the corners of the top and bottom
/// segment bars outside the circle, where they were drawn in the digit color
/// (near-black, for a light badge) onto a fully transparent background. On a
/// dark taskbar those overhanging pieces are invisible, so the parts of each
/// digit that survived were the ones inside the circle -- turning "60" into
/// an unreadable blob. The digits looked *bigger* this way, which is
/// presumably why it survived earlier design passes, but they read worse.
const DIGIT_SAFE_RADIUS: f32 = BADGE_RADIUS - 2.0;

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

/// CPU temperature (in Celsius) at/above which the temperature icon turns
/// amber, and at/above which it turns red.
///
/// These are deliberately NOT the same thresholds as the usage percentages
/// above, even though both render as "a number in a colored circle". A CPU at
/// 50% of anything is meaningless; what matters is absolute degrees. Modern
/// laptop CPUs sit in the 40-60 C range at idle and routinely touch 80 C
/// under sustained load without anything being wrong, so amber starts at 70
/// (working hard) and red at 85 (close enough to thermal throttling to be
/// worth a glance).
pub const TEMP_AMBER_C: u32 = 70;
pub const TEMP_RED_C: u32 = 85;

/// Green/amber/red for a CPU temperature in whole degrees Celsius. Mirrors
/// [`color_for_pct`] in shape so both tray icons read as the same family of
/// status indicator, but against temperature thresholds rather than
/// percentage ones.
pub fn color_for_temp_c(celsius: u32) -> [u8; 3] {
    if celsius >= TEMP_RED_C {
        RED
    } else if celsius >= TEMP_AMBER_C {
        AMBER
    } else {
        GREEN
    }
}

/// Renders a filled circle of the given color into a square RGBA buffer,
/// with `pct` (when present) drawn as centered digit text on top, and wraps
/// it as a `tray_icon::Icon`. `pct` is `None` for the "unavailable" state,
/// which keeps today's plain-color-no-digits look rather than guessing.
pub fn render(color: [u8; 3], pct: Option<u32>, shape: BadgeShape) -> tray_icon::Icon {
    let rgba = build_rgba(color, pct, shape);
    tray_icon::Icon::from_rgba(rgba, SIZE, SIZE).expect("icon buffer has valid dimensions")
}

/// The actual pixel-buffer logic behind [`render`], factored out so tests
/// can inspect the raw RGBA bytes directly (`tray_icon::Icon` doesn't expose
/// its buffer back out) rather than only being able to build an opaque
/// platform icon handle.
fn build_rgba(color: [u8; 3], pct: Option<u32>, shape: BadgeShape) -> Vec<u8> {
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];

    let center = SIZE as f32 / 2.0;

    for y in 0..SIZE {
        for x in 0..SIZE {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;

            // Both shapes get a one-pixel anti-aliased edge, so neither looks
            // jagged at the small sizes Windows renders tray icons at.
            let coverage = match shape {
                BadgeShape::Circle => {
                    let dx = px - center;
                    let dy = py - center;
                    (BADGE_RADIUS + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0)
                }
                BadgeShape::RoundedSquare => rounded_rect_coverage(
                    px,
                    py,
                    center,
                    center,
                    SQUARE_HALF,
                    SQUARE_HALF,
                    SQUARE_CORNER_RADIUS,
                ),
            };
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
        draw_percentage(&mut rgba, pct, color, shape);
    }

    rgba
}

/// Perceptual-ish luminance (standard broadcast weights) used only to pick
/// black-vs-white text for readability; not trying to be color-accurate.
fn luminance(c: [u8; 3]) -> f32 {
    0.299 * c[0] as f32 + 0.587 * c[1] as f32 + 0.114 * c[2] as f32
}

/// Single flat color for the percentage digits against the given badge
/// background: whichever of black/white contrasts better with `bg`. No
/// separate outline color is needed -- see the module-level comment on why
/// outline-plus-fill was dropped.
fn text_color_for(bg: [u8; 3]) -> [u8; 3] {
    if luminance(bg) > 140.0 {
        [0, 0, 0]
    } else {
        [255, 255, 255]
    }
}

/// Which of the 7 segments (in `A, B, C, D, E, F, G` order -- top, top-right,
/// bottom-right, bottom, bottom-left, top-left, middle) are lit for each
/// digit 0-9. Standard seven-segment-display layout.
const SEGMENTS: [[bool; 7]; 10] = [
    [true, true, true, true, true, true, false],   // 0
    [false, true, true, false, false, false, false], // 1
    [true, true, false, true, true, false, true],  // 2
    [true, true, true, true, false, false, true],  // 3
    [false, true, true, false, false, true, true], // 4
    [true, false, true, true, false, true, true],  // 5
    [true, false, true, true, true, true, true],   // 6
    [true, true, true, false, false, false, false], // 7
    [true, true, true, true, true, true, true],    // 8
    [true, true, true, true, false, true, true],   // 9
];

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

/// (digit width, digit height, gap between digits) in pixels, on the `SIZE`
/// canvas. Bigger and bolder for fewer digits: the common case (1-2 digits)
/// shouldn't be sized down just to leave room for the rare 3-digit "100".
///
/// Every one of these is sized so the whole block of digits fits within
/// [`DIGIT_SAFE_RADIUS`] -- i.e. inside the circle, not merely inside the
/// square canvas. `digit_blocks_fit_inside_the_badge_circle` enforces that
/// and will fail if these are ever nudged back up past the boundary. The
/// numbers are meaningfully smaller than the pre-fix values, and legibility
/// went *up*, because none of the strokes are being silently cut off any
/// more.
fn digit_box_for_count(shape: BadgeShape, len: usize) -> (i32, i32, i32) {
    match shape {
        BadgeShape::Circle => match len {
            1 => (46, 70, 0),
            2 => (30, 56, 6),
            _ => (20, 52, 3),
        },
        // A square has meaningfully more usable area than the circle
        // inscribed in it, especially out toward the corners where a
        // multi-digit block actually needs the room -- so the square badge
        // isn't just a different silhouette, it also gets to render its
        // digits noticeably larger for the same tray footprint. That matters
        // here because the square is the temperature badge, and a
        // temperature is essentially always two digits.
        // Sized for a similar optical margin to the circle's rather than for
        // the maximum that geometrically fits: filling the square edge to
        // edge measured fine and looked cramped and heavy next to the round
        // badge, which reads as a rendering glitch rather than a deliberate
        // pair. Still larger than the circle's boxes at every digit count.
        BadgeShape::RoundedSquare => match len {
            1 => (50, 74, 0),
            2 => (34, 58, 6),
            _ => (24, 54, 3),
        },
    }
}

/// Whether a digit block of `total_w` x `h`, centered, fits inside `shape`.
///
/// Shared by the render-time `debug_assert!` and the unit test so the two can
/// never disagree about what "fits" means. For the circle it's the corner's
/// distance from the center against the safe radius; for the rounded square
/// it's the corner against the box, with the extra check that a corner poking
/// into the *rounded* part of a corner is still inside the arc.
fn digit_block_fits(shape: BadgeShape, total_w: i32, h: i32) -> bool {
    let half_w = total_w as f32 / 2.0;
    let half_h = h as f32 / 2.0;

    match shape {
        BadgeShape::Circle => half_w.hypot(half_h) <= DIGIT_SAFE_RADIUS,
        BadgeShape::RoundedSquare => {
            let limit = SQUARE_HALF - 1.0; // keep off the anti-aliased edge
            if half_w > limit || half_h > limit {
                return false;
            }
            // Inside the straight edges: only the rounded corners can still
            // clip the block, and only if the block extends past where the
            // rounding starts on BOTH axes at once.
            let straight = limit - SQUARE_CORNER_RADIUS;
            let over_x = (half_w - straight).max(0.0);
            let over_y = (half_h - straight).max(0.0);
            over_x.hypot(over_y) <= SQUARE_CORNER_RADIUS
        }
    }
}

/// Blends `color` into the pixel at (`x`, `y`) with the given coverage
/// (0.0 = untouched, 1.0 = fully replaced). No-op outside the canvas or at
/// zero coverage. This is what makes segment edges soft/anti-aliased
/// instead of hard-cut, the same idea already used for the outer circle's
/// edge below, just reused per-segment instead of per-badge.
fn blend_pixel(rgba: &mut [u8], x: i32, y: i32, color: [u8; 3], coverage: f32) {
    if x < 0 || y < 0 || x >= SIZE as i32 || y >= SIZE as i32 || coverage <= 0.0 {
        return;
    }
    let coverage = coverage.min(1.0);
    let idx = ((y as u32 * SIZE + x as u32) * 4) as usize;
    for c in 0..3 {
        let existing = rgba[idx + c] as f32;
        let target = color[c] as f32;
        rgba[idx + c] = (existing + (target - existing) * coverage).round() as u8;
    }
    rgba[idx + 3] = 255;
}

/// Anti-aliased coverage (0.0-1.0) of a rounded rectangle centered at
/// (`cx`, `cy`) with half-extents (`half_w`, `half_h`) and corner radius
/// `r`, at point (`px`, `py`). Standard rounded-box signed-distance-field
/// formula with a ~1px soft edge, the same analytic-AA approach the outer
/// badge circle already uses.
fn rounded_rect_coverage(px: f32, py: f32, cx: f32, cy: f32, half_w: f32, half_h: f32, r: f32) -> f32 {
    let dx = (px - cx).abs() - (half_w - r);
    let dy = (py - cy).abs() - (half_h - r);
    let outside_dist = dx.max(0.0).hypot(dy.max(0.0));
    let signed_dist = outside_dist - r;
    (0.5 - signed_dist).clamp(0.0, 1.0)
}

/// Fills a rounded rectangle with soft anti-aliased edges. Corner radius is
/// proportional to the smaller side (capped) rather than a flat pixel
/// count, so small and large segments both read as "rounded", not just the
/// big ones.
fn fill_rounded_rect(rgba: &mut [u8], x0: i32, y0: i32, w: i32, h: i32, color: [u8; 3]) {
    if w <= 0 || h <= 0 {
        return;
    }
    let half_w = w as f32 / 2.0;
    let half_h = h as f32 / 2.0;
    let cx = x0 as f32 + half_w;
    let cy = y0 as f32 + half_h;
    let r = (w.min(h) as f32 * 0.35).clamp(1.0, 4.0);

    let pad = 2; // room for the ~1px AA falloff outside the nominal box
    for y in (y0 - pad).max(0)..(y0 + h + pad).min(SIZE as i32) {
        for x in (x0 - pad).max(0)..(x0 + w + pad).min(SIZE as i32) {
            let coverage =
                rounded_rect_coverage(x as f32 + 0.5, y as f32 + 0.5, cx, cy, half_w, half_h, r);
            blend_pixel(rgba, x, y, color, coverage);
        }
    }
}

/// Draws one digit as bold seven-segment bars within the `w`x`h` box at
/// (`x0`, `y0`). Segment thickness is proportional to the box size (roughly
/// a quarter of the width) rather than a flat pixel count, so it stays
/// equally bold whether this is a big single digit or a small one sharing
/// space with two others. Segments are inset from each other by a small
/// gap (real seven-segment displays have one too) and rounded/anti-aliased
/// rather than hard rectangles, so this reads as a deliberately designed
/// badge rather than raw LEGO-block digits.
fn draw_seven_segment_digit(rgba: &mut [u8], x0: i32, y0: i32, w: i32, h: i32, digit: u8, color: [u8; 3]) {
    let segs = SEGMENTS[digit as usize];
    let t = (w / 4).max(5); // segment thickness: bold on purpose
    let gap = (t / 4).max(1); // visual separation between adjoining segments

    let top_y = y0;
    let mid_y = y0 + h / 2 - t / 2;
    let bot_y = y0 + h - t;

    // A: top, D: bottom, G: middle -- full-width horizontal bars, inset a
    // touch on each end so they don't visually fuse with the verticals.
    if segs[0] {
        fill_rounded_rect(rgba, x0 + gap, top_y, w - 2 * gap, t, color);
    }
    if segs[3] {
        fill_rounded_rect(rgba, x0 + gap, bot_y, w - 2 * gap, t, color);
    }
    if segs[6] {
        fill_rounded_rect(rgba, x0 + gap, mid_y, w - 2 * gap, t, color);
    }

    let upper_h = mid_y - (top_y + t) - gap;
    let lower_h = bot_y - (mid_y + t) - gap;

    // F: top-left, B: top-right -- verticals from below the top bar to the
    // middle bar.
    if segs[5] {
        fill_rounded_rect(rgba, x0, top_y + t + gap, t, upper_h, color);
    }
    if segs[1] {
        fill_rounded_rect(rgba, x0 + w - t, top_y + t + gap, t, upper_h, color);
    }
    // E: bottom-left, C: bottom-right -- verticals from below the middle bar
    // to the bottom bar.
    if segs[4] {
        fill_rounded_rect(rgba, x0, mid_y + t + gap, t, lower_h, color);
    }
    if segs[2] {
        fill_rounded_rect(rgba, x0 + w - t, mid_y + t + gap, t, lower_h, color);
    }
}

/// Draws the percentage as 1-3 bold seven-segment digits, centered, in a
/// single flat color chosen to contrast with `bg` (the badge color the icon
/// was already filled with).
fn draw_percentage(rgba: &mut [u8], pct: u32, bg: [u8; 3], shape: BadgeShape) {
    let digits = digits_of(pct);
    let (digit_w, digit_h, gap) = digit_box_for_count(shape, digits.len());
    let total_w = digits.len() as i32 * digit_w + (digits.len() as i32 - 1) * gap;

    // Belt-and-braces companion to the unit test of the same invariant: this
    // one fires on whatever is actually being rendered, including digit
    // counts a future change might introduce without remembering to extend
    // the test's 1..=3 loop.
    debug_assert!(
        digit_block_fits(shape, total_w, digit_h),
        "digit block {total_w}x{digit_h} would extend outside the {shape:?} badge"
    );

    let start_x = (SIZE as i32 - total_w) / 2;
    let start_y = (SIZE as i32 - digit_h) / 2;
    let color = text_color_for(bg);

    for (i, &digit) in digits.iter().enumerate() {
        let x0 = start_x + i as i32 * (digit_w + gap);
        draw_seven_segment_digit(rgba, x0, start_y, digit_w, digit_h, digit, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temperature_colors_follow_the_documented_thresholds() {
        assert_eq!(color_for_temp_c(35), GREEN);
        assert_eq!(color_for_temp_c(TEMP_AMBER_C - 1), GREEN);
        assert_eq!(color_for_temp_c(TEMP_AMBER_C), AMBER);
        assert_eq!(color_for_temp_c(TEMP_RED_C - 1), AMBER);
        assert_eq!(color_for_temp_c(TEMP_RED_C), RED);
        assert_eq!(color_for_temp_c(99), RED);
    }

    #[test]
    fn a_three_digit_temperature_still_renders_without_panicking() {
        // A CPU reading 100 C+ is exactly when the icon matters most, and it
        // is the one case that takes the 3-digit layout branch. `digits_of`
        // clamps at 999, so nothing above can overflow the digit boxes.
        // Checked on the square badge specifically: that's the one that
        // actually shows temperatures.
        let rgba = build_rgba(RED, Some(101), BadgeShape::RoundedSquare);
        assert_eq!(rgba.len(), (SIZE * SIZE * 4) as usize);
    }

    #[test]
    fn digit_boxes_fit_within_the_canvas() {
        // Regression guard: whatever box size/gap is chosen per digit
        // count, the total width must actually fit on the SIZE x SIZE
        // canvas (with room to spare for centering), or digits would get
        // clipped at the edges instead of just looking small.
        for shape in [BadgeShape::Circle, BadgeShape::RoundedSquare] {
            for len in 1..=3usize {
                let (w, h, gap) = digit_box_for_count(shape, len);
                let total_w = len as i32 * w + (len as i32 - 1) * gap;
                assert!(
                    total_w <= SIZE as i32,
                    "{shape:?} {len}-digit total width {total_w} exceeds canvas {SIZE}"
                );
                assert!(h <= SIZE as i32, "{shape:?} {len}-digit height {h} exceeds canvas {SIZE}");
            }
        }
    }

    #[test]
    fn digit_blocks_fit_inside_the_badge_circle() {
        // The bug this pins down: `digit_boxes_fit_within_the_canvas` above
        // passed the whole time the icon was rendering unreadable digits,
        // because fitting the square canvas is a strictly weaker condition
        // than fitting the circle drawn on it. The corners of the digit
        // block are the furthest points from the center, so they are what
        // has to be inside the radius.
        for shape in [BadgeShape::Circle, BadgeShape::RoundedSquare] {
            for len in 1..=3usize {
                let (w, h, gap) = digit_box_for_count(shape, len);
                let total_w = len as i32 * w + (len as i32 - 1) * gap;
                assert!(
                    digit_block_fits(shape, total_w, h),
                    "{shape:?} {len}-digit block ({total_w}x{h}) extends outside the badge -- \
                     its corners would be drawn onto transparent background and disappear \
                     against a dark taskbar"
                );
            }
        }
    }

    #[test]
    fn the_square_badge_fits_larger_digits_than_the_circle() {
        // Not just decoration: the whole reason the temperature badge is the
        // square one is that a temperature is always two digits, and a square
        // has more room for them than the circle inscribed in it. If a change
        // ever made the square's digits the smaller of the two, the shapes
        // would be assigned backwards.
        for len in 1..=3usize {
            let (cw, ch, _) = digit_box_for_count(BadgeShape::Circle, len);
            let (sw, sh, _) = digit_box_for_count(BadgeShape::RoundedSquare, len);
            assert!(
                sw > cw && sh > ch,
                "square {len}-digit box ({sw}x{sh}) should be larger than the circle's ({cw}x{ch})"
            );
        }
    }

    #[test]
    fn the_two_badges_are_actually_distinguishable() {
        // The point of the whole shape change is that the two tray icons can
        // be told apart. The corners are where they differ: a circle leaves
        // them transparent, a rounded square fills them. If some future
        // corner-radius tweak rounded the square until it matched the circle,
        // this catches it -- whereas every other test here would still pass.
        let circle = build_rgba(GREEN, Some(42), BadgeShape::Circle);
        let square = build_rgba(GREEN, Some(42), BadgeShape::RoundedSquare);

        // A point diagonally out toward a corner, outside the circle's radius
        // but inside the square's rounded corner.
        let (x, y) = (14u32, 14u32);
        let alpha_at = |buf: &[u8]| buf[(((y * SIZE) + x) * 4 + 3) as usize];

        assert_eq!(alpha_at(&circle), 0, "circle badge should not fill its corners");
        assert!(
            alpha_at(&square) > 200,
            "square badge should fill its corners (got alpha {})",
            alpha_at(&square)
        );
    }

    #[test]
    fn segment_thickness_leaves_room_for_a_middle_gap() {
        // The middle bar (G) sits between the upper and lower vertical
        // segments, each further inset by a small visual `gap`; if segment
        // thickness (or the gap) were ever too large relative to digit
        // height, the upper/lower verticals would have zero or negative
        // height and segments would overlap into a solid blob instead of a
        // legible digit shape. Mirrors the real layout math in
        // `draw_seven_segment_digit` exactly (including the gap inset),
        // not just an approximation of it.
        for shape in [BadgeShape::Circle, BadgeShape::RoundedSquare] {
            for len in 1..=3usize {
                let (w, h, _spacing) = digit_box_for_count(shape, len);
                let t = (w / 4).max(5);
                let gap = (t / 4).max(1);
                let mid_y = h / 2 - t / 2;
                let upper_h = mid_y - t - gap;
                let lower_h = (h - t) - (mid_y + t) - gap;
                assert!(
                    upper_h > 0,
                    "{shape:?} {len}-digit upper vertical segment has non-positive height"
                );
                assert!(
                    lower_h > 0,
                    "{shape:?} {len}-digit lower vertical segment has non-positive height"
                );
            }
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

        let cases: [(&str, [u8; 3], u32, BadgeShape); 7] = [
            ("usage_amber_42", AMBER, 42, BadgeShape::Circle),
            ("usage_red_87", RED, 87, BadgeShape::Circle),
            ("usage_green_7", GREEN, 7, BadgeShape::Circle),
            ("usage_amber_100", AMBER, 100, BadgeShape::Circle),
            ("usage_red_5", RED, 5, BadgeShape::Circle),
            ("temp_green_62", GREEN, 62, BadgeShape::RoundedSquare),
            ("temp_red_91", RED, 91, BadgeShape::RoundedSquare),
        ];

        for (name, color, pct, shape) in cases {
            let rgba = build_rgba(color, Some(pct), shape);
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
