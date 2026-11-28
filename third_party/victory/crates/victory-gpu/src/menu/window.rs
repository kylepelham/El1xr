//! (El1xr) The menu's window: a layered, topmost tool window over the game window's client area,
//! on a thread of its own. Hidden while the menu is closed; open, it takes the mouse over the
//! panel, and the keys and the pad are polled every 30 ms.

use std::cell::RefCell;
use std::sync::Once;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use windows_sys::Win32::Foundation::{HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::draw::Painter;
use super::input::Key;
use super::layout::Layout;
use super::selftest::{Out, Selftest};
use super::{Driver, PAD_DOWN, Spec};
use crate::pixels::Bitmap;

const TICK: usize = 1;
const TICK_MS: u32 = 30;

const VK_ESCAPE: i32 = 0x1B;
const VK_RETURN: i32 = 0x0D;
const VK_SPACE: i32 = 0x20;
const VK_PRIOR: i32 = 0x21;
const VK_NEXT: i32 = 0x22;
const VK_LEFT: i32 = 0x25;
const VK_UP: i32 = 0x26;
const VK_RIGHT: i32 = 0x27;
const VK_DOWN: i32 = 0x28;
const VK_TAB: i32 = 0x09;

/// The keys the menu reads while open, and what each means.
const KEYS: [(i32, Key); 13] = [
    (VK_ESCAPE, Key::Esc),
    (VK_UP, Key::Up),
    (VK_DOWN, Key::Down),
    (VK_LEFT, Key::Left),
    (VK_RIGHT, Key::Right),
    (VK_RETURN, Key::Enter),
    (VK_SPACE, Key::Enter),
    (b'Q' as i32, Key::PrevTab),
    (VK_PRIOR, Key::PrevTab),
    (b'E' as i32, Key::NextTab),
    (VK_NEXT, Key::NextTab),
    (VK_TAB, Key::NextTab),
    (b'X' as i32, Key::Expert),
];

struct Ctx {
    spec: &'static Spec,
    game: HWND,
    window: HWND,
    driver: Driver,
    painter: Option<Painter>,
    layout: Option<Layout>,
    /// Where the window sits on the screen.
    at: POINT,
    hover: Option<usize>,
    dirty: bool,
    shown: bool,
    /// `VICTORY_MENU_SELFTEST`.
    selftest: Option<Selftest>,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
}

pub(super) fn start(game: usize, spec: &'static Spec) {
    static SETTINGS: Once = Once::new();
    static TRAINER: Once = Once::new();
    let once = if spec.selftest { &SETTINGS } else { &TRAINER };
    once.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name(spec.thread.into())
            .spawn(move || run(game as HWND, spec));
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "menu thread not started");
        }
    });
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn ours_in_front() -> bool {
    let mut pid = 0u32;
    // SAFETY: plain queries.
    unsafe {
        let front = GetForegroundWindow();
        GetWindowThreadProcessId(front, &mut pid);
        !front.is_null() && pid == GetCurrentProcessId()
    }
}

fn down(vk: i32) -> bool {
    // SAFETY: a plain query.
    unsafe { GetAsyncKeyState(vk) < 0 }
}

/// The game window's client area on the screen.
fn client(game: HWND) -> Option<(i32, i32, POINT)> {
    // SAFETY: plain queries on a window handle (a dead one just fails).
    unsafe {
        let mut r = std::mem::zeroed::<RECT>();
        let mut origin = POINT { x: 0, y: 0 };
        let ok = IsWindow(game) != 0
            && IsIconic(game) == 0
            && GetClientRect(game, &mut r) != 0
            && ClientToScreen(game, &mut origin) != 0
            && r.right > 0
            && r.bottom > 0;
        ok.then_some((r.right, r.bottom, origin))
    }
}

