//! (El1xr) Draws the menu into a premultiplied bitmap: GDI rasterises the glyphs (as the overlay
//! does), everything else is flat fills. Style per `planning/style.md`: a flat night-purple panel,
//! plain text for status and values, a quiet row background for the selection, section titles as
//! small capitals over a hairline, and the sunset accent only under the active tab and on the
//! focused value's arrows.

use windows_sys::Win32::Foundation::SIZE;
use windows_sys::Win32::Graphics::Gdi::*;

use super::layout::{ABOUT_LINES, Layout, Rect};
use super::model::{Apply, Info, Kind, MenuModel};
use super::state::{Menu, Row, selectable, shown};
use crate::pixels::Bitmap;

const PANEL: [u8; 3] = [0x16, 0x0d, 0x2b];
const RULE: [u8; 3] = [0x3a, 0x2d, 0x55];
const ROW_SEL: [u8; 3] = [0x25, 0x18, 0x40];
const BANNER: [u8; 3] = [0x1f, 0x14, 0x38];
const TEXT: [u8; 3] = [0xf4, 0xf0, 0xfa];
const SOFT: [u8; 3] = [0xd2, 0xca, 0xe2];
const DIM: [u8; 3] = [0x9a, 0x8f, 0xb4];
/// The one accent (sunset orange).
const ACCENT: [u8; 3] = [0xff, 0x8a, 0x3d];
/// Apply classes, as quiet text.
const LIVE: [u8; 3] = [0x8c, 0xc4, 0xab];
const RESTART: [u8; 3] = [0xd6, 0xa5, 0x7a];
const REBUILD: [u8; 3] = [0xc9, 0x8f, 0xab];

/// Font slots.
const TITLE: usize = 0;
const TAB: usize = 1;
const LABEL: usize = 2;
const HELP: usize = 3;
const VALUE: usize = 4;
const CAPS: usize = 5;
const SMALL: usize = 6;
/// Face, size at 1080 lines, weight.
const FONTS: [(&str, f32, i32); 7] = [
    ("Segoe UI", 26.0, 600),
    ("Segoe UI", 15.5, 600),
    ("Segoe UI", 16.5, 600),
    ("Segoe UI", 13.5, 400),
    ("Segoe UI", 15.5, 400),
    ("Segoe UI", 11.5, 600),
    ("Segoe UI", 13.0, 400),
];

/// Signed distance from (x, y) to a `w` x `h` rectangle with corner radius `r` at the origin.
fn sdf(x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let (qx, qy) = (
        (x - w / 2.0).abs() - (w / 2.0 - r),
        (y - h / 2.0).abs() - (h / 2.0 - r),
    );
    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
}

