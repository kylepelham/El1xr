//! (texflicker, #96) Black flicker: host slots made null, or made to show a
//! texture with nothing in it, while the title still reads them.
//!
//! `VICTORY_FLICKER_KEEP=N` (frames, 1-600; off by default): a slot whose
//! view a newer view of other memory there made stale
//! ([`super::HostHeap::mirror`], `VICTORY_STALE_TEXTURES`) is not made null
//! at once. Made null at once, the slot read zeros in the lists already in
//! the air and in the rest of the frame's lists, which the title recorded
//! against the old view (the title's record in that slot did not change):
//! a surface black for a frame. With the switch the slot forgets its view
//! (a title write of any view makes it again, as before) but goes on
//! showing the old host copy for `N` frames and until every submission
//! begun when it went stale has finished; then it is made null, unless it
//! was made again meanwhile.
//!
//! Batch 5 found nearly every kept slot made null at the end of its wait
//! (`kept_nulled` = `stale_kept`, `kept_remade` about 2%): the title neither
//! wrote nor drew from them again within 3 frames. A long wait (`=600`, ten
//! seconds and more) asks whether it reads them later: if the black flashes
//! go with it, a stale slot was read after its null; if not, the stale path
//! is not the flicker (`VICTORY_STALE_TEXTURES=0`, its own launch, says the
//! same with no slot ever made stale).
//!
//! `kept_read` counts kept slots a draw's table read during their wait
//! (made null anyway): the slots whose null can show as black.
//! `VICTORY_FLICKER_KEEP_READ=1` (with `VICTORY_FLICKER_KEEP=N`) keeps such
//! a slot another N frames instead, as long as draws read it, up to
//! 600 frames after it went stale; a slot no draw read is made null as
//! before.
//!
//! `VICTORY_FLICKER_KEEP_MS=N` (milliseconds, up to 120,000; off by default;
//! after zm0kiemedia/Neptune_VI `61f2206058`, "stale slots kept until
//! replaced, black sky fix"): a stale slot keeps showing its old view until
//! the title makes it again (its replacement view lands) or `N` ms have
//! passed, whichever is first, instead of `VICTORY_FLICKER_KEEP`'s frames.
//! Batch 5 found the title rarely writes a stale slot again within 600
//! frames, so the frame wait mostly ended in a null; this one waits two
//! minutes. Taken over `VICTORY_FLICKER_KEEP` when both are set. The cost:
//! the old host copies stay alive for that long (memory on the card).
//!
//! `VICTORY_FLICKER_WATCH=1`: a line a frame (frames with any) counting the
//! paths that make a slot read black: stale slots made null (and how many
//! while a submission was in the air), stale slots kept and made null
//! later, views made of a texture made level by level with no level
//! uploaded (`crate::tiles`: reads zeros), and texture views the host could
//! not make (null). The first views of empty textures each frame are logged
//! with their size and format.
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

/// Frames a stale slot keeps its old view (`VICTORY_FLICKER_KEEP`); 0 off.
/// Live: slots held under an earlier value keep the frame they were given,
/// and [`super::HostHeap::stale_due`] lets them go after it is turned off.
pub(super) fn keep_frames() -> u64 {
    static N: victory_core::settings::Live = victory_core::settings::Live::new("VICTORY_FLICKER_KEEP", parse_keep);
    N.get()
}

/// Milliseconds a stale slot keeps its old view until made again
/// (`VICTORY_FLICKER_KEEP_MS`); 0 off. Live, as [`keep_frames`].
pub(super) fn keep_ms() -> u64 {
    static N: victory_core::settings::Live = victory_core::settings::Live::new("VICTORY_FLICKER_KEEP_MS", parse_keep_ms);
    N.get()
}

/// The longest wait `VICTORY_FLICKER_KEEP_MS` takes (Neptune's 120 s).
const MOST_MS: u64 = 120_000;

fn parse_keep_ms(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(0, |n| n.min(MOST_MS))
}

/// `VICTORY_FLICKER_KEEP_READ=1`: a kept slot a draw read during its wait
/// waits again.
fn keep_read() -> bool {
    static ON: victory_core::settings::Live = victory_core::settings::Live::flag("VICTORY_FLICKER_KEEP_READ");
    ON.get_bool()
}