impl Ctx {
    fn tick(&mut self, now: u64) {
        let front = ours_in_front();
        let mut keys = Vec::new();
        if front {
            if down(i32::from((self.spec.hotkey)())) {
                keys.push(Key::Hotkey);
            }
            if self.driver.menu.open || self.driver.blocked() {
                keys.extend(KEYS.iter().filter(|(vk, _)| down(*vk)).map(|&(_, k)| k));
            }
        }
        let mut testing = false;
        if let Some(test) = self.selftest.as_mut() {
            let t = test.tick(now, &mut self.driver, self.spec.blocked.load(Relaxed), front);
            keys.extend(t.keys);
            testing = t.active;
            for out in t.out {
                match out {
                    Out::Log(line) => tracing::info!("{line}"),
                    Out::Warn(line) => tracing::warn!("{line}"),
                    Out::Grab(name) => grab_to(self.game, &name),
                }
            }
        }
        let pad = PAD_DOWN.load(Relaxed) | self.spec.pressed.swap(0, Relaxed);
        let wheel = self.spec.wheel.swap(0, Relaxed);
        self.dirty |= self.driver.tick(now, &keys, pad, wheel);
        self.spec.blocked.store(self.driver.blocked(), Relaxed);
        self.show(front || testing);
    }

    /// Draws and places the window when the menu is open and in front, hides it otherwise.
    fn show(&mut self, front: bool) {
        let place = client(self.game).filter(|_| front && self.driver.menu.open);
        let Some((w, h, origin)) = place else {
            if self.shown {
                // SAFETY: our own window.
                unsafe { ShowWindow(self.window, SW_HIDE) };
                self.shown = false;
                self.painter = None;
                self.layout = None;
            }
            return;
        };
        let l = self.driver.layout(w, h);
        let at = POINT {
            x: origin.x + (w - l.window.w) / 2,
            y: origin.y + (h - l.window.h) / 2,
        };
        let moved = (at.x, at.y) != (self.at.x, self.at.y)
            || self.layout.as_ref().is_none_or(|o| o.window != l.window);
        if !(self.dirty || moved || !self.shown) {
            self.layout = Some(l);
            return;
        }
        self.painter = Painter::for_layout(self.painter.take(), &l);
        let picture = self
            .painter
            .as_ref()
            .and_then(|p| p.paint(&l, &self.driver.menu, &*self.driver.model, self.hover));
        if let Some(bm) = picture {
            // SAFETY: our own layered window, on this thread.
            unsafe {
                present(self.window, &bm, at);
                if !self.shown {
                    SetWindowPos(
                        self.window,
                        HWND_TOPMOST,
                        at.x,
                        at.y,
                        bm.w as i32,
                        bm.h as i32,
                        SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    );
                }
            }
            self.shown = true;
        }
        (self.at, self.dirty, self.layout) = (at, false, Some(l));
    }

    fn hit(&self, lp: isize) -> Option<super::layout::Hit> {
        let (x, y) = (
            (lp & 0xffff) as i16 as i32,
            ((lp >> 16) & 0xffff) as i16 as i32,
        );
        self.layout.as_ref()?.hit(x, y)
    }
}

/// Sets the layered window's picture and position in one go.
///
/// # Safety
/// `window` is a live layered window of this thread.
unsafe fn present(window: HWND, bm: &Bitmap, at: POINT) {
    // SAFETY: a 32-bit top-down DIB the size of the bitmap, filled with its premultiplied pixels
    // as BGRA, handed to UpdateLayeredWindow, then freed.
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        let dc = CreateCompatibleDC(screen);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: bm.w as i32,
                biHeight: -(bm.h as i32),
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
        let dib = CreateDIBSection(
            dc,
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        );
        if !dib.is_null() && !bits.is_null() {
            let dst = std::slice::from_raw_parts_mut(bits.cast::<u8>(), bm.px.len());
            for (d, s) in dst.chunks_exact_mut(4).zip(bm.px.chunks_exact(4)) {
                d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
            }
            let old = SelectObject(dc, dib);
            let size = SIZE {
                cx: bm.w as i32,
                cy: bm.h as i32,
            };
            let from = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            UpdateLayeredWindow(window, screen, &at, &size, dc, &from, 0, &blend, ULW_ALPHA);
            SelectObject(dc, old);
            DeleteObject(dib);
        }
        DeleteDC(dc);
        ReleaseDC(std::ptr::null_mut(), screen);
    }
}

