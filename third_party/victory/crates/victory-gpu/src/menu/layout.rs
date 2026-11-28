//! (El1xr) Where the menu's parts go, for a game picture of a given size, with no OS calls: the
//! panel, its header, tabs, banner, rows and footer, and what a point in it hits. Sizes are
//! designed for a 1080-line picture and scaled with its height, so 4K gets twice the pixels.

use super::state::{Menu, Tab};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }
}

/// What a point hits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Close,
    Expert,
    Tab(Tab),
    /// A row (index into the tab's rows) outside its value control.
    Row(usize),
    /// The left or right half of a row's value control.
    ValueLeft(usize),
    ValueRight(usize),
}

pub struct Layout {
    pub s: f32,
    /// The whole window: the panel plus room for its shadow.
    pub window: Rect,
    pub panel: Rect,
    pub header: Rect,
    pub close: Rect,
    pub expert: Rect,
    pub tabs: Vec<(Tab, Rect)>,
    pub banner: Option<Rect>,
    pub list: Rect,
    /// The visible rows: index into the tab's rows, and the row's rectangle.
    pub rows: Vec<(usize, Rect)>,
    pub footer: Rect,
    /// How many rows fit.
    pub visible: usize,
}

/// The menu's scale for a picture `frame_h` lines tall.
pub fn scale(frame_h: i32) -> f32 {
    (frame_h as f32 / 1080.0).clamp(0.75, 4.0)
}

const PANEL_W: f32 = 1040.0;
const PANEL_H: f32 = 700.0;
const SHADOW: f32 = 28.0;
const PAD: f32 = 28.0;
const HEADER_H: f32 = 76.0;
const TABS_H: f32 = 46.0;
/// A tab's width: its title's letters at about this many pixels each, plus padding.
const TAB_LETTER_W: f32 = 8.6;
const TAB_PAD_W: f32 = 26.0;
const TAB_GAP: f32 = 4.0;
const BANNER_H: f32 = 40.0;
const FOOTER_H: f32 = 46.0;
const ROW_H: f32 = 60.0;
/// A section title's row.
const HEADING_H: f32 = 34.0;
const VALUE_W: f32 = 236.0;
const BADGE_W: f32 = 98.0;

/// Lays the menu out for a `frame_w` x `frame_h` picture with `tabs` and `rows` rows in the
/// current tab (`heading(i)`: row `i` is a section title, a shorter row), and scrolls `menu` so
/// its selected row is visible.
pub fn layout(
    frame_w: i32,
    frame_h: i32,
    menu: &mut Menu,
    tabs: &[Tab],
    rows: usize,
    heading: &dyn Fn(usize) -> bool,
    pending: bool,
) -> Layout {
    let s = scale(frame_h);
    let px = |v: f32| (v * s).round() as i32;
    let shadow = px(SHADOW);
    let w = px(PANEL_W).min(frame_w - 2 * shadow).max(px(480.0));
    let h = px(PANEL_H).min(frame_h - 2 * shadow).max(px(320.0));
    let window = Rect {
        x: 0,
        y: 0,
        w: w + 2 * shadow,
        h: h + 2 * shadow,
    };
    let panel = Rect {
        x: shadow,
        y: shadow,
        w,
        h,
    };
    let pad = px(PAD);
    let header = Rect {
        x: panel.x,
        y: panel.y,
        w,
        h: px(HEADER_H),
    };
    let button = px(34.0);
    let close = Rect {
        x: panel.right() - pad - button,
        y: header.y + (header.h - button) / 2,
        w: button,
        h: button,
    };
    let expert_w = px(128.0);
    let expert = Rect {
        x: close.x - px(14.0) - expert_w,
        y: close.y,
        w: expert_w,
        h: button,
    };
    let mut y = header.bottom();
    // Tabs as wide as their titles, narrowed together when they would not fit.
    let wanted: Vec<f32> = tabs
        .iter()
        .map(|t| t.title().chars().count() as f32 * TAB_LETTER_W + TAB_PAD_W)
        .collect();
    let room = (w - 2 * pad) as f32 / s - TAB_GAP * tabs.len().saturating_sub(1) as f32;
    let fit = (room / wanted.iter().sum::<f32>().max(1.0)).min(1.0);
    let mut at = 0.0;
    let tabs = tabs
        .iter()
        .zip(wanted)
        .map(|(&t, want)| {
            let x = panel.x + pad + px(at);
            let r = Rect {
                x,
                y,
                w: panel.x + pad + px(at + want * fit) - x,
                h: px(TABS_H),
            };
            at += want * fit + TAB_GAP;
            (t, r)
        })
        .collect();
    y += px(TABS_H) + px(10.0);
    let banner = pending.then(|| {
        let r = Rect {
            x: panel.x + pad,
            y,
            w: w - 2 * pad,
            h: px(BANNER_H),
        };
        y += r.h + px(8.0);
        r
    });
    let footer = Rect {
        x: panel.x,
        y: panel.bottom() - px(FOOTER_H),
        w,
        h: px(FOOTER_H),
    };
    let list = Rect {
        x: panel.x + pad,
        y,
        w: w - 2 * pad,
        h: (footer.y - px(6.0) - y).max(0),
    };
    let row_h = px(ROW_H);
    // Counted as full rows, so a page with section titles always fits.
    let visible = (list.h / row_h.max(1)).max(1) as usize;
    menu.follow(visible, rows);
    let mut top = list.y;
    let rows = (menu.scroll..rows.min(menu.scroll + visible))
        .map(|i| {
            let h = if heading(i) { px(HEADING_H) } else { row_h };
            let r = Rect {
                x: list.x,
                y: top,
                w: list.w,
                h,
            };
            top += h;
            (i, r)
        })
        .collect();
    Layout {
        s,
        window,
        panel,
        header,
        close,
        expert,
        tabs,
        banner,
        list,
        rows,
        footer,
        visible,
    }
}

