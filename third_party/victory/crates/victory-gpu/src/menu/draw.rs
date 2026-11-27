//! (El1xr) Draws the menu into a premultiplied bitmap: GDI rasterises the glyphs (as the overlay
//! does), everything else is plain pixel maths. Night-purple panel and sunset accents from the
//! README charts (`docs/images/*.svg`).

use windows_sys::Win32::Foundation::SIZE;
use windows_sys::Win32::Graphics::Gdi::*;

use super::layout::{Layout, Rect};
use super::model::{Apply, Kind, MenuModel};
use super::state::{Menu, Row, Tab, shown};
use crate::pixels::Bitmap;

const BG_TOP: [u8; 3] = [0x12, 0x0a, 0x24];
const BG_BOTTOM: [u8; 3] = [0x1b, 0x0f, 0x33];
const BORDER: [u8; 3] = [0x4a, 0x3a, 0x66];
const ROW_SEL: [u8; 3] = [0x2b, 0x1a, 0x47];
const CONTROL: [u8; 3] = [0x24, 0x16, 0x40];
const TEXT: [u8; 3] = [0xf6, 0xee, 0xfe];
const SOFT: [u8; 3] = [0xdd, 0xd3, 0xee];
const DIM: [u8; 3] = [0xa9, 0x9b, 0xc6];
const GOLD: [u8; 3] = [0xff, 0xd3, 0x6e];
const ORANGE: [u8; 3] = [0xff, 0x8a, 0x3d];
const PINK: [u8; 3] = [0xff, 0x3d, 0x8b];
const VIOLET: [u8; 3] = [0x9b, 0x5c, 0xff];
const MINT: [u8; 3] = [0x7c, 0xf0, 0xc0];
const SUNSET: [[u8; 3]; 4] = [GOLD, ORANGE, PINK, VIOLET];

/// Font slots.
const TITLE: usize = 0;
const TAB: usize = 1;
const LABEL: usize = 2;
const HELP: usize = 3;
const VALUE: usize = 4;
const BADGE: usize = 5;
const SMALL: usize = 6;
/// Face, size at 1080 lines, weight.
const FONTS: [(&str, f32, i32); 7] = [
    ("Bahnschrift", 30.0, 600),
    ("Bahnschrift", 18.0, 600),
    ("Segoe UI", 16.5, 600),
    ("Segoe UI", 13.5, 400),
    ("Segoe UI", 15.5, 600),
    ("Bahnschrift", 12.5, 700),
    ("Segoe UI", 13.5, 400),
];

/// Sunset gradient at `t` (0..1).
fn sunset(t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0) * (SUNSET.len() - 1) as f32;
    let i = (t as usize).min(SUNSET.len() - 2);
    mix(SUNSET[i], SUNSET[i + 1], t - i as f32)
}

fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    std::array::from_fn(|k| {
        (f32::from(a[k]) + (f32::from(b[k]) - f32::from(a[k])) * t).round() as u8
    })
}

/// Signed distance from (x, y) to a `w` x `h` rectangle with corner radius `r` at the origin.
fn sdf(x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let (qx, qy) = (
        (x - w / 2.0).abs() - (w / 2.0 - r),
        (y - h / 2.0).abs() - (h / 2.0 - r),
    );
    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
}

/// A rounded rectangle coloured per pixel by `colour(u, v)` (0..1 across and down), opacity `a`.
fn round_rect(bm: &mut Bitmap, r: Rect, radius: f32, a: u32, colour: impl Fn(f32, f32) -> [u8; 3]) {
    let radius = radius.min(r.w as f32 / 2.0).min(r.h as f32 / 2.0);
    for y in 0..r.h.max(0) {
        for x in 0..r.w.max(0) {
            let d = sdf(
                x as f32 + 0.5,
                y as f32 + 0.5,
                r.w as f32,
                r.h as f32,
                radius,
            );
            let c = (0.5 - d).clamp(0.0, 1.0);
            if c > 0.0 && r.x + x >= 0 && r.y + y >= 0 {
                let rgb = colour(x as f32 / r.w as f32, y as f32 / r.h as f32);
                bm.over(
                    (r.x + x) as usize,
                    (r.y + y) as usize,
                    rgb,
                    (c * a as f32 + 0.5) as u32,
                );
            }
        }
    }
}

