//! (El1xr) `VICTORY_FRAME_GRAPH=1`: the overlay panel shows the last frame times as thin bars, a
//! faint line at 33.3 ms, and hitches ([`super::ring::hitch_threshold`]) in the overlay's red.
//! Flat: no fill under the bars, no glow. Drawn on the overlay thread twice a second.

use crate::pixels::Bitmap;

/// The graph's top: 66.7 ms (15 fps); a longer frame reaches it.
const TOP_MS: f32 = 2000.0 / 30.0;
const GUIDE_MS: f32 = 1000.0 / 30.0;
const BAR: [u8; 3] = [150, 156, 168];
const HITCH: [u8; 3] = [255, 98, 88];

/// One bar per frame time, newest at the right: its height in pixels (at least 1) and whether it
/// is a hitch.
pub(crate) fn bars(ms: &[f32], h: usize) -> Vec<(usize, bool)> {
    let limit = super::ring::hitch_threshold(ms);
    ms.iter()
        .map(|&m| {
            let px = ((m / TOP_MS).clamp(0.0, 1.0) * h as f32).round() as usize;
            (px.max(1), m >= limit)
        })
        .collect()
}

/// Draws the graph of `ms` into `bm` at `(x, y)`, `w` x `h` pixels.
pub(crate) fn draw(bm: &mut Bitmap, (x, y, w, h): (usize, usize, usize, usize), ms: &[f32]) {
    let guide = y + h - ((GUIDE_MS / TOP_MS) * h as f32).round() as usize;
    for gx in x..x + w {
        bm.over(gx, guide, [255, 255, 255], 40);
    }
    let shown = &ms[ms.len().saturating_sub(w)..];
    let start = x + w - shown.len();
    for (i, (height, hitch)) in bars(shown, h).into_iter().enumerate() {
        let (rgb, a) = if hitch { (HITCH, 255) } else { (BAR, 200) };
        for gy in y + h - height..y + h {
            bm.over(start + i, gy, rgb, a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_scale_to_66_ms_and_mark_hitches() {
        let ms = [33.3, 16.7, 100.0, 34.0, 0.0];
        let b = bars(&ms, 40);
        assert_eq!(b[0], (20, false));
        assert_eq!(b[1], (10, false));
        assert_eq!(b[2], (40, true), "clamped to the top, a hitch");
        assert_eq!(b[4], (1, false), "always visible");
    }

    #[test]
    fn the_newest_frame_is_the_rightmost_bar() {
        let mut bm = Bitmap::new(10, 20);
        draw(&mut bm, (0, 0, 10, 20), &[40.0, 40.0, 120.0]);
        let alpha = |x: usize, y: usize| bm.px[(y * 10 + x) * 4 + 3];
        assert!(
            alpha(9, 19) > 0 && alpha(9, 0) > 0,
            "the hitch reaches the top"
        );
        assert!(alpha(8, 19) > 0 && alpha(8, 5) == 0, "40 ms bars are 12 px");
        assert_eq!(alpha(0, 19), 0, "no bar where no frame is");
        assert!(alpha(0, 10) > 0, "the 33 ms line spans the width");
    }
}
