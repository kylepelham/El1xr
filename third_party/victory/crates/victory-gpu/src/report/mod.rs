//! (El1xr) Report key, `VICTORY_REPORT_KEY` (default off): when Kyle sees something wrong (a sill
//! going dark, a flash, a stutter), one press saves a bundle that says exactly what and when. F11
//! (`VICTORY_REPORT_KEY=1`; a key name like `F7` or a virtual-key number picks another; F10 is the
//! overlay's, F12 the screenshot's) or View + RS held for a second presses it.
//!
//! The bundle ([`bundle`]) goes into `report-<date>_<time>-f<frame>` under `VICTORY_REPORT_DIR`,
//! else `<data dir>\reports`: the presented frame read back about two seconds and about one second
//! before the press and the one presented right after it, the last ten seconds of frame times
//! ([`ring`]), the switches in effect, and a summary. victory.log gets
//! `report key: frame=<n> folder=<path>` so the log can be read around it.
//!
//! Cost: a mutex and a ring entry per present. Once a second the presented texture is copied into
//! one of three readback buffers in the present's own list (as `VICTORY_SNAP_EVERY_S` does; nothing
//! is waited for on the presenting thread); a press makes the next present copy into the oldest,
//! and only then is anything mapped or encoded, on a thread of its own below normal priority. The
//! buffers hold 3 x the presented size in system memory (100 MB at 3840x2160 RGBA8).
//!
//! `VICTORY_REPORT_SELFTEST=60,90` presses at those seconds after the first present (implies the
//! key): 1st, 3rd... passes by the key, 2nd, 4th... by the pad chord, through the same detector
//! ([`trigger`]); each logs `report selftest: pass at <s>s ok ...` or a WARN naming what is missing.
//! `VICTORY_FRAME_GRAPH=1` draws the last frame times in the overlay panel ([`graph`]).

mod bundle;
pub(crate) mod graph;
mod ring;
pub(crate) mod trigger;

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, Once, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

use bundle::{Buffer, Job, Picture};
use ring::{Entry, Ring};
use trigger::{Cause, Trigger};

const VK_F11: u8 = 0x7A;
/// How often the presented texture is kept for "before" pictures.
const EVERY: Duration = Duration::from_secs(1);
const SLOTS: usize = 3;
const POLL: Duration = Duration::from_millis(30);
/// A press no present took in this long is written without the frame at it (a hang).
const NO_PRESENT: Duration = Duration::from_secs(3);

struct Config {
    /// The report key's virtual key; `None` with only the frame graph on.
    key: Option<u8>,
    selftest: Vec<u64>,
    graph: bool,
}

fn config() -> Option<&'static Config> {
    static CONFIG: OnceLock<Option<Config>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
            let selftest = var("VICTORY_REPORT_SELFTEST").map(|v| trigger::parse_selftest(&v)).unwrap_or_default();
            let key = parse_key(var("VICTORY_REPORT_KEY").as_deref()).or_else(|| (!selftest.is_empty()).then_some(VK_F11));
            let graph = var("VICTORY_FRAME_GRAPH").as_deref() == Some("1");
            if key.is_none() && !graph {
                return None;
            }
            if let Some(vk) = key {
                tracing::info!(
                    "report key on (VICTORY_REPORT_KEY): {} or View + RS held 1 s saves a report into {}{}",
                    crate::menu::key_name(vk),
                    root().display(),
                    if selftest.is_empty() { String::new() } else { format!("; selftest presses at {selftest:?} s") }
                );
            }
            Some(Config { key, selftest, graph })
        })
        .as_ref()
}

/// `1`: F11; `0` or nothing: off; else a key name ([`crate::menu::parse_key`]).
fn parse_key(v: Option<&str>) -> Option<u8> {
    match v? {
        "0" => None,
        "1" => Some(VK_F11),
        other => crate::menu::parse_key(other),
    }
}

/// Whether the report key or its pad chord is on (the HLE feeds the pad, the chord is hidden).
pub(crate) fn keyed() -> bool {
    config().is_some_and(|c| c.key.is_some())
}

