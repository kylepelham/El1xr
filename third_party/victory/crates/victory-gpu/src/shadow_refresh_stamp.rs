//! (feedcard, #88) `VICTORY_REFRESH_STAMP=1` (Live; off by default): a refresh
//! of a copy's range that the same stretch of the submission already refreshed
//! whole returns at once. A stretch ends at each submission's start and at each
//! step of ours that writes title memory (`Batch::title_written`, where the
//! batch is flushed): [`expire`].
//!
//! After Peppacodes/playablegames-peppa `3b93a4ceb6` (its callers came in
//! `8d26345581`). There, 216 of ~1,180 refreshes a frame asked again for a copy
//! and range of the same submission. Here a copy's refreshes in one flush are
//! already joined into one step where nothing comes between, so what is left is
//! a refresh after one of the copy's declared shader writes in the same flush
//! (`ShadowStep::Write`/`Barrier`), which runs right after the first refresh on
//! the same thread. Its work then is the write-watch look (watched copies,
//! about 1.5 us) or the whole copy again (copies that are not write-watched).
//!
//! Not stamped: a refresh cut around ranges shaders are writing, and copies
//! with lazy copy-back state (pages held on the card or being synced back),
//! whose refreshes change `copied` behind the stamp. The stamp is dropped when
//! a copy-back writes title memory from the copy or marks its pages held
//! (`Shadow::holds`, the merges, `merge_under_writes`), whether the switch is
//! on or not. A title write that lands between the two refreshes is not seen
//! by the second; the first refresh already took the title's bytes for this
//! stretch, as the next submission's refresh takes later ones. Not in the
//! cache key: no shader or pipeline output.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_REFRESH_STAMP", |v| u64::from(v == Some("1")));

