//! Timed pictures for bench runs, in the release build: with `VICTORY_SNAP_EVERY_S=<n>`, every
//! `n` seconds of presents (the first `n` seconds after the first present) the presented texture
//! is copied into a readback buffer kept for it, in the present's own list: nothing more is
//! waited for on the presenting thread. A thread of its own waits for that submission, converts
//! the picture ([`crate::screenshot::to_rgb`]) and writes it at half size as
//! `snap-<seconds>s-f<frame>.png` into `VICTORY_SNAP_DIR` (default `snaps`). The buffer is made
//! on another thread too, the first time and whenever the size changes: that snap waits for the
//! first present after it is ready. One snap at a time; a present while one is being written
//! takes none. The picture is the title's game plane, without a second (UI) plane over it.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

/// The shortest period taken: a smaller one is a typo, not a wish to slow every frame.
const MIN_EVERY: Duration = Duration::from_secs(1);
/// How long the writer waits for the snap's submission to land before it gives up.
const LAND_TIMEOUT: Duration = Duration::from_secs(30);
const LAND_POLL: Duration = Duration::from_millis(5);

pub(crate) struct Config {
    every: Duration,
    folder: PathBuf,
}

fn config() -> Option<&'static Config> {
    static CONFIG: OnceLock<Option<Config>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let every = parse_every(&std::env::var("VICTORY_SNAP_EVERY_S").ok()?)?;
            let folder = std::env::var_os("VICTORY_SNAP_DIR").map_or_else(|| PathBuf::from("snaps"), PathBuf::from);
            tracing::info!(every_s = every.as_secs_f64(), folder = %folder.display(), "timed snaps on (VICTORY_SNAP_EVERY_S)");
            Some(Config { every, folder })
        })
        .as_ref()
}

/// Seconds between snaps; `None` (off) for 0, a negative or no number; at least [`MIN_EVERY`].
fn parse_every(text: &str) -> Option<Duration> {
    let secs: f64 = text.trim().parse().ok()?;
    (secs.is_finite() && secs > 0.0).then(|| Duration::from_secs_f64(secs).max(MIN_EVERY))
}

/// When the snap after one due at `due` and taken at `now` is due: a period after the one due,
/// or after `now` when that is already past (a long frame, a buffer being made).
fn next_due(now: Instant, due: Instant, every: Duration) -> Instant {
    let next = due + every;
    if next <= now { now + every } else { next }
}

struct Buffer(ID3D12Resource);
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Buffer {}

struct Device(ID3D12Device);
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Device {}

#[derive(Default)]
struct State {
    /// The first present seen: snaps are named by the seconds since.
    start: Option<Instant>,
    due: Option<Instant>,
    /// A snap is being written: no other is taken.
    busy: bool,
    /// The readback buffer is being made.
    making: bool,
    /// The readback buffer and its size in bytes.
    buffer: Option<(u64, Buffer)>,
    /// No readback buffer could be made: snaps are off.
    off: bool,
    /// A presented format [`crate::screenshot::to_rgb`] does not convert was seen (said once;
    /// the snap waits for a present of a format it converts).
    warned: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    start: None,
    due: None,
    busy: false,
    making: false,
    buffer: None,
    off: false,
    warned: false,
});

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// A snap recorded into a present's list, to be written once that list has run.
pub(crate) struct Pending {
    buffer: Buffer,
    footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    frame: u64,
    since_start: Duration,
}

/// At a present (`texture` in the copy-source state, `list` open): records the copy of a snap
/// when one is due.
pub(crate) fn record(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    frame: u64,
) -> Option<Pending> {
    let config = config()?;
    record_in(&STATE, config.every, device, list, texture, frame)
}