/// Runs `f` with this thread per-monitor DPI aware: screen and window coordinates in pixels,
/// whatever the process's own awareness.
fn in_pixels<T>(f: impl FnOnce() -> T) -> T {
    // SAFETY: the thread's awareness, set back after.
    unsafe {
        let old = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let out = f();
        if !old.is_null() {
            SetThreadDpiAwarenessContext(old);
        }
        out
    }
}

/// The screen over the game window's client area as it is seen (the game with the layered
/// windows over it), as RGB rows in pixels.
fn grab(game: HWND) -> Option<(usize, usize, Vec<u8>)> {
    in_pixels(|| {
        let (w, h, at) = client(game)?;
        // SAFETY: a screen DC copied into a bitmap of ours, read back as 32-bit top-down rows, all
        // freed before returning.
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            let dc = CreateCompatibleDC(screen);
            let bitmap = CreateCompatibleBitmap(screen, w, h);
            let old = SelectObject(dc, bitmap);
            let copied = BitBlt(dc, 0, 0, w, h, screen, at.x, at.y, SRCCOPY | CAPTUREBLT) != 0;
            SelectObject(dc, old);
            let mut info = std::mem::zeroed::<BITMAPINFO>();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            };
            let mut bgra = vec![0u8; w as usize * h as usize * 4];
            let rows = if copied {
                GetDIBits(dc, bitmap, 0, h as u32, bgra.as_mut_ptr().cast(), &mut info, DIB_RGB_COLORS)
            } else {
                0
            };
            DeleteObject(bitmap);
            DeleteDC(dc);
            ReleaseDC(std::ptr::null_mut(), screen);
            (rows == h).then(|| {
                let rgb = bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
                (w as usize, h as usize, rgb)
            })
        }
    })
}

/// Grabs the screen over the game window and writes it as `name` into `VICTORY_SNAP_DIR`
/// (default `snaps`), off this thread.
fn grab_to(game: HWND, name: &str) {
    let Some((w, h, rgb)) = grab(game) else {
        tracing::warn!("menu selftest: {name}: the screen over the game window could not be grabbed");
        return;
    };
    let dir = std::env::var_os("VICTORY_SNAP_DIR")
        .map_or_else(|| std::path::PathBuf::from("snaps"), std::path::PathBuf::from);
    let path = dir.join(name);
    let spawned = std::thread::Builder::new()
        .name("victory-menu-grab".into())
        .spawn(move || {
            let png = crate::screenshot::encode_png(w as u32, h as u32, &rgb);
            match std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, png)) {
                Ok(()) => tracing::info!("menu selftest: wrote {} ({w}x{h}, screen over the game window)", path.display()),
                Err(e) => tracing::warn!("menu selftest: {} not written: {e}", path.display()),
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "menu selftest: grab writer not started");
    }
}