#[cfg(test)]
thread_local! {
    static FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Live: read once per refresh.
pub(crate) fn on() -> bool {
    #[cfg(test)]
    if let Some(forced) = FORCED.with(std::cell::Cell::get) {
        return forced;
    }
    ON.get_bool()
}

/// The stretch of the submission now; a stamp of another one holds nothing.
static EPOCH: AtomicU64 = AtomicU64::new(1);

/// A submission begins, or a step of ours is about to write title memory.
pub(crate) fn expire() {
    EPOCH.fetch_add(1, Ordering::AcqRel);
}

/// With the switch on: the stretch a refresh starting now stamps with.
pub(crate) fn epoch_if_on() -> Option<u64> {
    on().then(|| EPOCH.load(Ordering::Acquire))
}

/// Refreshes answered by their stamp, and refreshes stamped, since the last line.
static COUNTS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

pub(crate) fn answered() {
    COUNTS[0].fetch_add(1, Ordering::Relaxed);
}

/// With the batch flush's lines.
pub(crate) fn line(frames: f64) {
    let answered = COUNTS[0].swap(0, Ordering::Relaxed) as f64 / frames;
    let stamped = COUNTS[1].swap(0, Ordering::Relaxed) as f64 / frames;
    if on() {
        tracing::info!(
            "refreshes answered by their stamp per frame (VICTORY_REFRESH_STAMP=1): {answered:.0} of {stamped:.0} stamped"
        );
    }
}

/// A copy's last range refreshed whole and the stretch it was in (0: none).
#[derive(Default)]
pub(crate) struct Stamp(Mutex<(u64, usize, usize)>);

impl Stamp {
    /// Whether `[from, to)` lies in the range refreshed whole in stretch `epoch`.
    pub(crate) fn holds(&self, epoch: u64, from: usize, to: usize) -> bool {
        let (at, a, e) = *self.0.lock().unwrap_or_else(|e| e.into_inner());
        at == epoch && a <= from && to <= e
    }

    /// `[from, to)` was refreshed whole in stretch `epoch`.
    pub(crate) fn set(&self, epoch: u64, from: usize, to: usize) {
        COUNTS[1].fetch_add(1, Ordering::Relaxed);
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = (epoch, from, to);
    }

    /// What the copy holds changed other than by a refresh.
    pub(crate) fn clear(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = (0, 0, 0);
    }
}

impl super::Shadow {
    /// Whether this copy's refreshes may be stamped: no lazy copy-back state
    /// anywhere (pages of another copy held on the card change what this one
    /// must read) nor on this copy.
    pub(super) fn stampable(&self) -> bool {
        super::LAZY_ARMED_COUNT.load(Ordering::Acquire) == 0
            && self.deferred_count.load(Ordering::Acquire) == 0
            && self.resync_count.load(Ordering::Acquire) == 0
    }
}

#[cfg(test)]
pub(crate) fn force(on: Option<bool>) {
    FORCED.with(|f| f.set(on));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_holds_its_range_in_its_stretch_only() {
        let s = Stamp::default();
        let e = EPOCH.load(Ordering::Acquire);
        assert!(!s.holds(e, 0, 1), "no stamp yet");
        s.set(e, 100, 200);
        assert!(s.holds(e, 100, 200));
        assert!(s.holds(e, 120, 180));
        assert!(!s.holds(e, 99, 150), "starts before");
        assert!(!s.holds(e, 150, 201), "ends after");
        assert!(!s.holds(e + 1, 120, 180), "another stretch");
        s.clear();
        assert!(!s.holds(e, 120, 180), "cleared");
    }

    #[test]
    fn off_gives_no_stretch_and_expire_moves_it() {
        force(Some(false));
        assert_eq!(epoch_if_on(), None);
        force(Some(true));
        let a = epoch_if_on().expect("on");
        expire();
        let b = epoch_if_on().expect("on");
        assert!(b > a);
        force(None);
    }

    /// The copy a submission's refreshes leave is the same with the switch as
    /// without when nothing writes title memory between them; the second
    /// refresh of a range inside the first copies nothing with the switch, and
    /// a new stretch or a dropped stamp copies again. The title memory is a
    /// plain allocation, not write-watched, so every refresh without the
    /// stamp copies its whole range.
    #[test]
    fn stamped_refreshes_leave_the_same_copy() {
        use std::sync::Arc;
        let Some(h) = crate::host::host() else {
            return;
        };
        const LEN: usize = 3 * super::super::PAGE;
        let title: Vec<u8> = (0..LEN + 4096).map(|i| (i * 7 + 3) as u8).collect();
        let base = title.as_ptr() as usize;
        let addr = (base + 4095) & !4095;
        let make = || -> Option<Arc<super::super::Shadow>> {
            let size = (LEN as u64).next_multiple_of(super::super::PAGE as u64);
            let (buffer, data) = h.shared_buffer(size)?;
            // SAFETY: live D3D12 buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            Some(Arc::new(super::super::Shadow::new(
                buffer,
                data,
                gpu,
                addr as u64,
                LEN as u64,
            )))
        };
        let run = |stamp: bool| -> Option<(Vec<usize>, Vec<u8>)> {
            let s = make()?;
            force(Some(stamp));
            expire();
            let mut copied = vec![
                s.update(addr, LEN),
                s.update(addr + 100, 5000),
                s.update(addr + LEN / 2, LEN / 2),
            ];
            expire();
            copied.push(s.update(addr + 100, 5000));
            s.holds(&[], &[]);
            copied.push(s.update(addr + 100, 5000));
            force(None);
            // SAFETY: the copy's CPU side is mapped for `LEN` bytes.
            let bytes = unsafe { std::slice::from_raw_parts(s.data, LEN) }.to_vec();
            Some((copied, bytes))
        };
        let Some((plain, plain_bytes)) = run(false) else {
            return;
        };
        let (stamped, stamped_bytes) = run(true).expect("second copy");
        assert_eq!(plain, vec![LEN, 5000, LEN / 2, 5000, 5000]);
        assert_eq!(
            stamped,
            vec![LEN, 0, 0, 5000, 5000],
            "inside the first: nothing; new stretch or held pages: again"
        );
        assert_eq!(plain_bytes, stamped_bytes);
        assert_eq!(&plain_bytes[..], &title[addr - base..addr - base + LEN]);
    }
}