/// A one-pixel-wide rounded outline.
fn outline(bm: &mut Bitmap, r: Rect, radius: f32, width: f32, rgb: [u8; 3], a: u32) {
    for y in 0..r.h.max(0) {
        for x in 0..r.w.max(0) {
            let d = sdf(
                x as f32 + 0.5,
                y as f32 + 0.5,
                r.w as f32,
                r.h as f32,
                radius,
            );
            let c = (0.5 - (d + width / 2.0).abs() + width / 2.0).clamp(0.0, 1.0);
            if c > 0.0 && r.x + x >= 0 && r.y + y >= 0 {
                bm.over(
                    (r.x + x) as usize,
                    (r.y + y) as usize,
                    rgb,
                    (c * a as f32 + 0.5) as u32,
                );
            }
        }
    }
}

/// The panel with its shadow and background glow; depends only on the layout's size, so it is
/// drawn once per picture size.
fn base(l: &Layout) -> Bitmap {
    let mut bm = Bitmap::new(l.window.w as usize, l.window.h as usize);
    let p = l.panel;
    let (radius, blur) = (l.px(18.0) as f32, (p.x as f32).max(1.0));
    for y in 0..l.window.h {
        for x in 0..l.window.w {
            let (fx, fy) = ((x - p.x) as f32 + 0.5, (y - p.y - l.px(6.0)) as f32 + 0.5);
            let d = sdf(fx, fy, p.w as f32, p.h as f32, radius);
            if d > 0.0 && d < blur {
                let k = 1.0 - d / blur;
                bm.over(x as usize, y as usize, [0, 0, 0], (110.0 * k * k) as u32);
            }
        }
    }
    round_rect(&mut bm, p, radius, 252, |u, v| {
        let bg = mix(BG_TOP, BG_BOTTOM, v);
        // The charts' glow: pink, fading to violet, from the bottom right.
        let (dx, dy) = ((u - 0.88) * 1.6, v - 1.05);
        let g = (1.0 - (dx * dx + dy * dy).sqrt() / 0.8).max(0.0);
        let glow = mix(VIOLET, PINK, g);
        mix(bg, glow, g * g * 0.28)
    });
    outline(&mut bm, p, radius, l.s.max(1.0), BORDER, 200);
    // Sunset rule under the header.
    let rule = Rect {
        x: p.x + l.px(28.0),
        y: l.header.bottom() - l.px(3.0),
        w: p.w - 2 * l.px(28.0),
        h: l.px(3.0).max(2),
    };
    round_rect(&mut bm, rule, rule.h as f32 / 2.0, 255, |u, _| sunset(u));
    bm
}

/// GDI as a glyph rasteriser: each run drawn white on black into a scratch DIB, its grey level
/// the coverage blended in the run's colour.
pub(super) struct Text {
    dc: HDC,
    fonts: Vec<HFONT>,
    ascent: Vec<i32>,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

impl Text {
    pub(super) fn new(s: f32) -> Option<Text> {
        // SAFETY: a memory DC compatible with the screen and plain font creations.
        let dc = unsafe { CreateCompatibleDC(std::ptr::null_mut()) };
        let fonts: Vec<HFONT> = FONTS
            .iter()
            .map(|&(face, px, weight)| {
                let face = wide(face);
                unsafe {
                    CreateFontW(
                        -(px * s).round() as i32,
                        0,
                        0,
                        0,
                        weight,
                        0,
                        0,
                        0,
                        1,
                        0,
                        0,
                        ANTIALIASED_QUALITY as u32,
                        0,
                        face.as_ptr(),
                    )
                }
            })
            .collect();
        let mut t = Text {
            dc,
            fonts,
            ascent: vec![0; FONTS.len()],
        };
        if dc.is_null() || t.fonts.iter().any(|f| f.is_null()) {
            return None;
        }
        for i in 0..FONTS.len() {
            // SAFETY: live DC and font; a plain struct filled by the call.
            unsafe {
                SelectObject(dc, t.fonts[i]);
                let mut tm = std::mem::zeroed::<TEXTMETRICW>();
                GetTextMetricsW(dc, &mut tm);
                t.ascent[i] = tm.tmAscent;
            }
        }
        Some(t)
    }