/// Makes this thread's DPI awareness the game window's, so the two windows agree on
/// coordinates and the menu sits over the picture at any display scaling; the awareness
/// (0 unaware, 1 system, 2 per monitor) and the window's DPI.
fn match_dpi(game: HWND) -> (i32, u32) {
    // SAFETY: plain queries on a window handle, and this thread's own awareness.
    unsafe {
        let context = GetWindowDpiAwarenessContext(game);
        if !context.is_null() {
            SetThreadDpiAwarenessContext(context);
        }
        (GetAwarenessFromDpiAwarenessContext(context), GetDpiForWindow(game))
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    // Mouse messages only borrow the context: the window calls that `show` makes send others.
    let with = |f: &mut dyn FnMut(&mut Ctx)| {
        CTX.with(|c| {
            if let Ok(mut c) = c.try_borrow_mut()
                && let Some(ctx) = c.as_mut()
            {
                f(ctx);
            }
        })
    };
    // SAFETY: the standard handlers; the picture is set by `present`, not by painting.
    unsafe {
        match msg {
            WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
            WM_SETCURSOR => {
                SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_ARROW));
                1
            }
            WM_MOUSEMOVE => {
                with(&mut |ctx| {
                    let hover = match ctx.hit(lp) {
                        Some(
                            super::layout::Hit::Row(i)
                            | super::layout::Hit::ValueLeft(i)
                            | super::layout::Hit::ValueRight(i),
                        ) => Some(i),
                        _ => None,
                    };
                    if hover != ctx.hover {
                        (ctx.hover, ctx.dirty) = (hover, true);
                        ctx.show(true);
                    }
                });
                0
            }
            WM_LBUTTONDOWN => {
                with(&mut |ctx| {
                    let hit = ctx.hit(lp);
                    if ctx.driver.click(hit) {
                        ctx.dirty = true;
                        ctx.spec.blocked.store(ctx.driver.blocked(), Relaxed);
                        ctx.show(true);
                    }
                });
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn run(game: HWND, spec: &'static Spec) {
    let class = wide(spec.class);
    let (awareness, dpi) = match_dpi(game);
    // SAFETY: window class and window made and used on this thread only.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);
        let window = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class.as_ptr(),
            class.as_ptr(),
            WS_POPUP,
            0,
            0,
            10,
            10,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if window.is_null() {
            tracing::warn!("menu window not created");
            return;
        }
        tracing::info!(hotkey = %super::key_name((spec.hotkey)()), dpi_awareness = awareness, dpi, "{}", spec.what);
        let selftest = std::env::var("VICTORY_MENU_SELFTEST")
            .ok()
            .filter(|_| spec.selftest)
            .and_then(|v| super::selftest::parse(&v))
            .map(Selftest::new);
        if let Some(test) = &selftest {
            let plan = test.plan();
            let at: Vec<f64> = plan.at_ms.iter().map(|&ms| ms as f64 / 1000.0).collect();
            tracing::info!("menu selftest: planned at {at:?} s, changing {}", plan.setting.as_deref().unwrap_or("the first Live Display setting (VICTORY_OVERLAY if Live)"));
        }
        CTX.with(|c| {
            *c.borrow_mut() = Some(Ctx {
                spec,
                game,
                window,
                driver: Driver::new((spec.model)()),
                painter: None,
                layout: None,
                at: POINT { x: 0, y: 0 },
                hover: None,
                dirty: true,
                shown: false,
                selftest,
            })
        });
        let start = Instant::now();
        SetTimer(window, TICK, TICK_MS, None);
        let mut msg = std::mem::zeroed::<MSG>();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            if msg.message == WM_TIMER && msg.hwnd == window && msg.wParam == TICK {
                let now = start.elapsed().as_millis() as u64;
                CTX.with(|c| {
                    if let Some(ctx) = c.borrow_mut().as_mut() {
                        ctx.tick(now);
                    }
                });
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Renders the menu for a `w` x `h` picture into a premultiplied bitmap, as the window would show
/// it (offscreen previews for the tests).
#[cfg(test)]
pub(super) fn render(driver: &mut Driver, w: i32, h: i32) -> Option<Bitmap> {
    let l = driver.layout(w, h);
    Painter::for_layout(None, &l)?.paint(&l, &driver.menu, &*driver.model, None)
}

#[cfg(test)]
mod tests {
    use super::super::input::{PAD_DOWN as DOWN, PAD_RB};
    use super::super::model::Registry;
    use super::super::state::Tab;
    use super::*;

    /// A stand-in game picture: a dusk sky over dark blocks.
    fn backdrop(w: usize, h: usize) -> Vec<u8> {
        let mut rgb = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let v = y as f32 / h as f32;
                let mut c = [
                    (40.0 + 180.0 * v) as u8,
                    (40.0 + 60.0 * v) as u8,
                    (90.0 - 30.0 * v) as u8,
                ];
                let block = (x * 7 / w.max(1)) as f32;
                if v > 0.55 + 0.08 * (block * 1.7).sin().abs() {
                    c = [18, 14, 24];
                }
                rgb[(y * w + x) * 3..][..3].copy_from_slice(&c);
            }
        }
        rgb
    }

    fn save(name: &str, driver: &mut Driver, w: usize, h: usize) -> Bitmap {
        let bm = render(driver, w as i32, h as i32).expect("open menu draws");
        if let Some(dir) = std::env::var_os("VICTORY_MENU_PNG_DIR") {
            let mut rgb = backdrop(w, h);
            crate::pixels::blend_onto_rgb(&mut rgb, w, h, &bm, (w - bm.w) / 2, (h - bm.h) / 2);
            let png = crate::screenshot::encode_png(w as u32, h as u32, &rgb);
            std::fs::write(std::path::Path::new(&dir).join(name), png).unwrap();
        }
        bm
    }

    /// The menu's thread takes the game window's DPI awareness, whatever its own was: a game
    /// window made per-monitor aware (as a 4K screen at 150 % with an aware title would have it)
    /// and the menu agree on coordinates.
    #[test]
    fn the_menu_thread_takes_the_game_windows_dpi_awareness() {
        let (tx, rx) = std::sync::mpsc::channel::<(usize, std::sync::mpsc::Sender<()>)>();
        let maker = std::thread::spawn(move || {
            // SAFETY: a hidden STATIC window on this thread, destroyed when the test is done.
            unsafe {
                SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                let class = wide("STATIC");
                let w = CreateWindowExW(0, class.as_ptr(), class.as_ptr(), WS_POPUP, 0, 0, 64, 64,
                    std::ptr::null_mut(), std::ptr::null_mut(), GetModuleHandleW(std::ptr::null()), std::ptr::null());
                assert!(!w.is_null());
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                tx.send((w as usize, done_tx)).unwrap();
                done_rx.recv().unwrap();
                DestroyWindow(w);
            }
        });
        let (game, done) = rx.recv().unwrap();
        let checked = std::thread::spawn(move || {
            let game = game as HWND;
            // SAFETY: plain queries on a live window and this thread's awareness.
            unsafe {
                SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_UNAWARE);
                let (awareness, dpi) = match_dpi(game);
                let same = AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(), GetWindowDpiAwarenessContext(game));
                (awareness, dpi, same != 0)
            }
        })
        .join()
        .unwrap();
        done.send(()).unwrap();
        maker.join().unwrap();
        let (awareness, dpi, same) = checked;
        assert_eq!(awareness, DPI_AWARENESS_PER_MONITOR_AWARE);
        assert!(dpi >= 96, "{dpi}");
        assert!(same);
        assert_eq!(in_pixels(|| 7), 7);
    }

    /// The grab sees layered windows (the menu) over what is under them, at their place on the
    /// screen. Shows a small window for a moment, so it runs only on request.
    #[test]
    #[ignore = "shows a window on the real screen and grabs it"]
    fn grab_sees_a_layered_window_on_screen() {
        // SAFETY: a layered window of this test, destroyed at the end.
        unsafe {
            let class = wide("STATIC");
            let w = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, class.as_ptr(), class.as_ptr(),
                WS_POPUP, 0, 0, 64, 64, std::ptr::null_mut(), std::ptr::null_mut(), GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            );
            assert!(!w.is_null());
            let mut bm = Bitmap::new(64, 64);
            for y in 0..64 {
                for x in 0..64 {
                    bm.over(x, y, [250, 20, 200], 255);
                }
            }
            present(w, &bm, POINT { x: 40, y: 40 });
            SetWindowPos(w, HWND_TOPMOST, 40, 40, 64, 64, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            std::thread::sleep(std::time::Duration::from_millis(800));
            let (gw, gh, rgb) = grab(GetDesktopWindow()).expect("grab");
            DestroyWindow(w);
            assert!(gw > 104 && gh > 104);
            // Night light or a colour profile may shift the colour a little.
            let at = (72 * gw + 72) * 3;
            let near = rgb[at..at + 3].iter().zip([250u8, 20, 200]).all(|(&a, b)| a.abs_diff(b) <= 40);
            assert!(near, "{:?}", &rgb[at..at + 3]);
        }
    }

    #[test]
    fn closed_menu_draws_nothing() {
        let mut d = Driver::new(Box::new(Registry::default()));
        assert!(render(&mut d, 1920, 1080).is_none());
    }

    /// The live hooks `apps/victory` registers when the title's upscale is ours and
    /// `VICTORY_FFX_SCALE` was patched: here they do nothing but make the rows live.
    fn hook_display() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            for name in [
                "VICTORY_UPSCALE",
                "VICTORY_DLSS_QUALITY",
                "VICTORY_DLSS_SHARPNESS",
                "VICTORY_FFX_SCALE",
            ] {
                victory_core::settings::on_change(name, |_| {});
            }
        });
    }

    /// Renders the menu on the registry offscreen; with `VICTORY_MENU_PNG_DIR` set, over a
    /// stand-in picture to PNGs there.
    #[test]
    fn open_menu_renders_at_1080_and_4k() {
        let _lock = super::super::model::registry_lock();
        hook_display();
        let mut d = Driver::new(Box::new(Registry::default()));
        d.tick(0, &[Key::Hotkey], 0, 0);
        let small = save("menu_display_1080.png", &mut d, 1920, 1080);
        let mid = (small.h / 2 * small.w + small.w / 2) * 4 + 3;
        assert!(small.px[mid] > 200, "the panel is nearly opaque");
        assert_eq!(small.px[3], 0, "the corner is clear");
        // DLSS on (live), then frame generation (restart): the band shows; the 4K picture is the
        // 1080 one at twice the size.
        d.tick(30, &[Key::Right], 0, 0);
        d.tick(60, &[], 0, 0);
        d.tick(90, &[Key::Right], 0, 0);
        d.tick(120, &[], 0, 0);
        assert_eq!(d.model.get("VICTORY_UPSCALE").as_deref(), Some("dlss"));
        for (at, pad) in [(150, DOWN), (180, 0), (210, DOWN), (240, 0), (270, DOWN), (300, 0), (330, DOWN)] {
            d.tick(at, &[], pad, 0);
        }
        d.tick(360, &[Key::Enter], 0, 0);
        assert_eq!(d.model.pending_restart(), ["VICTORY_FRAME_GEN"]);
        let big = save("menu_display_4k.png", &mut d, 3840, 2160);
        assert_eq!((big.w, big.h), (small.w * 2, small.h * 2));
        d.tick(400, &[], PAD_RB, 0);
        assert_eq!(d.menu.tab, Tab::Group("Submission"));
        save("menu_submission_1080.png", &mut d, 1920, 1080);
        d.tick(430, &[Key::Expert], 0, 0);
        d.tick(460, &[], 0, 0);
        assert!(d.menu.expert);
        d.menu.tab = Tab::Group(super::super::state::EXPERIMENTAL);
        save("menu_experimental_1080.png", &mut d, 1920, 1080);
        d.menu.tab = Tab::Presets;
        d.menu.selected = 1;
        save("menu_presets_1440.png", &mut d, 2560, 1440);
        for name in ["VICTORY_UPSCALE", "VICTORY_FRAME_GEN"] {
            d.model.set(name, "").unwrap();
        }
        assert!(d.model.pending_restart().is_empty());
    }
}
