//! (El1xr) `VICTORY_MENU_SELFTEST` pictures from the game's own pixels only. The menu thread
//! [`ask`]s for one; the next present reads the presented texture back as an F12 shot does
//! ([`crate::screenshot::read_back`]), and [`write`] lays the menu's own bitmap over it and
//! writes it under the asked name into the snap folder (`VICTORY_SNAP_DIR`, default `snaps`).
//! Nothing here or in the menu reads the screen or another window: when no frame is presented
//! the ask lapses ([`lapsed`]) and the menu logs that the picture was skipped.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::pixels::Bitmap;

pub(crate) struct Ask {
    pub name: String,
    /// The open menu as drawn for a picture of this size (centred on it); `None` with it closed.
    pub menu: Option<(Bitmap, (usize, usize))>,
    asked: Instant,
}

static PENDING: Mutex<Option<Ask>> = Mutex::new(None);

fn pending() -> MutexGuard<'static, Option<Ask>> {
    PENDING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Queues a picture for the next present; the name of an earlier ask no present took.
pub(crate) fn ask(name: String, menu: Option<(Bitmap, (usize, usize))>) -> Option<String> {
    pending()
        .replace(Ask {
            name,
            menu,
            asked: Instant::now(),
        })
        .map(|old| old.name)
}

/// Drops the ask if it waited longer than `max` (no frame went through the read-back present);
/// its name.
pub(crate) fn lapsed(max: Duration) -> Option<String> {
    let mut p = pending();
    if p.as_ref().is_some_and(|a| a.asked.elapsed() > max) {
        p.take().map(|a| a.name)
    } else {
        None
    }
}

/// The ask for this present (the present thread takes it with an F12 ask).
pub(crate) fn take() -> Option<Ask> {
    pending().take()
}

/// `bm` resized by `f` (nearest pixel).
fn scaled(bm: &Bitmap, f: f64) -> Bitmap {
    let (w, h) = (
        ((bm.w as f64 * f).round() as usize).max(1),
        ((bm.h as f64 * f).round() as usize).max(1),
    );
    let mut out = Bitmap::new(w, h);
    for y in 0..h {
        let sy = ((y as f64 + 0.5) / f) as usize;
        for x in 0..w {
            let sx = ((x as f64 + 0.5) / f) as usize;
            let s = (sy.min(bm.h - 1) * bm.w + sx.min(bm.w - 1)) * 4;
            out.px[(y * w + x) * 4..][..4].copy_from_slice(&bm.px[s..s + 4]);
        }
    }
    out
}

/// Lays the menu, drawn for a picture of `drawn_for`, centred over the `w` x `h` RGB picture,
/// scaled to it when the sizes differ (the presented texture need not be the window's size).
pub(crate) fn compose(
    rgb: &mut [u8],
    w: usize,
    h: usize,
    menu: &Bitmap,
    drawn_for: (usize, usize),
) {
    let f = (w as f64 / drawn_for.0.max(1) as f64).min(h as f64 / drawn_for.1.max(1) as f64);
    let resized;
    let bm = if (f - 1.0).abs() < 1e-3 {
        menu
    } else {
        resized = scaled(menu, f);
        &resized
    };
    crate::pixels::blend_onto_rgb(
        rgb,
        w,
        h,
        bm,
        w.saturating_sub(bm.w) / 2,
        h.saturating_sub(bm.h) / 2,
    );
}