impl Layout {
    pub fn px(&self, v: f32) -> i32 {
        (v * self.s).round() as i32
    }

    /// A row's value control, at its right end.
    pub fn value(&self, row: Rect) -> Rect {
        let (w, h) = (self.px(VALUE_W), self.px(36.0));
        Rect {
            x: row.right() - self.px(12.0) - w,
            y: row.y + (row.h - h) / 2,
            w,
            h,
        }
    }

    /// A row's apply-class badge, left of its value control.
    pub fn badge(&self, row: Rect) -> Rect {
        let v = self.value(row);
        let (w, h) = (self.px(BADGE_W), self.px(24.0));
        Rect {
            x: v.x - self.px(16.0) - w,
            y: row.y + (row.h - h) / 2,
            w,
            h,
        }
    }

    pub fn hit(&self, x: i32, y: i32) -> Option<Hit> {
        if self.close.contains(x, y) {
            return Some(Hit::Close);
        }
        if self.expert.contains(x, y) {
            return Some(Hit::Expert);
        }
        if let Some(&(t, _)) = self.tabs.iter().find(|(_, r)| r.contains(x, y)) {
            return Some(Hit::Tab(t));
        }
        let &(i, row) = self.rows.iter().find(|(_, r)| r.contains(x, y))?;
        let v = self.value(row);
        Some(if !v.contains(x, y) {
            Hit::Row(i)
        } else if x < v.x + v.w / 2 {
            Hit::ValueLeft(i)
        } else {
            Hit::ValueRight(i)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> Menu {
        Menu {
            open: true,
            ..Menu::default()
        }
    }

    /// The tabs of the registry with the expert toggle on (the most there can be).
    fn tabs() -> Vec<Tab> {
        super::super::state::tabs(&super::super::model::Registry::default(), true)
    }

    #[test]
    fn panel_scales_with_the_picture_and_fits() {
        for (fw, fh) in [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)] {
            let l = layout(fw, fh, &mut open(), &tabs(), 6, &|_| false, true);
            assert!(l.window.w <= fw && l.window.h <= fh, "{fw}x{fh}");
            assert!(l.footer.y >= l.list.bottom());
            assert!(
                l.tabs
                    .iter()
                    .all(|(_, r)| r.right() <= l.expert.x || r.y >= l.expert.bottom())
            );
            let last = l.tabs.last().unwrap().1;
            assert!(last.right() <= l.panel.right() - l.px(PAD), "{fw}x{fh}");
            assert!(l.tabs.iter().all(|(t, r)| r.w >= l.px(t.title().len() as f32 * 7.0)), "{fw}x{fh}: titles fit");
            let row = l.rows[0].1;
            assert!(l.badge(row).x > row.x + row.w / 3, "label room");
        }
        let small = layout(1920, 1080, &mut open(), &tabs(), 6, &|_| false, false);
        let big = layout(3840, 2160, &mut open(), &tabs(), 6, &|_| false, false);
        assert_eq!(big.panel.w, small.panel.w * 2);
        assert_eq!(big.visible, small.visible);
    }

    #[test]
    fn banner_takes_room_from_the_list() {
        let without = layout(1920, 1080, &mut open(), &tabs(), 30, &|_| false, false);
        let with = layout(1920, 1080, &mut open(), &tabs(), 30, &|_| false, true);
        assert!(with.banner.is_some() && without.banner.is_none());
        assert!(with.list.y > without.list.y);
        assert!(with.visible <= without.visible);
    }

    #[test]
    fn rows_scroll_to_the_selection() {
        let mut m = Menu {
            selected: 25,
            ..open()
        };
        let l = layout(1920, 1080, &mut m, &tabs(), 30, &|_| false, false);
        assert_eq!(l.rows.last().unwrap().0, 25);
        assert_eq!(l.rows.len(), l.visible);
        assert_eq!(m.scroll, 26 - l.visible);
    }

    #[test]
    fn hits() {
        let mut m = open();
        let l = layout(1920, 1080, &mut m, &tabs(), 4, &|_| false, false);
        let centre = |r: Rect| (r.x + r.w / 2, r.y + r.h / 2);
        let (x, y) = centre(l.close);
        assert_eq!(l.hit(x, y), Some(Hit::Close));
        let (x, y) = centre(l.expert);
        assert_eq!(l.hit(x, y), Some(Hit::Expert));
        let (x, y) = centre(l.tabs[1].1);
        assert_eq!(l.hit(x, y), Some(Hit::Tab(Tab::Group("Submission"))));
        let row = l.rows[2].1;
        assert_eq!(l.hit(row.x + 5, row.y + 5), Some(Hit::Row(2)));
        let v = l.value(row);
        assert_eq!(l.hit(v.x + 3, v.y + 3), Some(Hit::ValueLeft(2)));
        assert_eq!(l.hit(v.right() - 3, v.y + 3), Some(Hit::ValueRight(2)));
        assert_eq!(l.hit(0, 0), None, "the shadow hits nothing");
        let below = l.rows.last().unwrap().1.bottom() + 2;
        assert_eq!(l.hit(row.x + 5, below), None);
    }
}
