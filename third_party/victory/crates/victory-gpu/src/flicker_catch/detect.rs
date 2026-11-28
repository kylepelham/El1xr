//! Judges presented frames by their tiles' mean luminance
//! ([`super`], `VICTORY_FLICKER_CATCH`).

/// The small picture read back: so many pixels wide and high.
pub(crate) const WIDE: usize = 640;
pub(crate) const HIGH: usize = 360;
/// Tiles across and down.
pub(crate) const ACROSS: usize = 16;
pub(crate) const DOWN: usize = 9;
pub(crate) const TILES: usize = ACROSS * DOWN;

/// A tile lit enough to go dark: mean luminance (0-255).
const LIT: f32 = 16.0;
/// A tile went dark: under this share of its luminance the frame before,
/// and by at least [`FALL`].
const DARK: f32 = 0.35;
const FALL: f32 = 12.0;
/// A frame went dark: at least so many of all tiles did.
const DARK_TILES: usize = TILES / 4;
/// A dark tile is back: at this share of its luminance before.
const BACK: f32 = 0.6;
/// A dark frame ends a flicker when this share of its dark tiles is back,
/// and goes on while this share is still dark.
const BACK_SHARE: f32 = 0.7;
const STILL_SHARE: f32 = 0.5;
/// The most frames a flicker lasts; darker longer is a cut or a fade.
pub(crate) const MOST: u32 = 3;
/// A whole frame is black: every tile under this.
const BLACK: f32 = 1.0;

/// What a frame's judgement found.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Event {
    /// Frame `frame` went dark and the picture was back `frames` frames
    /// later: `dark` tiles of [`TILES`], their mean luminance before and in
    /// the dark frame; `black`: the whole frame was black.
    Flicker {
        frame: u64,
        frames: u32,
        dark: usize,
        before: f32,
        now: f32,
        black: bool,
    },
    /// Frames from `frame` on were black, `frames` of them (more than
    /// [`MOST`]).
    Black { frame: u64, frames: u32 },
    /// (`VICTORY_FLICKER_CATCH_REGION`) Frame `frame` had `tiles` adjacent
    /// tiles (fewer than a frame's flicker needs) go dark and back the
    /// frame after: their mean luminance before and in the dark frame.
    Region {
        frame: u64,
        tiles: Vec<usize>,
        before: f32,
        now: f32,
    },
}

impl Event {
    pub(crate) fn describe(&self) -> String {
        match *self {
            Event::Flicker {
                frames,
                dark,
                before,
                now,
                black,
                ..
            } => format!(
                "{}dark: {dark} of {TILES} tiles, their luma {before:.0}->{now:.0}, back after {frames} frame{}",
                if black { "whole frame black; " } else { "" },
                if frames == 1 { "" } else { "s" }
            ),
            Event::Black { frames, .. } => format!("whole frame black for {frames} frames"),
            Event::Region {
                ref tiles,
                before,
                now,
                ..
            } => {
                let (xs, ys) = (tiles.iter().map(|&t| t % ACROSS), tiles.iter().map(|&t| t / ACROSS));
                format!(
                    "region dark: {} adjacent tiles of {TILES} (x {}-{}, y {}-{}), their luma {before:.0}->{now:.0}, back after 1 frame",
                    tiles.len(),
                    xs.clone().min().unwrap_or(0),
                    xs.max().unwrap_or(0),
                    ys.clone().min().unwrap_or(0),
                    ys.max().unwrap_or(0)
                )
            }
        }
    }
}

struct Pending {
    frame: u64,
    before: Vec<f32>,
    dark: Vec<usize>,
    frames: u32,
    black: bool,
    now: f32,
}

#[derive(Default)]
pub(crate) struct Detector {
    last: Option<(u64, Vec<f32>)>,
    pending: Option<Pending>,
    /// A black run: its first frame and length.
    black: Option<(u64, u32)>,
    /// (`VICTORY_FLICKER_CATCH_REGION`) Adjacent dark tiles that make a
    /// region (0: regions not looked for), and a region waiting for the
    /// frame after: its frame, tiles, their luminance before and now.
    pub(crate) region: usize,
    region_pending: Option<(u64, Vec<usize>, Vec<f32>, f32)>,
}

/// The largest group of `dark` tiles joined by their sides.
pub(crate) fn largest_group(dark: &[usize]) -> Vec<usize> {
    let mut is_dark = [false; TILES];
    for &i in dark {
        if i < TILES {
            is_dark[i] = true;
        }
    }
    let mut seen = [false; TILES];
    let mut best = Vec::new();
    for &start in dark {
        if start >= TILES || seen[start] {
            continue;
        }
        seen[start] = true;
        let mut group = vec![start];
        let mut at = 0;
        while at < group.len() {
            let i = group[at];
            at += 1;
            let (x, y) = (i % ACROSS, i / ACROSS);
            let sides = [
                (x > 0).then(|| i - 1),
                (x + 1 < ACROSS).then(|| i + 1),
                (y > 0).then(|| i - ACROSS),
                (y + 1 < DOWN).then(|| i + ACROSS),
            ];
            for j in sides.into_iter().flatten() {
                if is_dark[j] && !seen[j] {
                    seen[j] = true;
                    group.push(j);
                }
            }
        }
        if group.len() > best.len() {
            best = group;
        }
    }
    best.sort_unstable();
    best
}