/// A filled rectangle with corner radius `radius`, opacity `a`.
fn fill(bm: &mut Bitmap, r: Rect, radius: f32, a: u32, rgb: [u8; 3]) {
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

/// A horizontal hairline.
fn rule(bm: &mut Bitmap, l: &Layout, x: i32, y: i32, w: i32, rgb: [u8; 3]) {
    let h = l.s.round().max(1.0) as i32;
    fill(bm, Rect { x, y, w, h }, 0.0, 255, rgb);
}

/// The panel with a soft drop shadow; depends only on the layout's size, so it is drawn once
/// per picture size.
fn base(l: &Layout) -> Bitmap {
    let mut bm = Bitmap::new(l.window.w as usize, l.window.h as usize);
    let p = l.panel;
    let (radius, blur) = (l.px(6.0) as f32, (p.x as f32).max(1.0));
    for y in 0..l.window.h {
        for x in 0..l.window.w {
            let (fx, fy) = ((x - p.x) as f32 + 0.5, (y - p.y - l.px(4.0)) as f32 + 0.5);
            let d = sdf(fx, fy, p.w as f32, p.h as f32, radius);
            if d > 0.0 && d < blur {
                let k = 1.0 - d / blur;
                bm.over(x as usize, y as usize, [0, 0, 0], (90.0 * k * k) as u32);
            }
        }
    }
    fill(&mut bm, p, radius, 255, PANEL);
    let pad = l.px(28.0);
    rule(&mut bm, l, p.x + pad, l.header.bottom() - l.px(1.0), p.w - 2 * pad, RULE);
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

    /// `text` in lines of whole words no wider than `width` (a longer word keeps a line of its own).
    fn wrap(&self, font: usize, text: &str, width: i32) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let mut line = String::new();
        for word in text.split_whitespace() {
            let longer = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if line.is_empty() || self.measure(font, &longer).0 <= width {
                line = longer;
            } else {
                lines.push(std::mem::replace(&mut line, word.to_string()));
            }
        }
        if !line.is_empty() {
            lines.push(line);
        }
        lines
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

/// The line under a setting's label: what it gives (`detail`) or its help, then the switch's
/// name for experts only (players read the label and the line).
pub(super) fn setting_line(info: &Info, model: &dyn MenuModel, expert: bool) -> String {
    let detail = model.detail(info.name);
    match (detail.as_deref().unwrap_or(info.help), info.label == info.name) {
        (help, true) => help.to_string(),
        ("", false) => info.name.to_string(),
        (help, false) if expert => format!("{help}   {}", info.name),
        (help, false) => help.to_string(),
    }
}

/// The selected row's whole description for the band under the list: a setting's full help
/// (its line when it has none) and its switch's name for experts, a preset's help.
pub(super) fn about(row: Row, model: &dyn MenuModel, expert: bool) -> String {
    match row {
        Row::Setting(i) => {
            let info = &model.infos()[i];
            let help = match info.help {
                "" => model.detail(info.name).unwrap_or_default(),
                help => help.to_string(),
            };
            match (help.is_empty(), expert && info.label != info.name) {
                (_, false) => help,
                (true, true) => info.name.to_string(),
                (false, true) => format!("{help}   {}", info.name),
            }
        }
        Row::Preset(p) => model.presets()[p].help.to_string(),
        Row::Save => "Writes every changed value to the settings file; changes that apply at the next start are written as soon as they are made.".into(),
        Row::Undo => "Every setting changed while running back to its launch value.".into(),
        Row::Heading(_) => String::new(),
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
        // Header: the name, then what this is, on one baseline.
        let (nw, nh) = t.measure(TITLE, "El1xr");
        let ty = l.header.y + (l.header.h - nh) / 2;
        t.draw(&mut bm, TITLE, "El1xr", l.panel.x + pad, ty, TEXT);
        let sub_y = ty + t.ascent[TITLE] - t.ascent[TAB];
        t.draw(&mut bm, TAB, model.title(), l.panel.x + pad + nw + px(12.0), sub_y, DIM);
        // Expert switch and close, as plain text at the right.
        let e = l.expert;
        let state = if menu.expert { "ON" } else { "OFF" };
        let sw = t.measure(CAPS, state).0;
        t.mid(&mut bm, CAPS, state, e.right() - sw, e, if menu.expert { TEXT } else { DIM });
        let label = "EXPERT";
        let lw = t.measure(CAPS, label).0;
        t.mid(&mut bm, CAPS, label, e.right() - sw - px(8.0) - lw, e, DIM);
        // What the tags mean, ending before the expert switch when there is room.
        let sub_w = t.measure(TAB, model.title()).0;
        let room = (
            l.panel.x + pad + nw + px(12.0) + sub_w + px(40.0),
            e.right() - sw - px(8.0) - lw - px(40.0),
        );
        self.legend(&mut bm, l, Rect { x: room.0, y: e.y, w: room.1 - room.0, h: e.h });
        t.centre(&mut bm, TAB, "\u{2715}", l.close, SOFT);
        // Tabs: plain titles, the current one bright with the thin accent underline.
        for &(tab, r) in &l.tabs {
            let on = tab == menu.tab;
            let title = tab.title();
            let w = t.measure(TAB, title).0;
            let x = r.x + (r.w - w) / 2;
            t.mid(&mut bm, TAB, title, x, r, if on { TEXT } else { DIM });
            if on {
                let line = Rect { x, y: r.bottom() - px(2.0), w, h: px(2.0).max(2) };
                fill(&mut bm, line, 0.0, 255, ACCENT);
            }
        }
        // Restart notice: a flat band, a quiet label, the settings that wait with their new
        // values, and whether they reached the settings file.
        if let Some(b) = l.banner {
            let pending = model.pending_restart();
            fill(&mut bm, b, px(4.0) as f32, 255, BANNER);
            let labels: Vec<String> = pending
                .iter()
                .map(|&n| {
                    let info = victory_core::settings::info(n);
                    let label = info.map_or(n, |i| i.label);
                    let value = model.get(n).unwrap_or_default();
                    let value = info.map_or(value.clone(), |i| shown(n, i.kind, &value));
                    match model.apply_of(n) {
                        Apply::Retranslate => format!("{label}: {value} (shaders rebuilt)"),
                        _ => format!("{label}: {value}"),
                    }
                })
                .collect();
            let head = "RESTART TO APPLY";
            t.mid(&mut bm, CAPS, head, b.x + px(16.0), b, RESTART);
            let (kept, kept_rgb) = if menu.unsaved {
                ("NOT SAVED", RESTART)
            } else {
                ("SAVED", DIM)
            };
            let kw = t.measure(CAPS, kept).0;
            t.mid(&mut bm, CAPS, kept, b.right() - px(16.0) - kw, b, kept_rgb);
            let x = b.x + px(16.0) + t.measure(CAPS, head).0 + px(16.0);
            let list = t.fit(HELP, &labels.join(",  "), b.right() - px(32.0) - kw - x);
            t.mid(&mut bm, HELP, &list, x, b, SOFT);
        }
        // Rows: the selection (and the hovered row, fainter) on a quiet flat background.
        let rows = menu.rows(model);
        for &(i, r) in &l.rows {
            let selected = i == menu.selected;
            if (selected || hover == Some(i)) && selectable(&rows[i]) {
                let a = if selected { 255 } else { 110 };
                fill(&mut bm, r, px(4.0) as f32, a, ROW_SEL);
            }
            self.row(&mut bm, l, r, rows[i], model, selected, menu.expert);
        }
        if rows.is_empty() {
            t.centre(&mut bm, HELP, "Nothing here yet", l.list, DIM);
        }
        // Scroll position: a hairline track and a plain thumb.
        let n = rows.len();
        if n > l.visible {
            let track = Rect {
                x: l.list.right() + px(10.0),
                y: l.list.y,
                w: px(2.0).max(1),
                h: l.visible as i32 * px(60.0),
            };
            fill(&mut bm, track, 0.0, 255, RULE);
            let thumb = Rect {
                y: track.y + (track.h as usize * menu.scroll / n) as i32,
                h: (track.h as usize * l.visible / n) as i32,
                ..track
            };
            fill(&mut bm, thumb, 0.0, 255, DIM);
        }
        // The selected row's whole description, wrapped under the list.
        if let Some(&row) = rows.get(menu.selected) {
            let a = l.about;
            rule(&mut bm, l, a.x, a.y, a.w, RULE);
            let text = about(row, model, menu.expert);
            let line_h = t.measure(HELP, "Ag").1;
            let mut lines = t.wrap(HELP, &text, a.w);
            if lines.len() > ABOUT_LINES {
                let rest = lines.split_off(ABOUT_LINES - 1).join(" ");
                lines.push(t.fit(HELP, &rest, a.w));
            }
            for (k, line) in lines.iter().enumerate() {
                t.draw(&mut bm, HELP, line, a.x, a.y + px(10.0) + k as i32 * line_h, SOFT);
            }
        }
        // Footer: hints, then the last change's outcome at the right.
        let f = l.footer;
        rule(&mut bm, l, f.x + pad, f.y, f.w - 2 * pad, RULE);
        let hints = format!(
            "{} / Esc close   \u{2191}\u{2193} select   \u{2190}\u{2192} change   Q E tabs   X expert      Pad: A change  B close  LB RB tabs  Y expert",
            super::hotkey_name()
        );
        let status = t.fit(SMALL, menu.status.as_deref().unwrap_or_default(), (f.w - 2 * pad) * 3 / 5);
        let sw = t.measure(SMALL, &status).0;
        let hints = t.fit(SMALL, &hints, f.w - 2 * pad - sw - px(24.0));
        t.mid(&mut bm, SMALL, &hints, f.x + pad, f, DIM);
        t.mid(&mut bm, SMALL, &status, f.right() - pad - sw, f, SOFT);
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
        expert: bool,
    ) {
        let t = &self.text;
        let px = |v: f32| l.px(v);
        let x = r.x + px(16.0);
        let (lh, hh) = (t.measure(LABEL, "Ag").1, t.measure(HELP, "Ag").1);
        let label_w = l.badge(r).x - px(16.0) - x;
        let (label, help) = match row {
            Row::Setting(i) => {
                let info = model.infos()[i];
                self.apply_class(bm, l.badge(r), model.apply_of(info.name));
                let value = model.get(info.name).unwrap_or_default();
                // Unset: what it stands for (the size, the mode), in the quieter colour.
                let (value, chosen) = match model.effective(info.name) {
                    Some(e) if value.is_empty() => (e, false),
                    _ => (value, true),
                };
                self.value(bm, l, l.value(r), info.name, info.kind, &value, selected, chosen);
                (info.label.to_string(), setting_line(&info, model, expert))
            }
            Row::Preset(p) => {
                let preset = model.presets()[p];
                self.action(bm, l.value(r), "APPLY", selected);
                (format!("{} preset", preset.name), preset.help.to_string())
            }
            Row::Save => {
                self.action(bm, l.value(r), "SAVE", selected);
                let help = match victory_core::settings::file_path() {
                    Some(path) => format!("Write the changed values to {}", short_path(&path)),
                    None => "No settings file in this run (VICTORY_SETTINGS_FILE=none)".into(),
                };
                ("Save settings".into(), help)
            }
            Row::Undo => {
                self.action(bm, l.value(r), "UNDO", selected);
                (
                    "Undo changes".into(),
                    "Every setting changed while running back to its launch value.".into(),
                )
            }
            Row::Heading(title) => {
                let title = title.to_uppercase();
                let h = t.measure(CAPS, &title).1;
                let line_y = r.bottom() - px(6.0);
                t.draw(bm, CAPS, &title, x, line_y - px(5.0) - h, SOFT);
                rule(bm, l, x, line_y, r.right() - px(12.0) - x, RULE);
                return;
            }
        };
        // A row without help has its label centred.
        let hh = if help.is_empty() { 0 } else { hh };
        let top = r.y + (r.h - lh - hh) / 2;
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

    /// The apply class as small capitals, left-aligned in its column.
    /// The tags and when each change applies, right-aligned in `r`; nothing if it does not fit.
    fn legend(&self, bm: &mut Bitmap, l: &Layout, r: Rect) {
        let t = &self.text;
        let parts = [
            ("LIVE", LIVE, "now"),
            ("RESTART", RESTART, "next launch"),
            ("REBUILD", REBUILD, "next launch, shaders rebuilt"),
        ];
        let (gap, space) = (l.px(22.0), l.px(6.0));
        let width = |(tag, _, when): &(&str, [u8; 3], &str)| t.measure(CAPS, tag).0 + space + t.measure(SMALL, when).0;
        let total: i32 = parts.iter().map(width).sum::<i32>() + gap * (parts.len() as i32 - 1);
        if total > r.w {
            return;
        }
        let mut x = r.right() - total;
        for part in &parts {
            let (tag, rgb, when) = *part;
            t.mid(bm, CAPS, tag, x, r, rgb);
            t.mid(bm, SMALL, when, x + t.measure(CAPS, tag).0 + space, r, DIM);
            x += width(part) + gap;
        }
    }

    fn apply_class(&self, bm: &mut Bitmap, b: Rect, apply: Apply) {
        let (text, rgb) = match apply {
            Apply::Live => ("LIVE", LIVE),
            Apply::Restart => ("RESTART", RESTART),
            Apply::Retranslate => ("REBUILD", REBUILD),
        };
        self.text.mid(bm, CAPS, text, b.x, b, rgb);
    }

    /// A row's action, as small capitals right-aligned in the value column.
    fn action(&self, bm: &mut Bitmap, v: Rect, text: &str, selected: bool) {
        let w = self.text.measure(CAPS, text).0;
        let rgb = if selected { ACCENT } else { SOFT };
        self.text.mid(bm, CAPS, text, v.right() - w, v, rgb);
    }

    /// A value as plain text between small arrows (accent on the focused row), right-aligned in
    /// the value column; free text without arrows.
    #[allow(clippy::too_many_arguments)]
    fn value(
        &self,
        bm: &mut Bitmap,
        l: &Layout,
        v: Rect,
        name: &str,
        kind: Kind,
        value: &str,
        selected: bool,
        chosen: bool,
    ) {
        let t = &self.text;
        let s = shown(name, kind, value);
        if kind == Kind::Text {
            let s = t.fit(SMALL, &s, v.w);
            let w = t.measure(SMALL, &s).0;
            t.mid(bm, SMALL, &s, v.right() - w, v, SOFT);
            return;
        }
        let (left, right) = ("\u{2039}", "\u{203a}");
        let arrow = if selected { ACCENT } else { DIM };
        let rw = t.measure(VALUE, right).0;
        t.mid(bm, VALUE, right, v.right() - rw, v, arrow);
        t.mid(bm, VALUE, left, v.x + l.px(76.0), v, arrow);
        let off = (kind == Kind::Bool && value != "1") || !chosen;
        let inner = Rect { x: v.x + l.px(76.0), w: v.right() - rw - v.x - l.px(76.0), ..v };
        let s = t.fit(VALUE, &s, inner.w - l.px(32.0));
        t.centre(bm, VALUE, &s, inner, if off { DIM } else { TEXT });
    }
}

/// `path` with the user's local data folder written as `%LOCALAPPDATA%`.
fn short_path(path: &std::path::Path) -> String {
    let full = path.display().to_string();
    match std::env::var("LOCALAPPDATA") {
        Ok(base) if !base.is_empty() && full.starts_with(&base) => {
            format!("%LOCALAPPDATA%{}", &full[base.len()..])
        }
        _ => full,
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::{Registry, registry_lock};
    use super::super::state::{Tab, rows, tabs};
    use super::*;

    /// What would be cut at `w` x `h` on every tab, the expert one too: a label wider than its
    /// room, a Display row's line wider than its room (players read those in the row), and a
    /// description longer than the band under the list.
    fn cut(w: i32, h: i32) -> Vec<String> {
        let model = Registry::default();
        let mut menu = Menu::default();
        let all = tabs(&model, true);
        let l = super::super::layout::layout(w, h, &mut menu, &all, 6, &|_| false, false);
        let text = Text::new(l.s).expect("fonts");
        let r = l.rows[0].1;
        let x = r.x + l.px(16.0);
        let room = l.badge(r).x - l.px(16.0) - x;
        let mut cut = Vec::new();
        for tab in all {
            for row in rows(&model, tab, true) {
                let name = match row {
                    Row::Setting(i) => model.infos()[i].name.to_string(),
                    Row::Preset(p) => model.presets()[p].name.to_string(),
                    _ => format!("{row:?}"),
                };
                let mut wide = |font: usize, what: &str, s: &str, room: i32| {
                    let px = text.measure(font, s).0;
                    if px > room {
                        cut.push(format!("{w}x{h} {} {name} {what} +{}px: {s}", tab.title(), px - room));
                    }
                };
                if let Row::Setting(i) = row {
                    let info = &model.infos()[i];
                    wide(LABEL, "label", info.label, room);
                    if tab == Tab::DISPLAY && !info.expert {
                        wide(HELP, "line", &setting_line(info, &model, false), room);
                    }
                }
                for expert in [false, true] {
                    let lines = text.wrap(HELP, &about(row, &model, expert), l.about.w);
                    if lines.len() > ABOUT_LINES {
                        cut.push(format!("{w}x{h} {} {name} about: {} lines", tab.title(), lines.len()));
                    }
                }
            }
        }
        cut
    }

    #[test]
    fn nothing_is_cut_at_1080_or_1440() {
        let _lock = registry_lock();
        let cut: Vec<String> = [(1920, 1080), (2560, 1440)].iter().flat_map(|&(w, h)| cut(w, h)).collect();
        assert!(cut.is_empty(), "{} cut:\n{}", cut.len(), cut.join("\n"));
    }

    #[test]
    fn wrapping_keeps_whole_words_within_the_width() {
        let text = Text::new(1.0).expect("fonts");
        let s = "the selected row's whole description, wrapped under the list in whole words";
        let width = text.measure(HELP, "the selected row's whole").0;
        let lines = text.wrap(HELP, s, width);
        assert!(lines.len() > 2);
        assert_eq!(lines.join(" "), s);
        assert!(lines.iter().all(|l| text.measure(HELP, l).0 <= width), "{lines:?}");
        assert!(text.wrap(HELP, "", width).is_empty());
    }
}