    fn measure(&self, font: usize, text: &str) -> (i32, i32) {
        let t: Vec<u16> = text.encode_utf16().collect();
        let mut size = SIZE { cx: 0, cy: 0 };
        // SAFETY: live DC and font, a slice and its length.
        unsafe {
            SelectObject(self.dc, self.fonts[font]);
            GetTextExtentPoint32W(self.dc, t.as_ptr(), t.len() as i32, &mut size);
        }
        (size.cx, size.cy)
    }

    /// `text` cut with an ellipsis to fit `width`.
    fn fit(&self, font: usize, text: &str, width: i32) -> String {
        if self.measure(font, text).0 <= width {
            return text.to_string();
        }
        let mut chars: Vec<char> = text.chars().collect();
        while !chars.is_empty() {
            chars.pop();
            let t: String = chars.iter().collect::<String>() + "\u{2026}";
            if self.measure(font, &t).0 <= width {
                return t;
            }
        }
        String::new()
    }

    /// Draws `text` with its top-left at (x, y).
    fn draw(&self, bm: &mut Bitmap, font: usize, text: &str, x: i32, y: i32, rgb: [u8; 3]) {
        self.draw_with(bm, font, text, x, y, |_| rgb);
    }

    /// Draws `text` coloured by `colour(u)` (0..1 across the run).
    fn draw_with(
        &self,
        bm: &mut Bitmap,
        font: usize,
        text: &str,
        x: i32,
        y: i32,
        colour: impl Fn(f32) -> [u8; 3],
    ) {
        let t: Vec<u16> = text.encode_utf16().collect();
        let (w, h) = self.measure(font, text);
        let w = w + 2;
        if w <= 2 || h <= 0 {
            return;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [RGBQUAD {
                rgbBlue: 0,
                rgbGreen: 0,
                rgbRed: 0,
                rgbReserved: 0,
            }],
        };
        let mut bits = std::ptr::null_mut();
        // SAFETY: a zeroed DIB section of w x h x 4 bytes selected for the text, read after
        // GdiFlush, and freed here.
        unsafe {
            let dib = CreateDIBSection(
                self.dc,
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            );
            if dib.is_null() || bits.is_null() {
                return;
            }
            let old = SelectObject(self.dc, dib);
            SelectObject(self.dc, self.fonts[font]);
            SetBkMode(self.dc, TRANSPARENT as i32);
            SetTextColor(self.dc, 0x00ff_ffff);
            TextOutW(self.dc, 0, 0, t.as_ptr(), t.len() as i32);
            GdiFlush();
            let grey = std::slice::from_raw_parts(bits.cast::<u8>(), (w * h) as usize * 4);
            for col in 0..w {
                let rgb = colour(col as f32 / w as f32);
                for row in 0..h {
                    let (px, py) = (x + col, y + row);
                    let a = u32::from(grey[((row * w + col) * 4 + 1) as usize]);
                    if px >= 0 && py >= 0 && a > 0 {
                        bm.over(px as usize, py as usize, rgb, a);
                    }
                }
            }
            SelectObject(self.dc, old);
            DeleteObject(dib);
        }
    }

    /// Draws `text` centred vertically in `r`, at `x`.
    fn mid(&self, bm: &mut Bitmap, font: usize, text: &str, x: i32, r: Rect, rgb: [u8; 3]) {
        let h = self.measure(font, text).1;
        self.draw(bm, font, text, x, r.y + (r.h - h) / 2, rgb);
    }

    /// Draws `text` centred in `r`.
    fn centre(&self, bm: &mut Bitmap, font: usize, text: &str, r: Rect, rgb: [u8; 3]) {
        let w = self.measure(font, text).0;
        self.mid(bm, font, text, r.x + (r.w - w) / 2, r, rgb);
    }
}

impl Drop for Text {
    fn drop(&mut self) {
        // SAFETY: objects this value made.
        unsafe {
            for &f in &self.fonts {
                if !f.is_null() {
                    DeleteObject(f);
                }
            }
            if !self.dc.is_null() {
                DeleteDC(self.dc);
            }
        }
    }
}

/// Fonts and the panel background for one picture size, kept between redraws.
pub(super) struct Painter {
    key: (i32, f32),
    text: Text,
    base: Bitmap,
}

impl Painter {
    pub(super) fn for_layout(cached: Option<Painter>, l: &Layout) -> Option<Painter> {
        let key = (l.window.w, l.s);
        match cached {
            Some(p) if p.key == key && p.base.h == l.window.h as usize => Some(p),
            _ => Some(Painter {
                key,
                text: Text::new(l.s)?,
                base: base(l),
            }),
        }
    }