/// The longest wait `VICTORY_FLICKER_KEEP` takes, and the longest a slot
/// is kept in all (`VICTORY_FLICKER_KEEP_READ`).
const MOST_FRAMES: u64 = 600;

fn parse_keep(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(0, |n| n.min(MOST_FRAMES))
}

/// `VICTORY_FLICKER_WATCH=1`.
pub(super) fn watch() -> bool {
    static ON: victory_core::settings::Live = victory_core::settings::Live::flag("VICTORY_FLICKER_WATCH");
    ON.get_bool()
}

/// What a line counts, in its order.
#[derive(Clone, Copy)]
pub(super) enum Count {
    /// Stale slots made null at once.
    StaleNulled,
    /// Of those, while a submission was in the air.
    StaleNulledInAir,
    /// Stale slots that kept their old view (`VICTORY_FLICKER_KEEP`).
    StaleKept,
    /// Kept slots made null once their wait was over.
    KeptNulled,
    /// Kept slots made again meanwhile (left as they were).
    KeptRemade,
    /// Views of a texture made level by level with no level uploaded.
    EmptyViews,
    /// Texture views the host could not make (made null).
    Unmade,
    /// Kept slots a draw's table read during their wait.
    KeptRead,
}

const COUNTS: usize = 8;
const NAMES: [&str; COUNTS] = [
    "stale_nulled",
    "stale_nulled_in_air",
    "stale_kept",
    "kept_nulled",
    "kept_remade",
    "empty_views",
    "unmade_views",
    "kept_read",
];
/// Empty views logged one by one in a frame.
const EMPTY_LOGGED: u64 = 8;

static FRAME: AtomicU64 = AtomicU64::new(0);
static NOW: [AtomicU64; COUNTS] = [const { AtomicU64::new(0) }; COUNTS];
static TOTAL: [AtomicU64; COUNTS] = [const { AtomicU64::new(0) }; COUNTS];

/// Counts `what` in the current frame; the frame before is logged first if
/// it had any.
pub(super) fn count(what: Count) {
    crate::flicker_catch::descriptor(what as usize);
    if !watch() {
        return;
    }
    let frame = crate::frame::current();
    let last = FRAME.load(Relaxed);
    if frame != last
        && FRAME
            .compare_exchange(last, frame, Relaxed, Relaxed)
            .is_ok()
    {
        let counts = NOW.each_ref().map(|c| c.swap(0, Relaxed));
        if counts.iter().any(|&c| c > 0) {
            tracing::info!(
                "flicker watch frame={last} {} total_stale_nulled={} total_stale_kept={} total_empty_views={}",
                line(&counts),
                TOTAL[Count::StaleNulled as usize].load(Relaxed),
                TOTAL[Count::StaleKept as usize].load(Relaxed),
                TOTAL[Count::EmptyViews as usize].load(Relaxed),
            );
        }
    }
    NOW[what as usize].fetch_add(1, Relaxed);
    TOTAL[what as usize].fetch_add(1, Relaxed);
}