/// `VICTORY_FRAME_GRAPH=1`.
pub(crate) fn graph_on() -> bool {
    config().is_some_and(|c| c.graph)
}

static DATA_REPORTS: OnceLock<PathBuf> = OnceLock::new();

/// Points reports at `<data dir>\reports` (the app passes it; `VICTORY_REPORT_DIR` wins).
pub fn use_folder(path: PathBuf) {
    let _ = DATA_REPORTS.set(path);
}

fn root() -> PathBuf {
    std::env::var_os("VICTORY_REPORT_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| DATA_REPORTS.get().cloned())
        .unwrap_or_else(|| std::env::temp_dir().join("victory-reports"))
}

#[derive(Clone, Copy)]
struct Slot {
    frame: u64,
    at: Instant,
    flight: u64,
    /// The copy is recorded and sent; a slot being copied into is not.
    filled: bool,
    footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
}

struct Press {
    cause: Cause,
    at: Instant,
    selftest: Option<u64>,
}

struct State {
    start: Option<Instant>,
    last: Option<Instant>,
    ring: Ring,
    due: Option<Instant>,
    /// The readback buffers and their size in bytes.
    buffers: Option<(u64, Vec<Buffer>)>,
    making: bool,
    off: bool,
    slots: [Option<Slot>; SLOTS],
    next: usize,
    press: Option<Press>,
    /// A report is being written: no copy goes into the buffers.
    busy: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    start: None,
    last: None,
    ring: Ring::new(),
    due: None,
    buffers: None,
    making: false,
    off: false,
    slots: [None; SLOTS],
    next: 0,
    press: None,
    busy: false,
});

fn lock() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The last `n` frame times, for the graph.
pub(crate) fn last_ms(n: usize) -> Vec<f32> {
    lock().ring.last_ms(n)
}

/// A copy recorded into a present's list.
pub(crate) struct Recorded {
    slot: usize,
    press: Option<Press>,
}

/// At each present (`texture` in the copy-source state, `list` open): notes the frame time, and
/// records a copy when a "before" picture is due or a press waits for the frame at it.
pub(crate) fn record(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    frame: u64,
) -> Option<Recorded> {
    let config = config()?;
    let now = Instant::now();
    let mut s = lock();
    s.start.get_or_insert(now);
    if let Some(last) = s.last.replace(now) {
        s.ring.push(Entry {
            frame,
            at: now,
            ms: now.duration_since(last).as_secs_f32() * 1000.0,
        });
    }
    config.key?;
    static KEYS: Once = Once::new();
    KEYS.call_once(|| spawn_keys(config));
    drop(s);
    copy(device, list, texture, frame, now)
}

