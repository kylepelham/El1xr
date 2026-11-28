//! `VICTORY_ROOT_VIEW_MEMO=1` (restart; off unless `1`): a draw's root views the list already
//! bound and brought up to date are not looked up and refreshed again.
//!
//! The ~25 ms a frame of bind CPU outside tables and the texture pass (`binding draws and
//! dispatches per frame ... the rest`) is mostly the root views: per draw, each root CBV/SRV
//! address goes through `shadow::find` (the thread's look-up map, an `Arc` clone and drop) and
//! `Shadow::refresh` (the list's `deferred` mutex and a set insert), and the title sets the same
//! few per-pass constant and buffer addresses draw after draw. In the title threads' profile
//! `LocalKey::with` (that map) and `HostList::defer_refresh` were 39 samples, more than `bind`'s
//! own code.
//!
//! What the memo skips gives nothing new:
//! - a refresh of the same copy, address and length in the same list while its count of changes
//!   stands: the list's `refreshed` set already holds that key (it is cleared only at a title
//!   write, `Deferred::title_write`, and at a reset, and both move `HostList::changes`), so
//!   `defer_refresh` would do nothing;
//! - the look-up of the same root view address while the copies' tables have not been taken to
//!   write (`shadow::shape`): `find` gives the same copy (its own per-thread map is kept on the
//!   same count). The copy is still marked used, as `find` marks it.
//!
//! Root UAVs are not memoized (each bind notes the shader's writes), nor root CBVs off 256 bytes
//! (copied into scratch memory per draw).

/// The switch, read once.
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_ROOT_VIEW_MEMO").is_some_and(|v| v == "1");
        if on {
            tracing::info!("root views already bound and refreshed in a list are not looked up again (VICTORY_ROOT_VIEW_MEMO=1)");
        }
        on
    })
}

/// Root views remembered at most (a draw binds a handful; the oldest goes first).
const VIEWS: usize = 16;
/// Refreshes remembered at most.
const REFRESHED: usize = 32;

/// Where the memo stands: the list (address) and its count of changes.
pub(crate) type ListAt = (usize, u64);

/// What one title list's binds remember (`S`: the host copy, `Arc<Shadow>` in the game).
pub(crate) struct RootMemo<S> {
    /// The list and count, and the copies' table count, the views were found at.
    views_at: (ListAt, u64),
    /// Address, SRV (else CBV), the GPU address bound, the copy.
    views: Vec<(u64, bool, u64, S)>,
    next_view: usize,
    /// The list and count the refreshes were made at.
    refreshed_at: ListAt,
    /// Copy (address of), title address, length.
    refreshed: Vec<(usize, u64, u64)>,
    next_refreshed: usize,
}

impl<S> Default for RootMemo<S> {
    fn default() -> Self {
        RootMemo {
            views_at: ((0, u64::MAX), u64::MAX),
            views: Vec::new(),
            next_view: 0,
            refreshed_at: (0, u64::MAX),
            refreshed: Vec::new(),
            next_refreshed: 0,
        }
    }
}

impl<S: Clone> RootMemo<S> {
    /// Forgets everything (the title list was reset).
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    fn views_at(&mut self, at: (ListAt, u64)) {
        if self.views_at != at {
            self.views_at = at;
            self.views.clear();
            self.next_view = 0;
        }
    }

    /// The GPU address and copy of the root view at `address` bound before at `at` (the list,
    /// its count, the copies' table count), if any.
    pub(crate) fn view(&mut self, at: (ListAt, u64), address: u64, srv: bool) -> Option<(u64, S)> {
        self.views_at(at);
        let hit = self
            .views
            .iter()
            .find(|v| v.0 == address && v.1 == srv)
            .map(|v| (v.2, v.3.clone()));
        stats::count(hit.is_some());
        hit
    }

    /// Notes the root view at `address` found at `at` and brought up to date.
    pub(crate) fn note_view(
        &mut self,
        at: (ListAt, u64),
        address: u64,
        srv: bool,
        gpu: u64,
        copy: S,
    ) {
        self.views_at(at);
        let v = (address, srv, gpu, copy);
        if self.views.len() < VIEWS {
            self.views.push(v);
        } else {
            self.views[self.next_view] = v;
            self.next_view = (self.next_view + 1) % VIEWS;
        }
    }

    /// Whether the refresh of `copy`'s `[address, address + len)` is made already at `at`; if
    /// not, notes it as made (the caller makes it).
    pub(crate) fn refreshed(&mut self, at: ListAt, copy: usize, address: u64, len: u64) -> bool {
        if self.refreshed_at != at {
            self.refreshed_at = at;
            self.refreshed.clear();
            self.next_refreshed = 0;
        }
        let key = (copy, address, len);
        if self.refreshed.contains(&key) {
            stats::count_refresh(true);
            return true;
        }
        stats::count_refresh(false);
        if self.refreshed.len() < REFRESHED {
            self.refreshed.push(key);
        } else {
            self.refreshed[self.next_refreshed] = key;
            self.next_refreshed = (self.next_refreshed + 1) % REFRESHED;
        }
        false
    }
}

/// Hits and misses since the last line.
pub(crate) mod stats {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    static N: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

    pub(super) fn count(hit: bool) {
        N[usize::from(!hit)].fetch_add(1, Relaxed);
    }

    pub(super) fn count_refresh(hit: bool) {
        N[2 + usize::from(!hit)].fetch_add(1, Relaxed);
    }