    /// The menu as `menu`, `model` and `l` say, or nothing when it is closed.
    pub(super) fn paint(
        &self,
        l: &Layout,
        menu: &Menu,
        model: &dyn MenuModel,
        hover: Option<usize>,
    ) -> Option<Bitmap> {
        if !menu.open {
            return None;
        }
        let mut bm = Bitmap {
            w: self.base.w,
            h: self.base.h,
            px: self.base.px.clone(),
        };
        let t = &self.text;
        let px = |v: f32| l.px(v);
        let pad = px(28.0);
        // Header: the name in the sunset gradient, then what this is.
        let (nw, nh) = t.measure(TITLE, "El1xr");
        let ty = l.header.y + (l.header.h - px(3.0) - nh) / 2;
        t.draw_with(&mut bm, TITLE, "El1xr", l.panel.x + pad, ty, sunset);
        let sub_y = ty + t.ascent[TITLE] - t.ascent[TAB];
        t.draw(
            &mut bm,
            TAB,
            "SETTINGS",
            l.panel.x + pad + nw + px(14.0),
            sub_y,
            SOFT,
        );
        // Expert toggle and close button.
        let e = l.expert;
        if menu.expert {
            round_rect(&mut bm, e, e.h as f32 / 2.0, 255, |u, _| {
                sunset(u * 0.7 + 0.15)
            });
            t.centre(&mut bm, BADGE, "EXPERT  ON", e, BG_TOP);
        } else {
            round_rect(&mut bm, e, e.h as f32 / 2.0, 255, |_, _| CONTROL);
            outline(&mut bm, e, e.h as f32 / 2.0, l.s.max(1.0), BORDER, 255);
            t.centre(&mut bm, BADGE, "EXPERT  OFF", e, DIM);
        }
        round_rect(&mut bm, l.close, l.close.h as f32 / 2.0, 255, |_, _| {
            CONTROL
        });
        t.centre(&mut bm, TAB, "\u{2715}", l.close, SOFT);
        // Tabs: the current one lit, with a sunset underline.
        for &(tab, r) in &l.tabs {
            let on = tab == menu.tab;
            if on {
                round_rect(&mut bm, r, px(10.0) as f32, 255, |_, _| ROW_SEL);
                let bar = Rect {
                    x: r.x + px(18.0),
                    y: r.bottom() - px(3.0),
                    w: r.w - 2 * px(18.0),
                    h: px(3.0).max(2),
                };
                round_rect(&mut bm, bar, bar.h as f32 / 2.0, 255, |u, _| sunset(u));
            }
            let title = tab.title().to_uppercase();
            t.centre(
                &mut bm,
                TAB,
                &title,
                r,
                if on {
                    TEXT
                } else if tab == Tab::Experimental {
                    PINK
                } else {
                    DIM
                },
            );
        }
        // Restart banner.
        if let Some(b) = l.banner {
            let pending = model.pending_restart();
            round_rect(&mut bm, b, px(10.0) as f32, 255, |u, _| {
                mix([0x3a, 0x1d, 0x30], [0x2a, 0x16, 0x3c], u)
            });
            let stripe = Rect {
                x: b.x,
                y: b.y,
                w: px(5.0),
                h: b.h,
            };
            round_rect(&mut bm, stripe, stripe.w as f32 / 2.0, 255, |_, v| {
                sunset(0.2 + v * 0.4)
            });
            let labels: Vec<&str> = pending
                .iter()
                .map(|n| {
                    model
                        .infos()
                        .iter()
                        .find(|i| i.name == *n)
                        .map_or(*n, |i| i.label)
                })
                .collect();
            let head = "RESTART NEEDED";
            t.mid(&mut bm, BADGE, head, b.x + px(20.0), b, ORANGE);
            let x = b.x + px(20.0) + t.measure(BADGE, head).0 + px(14.0);
            let list = t.fit(HELP, &labels.join("  \u{b7}  "), b.right() - px(16.0) - x);
            t.mid(&mut bm, HELP, &list, x, b, SOFT);
        }
        // Rows.
        let rows = menu.rows(model);
        for &(i, r) in &l.rows {
            let selected = i == menu.selected;
            if selected || hover == Some(i) {
                round_rect(
                    &mut bm,
                    r,
                    px(12.0) as f32,
                    if selected { 255 } else { 120 },
                    |_, _| ROW_SEL,
                );
            }
            if selected {
                let bar = Rect {
                    x: r.x,
                    y: r.y + px(12.0),
                    w: px(4.0),
                    h: r.h - 2 * px(12.0),
                };
                round_rect(&mut bm, bar, bar.w as f32 / 2.0, 255, |_, v| {
                    sunset(v * 0.66)
                });
            }
            self.row(&mut bm, l, r, rows[i], model, selected);
        }
        if rows.is_empty() {
            t.centre(&mut bm, HELP, "Nothing here yet", l.list, DIM);
        }
        // Scroll marks.
        let n = rows.len();
        if n > l.visible {
            let track = Rect {
                x: l.list.right() + px(10.0),
                y: l.list.y,
                w: px(4.0),
                h: l.visible as i32 * px(60.0),
            };
            round_rect(&mut bm, track, track.w as f32 / 2.0, 255, |_, _| CONTROL);
            let thumb_h = (track.h as usize * l.visible / n) as i32;
            let thumb = Rect {
                y: track.y + (track.h as usize * menu.scroll / n) as i32,
                h: thumb_h,
                ..track
            };
            round_rect(&mut bm, thumb, thumb.w as f32 / 2.0, 255, |_, v| sunset(v));
        }
        // Footer: hints, then the last change's outcome at the right.
        let f = l.footer;
        let line = Rect {
            x: f.x + pad,
            y: f.y,
            w: f.w - 2 * pad,
            h: l.s.max(1.0) as i32,
        };
        round_rect(&mut bm, line, 0.0, 70, |_, _| BORDER);
        let hints = format!(
            "{} / Esc close   \u{2191}\u{2193} select   \u{2190}\u{2192} change   Q E tabs   X expert      Pad: A change  B close  LB RB tabs  Y expert",
            super::hotkey_name()
        );
        let status = menu.status.clone().unwrap_or_default();
        let sw = t.measure(SMALL, &status).0;
        let hints = t.fit(SMALL, &hints, f.w - 2 * pad - sw - px(24.0));
        t.mid(&mut bm, SMALL, &hints, f.x + pad, f, DIM);
        t.mid(&mut bm, SMALL, &status, f.right() - pad - sw, f, GOLD);
        Some(bm)
    }