fn record_in(
    state: &'static Mutex<State>,
    every: Duration,
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    frame: u64,
) -> Option<Pending> {
    let now = Instant::now();
    let mut s = lock(state);
    let start = *s.start.get_or_insert(now);
    let due = *s.due.get_or_insert(start + every);
    if s.busy || s.off || now < due {
        return None;
    }
    // SAFETY: a live texture.
    let desc = unsafe { texture.GetDesc() };
    if crate::screenshot::to_rgb(desc.Format.0 as u32, 1, 1, 8, &[0; 8]).is_none() {
        if !s.warned {
            s.warned = true;
            tracing::warn!(
                format = desc.Format.0,
                "snap skipped: the presented format is not converted"
            );
        }
        return None;
    }
    let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    let mut total = 0;
    // SAFETY: outputs for one subresource of a live texture's description.
    unsafe {
        device.GetCopyableFootprints(
            &desc,
            0,
            1,
            0,
            Some(&mut footprint),
            None,
            None,
            Some(&mut total),
        )
    };
    let buffer = match &s.buffer {
        Some((size, buffer)) if *size == total => buffer.0.clone(),
        _ => {
            if !s.making {
                s.making = true;
                make_buffer(state, Device(device.clone()), total);
            }
            return None;
        }
    };
    s.busy = true;
    s.due = Some(next_due(now, due, every));
    drop(s);
    let to = D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: the location borrows the buffer for the call; no reference is taken.
        pResource: unsafe { std::mem::transmute_copy(&buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: footprint,
        },
    };
    let from = D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above, for the texture.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: 0,
        },
    };
    // SAFETY: an open list; the texture is a copy source, the buffer a copy destination.
    unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    Some(Pending {
        buffer: Buffer(buffer),
        footprint,
        frame,
        since_start: now - start,
    })
}

/// Makes the readback buffer of `size` bytes on a thread of its own.
fn make_buffer(state: &'static Mutex<State>, device: Device, size: u64) {
    let made = std::thread::Builder::new().name("victory-snap-buffer".into()).spawn(move || {
        let device = device;
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        let made = unsafe {
            device.0.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut buffer)
        };
        let mut s = lock(state);
        s.making = false;
        match (made, buffer) {
            (Ok(()), Some(buffer)) => s.buffer = Some((size, Buffer(buffer))),
            (made, _) => {
                tracing::warn!(error = ?made.err(), size, "timed snaps off: no readback buffer");
                s.off = true;
            }
        }
    });
    if made.is_err() {
        lock(state).making = false;
    }
}

/// After the present's list is sent: `flight` is the submission to wait for (0: it was waited
/// for already). The snap is written on a thread of its own.
pub(crate) fn finish(pending: Option<Pending>, flight: u64) {
    if let (Some(pending), Some(config)) = (pending, config()) {
        finish_in(&STATE, pending, flight, config.folder.clone());
    }
}

fn finish_in(state: &'static Mutex<State>, pending: Pending, flight: u64, folder: PathBuf) {
    let spawned = std::thread::Builder::new()
        .name("victory-snap".into())
        .spawn(move || {
            let pending = pending;
            if landed(flight) {
                write(&pending, &folder);
            } else {
                tracing::warn!(
                    frame = pending.frame,
                    flight,
                    "snap not saved: its submission did not land"
                );
            }
            lock(state).busy = false;
        });
    if spawned.is_err() {
        lock(state).busy = false;
    }
}

fn landed(flight: u64) -> bool {
    let deadline = Instant::now() + LAND_TIMEOUT;
    while flight != 0 && crate::host::submissions_finished() < flight {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(LAND_POLL);
    }
    true
}

fn write(pending: &Pending, folder: &Path) {
    let f = pending.footprint.Footprint;
    let (w, h, pitch) = (f.Width as usize, f.Height as usize, f.RowPitch as usize);
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy has run; the mapping spans the footprint's rows.
    let rgb = unsafe {
        if pending.buffer.0.Map(0, None, Some(&mut data)).is_err() || data.is_null() {
            return;
        }
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), pitch * h);
        let rgb = crate::screenshot::to_rgb(f.Format.0 as u32, w, h, pitch, rows);
        pending.buffer.0.Unmap(0, None);
        rgb
    };
    let Some(rgb) = rgb else { return };
    let (half, hw, hh) = halve(&rgb, w, h);
    let png = crate::screenshot::encode_png(hw as u32, hh as u32, &half);
    let path = folder.join(name(pending.since_start, pending.frame));
    match std::fs::create_dir_all(folder).and_then(|()| std::fs::write(&path, png)) {
        Ok(()) => tracing::info!(path = %path.display(), frame = pending.frame, "snap saved"),
        Err(e) => tracing::warn!(error = %e, path = %path.display(), "snap not saved"),
    }
}

fn name(since_start: Duration, frame: u64) -> String {
    format!("snap-{:04}s-f{frame}.png", since_start.as_secs())
}