/// Records the copy of a "before" picture when one is due, or of the frame at a waiting press.
fn copy(device: &ID3D12Device, list: &ID3D12GraphicsCommandList, texture: &ID3D12Resource, frame: u64, now: Instant) -> Option<Recorded> {
    let mut s = lock();
    let start = *s.start.get_or_insert(now);
    let due = *s.due.get_or_insert(start + EVERY);
    if s.busy || s.off || (s.press.is_none() && now < due) {
        return None;
    }
    // SAFETY: a live texture.
    let desc = unsafe { texture.GetDesc() };
    if crate::screenshot::to_rgb(desc.Format.0 as u32, 1, 1, 8, &[0; 8]).is_none() {
        s.due = Some(now + EVERY);
        if let Some(press) = s.press.take() {
            let why = format!("the presented format {} is not converted", desc.Format.0);
            write_without_frame(&mut s, press, frame, why);
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
    let buffer = match &s.buffers {
        Some((size, b)) if *size == total => b[s.next].0.clone(),
        _ => {
            if !s.making {
                s.making = true;
                s.slots = [None; SLOTS];
                make_buffers(device.clone(), total);
            }
            return None;
        }
    };
    let slot = s.next;
    s.next = (slot + 1) % SLOTS;
    s.slots[slot] = Some(Slot {
        frame,
        at: now,
        flight: 0,
        filled: false,
        footprint,
    });
    let press = s.press.take();
    if press.is_some() {
        s.busy = true;
    } else {
        s.due = Some(if due + EVERY <= now {
            now + EVERY
        } else {
            due + EVERY
        });
    }
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
    Some(Recorded { slot, press })
}

/// After the present's list is sent (`flight`: its submission, 0 when already waited for): a
/// press's frame starts the bundle.
pub(crate) fn sent(recorded: Option<Recorded>, flight: u64) {
    let Some(r) = recorded else { return };
    let mut s = lock();
    let frame = s.slots[r.slot].as_mut().map(|slot| {
        slot.flight = flight;
        slot.filled = true;
        slot.frame
    });
    match (r.press, frame) {
        (Some(press), Some(frame)) => {
            let pictures = pictures(&s, press.at, Some(r.slot));
            start_job(&mut s, press, frame, pictures, None);
        }
        // The buffers were made again in between: the slot is gone.
        (Some(press), None) => {
            let frame = s.ring.last_frame().unwrap_or(0);
            write_without_frame(&mut s, press, frame, "the presented size changed".into());
        }
        (None, _) => {}
    }
}

/// The filled slots as pictures, oldest first: `before-N`... `before-1`, then `at` for `at_slot`.
fn pictures(s: &State, pressed: Instant, at_slot: Option<usize>) -> Vec<Picture> {
    let Some((_, buffers)) = &s.buffers else {
        return Vec::new();
    };
    let mut filled: Vec<(usize, Slot)> = s
        .slots
        .iter()
        .enumerate()
        .filter_map(|(i, x)| x.filter(|x| x.filled).map(|x| (i, x)))
        .collect();
    filled.sort_by_key(|(i, x)| (Some(*i) == at_slot, x.frame));
    let befores = filled.iter().filter(|(i, _)| Some(*i) != at_slot).count();
    filled
        .into_iter()
        .enumerate()
        .map(|(n, (i, x))| Picture {
            name: if Some(i) == at_slot {
                "at"
            } else {
                ["before-1", "before-2", "before-3"][(befores - 1 - n).min(SLOTS - 1)]
            },
            frame: x.frame,
            before_s: pressed.saturating_duration_since(x.at).as_secs_f64(),
            buffer: Buffer(buffers[i].0.clone()),
            footprint: x.footprint,
            flight: x.flight,
        })
        .collect()
}

fn start_job(
    s: &mut State,
    press: Press,
    frame: u64,
    pictures: Vec<Picture>,
    note: Option<String>,
) {
    let folder = bundle::folder_in(&root(), &bundle::stamp(), frame);
    tracing::info!("report key: frame={frame} folder={}", folder.display());
    let job = Job {
        folder,
        cause: press.cause,
        frame,
        pressed: press.at,
        pictures,
        frames: s.ring.since(Instant::now(), ring::WINDOW),
        note,
        selftest: press.selftest,
    };
    s.busy = bundle::spawn(job, || lock().busy = false);
}

fn write_without_frame(s: &mut State, press: Press, frame: u64, why: String) {
    let pictures = pictures(s, press.at, None);
    start_job(
        s,
        press,
        frame,
        pictures,
        Some(format!("no picture at the press: {why}")),
    );
}

fn make_buffers(device: ID3D12Device, size: u64) {
    struct Device(ID3D12Device);
    // SAFETY: D3D12 objects are free-threaded.
    unsafe impl Send for Device {}
    let device = Device(device);
    let made = std::thread::Builder::new().name("victory-report-buffers".into()).spawn(move || {
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
        let mut buffers = Vec::new();
        let mut error = None;
        for _ in 0..SLOTS {
            let mut buffer: Option<ID3D12Resource> = None;
            // SAFETY: plain creation.
            let made = unsafe { device.0.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut buffer) };
            match (made, buffer) {
                (Ok(()), Some(b)) => buffers.push(Buffer(b)),
                (made, _) => error = Some(made.err()),
            }
        }
        let mut s = lock();
        s.making = false;
        match error {
            None => s.buffers = Some((size, buffers)),
            Some(e) => {
                tracing::warn!(error = ?e, size, "report key: pictures off: no readback buffers");
                s.off = true;
            }
        }
    });
    if made.is_err() {
        lock().making = false;
    }
}

/// Polls the key and the pad every 30 ms (the key only while a window of ours is in front), and
/// plays `VICTORY_REPORT_SELFTEST`'s presses.
fn spawn_keys(config: &'static Config) {
    let Some(vk) = config.key else { return };
    let spawned = std::thread::Builder::new()
        .name("victory-report-key".into())
        .spawn(move || {
            let mut trigger = Trigger::default();
            let began = Instant::now();
            loop {
                std::thread::sleep(POLL);
                let start = lock().start.unwrap_or(began);
                let since_start = start.elapsed().as_millis() as u64;
                let (fake_key, fake_pad, pass) = trigger::injected(&config.selftest, since_start);
                let key =
                    !crate::menu::owns_key(vk) && crate::overlay::ours_in_front() && key_down(vk);
                if let Some(cause) = trigger.tick(
                    since_start,
                    key || fake_key,
                    crate::menu::pad_held() | fake_pad,
                ) {
                    press(cause, pass);
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "report key: key thread not started");
    }
}

fn key_down(vk: u8) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    // SAFETY: a plain query.
    unsafe { GetAsyncKeyState(i32::from(vk)) < 0 }
}

/// A press: the next present takes it; if none does within [`NO_PRESENT`], it is written here
/// without the frame at it.
fn press(cause: Cause, selftest: Option<u64>) {
    {
        let mut s = lock();
        if s.busy || s.press.is_some() {
            tracing::info!(
                "report key: pressed by {} while a report is being written: skipped",
                cause.name()
            );
            return;
        }
        s.press = Some(Press {
            cause,
            at: Instant::now(),
            selftest,
        });
    }
    std::thread::sleep(NO_PRESENT);
    let mut s = lock();
    if let Some(press) = s.press.take() {
        let frame = s.ring.last_frame().unwrap_or(0);
        write_without_frame(
            &mut s,
            press,
            frame,
            format!(
                "no frame was presented within {} s of the press (a hang?)",
                NO_PRESENT.as_secs()
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_f11_or_a_key_and_f10_and_f12_stay_theirs() {
        assert_eq!(parse_key(Some("1")), Some(VK_F11));
        assert_eq!(parse_key(Some("0")), None);
        assert_eq!(parse_key(None), None);
        assert_eq!(parse_key(Some("F7")), Some(0x76));
        assert_eq!(parse_key(Some("nope")), None);
        let overlay = include_str!("../overlay.rs");
        assert!(
            overlay.contains("const VK_F10: i32 = 0x79;")
                && overlay.contains("const VK_F12: i32 = 0x7B;")
        );
        assert_ne!(VK_F11, crate::menu::parse_key("F9").unwrap());
        assert_ne!(VK_F11, crate::menu::parse_key("F8").unwrap());
    }

    /// The present path only notes the frame and records a copy; nothing is mapped, encoded or
    /// written on it, and host.rs calls exactly the two hooks.
    #[test]
    fn the_present_path_only_records() {
        let host = include_str!("../host.rs");
        assert_eq!(host.matches("crate::report::").count(), 2);
        assert!(
            host.contains(
                "let report = crate::report::record(&self.device, list, texture, frame);"
            )
        );
        assert!(host.contains("crate::report::sent(report, slot.map_or(0, |i| p.ahead[i].1));"));
        let here = include_str!("mod.rs");
        for f in ["pub(crate) fn record(", "fn copy("] {
            let body = here.split(f).nth(1).unwrap().split("\n}\n").next().unwrap();
            for slow in ["Map(", "encode_png", "std::fs::", "sleep("] {
                assert!(!body.contains(slow), "{slow} in {f}");
            }
        }
    }

    /// Pictures are ordered oldest first and named by how far back they are.
    #[test]
    fn pictures_are_named_before_2_before_1_at() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let t = Instant::now();
        let buffer = |_| {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_READBACK,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 256,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut b: Option<ID3D12Resource> = None;
            // SAFETY: plain creation.
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        None,
                        &mut b,
                    )
                    .unwrap()
            };
            Buffer(b.unwrap())
        };
        let slot = |frame, back_ms| {
            Some(Slot {
                frame,
                at: t - Duration::from_millis(back_ms),
                flight: 0,
                filled: true,
                footprint: Default::default(),
            })
        };
        let mut s = State {
            start: None,
            last: None,
            ring: Ring::new(),
            due: None,
            buffers: Some((256, (0..SLOTS).map(buffer).collect())),
            making: false,
            off: false,
            slots: [slot(160, 0), slot(100, 2000), slot(130, 1000)],
            next: 0,
            press: None,
            busy: false,
        };
        let names = |p: Vec<Picture>| p.iter().map(|p| (p.name, p.frame)).collect::<Vec<_>>();
        assert_eq!(
            names(pictures(&s, t, Some(0))),
            vec![("before-2", 100), ("before-1", 130), ("at", 160)]
        );
        assert_eq!(
            names(pictures(&s, t, None)),
            vec![("before-3", 100), ("before-2", 130), ("before-1", 160)]
        );
        s.slots[1].as_mut().unwrap().filled = false;
        assert_eq!(
            names(pictures(&s, t, Some(0))),
            vec![("before-1", 130), ("at", 160)]
        );
        assert!((pictures(&s, t, Some(0))[0].before_s - 1.0).abs() < 0.01);
    }

    /// The whole path on the host GPU: "before" copies every due present into the ring of
    /// buffers, a press takes the next present, and the writer saves before-2, before-1 and at
    /// with the frame times, switches and summary.
    #[test]
    fn a_press_writes_a_bundle_with_the_frames_before_and_at_it() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let dir = tempfile::tempdir().unwrap();
        use_folder(dir.path().to_path_buf());
        if root() != dir.path() {
            return; // VICTORY_REPORT_DIR is set in this environment
        }
        let texture = texture(&host.device, 8, 4);
        let present = |frame: u64| {
            let list = host.new_list().unwrap();
            let now = Instant::now();
            lock().ring.push(Entry { frame, at: now, ms: 40.0 });
            let recorded = copy(&host.device, &list.list, &texture, frame, now);
            list.close();
            host.execute(&[&list]);
            sent(recorded, 0);
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        lock().due = Some(Instant::now());
        present(1);
        while lock().buffers.is_none() {
            assert!(Instant::now() < deadline, "no readback buffers made");
            std::thread::sleep(Duration::from_millis(5));
        }
        for frame in [2, 3] {
            lock().due = Some(Instant::now());
            present(frame);
        }
        present(4);
        assert_eq!(lock().slots.iter().flatten().filter(|x| x.filled).count(), 2, "frame 4 was not due");
        lock().press = Some(Press { cause: Cause::Key, at: Instant::now(), selftest: Some(1) });
        present(5);
        assert!(lock().press.is_none(), "the present took the press");
        while lock().busy {
            assert!(Instant::now() < deadline, "the report was not written");
            std::thread::sleep(Duration::from_millis(5));
        }
        let folder = std::fs::read_dir(dir.path()).unwrap().flatten().next().unwrap().path();
        assert!(folder.file_name().unwrap().to_string_lossy().ends_with("-f5"), "{}", folder.display());
        for f in ["before-2-f2.png", "before-1-f3.png", "at-f5.png"] {
            let png = std::fs::read(folder.join(f)).unwrap();
            assert_eq!(&png[16..24], b"\0\0\0\x08\0\0\0\x04", "{f}: full size");
        }
        let csv = std::fs::read_to_string(folder.join("frametimes.csv")).unwrap();
        assert_eq!(csv.lines().count(), 6, "{csv}");
        let summary = std::fs::read_to_string(folder.join("summary.txt")).unwrap();
        assert!(summary.starts_with(&format!("report key: frame=5 folder={}\n", folder.display())), "{summary}");
        assert!(std::fs::read_to_string(folder.join("switches.txt")).unwrap().starts_with("# switches"));
    }

    fn texture(device: &ID3D12Device, width: u64, height: u32) -> ID3D12Resource {
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: width,
            Height: height,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        let mut texture: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe {
            device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_SOURCE, None, &mut texture).unwrap();
        }
        texture.unwrap()
    }
}