    fn row(
        &self,
        bm: &mut Bitmap,
        l: &Layout,
        r: Rect,
        row: Row,
        model: &dyn MenuModel,
        selected: bool,
    ) {
        let t = &self.text;
        let px = |v: f32| l.px(v);
        let x = r.x + px(22.0);
        let (lh, hh) = (t.measure(LABEL, "Ag").1, t.measure(HELP, "Ag").1);
        let top = r.y + (r.h - lh - hh) / 2;
        let label_w = l.badge(r).x - px(16.0) - x;
        let (label, help) = match row {
            Row::Setting(i) => {
                let info = model.infos()[i];
                self.badge(bm, l.badge(r), info.apply);
                let value = model
                    .get(info.name)
                    .unwrap_or_else(|| info.default.to_string());
                self.value(bm, l, l.value(r), info.kind, &value, selected);
                (
                    info.label.to_string(),
                    format!("{}   {}", info.help, info.name),
                )
            }
            Row::Preset(p) => {
                let preset = model.presets()[p];
                self.button(bm, l, l.value(r), "APPLY", selected);
                (format!("{} preset", preset.name), preset.help.to_string())
            }
            Row::Save => {
                self.button(bm, l, l.value(r), "SAVE", selected);
                (
                    "Save settings".into(),
                    "Write the changed values to the settings file.".into(),
                )
            }
            Row::Reset => {
                self.button(bm, l, l.value(r), "RESET", selected);
                (
                    "Reset to defaults".into(),
                    "Every setting back to its default value.".into(),
                )
            }
        };
        t.draw(
            bm,
            LABEL,
            &t.fit(LABEL, &label, label_w),
            x,
            top,
            if selected { TEXT } else { SOFT },
        );
        t.draw(bm, HELP, &t.fit(HELP, &help, label_w), x, top + lh, DIM);
    }