fn mean(tiles: &[f32], of: &[usize]) -> f32 {
    if of.is_empty() {
        return 0.0;
    }
    of.iter().map(|&i| tiles[i]).sum::<f32>() / of.len() as f32
}

impl Detector {
    /// Frame `frame`'s tiles ([`TILES`] luminances, 0-255): the events it ends.
    pub(crate) fn push(&mut self, frame: u64, tiles: Vec<f32>) -> Vec<Event> {
        let mut events = Vec::new();
        if self.last.as_ref().is_some_and(|(f, _)| f + 1 != frame) {
            // A frame not read: nothing spans the gap.
            self.pending = None;
            self.black = None;
            self.last = None;
            self.region_pending = None;
        }
        if let Some((first, group, before, now)) = self.region_pending.take() {
            let back = group.iter().filter(|&&i| tiles[i] >= before[i] * BACK).count();
            if back as f32 >= group.len() as f32 * BACK_SHARE {
                events.push(Event::Region {
                    frame: first,
                    before: mean(&before, &group),
                    tiles: group,
                    now,
                });
            }
        }
        let black = tiles.iter().all(|&t| t < BLACK);
        match (&mut self.black, black) {
            (Some((_, n)), true) => *n += 1,
            (None, true) => self.black = Some((frame, 1)),
            (Some((first, n)), false) => {
                if *n > MOST {
                    events.push(Event::Black {
                        frame: *first,
                        frames: *n,
                    });
                }
                self.black = None;
            }
            (None, false) => {}
        }
        if let Some(mut p) = self.pending.take() {
            let back = p
                .dark
                .iter()
                .filter(|&&i| tiles[i] >= p.before[i] * BACK)
                .count();
            let still = p
                .dark
                .iter()
                .filter(|&&i| tiles[i] < p.before[i] * DARK)
                .count();
            if back as f32 >= p.dark.len() as f32 * BACK_SHARE {
                events.push(Event::Flicker {
                    frame: p.frame,
                    frames: p.frames,
                    dark: p.dark.len(),
                    before: mean(&p.before, &p.dark),
                    now: p.now,
                    black: p.black,
                });
            } else if still as f32 >= p.dark.len() as f32 * STILL_SHARE && p.frames < MOST {
                p.frames += 1;
                self.pending = Some(p);
            }
        } else if let Some((_, before)) = &self.last {
            let dark: Vec<usize> = (0..TILES)
                .filter(|&i| {
                    before[i] >= LIT && tiles[i] < before[i] * DARK && before[i] - tiles[i] >= FALL
                })
                .collect();
            if dark.len() >= DARK_TILES {
                self.pending = Some(Pending {
                    frame,
                    now: mean(&tiles, &dark),
                    before: before.clone(),
                    dark,
                    frames: 1,
                    black,
                });
            } else if self.region > 0 && dark.len() >= self.region {
                let group = largest_group(&dark);
                if group.len() >= self.region {
                    self.region_pending =
                        Some((frame, group.clone(), before.clone(), mean(&tiles, &group)));
                }
            }
        }
        self.last = Some((frame, tiles));
        events
    }
}

/// The tiles' mean luminance (Rec. 709, 0-255) of a small picture of
/// [`WIDE`] x [`HIGH`] pixels packed as `r | g << 8 | b << 16`; every other
/// pixel of every other row is read.
pub(crate) fn tiles_of(pixels: &[u32]) -> Vec<f32> {
    let (w, h) = (WIDE / ACROSS, HIGH / DOWN);
    let mut out = Vec::with_capacity(TILES);
    for ty in 0..DOWN {
        for tx in 0..ACROSS {
            let mut sum = 0.0f32;
            let mut n = 0u32;
            for y in (ty * h..(ty + 1) * h).step_by(2) {
                for x in (tx * w..(tx + 1) * w).step_by(2) {
                    let p = pixels.get(y * WIDE + x).copied().unwrap_or(0);
                    let (r, g, b) = (p & 0xff, p >> 8 & 0xff, p >> 16 & 0xff);
                    sum += 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
                    n += 1;
                }
            }
            out.push(sum / n.max(1) as f32);
        }
    }
    out
}

/// A small picture packed as [`tiles_of`] reads it, as 8-bit RGB.
pub(crate) fn rgb_of(pixels: &[u32]) -> Vec<u8> {
    pixels
        .iter()
        .flat_map(|&p| [p as u8, (p >> 8) as u8, (p >> 16) as u8])
        .collect()
}