/// Writes the asked picture from the presented frame's RGB rows (a worker thread).
pub(crate) fn write(ask: Ask, w: usize, h: usize, mut rgb: Vec<u8>) {
    if let Some((bm, drawn_for)) = &ask.menu {
        compose(&mut rgb, w, h, bm, *drawn_for);
    }
    let dir = std::env::var_os("VICTORY_SNAP_DIR").map_or_else(
        || std::path::PathBuf::from("snaps"),
        std::path::PathBuf::from,
    );
    let path = dir.join(&ask.name);
    let png = crate::screenshot::encode_png(w as u32, h as u32, &rgb);
    let menu = if ask.menu.is_some() {
        "with the menu"
    } else {
        "menu closed"
    };
    match std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, png)) {
        Ok(()) => tracing::info!(
            "menu selftest: wrote {} ({w}x{h}, the presented frame read back, {menu})",
            path.display()
        ),
        Err(e) => tracing::warn!("menu selftest: {} not written: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: usize, h: usize, rgb: [u8; 3]) -> Bitmap {
        let mut bm = Bitmap::new(w, h);
        for y in 0..h {
            for x in 0..w {
                bm.over(x, y, rgb, 255);
            }
        }
        bm
    }

    #[test]
    fn the_menu_lands_centred_and_scaled_to_the_presented_size() {
        let menu = solid(4, 2, [200, 10, 20]);
        let (w, h) = (16, 8);
        let mut rgb = vec![0u8; w * h * 3];
        compose(&mut rgb, w, h, &menu, (w, h));
        let at = |rgb: &[u8], x: usize, y: usize| rgb[(y * w + x) * 3..][..3].to_vec();
        assert_eq!(at(&rgb, 6, 3), [200, 10, 20]);
        assert_eq!(at(&rgb, 9, 4), [200, 10, 20]);
        assert_eq!(at(&rgb, 5, 3), [0, 0, 0]);
        assert_eq!(at(&rgb, 6, 2), [0, 0, 0]);
        // Drawn for a window half the presented size: twice as big on the picture.
        let mut rgb = vec![0u8; w * h * 3];
        compose(&mut rgb, w, h, &menu, (w / 2, h / 2));
        assert_eq!(at(&rgb, 4, 2), [200, 10, 20]);
        assert_eq!(at(&rgb, 11, 5), [200, 10, 20]);
        assert_eq!(at(&rgb, 3, 2), [0, 0, 0]);
        assert_eq!(at(&rgb, 12, 5), [0, 0, 0]);
    }

    /// The present takes a queued ask with the F12 flag; an ask nobody takes lapses.
    #[test]
    fn asks_are_taken_by_the_present_or_lapse() {
        assert!(ask("a.png".into(), None).is_none());
        assert_eq!(
            ask("b.png".into(), Some((solid(1, 1, [1, 2, 3]), (1, 1)))).as_deref(),
            Some("a.png")
        );
        assert!(
            crate::screenshot::take(),
            "a menu ask makes the present read back"
        );
        assert!(take().is_none(), "the present took it");
        assert!(!crate::screenshot::take());
        ask("c.png".into(), None);
        assert!(lapsed(Duration::from_secs(60)).is_none());
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(lapsed(Duration::from_millis(10)).as_deref(), Some("c.png"));
        assert!(take().is_none());
    }

    /// Nothing in the workspace's crates reads the screen, the desktop or another window's pixels:
    /// the selftest's pictures come only from the presented texture ([`crate::screenshot::read_back`]
    /// takes the swap chain's source texture). Names are split so this file does not match itself.
    #[test]
    fn nothing_reads_the_screen_or_the_desktop() {
        let forbidden = [
            concat!("Bit", "Blt"),
            concat!("Stretch", "Blt"),
            concat!("Print", "Window"),
            concat!("GetDesktop", "Window"),
            concat!("GetWindow", "DC"),
            concat!("CAPTURE", "BLT"),
            concat!("GetDI", "Bits"),
            concat!("CreateCompatible", "Bitmap"),
            concat!("Duplicate", "Output"),
            concat!("GraphicsCapture", "Item"),
            concat!("CopyFrom", "Screen"),
        ];
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut stack = vec![crates];
        let mut seen = 0;
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                let name = entry.file_name();
                if path.is_dir() {
                    if name != "target" && name != ".git" {
                        stack.push(path);
                    }
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                    seen += 1;
                    for api in forbidden {
                        assert!(!text.contains(api), "{} names {api}", path.display());
                    }
                }
            }
        }
        assert!(seen > 100, "{seen} files scanned");
        let shot = include_str!("screenshot.rs");
        assert!(shot.contains("texture: &ID3D12Resource) -> Option<Shot>"));
        assert!(include_str!("menu/window.rs").contains("crate::menu_shot::ask("));
    }
}