    fn badge(&self, bm: &mut Bitmap, b: Rect, apply: Apply) {
        let (text, rgb) = match apply {
            Apply::Live => ("LIVE", MINT),
            Apply::Restart => ("RESTART", ORANGE),
            Apply::Retranslate => ("REBUILD", PINK),
        };
        round_rect(bm, b, b.h as f32 / 2.0, 46, |_, _| rgb);
        outline(bm, b, b.h as f32 / 2.0, 1.0, rgb, 150);
        self.text.centre(bm, BADGE, text, b, rgb);
    }

    fn button(&self, bm: &mut Bitmap, l: &Layout, v: Rect, text: &str, selected: bool) {
        let w = l.px(120.0);
        let b = Rect {
            x: v.right() - w,
            w,
            ..v
        };
        if selected {
            round_rect(bm, b, b.h as f32 / 2.0, 255, |u, _| sunset(u * 0.7 + 0.15));
            self.text.centre(bm, BADGE, text, b, BG_TOP);
        } else {
            round_rect(bm, b, b.h as f32 / 2.0, 255, |_, _| CONTROL);
            outline(bm, b, b.h as f32 / 2.0, 1.0, BORDER, 255);
            self.text.centre(bm, BADGE, text, b, SOFT);
        }
    }

    fn value(&self, bm: &mut Bitmap, l: &Layout, v: Rect, kind: Kind, value: &str, selected: bool) {
        let t = &self.text;
        let px = |f: f32| l.px(f);
        if kind == Kind::Bool {
            let on = value == "1";
            let track = Rect {
                x: v.right() - px(56.0),
                y: v.y + (v.h - px(28.0)) / 2,
                w: px(56.0),
                h: px(28.0),
            };
            if on {
                round_rect(bm, track, track.h as f32 / 2.0, 255, |u, _| {
                    sunset(u * 0.66)
                });
            } else {
                round_rect(bm, track, track.h as f32 / 2.0, 255, |_, _| {
                    [0x3a, 0x2a, 0x55]
                });
            }
            let k = px(22.0);
            let kx = if on {
                track.right() - px(3.0) - k
            } else {
                track.x + px(3.0)
            };
            let knob = Rect {
                x: kx,
                y: track.y + (track.h - k) / 2,
                w: k,
                h: k,
            };
            round_rect(
                bm,
                knob,
                k as f32 / 2.0,
                255,
                |_, _| if on { TEXT } else { DIM },
            );
            let s = shown(kind, value);
            let w = t.measure(VALUE, &s).0;
            t.mid(
                bm,
                VALUE,
                &s,
                track.x - px(12.0) - w,
                v,
                if on { TEXT } else { DIM },
            );
            return;
        }
        round_rect(bm, v, v.h as f32 / 2.0, 255, |_, _| CONTROL);
        outline(
            bm,
            v,
            v.h as f32 / 2.0,
            1.0,
            if selected { ORANGE } else { BORDER },
            if selected { 200 } else { 255 },
        );
        let s = shown(kind, value);
        if kind == Kind::Text {
            let s = t.fit(SMALL, &s, v.w - px(28.0));
            t.centre(bm, SMALL, &s, v, SOFT);
            return;
        }
        let arrow = if selected { ORANGE } else { DIM };
        t.mid(bm, TAB, "\u{2039}", v.x + px(14.0), v, arrow);
        let rw = t.measure(TAB, "\u{203a}").0;
        t.mid(bm, TAB, "\u{203a}", v.right() - px(14.0) - rw, v, arrow);
        t.centre(bm, VALUE, &t.fit(VALUE, &s, v.w - px(64.0)), v, TEXT);
    }
}