    /// With the frame line (`draw::bind_split_line`).
    pub(crate) fn line(frames: f64) {
        if !super::on() || frames <= 0.0 {
            return;
        }
        let n: [f64; 4] = std::array::from_fn(|i| N[i].swap(0, Relaxed) as f64 / frames);
        tracing::info!(
            "root views per frame (VICTORY_ROOT_VIEW_MEMO=1): {:.0} found bound already, {:.0} looked up; root SRV refreshes {:.0} made already, {:.0} made",
            n[0],
            n[1],
            n[2],
            n[3]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// A model of the list and the copies: `find` names a copy by address and the tables'
    /// count; `refresh` is `defer_refresh` (a step only for a key not in `refreshed`); a title
    /// write clears `refreshed` and moves `changes`; a reset does too.
    struct Model {
        list: usize,
        changes: u64,
        shape: u64,
        refreshed: HashSet<(usize, u64, u64)>,
        steps: Vec<(usize, u64, u64)>,
        finds: usize,
    }

    impl Model {
        fn find(&mut self, address: u64) -> (usize, u64) {
            self.finds += 1;
            // The copy (an id that changes with the tables) and its GPU address.
            let copy = (address >> 16) as usize * 1000 + (self.shape % 3) as usize;
            (copy, 0xa_0000_0000 + address + self.shape % 3 * 0x100)
        }
        fn refresh(&mut self, copy: usize, address: u64, len: u64) {
            if self.refreshed.insert((copy, address, len)) {
                self.steps.push((copy, address, len));
            }
        }
        fn at(&self) -> ListAt {
            (self.list, self.changes)
        }
    }

    /// Binds a stream of draws' root views, the way `State::bind` would without and with the
    /// memo; what is bound and the steps recorded must be the same.
    fn run(memo_on: bool) -> (Vec<u64>, Vec<(usize, u64, u64)>, usize) {
        let mut m = Model {
            list: 0x1000,
            changes: 1,
            shape: 0,
            refreshed: HashSet::new(),
            steps: Vec::new(),
            finds: 0,
        };
        let mut memo: RootMemo<usize> = RootMemo::default();
        let mut bound = Vec::new();
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for draw in 0..5000u64 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            match seed % 97 {
                0 => {
                    // A title write recorded in the list.
                    m.changes += 1;
                    m.refreshed.clear();
                }
                1 => m.shape += 1,
                2 => {
                    // The list ran and was reset (the same object again).
                    m.changes += 1;
                    m.refreshed.clear();
                    m.steps.push((0, 0, 0));
                }
                _ => {}
            }
            // Per draw: per-pass constants, a per-material one, a root SRV of a mesh.
            let views = [
                (0x10_0000u64, false),
                (0x20_0000 + (draw / 50 % 4) * 0x1_0000, false),
                (0x40_0000 + (seed % 23) * 0x1_0000, true),
                (0x10_0000, true),
            ];
            for (address, srv) in views {
                let at = (m.at(), m.shape);
                let len = if srv { 1 << 20 } else { 1 << 16 };
                if memo_on && let Some((gpu, _copy)) = memo.view(at, address, srv) {
                    bound.push(gpu);
                    continue;
                }
                let (copy, gpu) = m.find(address);
                m.refresh(copy, address, len);
                bound.push(gpu);
                if memo_on {
                    memo.note_view(at, address, srv, gpu, copy);
                }
            }
            // A root SRV table's refresh (`State::root_srv_table`).
            let (copy, _) = m.find(0x80_0000 + (draw % 3) * 0x1_0000);
            let address = 0x80_0000 + (draw % 3) * 0x1_0000;
            if !(memo_on && memo.refreshed(m.at(), copy, address, 4096)) {
                m.refresh(copy, address, 4096);
            }
        }
        (bound, m.steps, m.finds)
    }

    #[test]
    fn the_memo_binds_and_records_what_binding_afresh_does() {
        let (bound_off, steps_off, finds_off) = run(false);
        let (bound_on, steps_on, finds_on) = run(true);
        assert_eq!(bound_on, bound_off);
        assert_eq!(steps_on, steps_off);
        // And it skips most of the look-ups.
        assert!(finds_on * 2 < finds_off, "{finds_on} of {finds_off}");
    }

    #[test]
    fn a_title_write_or_another_list_forgets_what_was_bound() {
        let mut memo: RootMemo<u8> = RootMemo::default();
        let at = ((0x1000, 5), 9);
        memo.note_view(at, 0x100, false, 0xa100, 1);
        assert_eq!(memo.view(at, 0x100, false), Some((0xa100, 1)));
        assert_eq!(memo.view(at, 0x100, true), None);
        assert_eq!(memo.view(((0x1000, 6), 9), 0x100, false), None);
        memo.note_view(at, 0x100, false, 0xa100, 1);
        assert_eq!(memo.view(((0x2000, 5), 9), 0x100, false), None);
        memo.note_view(at, 0x100, false, 0xa100, 1);
        assert_eq!(memo.view(((0x1000, 5), 10), 0x100, false), None);
        assert!(!memo.refreshed((0x1000, 5), 7, 0x100, 64));
        assert!(memo.refreshed((0x1000, 5), 7, 0x100, 64));
        assert!(!memo.refreshed((0x1000, 5), 7, 0x100, 128));
        assert!(!memo.refreshed((0x1000, 6), 7, 0x100, 64));
        memo.clear();
        assert!(!memo.refreshed((0x1000, 6), 7, 0x100, 64));
        // Past its room the oldest goes, never a wrong answer.
        for i in 0..(VIEWS as u64 + 4) {
            memo.note_view(at, i, true, i + 1, 0);
        }
        assert_eq!(memo.view(at, 0, true), None);
        assert_eq!(
            memo.view(at, VIEWS as u64 + 3, true),
            Some((VIEWS as u64 + 4, 0))
        );
    }
}