fn line(counts: &[u64; COUNTS]) -> String {
    NAMES
        .iter()
        .zip(counts)
        .map(|(name, n)| format!("{name}={n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A view was made of `copy`, shown from `first` (`None`: no level held
/// with memory, so it reads zeros).
pub(super) fn made_view(copy: &crate::resource::HostTexture, first: Option<u32>) {
    if copy.tiled.is_none() || first.is_some() {
        return;
    }
    if !watch() {
        crate::flicker_catch::descriptor(Count::EmptyViews as usize);
        return;
    }
    count(Count::EmptyViews);
    if NOW[Count::EmptyViews as usize].load(Relaxed) <= EMPTY_LOGGED {
        // SAFETY: a live texture.
        let desc = unsafe { copy.texture.GetDesc() };
        tracing::info!(
            frame = crate::frame::current(),
            width = desc.Width,
            height = desc.Height,
            format = desc.Format.0,
            levels = desc.MipLevels,
            "flicker watch: a view of a texture with no level uploaded (reads black)"
        );
    }
}

/// A stale slot waiting to be made null: (frame it may go, time it may go
/// (ms, `crate::host::now_ms`; 0: any), submission that must finish first,
/// frame it went stale, frame its wait began, host heap, slot, its key, the
/// host copy it showed).
struct Held {
    frame: u64,
    ms: u64,
    after: u64,
    stale: u64,
    since: u64,
    host: usize,
    index: u32,
    key: u64,
    raw: usize,
}

static LINE: Mutex<VecDeque<Held>> = Mutex::new(VecDeque::new());

/// Whether a slot was ever held: [`super::HostHeap::stale_due`] then runs
/// with the switch off too, so none is left showing its old view.
static EVER_HELD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a held slot may be made null at frame `frame` and time `ms`: its
/// frames and time are over and every submission begun when it went stale
/// has finished.
fn over(held: &Held, frame: u64, ms: u64, finished: u64) -> bool {
    held.frame <= frame && held.ms <= ms && held.after <= finished
}

impl super::HostHeap {
    /// Slot `index`, made from `made[index]`, went stale: with
    /// `VICTORY_FLICKER_KEEP` it forgets its view but keeps showing it for a
    /// while (true: the caller leaves it); else false and the caller makes
    /// it null.
    pub(super) fn stale_kept(&self, made: &mut [u64], index: u32) -> bool {
        let Some(&key) = made.get(index as usize) else {
            return false;
        };
        if key == 0 {
            return false;
        }
        let ms = keep_ms();
        let frames = if ms != 0 { 1 } else { keep_frames() };
        if frames == 0 || self.sampler {
            count(Count::StaleNulled);
            if crate::host::submissions_started() > crate::host::submissions_finished() {
                count(Count::StaleNulledInAir);
            }
            return false;
        }
        self.hold_stale(
            made,
            index,
            (
                crate::frame::current() + frames,
                if ms != 0 { crate::host::now_ms() + ms } else { 0 },
            ),
            crate::host::submissions_started(),
        )
    }

    /// [`super::HostHeap::stale_kept`] with the switch on: slot `index`
    /// keeps its view until frame and time `(frame, ms)` and submission `after`.
    fn hold_stale(&self, made: &mut [u64], index: u32, (frame, ms): (u64, u64), after: u64) -> bool {
        let key = made[index as usize];
        let raw = self.kept.get(index as usize).and_then(|k| {
            k.lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(raw_of)
        });
        let Some(raw) = raw else {
            // Nothing of the host's shown: nothing to keep.
            return false;
        };
        made[index as usize] = 0;
        if let Some(too) = self.made_too.get(index as usize) {
            too.store(0, Relaxed);
        }
        EVER_HELD.store(true, Relaxed);
        let now = crate::frame::current();
        LINE.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(Held {
                frame,
                ms,
                after,
                stale: now,
                since: now,
                host: std::ptr::from_ref(self) as usize,
                index,
                key,
                raw,
            });
        count(Count::StaleKept);
        true
    }

    /// Makes null the held slots of this heap whose wait is over, if they
    /// still show what they showed when they went stale.
    pub(super) fn stale_due(&self) {
        if keep_frames() == 0 && keep_ms() == 0 && !EVER_HELD.load(Relaxed) {
            return;
        }
        self.release_stale(
            (crate::frame::current(), crate::host::now_ms()),
            crate::host::submissions_finished(),
        );
    }

    /// Makes null the held slots of this heap whose wait is over at frame
    /// and time `(frame, ms)` with submissions up to `finished` done; how many.
    fn release_stale(&self, (frame, ms): (u64, u64), finished: u64) -> usize {
        let me = std::ptr::from_ref(self) as usize;
        let due: Vec<Held> = {
            let mut line = LINE.lock().unwrap_or_else(|e| e.into_inner());
            // A two-minute wait (`VICTORY_FLICKER_KEEP_MS`) holds thousands:
            // moved only when one of them is due.
            if !line.iter().any(|h| h.host == me && over(h, frame, ms, finished)) {
                return 0;
            }
            let (due, left): (Vec<Held>, Vec<Held>) = line
                .drain(..)
                .partition(|h| h.host == me && over(h, frame, ms, finished));
            line.extend(left);
            due
        };
        if due.is_empty() {
            return 0;
        }
        let mut nulled = 0;
        let mut again = Vec::new();
        let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        for mut held in due {
            let shown = self.kept.get(held.index as usize).and_then(|k| {
                k.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(raw_of)
            });
            let still = made.get(held.index as usize) == Some(&0) && shown == Some(held.raw);
            if !still {
                count(Count::KeptRemade);
                continue;
            }
            if self.read_since(held.index, held.since, frame) {
                count(Count::KeptRead);
                if let Some(until) = wait_again(&held, frame, keep_read(), keep_frames()) {
                    (held.frame, held.since) = (until, frame);
                    held.after = crate::host::submissions_started();
                    again.push(held);
                    continue;
                }
            }
            match super::view_at(held.key as u32, (held.key >> 32) as u32) {
                Some(view) => super::null_for(&view, self.cpu(held.index)),
                None => super::null_view(self.cpu(held.index)),
            }
            self.keep(held.index, None);
            count(Count::KeptNulled);
            nulled += 1;
        }
        drop(made);
        if !again.is_empty() {
            LINE.lock().unwrap_or_else(|e| e.into_inner()).extend(again);
        }
        nulled
    }

    /// Whether a draw's table read slot `index` in frame `since` or later
    /// (up to `frame`).
    fn read_since(&self, index: u32, since: u64, frame: u64) -> bool {
        let stamp = self.tabled_stamp(index);
        let now = (frame as u32).wrapping_add(1) & !super::TABLED_READ;
        stamp & super::TABLED_READ != 0
            && u64::from(now.wrapping_sub(stamp & !super::TABLED_READ)) <= frame.saturating_sub(since)
    }
}

/// The frame a kept slot a draw read during its wait waits until next
/// (`VICTORY_FLICKER_KEEP_READ`), or `None`: made null now.
fn wait_again(held: &Held, frame: u64, read: bool, frames: u64) -> Option<u64> {
    let until = frame + frames.max(1);
    (read && frames != 0 && until <= held.stale + MOST_FRAMES).then_some(until)
}

fn raw_of(resource: &ID3D12Resource) -> usize {
    windows::core::Interface::as_raw(resource) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switches_are_live() {
        use victory_core::settings::check_live;
        check_live("VICTORY_FLICKER_KEEP", "8", keep_frames, 0, 8);
        check_live("VICTORY_FLICKER_WATCH", "1", watch, false, true);
    }

    #[test]
    fn keep_is_off_unless_a_number_of_frames() {
        assert_eq!(parse_keep(None), 0);
        assert_eq!(parse_keep(Some("")), 0);
        assert_eq!(parse_keep(Some("0")), 0);
        assert_eq!(parse_keep(Some("x")), 0);
        assert_eq!(parse_keep(Some("3")), 3);
        assert_eq!(parse_keep(Some(" 5 ")), 5);
        assert_eq!(parse_keep(Some("600")), 600);
        assert_eq!(parse_keep(Some("1000")), 600);
    }

    fn held(frame: u64, after: u64) -> Held {
        Held {
            frame,
            ms: 0,
            after,
            stale: frame,
            since: frame,
            host: 1,
            index: 2,
            key: 3,
            raw: 4,
        }
    }

    #[test]
    fn a_kept_slot_waits_for_its_frames_and_its_submissions() {
        let h = held(10, 7);
        assert!(!over(&h, 9, 0, 7), "frames not over");
        assert!(
            !over(&h, 10, 0, 6),
            "a submission begun before it is still in the air"
        );
        assert!(over(&h, 10, 0, 7));
        assert!(over(&h, 12, 0, 9));
        // `VICTORY_FLICKER_KEEP_MS`: its time too.
        let h = Held { ms: 5_000, ..held(10, 7) };
        assert!(!over(&h, 400, 4_999, 9), "time not over");
        assert!(over(&h, 400, 5_000, 9));
    }

    #[test]
    fn keep_ms_is_off_unless_a_number_and_at_most_two_minutes() {
        assert_eq!(parse_keep_ms(None), 0);
        assert_eq!(parse_keep_ms(Some("x")), 0);
        assert_eq!(parse_keep_ms(Some(" 120000 ")), 120_000);
        assert_eq!(parse_keep_ms(Some("500000")), 120_000);
        victory_core::settings::check_live("VICTORY_FLICKER_KEEP_MS", "9000", keep_ms, 0, 9000);
    }

    /// A stale slot with the switch keeps its host copy and forgets its
    /// view; it is made null once its frames and submissions are over,
    /// and left alone if it was made again meanwhile.
    #[test]
    fn a_stale_slot_keeps_its_view_until_its_wait_is_over() {
        let Some(heap) = super::super::HostHeap::new(false, 8) else {
            return; // no host GPU
        };
        let Some(host) = crate::host::host() else {
            return;
        };
        let shown = |i: u32| {
            heap.kept[i as usize]
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(raw_of)
        };
        let (a, _) = host.upload_buffer(256).unwrap();
        let (b, _) = host.upload_buffer(256).unwrap();
        let key = 7u64 << 32 | 9;
        let mut made = vec![0u64; 8];
        for i in [1, 2] {
            made[i] = key;
            heap.keep(i as u32, Some(if i == 1 { a.clone() } else { b.clone() }));
        }
        assert!(heap.hold_stale(&mut made, 1, (100, 0), 40));
        assert!(heap.hold_stale(&mut made, 2, (100, 0), 40));
        assert_eq!(made[1], 0, "the view is forgotten: a title write makes it again");
        assert_eq!(shown(1), Some(raw_of(&a)), "the old host copy is still shown");
        // A slot showing nothing of the host's has nothing to keep.
        made[3] = key;
        assert!(!heap.hold_stale(&mut made, 3, (100, 0), 40));
        // Slot 2 made again meanwhile (another host copy).
        let (c, _) = host.upload_buffer(256).unwrap();
        heap.keep(2, Some(c.clone()));
        assert_eq!(heap.release_stale((99, 0), 40), 0, "frames not over");
        assert_eq!(heap.release_stale((100, 0), 39), 0, "a submission still in the air");
        assert_eq!(heap.release_stale((100, 0), 40), 1);
        assert_eq!(shown(1), None, "made null after its wait");
        assert_eq!(shown(2), Some(raw_of(&c)), "made again: left as it is");
        assert_eq!(heap.release_stale((200, 0), 99), 0, "nothing left");
    }

    #[test]
    fn a_line_names_every_count_in_order() {
        let mut counts = [0; COUNTS];
        counts[Count::StaleKept as usize] = 4;
        counts[Count::EmptyViews as usize] = 2;
        assert_eq!(
            line(&counts),
            "stale_nulled=0 stale_nulled_in_air=0 stale_kept=4 kept_nulled=0 kept_remade=0 empty_views=2 unmade_views=0 kept_read=0"
        );
    }

    /// With `VICTORY_FLICKER_KEEP_READ` a kept slot a draw read waits
    /// again, never past [`MOST_FRAMES`] after it went stale; without it,
    /// or with the wait off, it is made null.
    #[test]
    fn a_read_slot_waits_again_only_with_the_switch_and_within_bounds() {
        let h = held(10, 0);
        assert_eq!(wait_again(&h, 13, true, 3), Some(16));
        assert_eq!(wait_again(&h, 13, false, 3), None);
        assert_eq!(wait_again(&h, 13, true, 0), None);
        assert_eq!(wait_again(&h, 10 + MOST_FRAMES - 3, true, 3), Some(10 + MOST_FRAMES));
        assert_eq!(wait_again(&h, 10 + MOST_FRAMES - 2, true, 3), None);
    }

    /// A draw's table read during the wait is seen; one before it is not.
    #[test]
    fn a_table_read_during_the_wait_is_seen() {
        let Some(heap) = super::super::HostHeap::new(false, 8) else {
            return; // no host GPU
        };
        heap.tabled_in_as(1, true, 50, false);
        assert!(heap.read_since(1, 48, 52), "read in frame 50, wait from 48");
        assert!(heap.read_since(1, 50, 52), "read in the frame the wait began");
        assert!(!heap.read_since(1, 51, 52), "read before the wait");
        heap.tabled_in_as(2, false, 50, false);
        assert!(!heap.read_since(2, 48, 52), "a table made, not read");
        assert!(!heap.read_since(3, 0, 52), "never in a table");
    }

    #[test]
    fn keep_read_is_live() {
        victory_core::settings::check_live("VICTORY_FLICKER_KEEP_READ", "1", keep_read, false, true);
    }
}