/// 8-bit RGB rows at half size (every other pixel of every other row), as `crate::capture` saves.
fn halve(rgb: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
    let (hw, hh) = ((w / 2).max(1), (h / 2).max(1));
    let mut out = Vec::with_capacity(hw * hh * 3);
    for y in 0..hh {
        let row = (2 * y).min(h.saturating_sub(1)) * w * 3;
        for x in 0..hw {
            let at = row + (2 * x).min(w.saturating_sub(1)) * 3;
            out.extend_from_slice(&rgb[at..at + 3]);
        }
    }
    (out, hw, hh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_period_is_seconds_of_at_least_one() {
        assert_eq!(parse_every("15"), Some(Duration::from_secs(15)));
        assert_eq!(parse_every(" 2.5 "), Some(Duration::from_millis(2500)));
        assert_eq!(parse_every("0.01"), Some(MIN_EVERY));
        for off in ["0", "-3", "", "x", "inf", "NaN"] {
            assert_eq!(parse_every(off), None, "{off:?}");
        }
    }

    /// Snaps keep their cadence; one taken late (a long frame) does not make the next one early.
    #[test]
    fn the_next_snap_is_a_period_after_the_last_due_or_after_now() {
        let t = Instant::now();
        let every = Duration::from_secs(15);
        assert_eq!(next_due(t + Duration::from_millis(30), t, every), t + every);
        assert_eq!(
            next_due(t + Duration::from_secs(20), t, every),
            t + Duration::from_secs(35)
        );
    }

    #[test]
    fn snaps_are_named_by_seconds_and_frame() {
        assert_eq!(
            name(Duration::from_millis(45_900), 1234),
            "snap-0045s-f1234.png"
        );
    }

    #[test]
    fn halving_keeps_every_other_pixel_of_every_other_row() {
        // 3x3 pixels, each pixel's three bytes its index.
        let rgb: Vec<u8> = (0..9u8).flat_map(|i| [i; 3]).collect();
        assert_eq!(halve(&rgb, 3, 3), (vec![0, 0, 0], 1, 1));
        let rgb: Vec<u8> = (0..16u8).flat_map(|i| [i; 3]).collect();
        assert_eq!(
            halve(&rgb, 4, 4),
            (vec![0, 0, 0, 2, 2, 2, 8, 8, 8, 10, 10, 10], 2, 2)
        );
        assert_eq!(halve(&[7, 8, 9], 1, 1), (vec![7, 8, 9], 1, 1));
    }

    /// The whole path on the host GPU: the first due present asks for the buffer and takes no
    /// snap, the next records the copy into a list that runs, and the writer thread saves a PNG
    /// of half the texture's size. A present while that snap is being written takes none.
    #[test]
    fn a_due_snap_is_copied_in_the_present_list_and_saved_off_thread() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        static STATE: Mutex<State> = Mutex::new(State {
            start: None,
            due: None,
            busy: false,
            making: false,
            buffer: None,
            off: false,
    warned: false,
        });
        let folder = tempfile::tempdir().unwrap();
        let texture = texture(&host.device, 8, 4);
        let every = Duration::from_secs(60);
        lock(&STATE).due = Some(Instant::now());
        let list = host.new_list().unwrap();
        assert!(
            record_in(&STATE, every, &host.device, &list.list, &texture, 1).is_none(),
            "the buffer is made first"
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while lock(&STATE).buffer.is_none() {
            assert!(Instant::now() < deadline, "no readback buffer made");
            std::thread::sleep(Duration::from_millis(5));
        }
        let pending = record_in(&STATE, every, &host.device, &list.list, &texture, 2)
            .expect("a snap once the buffer is made");
        assert!(
            record_in(&STATE, every, &host.device, &list.list, &texture, 3).is_none(),
            "one at a time"
        );
        list.close();
        host.execute(&[&list]);
        finish_in(&STATE, pending, 0, folder.path().to_path_buf());
        while lock(&STATE).busy {
            assert!(Instant::now() < deadline, "the snap was not written");
            std::thread::sleep(Duration::from_millis(5));
        }
        let png = std::fs::read(folder.path().join("snap-0000s-f2.png")).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[16..24], b"\0\0\0\x04\0\0\0\x02", "half of 8x4");
        assert!(
            lock(&STATE).due.unwrap() > Instant::now() + Duration::from_secs(50),
            "the next a period later"
        );
    }

    fn texture(device: &ID3D12Device, width: u64, height: u32) -> ID3D12Resource {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: width,
            Height: height,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        let mut texture: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe {
            device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    None,
                    &mut texture,
                )
                .unwrap();
        }
        texture.unwrap()
    }
}
