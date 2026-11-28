//! (Neptune) Textures picked by index made ready off the thread that records
//! the frame (`VICTORY_TEXTURE_PREPARE_THREADS=N`; 0 or unset: none, and the
//! pass over the heap's texture views makes them itself, as before).
//!
//! That pass (`crate::draw`, `State::heap_textures`) runs on a thread that
//! records the frame. For each texture it brings in it makes the host copy
//! (a `D3D12` texture of its own, `crate::resource::host_resource`) and a
//! staging buffer (an upload buffer, made or taken from the pool and
//! zeroed), and records the copy; the staging buffer is detiled when the
//! list is submitted. While a long queue is due (the opening cutscene: 2,000
//! to 20,000 slots) it may take 250 ms a pass, 1.5 s while catching up, and
//! took all of it: the frame line's "heap buffers" stood at 964-1,740 ms and
//! the frame at a third to half a frame a second for a minute or two.
//!
//! With the switch the pass hands each texture it would make to N threads
//! of its own. They make the host copy (while the card has room), make the
//! staging buffer and detile the title's memory into it. The pass records
//! the upload of the ones that are ready (a copy per level, as before) and
//! passes over the others, which stay due. What it hands out in a frame is
//! held to the pass's bytes a frame (`VICTORY_TEXTURE_STAGE_MB`, 512 MB while
//! catching up), and so is what is handed out and not yet taken.
//!
//! What stays as it was: the pass asks whether the card has room before it
//! hands a slot out (and again every 32 MiB handed out), and the thread asks
//! again before it makes a new host copy (`crate::resource::room_lately`); a
//! staging buffer is held by the list that records its upload until that
//! list has run; the title writing a texture after it was detiled is found
//! at the list's submission (write watch, guards, hash), and after the
//! upload by the looks of `texture_slots_rechecked`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, OnceLock};

use super::Staging;
use crate::object::This;

/// The threads that make textures ready (`VICTORY_TEXTURE_PREPARE_THREADS`,
/// at most 16; none when unset or 0).
pub(crate) fn threads() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_TEXTURE_PREPARE_THREADS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0)
            .min(16)
    })
}

/// (texflicker, #96) `VICTORY_PREPARE_PRUNE_SAFE` (live; 0 unless set): 1:
/// a table never leaves a slot null for a texture whose host copy holds an
/// upload already (`crate::table_defer`), and what was made ready is let go
/// two seconds after it was last taken, not after it was made ([`prune`]);
/// 2: the first alone.
///
/// The diner's background went dark for one frame every ~2.06 s (b5 part N:
/// 20 dark frames in 75 s, none with no prepare threads; [`prune`] ran in the
/// frame before 15 times of 20). The texture pass lets go of a texture's
/// staging buffer once it recorded its upload (`crate::draw`, "Uploaded"),
/// so the texture reads as never staged; a table that reads it next asked
/// the prepare threads ([`held`]) and found the entry another slot's take
/// had left in the queue: it took that, and nothing was null. Two seconds
/// after that entry was made the prune let it go though tables took it every
/// frame; the next table that read the texture handed it out anew and left
/// its slot null for a frame, the texture's host copy full all along (the
/// far layer's textures, many at once); the entry made then was let go two
/// seconds later, and so on.
pub(crate) fn prune_safe() -> u64 {
    static LEVEL: victory_core::settings::Live =
        victory_core::settings::Live::new("VICTORY_PREPARE_PRUNE_SAFE", prune_safe_level);
    LEVEL.get()
}

fn prune_safe_level(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(0, |n| n.min(2))
}

/// A texture handed out: the title resource (kept alive until the thread is
/// done with it), the first mip the view that asked for it shows, and its
/// bytes as counted against the budget.
struct Job {
    resource: usize,
    first: u32,
    bytes: u64,
}

/// A texture made ready: its staging buffer, made and detiled (`None`:
/// nothing to stage, the pass goes its old way for it), the first mip that
/// holds, the bytes it counts, when it was made ready
/// (`crate::host::moment`), and the slots that waited for it.
struct Ready {
    staging: Option<Arc<Staging>>,
    first: u32,
    bytes: u64,
    at: u64,
    slots: Vec<u32>,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    /// What is handed out and not done, by resource and first mip, with
    /// the slots that wait for it.
    pending: HashMap<(usize, u32), Vec<u32>>,
    /// The slots of `pending` (how many pending jobs name each): the pass
    /// passes over them at once.
    waiting: HashMap<u32, u32>,
    /// What is made ready and not taken, by resource.
    ready: HashMap<usize, Vec<Ready>>,
    /// The slots of `ready` (how many entries name each): the pass looks
    /// at them even when it hands out nothing more.
    ready_slots: HashMap<u32, u32>,
    /// Bytes handed out and not done, or made ready and not taken.
    outstanding: u64,
    /// The frame `handed` counts for, and the bytes handed out in it.
    frame: u64,
    handed: u64,
    /// Threads started.
    started: usize,
}

impl Queue {
    fn wait(&mut self, slot: u32) {
        *self.waiting.entry(slot).or_insert(0) += 1;
    }

    fn unwait(&mut self, slots: &[u32]) {
        for slot in slots {
            if let Some(n) = self.waiting.get_mut(slot) {
                *n -= 1;
                if *n == 0 {
                    self.waiting.remove(slot);
                }
            }
        }
    }

    fn hint(&mut self, slots: &[u32]) {
        for &slot in slots {
            *self.ready_slots.entry(slot).or_insert(0) += 1;
        }
    }

    fn unhint(&mut self, slots: &[u32]) {
        for slot in slots {
            if let Some(n) = self.ready_slots.get_mut(slot) {
                *n -= 1;
                if *n == 0 {
                    self.ready_slots.remove(slot);
                }
            }
        }
    }

    /// Takes `ready` out of the counts (it leaves the queue).
    fn gone(&mut self, ready: &Ready) {
        self.outstanding = self.outstanding.saturating_sub(ready.bytes);
        self.unhint(&ready.slots);
    }

    /// [`take_ready`]'s look in the queue: the staging made ready for `key`
    /// from mip `first` on, and what leaves the queue with the take (to be
    /// dropped outside its lock). `serving`: the slot the pass serves now.
    /// `by_table`: a take with no slot served (a draw's table) retires the
    /// entry it took at once (`crate::texture::take_by_table`): the table of
    /// staging buffers holds it from now on, and the pass finds it there.
    /// `now` (`VICTORY_PREPARE_PRUNE_SAFE=1`): the entry taken counts its two
    /// seconds before the prune from now if it stays.
    fn take(
        &mut self,
        key: usize,
        first: u32,
        serving: Option<u32>,
        by_table: bool,
        now: Option<u64>,
    ) -> Option<(Option<Arc<Staging>>, Vec<Ready>)> {
        let all = self.ready.get_mut(&key)?;
        let best = all
            .iter()
            .enumerate()
            .filter(|(_, r)| r.first <= first)
            .max_by_key(|(_, r)| r.first)
            .map(|(i, _)| i)?;
        let staging = all[best].staging.clone();
        if let Some(now) = now {
            all[best].at = now;
        }
        // The slot served now waits for none of them any more.
        let mut served = 0;
        let mut table_slots = Vec::new();
        if let Some(slot) = serving {
            for ready in all.iter_mut() {
                if let Some(at) = ready.slots.iter().position(|&s| s == slot) {
                    ready.slots.swap_remove(at);
                    served += 1;
                }
            }
        } else if by_table {
            table_slots = std::mem::take(&mut all[best].slots);
        }
        // What no slot waits for any more goes (taken now, or not needed).
        let mut gone = Vec::new();
        let mut i = 0;
        while i < all.len() {
            if all[i].slots.is_empty() {
                gone.push(all.swap_remove(i));
            } else {
                i += 1;
            }
        }
        if all.is_empty() {
            self.ready.remove(&key);
        }
        if let Some(slot) = serving {
            for _ in 0..served {
                self.unhint(&[slot]);
            }
        }
        self.unhint(&table_slots);
        for ready in &gone {
            self.gone(ready);
        }
        Some((staging, gone))
    }
}

static QUEUE: LazyLock<Mutex<Queue>> = LazyLock::new(Mutex::default);
static WORK: Condvar = Condvar::new();

fn queue() -> MutexGuard<'static, Queue> {
    QUEUE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Since the last frame line: textures made ready; host copies the threads
/// made; title bytes made ready; the threads' microseconds in all, of them
/// making host copies, making staging buffers, and reading and detiling;
/// textures handed out that found the card without room (looked at again);
/// textures made ready that nothing took (let go after two seconds).
pub(crate) static STATS: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

impl Queue {
    /// Drops slots currently being prepared (`waiting`, returning how many were
    /// skipped) and moves slots with prepared textures (`ready_slots`) to the
    /// front of `due`, preserving relative order within each group.
    fn prioritize_due(&self, due: &mut Vec<(u32, u64)>) -> (usize, u64) {
        if self.waiting.is_empty() && self.ready_slots.is_empty() {
            return (0, 0);
        }
        let mut ready = Vec::new();
        let mut other = Vec::with_capacity(due.len());
        let mut waiting = 0u64;
        for &(slot, write) in due.iter() {
            if self.waiting.contains_key(&slot) {
                waiting += 1;
            } else if self.ready_slots.contains_key(&slot) {
                ready.push((slot, write));
            } else {
                other.push((slot, write));
            }
        }
        let ready_count = ready.len();
        due.clear();
        due.extend(ready);
        due.extend(other);
        (ready_count, waiting)
    }
}

/// Classifies a batch of due `(slot, write)` entries under a single lock of the
/// prepare queue: drops slots currently being prepared (returning how many were
/// skipped) and moves slots whose prepared textures are ready to the front so
/// they drain before new work burns the pass budget.
pub(crate) fn prioritize_due(due: &mut Vec<(u32, u64)>) -> (usize, u64) {
    queue().prioritize_due(due)
}

/// Where the pass stands with a slot before it looks at it (tests).
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Slot {
    /// Its texture is handed out and not ready: passed over.
    Waiting,
    /// Its texture was made ready for it.
    Ready,
    /// Neither.
    Other,
}

/// See [`Slot`].
#[cfg(test)]
pub(crate) fn slot(index: u32) -> Slot {
    let q = queue();
    if q.waiting.contains_key(&index) {
        Slot::Waiting
    } else if q.ready_slots.contains_key(&index) {
        Slot::Ready
    } else {
        Slot::Other
    }
}

/// What the pass does with a slot whose texture it would make.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Look {
    /// Its old way: the texture is ready (or there is nothing to hand out).
    Go,
    /// Handed out already, for another slot: passed over, it stays due.
    Wait,
    /// Handed out now (its bytes): passed over, it stays due.
    Handed(u64),
    /// The budget is spent: passed over, nothing more is handed out.
    Full,
}

/// Slot `slot` shows title texture `resource` from mip `first` on and the
/// pass would make it ready now (its host copy and staging buffer).
/// `recorded`: what this pass has recorded uploads of, by resource and first
/// mip (a second view of one is the old way's). `budget`: the bytes that
/// may be handed out in frame `frame`, and be out at once.
pub(crate) fn look(
    slot: u32,
    resource: This,
    first: u32,
    recorded: &HashSet<(usize, u32)>,
    budget: u64,
    frame: u64,
) -> Look {
    look_in(slot, resource, first, recorded, budget, frame, false)
}

/// What the prepare threads hold for title texture `resource` from mip
/// `first` on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Held {
    /// Handed out, not made ready yet.
    Pending,
    /// Made ready, not taken yet.
    Ready,
    Nothing,
}

pub(crate) fn held(resource: This, first: u32) -> Held {
    queue().held(resource as usize, first)
}

impl Queue {
    /// [`held`]'s look in the queue.
    fn held(&self, resource: usize, first: u32) -> Held {
        if (0..=first).any(|f| self.pending.contains_key(&(resource, f))) {
            Held::Pending
        } else if self
            .ready
            .get(&resource)
            .is_some_and(|all| all.iter().any(|r| r.first <= first))
        {
            Held::Ready
        } else {
            Held::Nothing
        }
    }
}

/// [`look`] for a draw that binds the texture through a table now
/// (`crate::table_defer`): handed out ahead of the texture pass's queue,
/// and past the frame's bytes while what is out stays under twice `budget`.
pub(crate) fn look_urgent(slot: u32, resource: This, first: u32, budget: u64, frame: u64) -> Look {
    look_in(slot, resource, first, &HashSet::new(), budget, frame, true)
}

fn look_in(
    slot: u32,
    resource: This,
    first: u32,
    recorded: &HashSet<(usize, u32)>,
    budget: u64,
    frame: u64,
    urgent: bool,
) -> Look {
    if threads() == 0
        || !crate::resource::title_fills(resource)
        || crate::resource::host_refused(resource)
        || recorded.contains(&(resource as usize, first))
    {
        return Look::Go;
    }
    let made = crate::resource::host_copy_made(resource);
    // Staged already (draws read it through a table): the old way.
    if made && crate::resource::staged_first(resource) <= first {
        return Look::Go;
    }
    let bytes = super::staged_bytes(resource, first);
    let mut q = queue();
    if q.ready
        .get(&(resource as usize))
        .is_some_and(|all| all.iter().any(|r| r.first <= first))
    {
        return Look::Go;
    }
    if let Some(key) = (0..=first)
        .map(|f| (resource as usize, f))
        .find(|key| q.pending.contains_key(key))
    {
        let added = q.pending.get_mut(&key).is_some_and(|slots| {
            if slots.contains(&slot) {
                false
            } else {
                slots.push(slot);
                true
            }
        });
        if added {
            q.wait(slot);
        }
        return Look::Wait;
    }
    if q.frame != frame {
        q.frame = frame;
        q.handed = 0;
    }
    if over_budget(q.handed, q.outstanding, budget, urgent) {
        return Look::Full;
    }
    crate::resource::keep_alive(resource);
    q.handed += bytes;
    q.outstanding += bytes;
    q.pending.insert((resource as usize, first), vec![slot]);
    q.wait(slot);
    let job = Job {
        resource: resource as usize,
        first,
        bytes,
    };
    if urgent {
        q.jobs.push_front(job);
    } else {
        q.jobs.push_back(job);
    }
    let start = threads().saturating_sub(q.started);
    q.started += start;
    drop(q);
    for _ in 0..start {
        start_thread();
    }
    WORK.notify_one();
    Look::Handed(bytes)
}

/// Whether nothing more may be handed out: the frame's bytes or what is out
/// reached `budget`; for an urgent look only what is out, against twice it.
fn over_budget(handed: u64, outstanding: u64, budget: u64, urgent: bool) -> bool {
    if urgent {
        outstanding >= budget.saturating_mul(2)
    } else {
        handed >= budget || outstanding >= budget
    }
}

fn start_thread() {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Relaxed);
    let mut builder = std::thread::Builder::new().name(format!("victory texture prepare {n}"));
    if crate::stack_commit::on() {
        builder = builder.stack_size(crate::stack_commit::RESERVE);
    }
    let started = builder.spawn(run);
    if let Err(e) = started {
        tracing::warn!(error = %e, "a texture prepare thread could not be started");
        // Its share is done here, on the thread that hands out: nothing
        // waits for a thread that is not there.
        queue().started -= 1;
        loop {
            let Some(job) = queue().jobs.pop_front() else {
                break;
            };
            let outcome = work(&job);
            finish(job, outcome);
        }
    } else if n == 0 {
        tracing::info!(
            threads = threads(),
            "textures picked by index are made ready on threads of their own (VICTORY_TEXTURE_PREPARE_THREADS)"
        );
    }
}

/// What a thread made of a texture.
enum Outcome {
    /// The card had no room for a new host copy: nothing made, the slot is
    /// looked at again.
    NoRoom,
    /// Made ready (`None`: the host copy if any, but nothing to stage).
    Done(Option<Arc<Staging>>),
}

fn run() {
    crate::topology::pin_background("victory-texture-prepare");
    if crate::stack_commit::on() {
        crate::stack_commit::commit();
    }
    loop {
        let job = {
            let mut q = queue();
            loop {
                if let Some(job) = q.jobs.pop_front() {
                    break job;
                }
                q = WORK.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        let started = std::time::Instant::now();
        let ([made, made_us], [_, staged_us, ..]) =
            (crate::resource::made_here(), super::staged_here());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&job)))
            .unwrap_or_else(|_| {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    tracing::error!(
                        "a texture prepare thread failed on a texture; it is looked at again"
                    )
                });
                Outcome::NoRoom
            });
        let ([made_now, made_us_now], [_, staged_us_now, ..]) =
            (crate::resource::made_here(), super::staged_here());
        STATS[1].fetch_add(made_now - made, Relaxed);
        STATS[3].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        STATS[4].fetch_add(made_us_now - made_us, Relaxed);
        STATS[5].fetch_add(staged_us_now - staged_us, Relaxed);
        finish(job, outcome);
    }
}

/// Makes `job`'s texture ready: its host copy (only while the card has room
/// for a new one), its staging buffer, and the title's memory detiled into
/// it.
fn work(job: &Job) -> Outcome {
    let resource = job.resource as This;
    if !crate::resource::host_copy_made(resource) && !crate::resource::room_lately() {
        return Outcome::NoRoom;
    }
    let Some(host) = crate::resource::host_copy(resource) else {
        return Outcome::Done(None);
    };
    if !super::title_written(&host.texture) {
        return Outcome::Done(None);
    }
    let Some(staging) = super::prepared_staging(resource, &host.texture, job.first) else {
        return Outcome::Done(None);
    };
    let started = std::time::Instant::now();
    // Not `refresh`: that counts as the submitting thread's "textures".
    staging.refresh_now();
    STATS[6].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
    Outcome::Done(Some(staging))
}

fn finish(job: Job, outcome: Outcome) {
    let done = matches!(outcome, Outcome::Done(Some(_)));
    let replaced = {
        let mut q = queue();
        let slots = q
            .pending
            .remove(&(job.resource, job.first))
            .unwrap_or_default();
        q.unwait(&slots);
        match outcome {
            Outcome::NoRoom => {
                STATS[7].fetch_add(1, Relaxed);
                q.outstanding = q.outstanding.saturating_sub(job.bytes);
                None
            }
            Outcome::Done(staging) => {
                let bytes = if staging.is_some() { job.bytes } else { 0 };
                q.outstanding = q.outstanding.saturating_sub(job.bytes - bytes);
                STATS[0].fetch_add(1, Relaxed);
                STATS[2].fetch_add(bytes, Relaxed);
                let first = staging.as_ref().map_or(job.first, |s| s.first);
                // One from the same mip already: retire it before hinting the
                // deduplicated union so shared slots are neither duplicated in
                // `Ready.slots` nor double-counted in `ready_slots`.
                let old = q.ready.get_mut(&job.resource).and_then(|all| {
                    all.iter()
                        .position(|r| r.first == first)
                        .map(|i| all.swap_remove(i))
                });
                let mut slots = slots;
                if let Some(old) = &old {
                    q.gone(old);
                    for &slot in &old.slots {
                        if !slots.contains(&slot) {
                            slots.push(slot);
                        }
                    }
                }
                q.hint(&slots);
                q.ready.entry(job.resource).or_default().push(Ready {
                    staging,
                    first,
                    bytes,
                    at: crate::host::moment(),
                    slots,
                });
                old
            }
        }
    };
    if done {
        crate::table_null::made(job.resource);
    }
    // Outside the lock: the last of the title's resource may go with it.
    let n = usize::from(replaced.is_some());
    super::prepare_fenced::let_go(replaced, n, false);
    crate::resource::let_go_kept(job.resource as This);
}

thread_local! {
    /// The slot whose upload the pass records now ([`serving`]).
    static SERVING: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// The pass records the upload of slot `slot` now (`None`: no longer): what
/// [`take_ready`] gives it serves that slot.
pub(crate) fn serving(slot: Option<u32>) {
    SERVING.set(slot);
}

/// The staging state made ready for title texture `resource` that serves
/// from mip `first` on (for [`super::staging`], which puts it in the table).
/// It stays made ready while a slot that waited for it is not served yet: a
/// slot of the same texture may take it first, in the very pass that handed
/// it out, and the one that waited would hand it out anew (the host copy
/// has the contents by then: its upload records no copy, only the check at
/// submission). `on_demand`: whether the host copy is made level by level
/// now; one made the other way is not taken.
pub(crate) fn take_ready(resource: This, first: u32, on_demand: bool) -> Option<Arc<Staging>> {
    if threads() == 0 {
        return None;
    }
    let (staging, gone) = queue().take(
        resource as usize,
        first,
        SERVING.get(),
        crate::texture::take_by_table::on(),
        (prune_safe() == 1).then(crate::host::moment),
    )?;
    let n = gone.len();
    super::prepare_fenced::let_go(gone, n, false);
    let staging = staging?;
    if staging.on_demand != on_demand {
        return None;
    }
    // Read while not all of its memory was mapped: the submission reads it
    // again (a partly mapped texture is otherwise read every 30 frames, and
    // what arrived since the thread read it would wait for that).
    staging
        .last
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .partial = None;
    staging.used.store(crate::host::moment(), Relaxed);
    Some(staging)
}

/// Slot `slot` of title texture `resource` was served another way (by the
/// upload another slot recorded in the same pass): it waits for nothing made
/// ready, and what no slot waits for any more goes.
pub(crate) fn served(slot: u32, resource: This) {
    if threads() == 0 {
        return;
    }
    let gone = {
        let mut q = queue();
        let key = resource as usize;
        let Some(all) = q.ready.get_mut(&key) else {
            return;
        };
        let mut served = 0;
        for ready in all.iter_mut() {
            if let Some(at) = ready.slots.iter().position(|&s| s == slot) {
                ready.slots.swap_remove(at);
                served += 1;
            }
        }
        if served == 0 {
            return;
        }
        let mut gone = Vec::new();
        let mut i = 0;
        while i < all.len() {
            if all[i].slots.is_empty() {
                gone.push(all.swap_remove(i));
            } else {
                i += 1;
            }
        }
        if all.is_empty() {
            q.ready.remove(&key);
        }
        for _ in 0..served {
            q.unhint(&[slot]);
        }
        for ready in &gone {
            q.gone(ready);
        }
        gone
    };
    let n = gone.len();
    super::prepare_fenced::let_go(gone, n, false);
}

/// The title released `resource`: what was made ready for it goes.
pub(crate) fn forget(resource: usize) {
    if threads() == 0 {
        return;
    }
    let gone = {
        let mut q = queue();
        let gone = q.ready.remove(&resource).unwrap_or_default();
        for ready in &gone {
            q.gone(ready);
        }
        gone
    };
    let n = gone.len();
    super::prepare_fenced::let_go(gone, n, false);
}

/// Lets go of what was made ready two seconds and two frames ago and not
/// taken (its slot was written again, or is no longer due); with
/// `VICTORY_PREPARE_PRUNE_SAFE=1`, taken last that long ago. Called by the
/// pass before it looks at the slots.
pub(crate) fn prune() {
    if threads() == 0 {
        return;
    }
    super::prepare_fenced::tick();
    let logs = crate::flicker_catch::on() || prune_safe() != 0 || super::prepare_fenced::on();
    let (gone, risky) = {
        let mut q = queue();
        if q.ready.is_empty() {
            return;
        }
        // Under the queue's lock: a texture the title releases leaves the
        // queue first (`forget`), so the ones named here are alive.
        q.stale(
            |at| crate::host::long_ago(at, 2000, 2),
            |resource, first| logs && filled_unstaged(resource, first),
        )
    };
    STATS[8].fetch_add(gone.len() as u64, Relaxed);
    if gone.is_empty() {
        return;
    }
    crate::flicker_catch::periodic::ran(crate::flicker_catch::periodic::Task::PrepPruned);
    let line = logs.then(|| prune_line(&gone, risky));
    let n = gone.len();
    let here_us = super::prepare_fenced::let_go(gone, n, false);
    if let Some(line) = line {
        tracing::info!(
            "{line}; let go here in {here_us} us (VICTORY_PREPARE_FENCED={})",
            u8::from(super::prepare_fenced::on())
        );
    }
}

/// Whether title texture `resource`'s host copy holds an upload from mip
/// `first` on while it has no staging buffer that serves from there: the
/// next table that reads it would leave its slot null to hand it out anew
/// (without `VICTORY_PREPARE_PRUNE_SAFE`).
fn filled_unstaged(resource: usize, first: u32) -> bool {
    let resource = resource as This;
    crate::resource::staged_first(resource) > first
        && crate::resource::host_copy_uploaded_from(resource).is_some_and(|f| f <= first)
}

impl Queue {
    /// [`prune`]'s look: takes out what `old` says was made ready (or, with
    /// `VICTORY_PREPARE_PRUNE_SAFE=1`, taken) long enough ago, and how many
    /// of those `risky` names (by resource and first mip).
    fn stale(
        &mut self,
        old: impl Fn(u64) -> bool,
        risky: impl Fn(usize, u32) -> bool,
    ) -> (Vec<Ready>, usize) {
        let mut stale = Vec::new();
        let mut risks = 0;
        for (&resource, all) in self.ready.iter_mut() {
            let mut i = 0;
            while i < all.len() {
                if old(all[i].at) {
                    let ready = all.swap_remove(i);
                    crate::table_null::pruned(resource);
                    risks += usize::from(risky(resource, ready.first));
                    stale.push(ready);
                } else {
                    i += 1;
                }
            }
        }
        self.ready.retain(|_, all| !all.is_empty());
        for ready in &stale {
            self.gone(ready);
        }
        (stale, risks)
    }
}

/// The `prepare prune:` line: what one prune let go, and how much of it was
/// still referenced (a staging buffer the table of staging buffers or a
/// list held too: it stays alive, only the queue's entry goes).
fn prune_line(gone: &[Ready], risky: usize) -> String {
    let staged: Vec<&Arc<Staging>> = gone.iter().filter_map(|r| r.staging.as_ref()).collect();
    let referenced = staged.iter().filter(|s| Arc::strong_count(s) > 1).count();
    let bytes: u64 = staged.iter().map(|s| s.bytes).sum();
    let freed: u64 = staged
        .iter()
        .filter(|s| Arc::strong_count(s) == 1)
        .map(|s| s.bytes)
        .sum();
    let waited = gone.iter().filter(|r| !r.slots.is_empty()).count();
    let slots: usize = gone.iter().map(|r| r.slots.len()).sum();
    format!(
        "prepare prune: {} entries let go ({} with a staging buffer, {} KiB; {} KiB freed), \
         still referenced={referenced} (staging held by the table or a list), with slots waiting={waited} ({slots} slots), \
         host copy filled and unstaged={risky} (a table reading one next leaves its slot null unless VICTORY_PREPARE_PRUNE_SAFE); \
         VICTORY_PREPARE_PRUNE_SAFE={}",
        gone.len(),
        staged.len(),
        bytes >> 10,
        freed >> 10,
        prune_safe()
    )
}

/// For the frame line: textures handed out and not done, made ready and not
/// taken, and the bytes of both.
pub(crate) fn census() -> [u64; 3] {
    let q = queue();
    [
        q.pending.len() as u64,
        q.ready.values().map(|all| all.len() as u64).sum(),
        q.outstanding,
    ]
}

impl Queue {
    /// (bootfast) Whether a texture of `resource` from mip `first` on may be
    /// made ready ahead ([`make_ready_ahead`]): none is handed out for it
    /// (from that mip or a larger one), and none made ready serves it.
    fn free_ahead(&self, resource: usize, first: u32) -> bool {
        !(0..=first).any(|f| self.pending.contains_key(&(resource, f)))
            && !self
                .ready
                .get(&resource)
                .is_some_and(|all| all.iter().any(|r| r.first <= first))
    }

    /// (bootfast) Files `ready`, which no slot waits for: the first
    /// [`take_ready`] of its texture takes it, or [`prune`] lets it go.
    fn put_ahead(&mut self, resource: usize, ready: Ready) {
        self.ready.entry(resource).or_default().push(ready);
    }
}

/// (bootfast, `VICTORY_BAKE_STAGE_AHEAD=1`) For a bake draw's pass, on one
/// of `crate::bake_ahead`'s threads: makes title texture `resource` ready
/// from mip `first` on as a prepare thread does ([`work`]: host copy while
/// the card has room, staging buffer, the title's memory detiled into it),
/// and files it for the pass, whose `staging` takes it ([`take_ready`]) and
/// records its upload in the list before the draw, as before. Nothing is
/// handed out, so no slot waits for it and the pass never passes a slot
/// over for it. Needs the prepare threads' table
/// (`VICTORY_TEXTURE_PREPARE_THREADS`): without it, `false` and nothing
/// done. The bytes of the staging buffer made ready, if one was.
pub(crate) fn make_ready_ahead(resource: This, first: u32) -> Option<u64> {
    if threads() == 0 || !queue().free_ahead(resource as usize, first) {
        return None;
    }
    let job = Job {
        resource: resource as usize,
        first,
        bytes: 0,
    };
    let Outcome::Done(Some(staging)) = work(&job) else {
        return None;
    };
    let bytes = staging.bytes;
    let ready = Ready {
        first: staging.first,
        staging: Some(staging),
        bytes: 0,
        at: crate::host::moment(),
        slots: Vec::new(),
    };
    let mut q = queue();
    if !q.free_ahead(job.resource, first) {
        // A pass handed it out meanwhile: its thread makes it ready.
        drop(q);
        drop(ready);
        return None;
    }
    q.put_ahead(job.resource, ready);
    Some(bytes)
}

/// Until nothing is handed out and not done, at most `most`: whether that
/// came (tests).
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn wait_idle(most: std::time::Duration) -> bool {
    let until = std::time::Instant::now() + most;
    loop {
        {
            let q = queue();
            if q.jobs.is_empty() && q.pending.is_empty() {
                return true;
            }
        }
        if std::time::Instant::now() >= until {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `prioritize_due` moves ready slots to the front, skips waiting slots,
    /// preserves relative order inside each group, and is a no-op when idle.
    #[test]
    fn prioritize_due_puts_ready_first_and_skips_waiting() {
        let mut q = Queue::default();
        let mut due = vec![(1, 10), (2, 11), (3, 12), (4, 13), (5, 14)];
        assert_eq!(q.prioritize_due(&mut due), (0, 0));
        assert_eq!(due, [(1, 10), (2, 11), (3, 12), (4, 13), (5, 14)]);

        q.wait(2);
        q.wait(4);
        q.hint(&[3, 5]);
        // Slot 2 is both waiting and ready: waiting wins (still pending newer work).
        q.hint(&[2]);
        let (ready_count, skipped) = q.prioritize_due(&mut due);
        assert_eq!(ready_count, 2);
        assert_eq!(skipped, 2);
        assert_eq!(due, [(3, 12), (5, 14), (1, 10)]);
    }

    fn ready(q: &mut Queue, key: usize, first: u32, bytes: u64, slots: &[u32]) {
        q.outstanding += bytes;
        q.hint(slots);
        q.ready.entry(key).or_default().push(Ready {
            staging: None,
            first,
            bytes,
            at: 0,
            slots: slots.to_vec(),
        });
    }

    /// A take by a draw's table (no slot served) leaves the entry in the
    /// queue, its bytes out, until the prune: as before, with the switch off.
    #[test]
    fn a_table_take_keeps_the_entry_with_the_switch_off() {
        let mut q = Queue::default();
        ready(&mut q, 9, 0, 100, &[7]);
        let (_, gone) = q.take(9, 0, None, false, None).unwrap();
        assert!(gone.is_empty());
        assert_eq!(q.outstanding, 100);
        assert!(q.ready_slots.contains_key(&7));
    }

    /// With it on, the take retires the entry: its bytes and its slot's
    /// hint leave at once; other entries of the texture stay.
    #[test]
    fn a_table_take_retires_the_entry_with_the_switch_on() {
        let mut q = Queue::default();
        ready(&mut q, 9, 0, 100, &[7]);
        ready(&mut q, 9, 2, 30, &[8]);
        let (_, gone) = q.take(9, 1, None, true, None).unwrap();
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].first, 0);
        assert_eq!(q.outstanding, 30);
        assert!(!q.ready_slots.contains_key(&7));
        assert!(q.ready_slots.contains_key(&8));
        assert_eq!(q.ready[&9].len(), 1);
    }

    /// A take that serves a slot is the same either way: only that slot.
    #[test]
    fn a_served_take_is_unchanged_by_the_switch() {
        for by_table in [false, true] {
            let mut q = Queue::default();
            ready(&mut q, 9, 0, 100, &[7, 8]);
            let (_, gone) = q.take(9, 0, Some(7), by_table, None).unwrap();
            assert!(gone.is_empty());
            assert_eq!(q.outstanding, 100);
            assert!(!q.ready_slots.contains_key(&7));
            assert!(q.ready_slots.contains_key(&8));
            let (_, gone) = q.take(9, 0, Some(8), by_table, None).unwrap();
            assert_eq!(gone.len(), 1);
            assert_eq!(q.outstanding, 0);
            assert!(q.ready.is_empty());
        }
    }

    /// Nothing for the texture, or nothing from that mip on: no take.
    #[test]
    fn no_take_without_a_ready_entry_that_serves() {
        let mut q = Queue::default();
        assert!(q.take(9, 0, None, true, None).is_none());
        ready(&mut q, 9, 3, 10, &[1]);
        assert!(q.take(9, 2, None, true, None).is_none());
        assert_eq!(q.outstanding, 10);
    }

    /// (texflicker, #96) The cycle behind the diner's dark frames: the pass
    /// handed a texture out for heap slot 7, and tables read it every frame
    /// as slot 8 (the pass let go of its staging buffer after its upload, so
    /// every table asks the queue). Each take serves no slot of the entry,
    /// which stays. Without the switch the prune let it go two seconds after
    /// it was made, taken or not, and the next table found nothing held
    /// (`crate::table_defer` then left its slot null and handed it out anew);
    /// with it, an entry taken within the two seconds stays.
    #[test]
    fn an_entry_tables_take_every_frame_survives_the_prune_only_when_safe() {
        const TWO_SECONDS: u64 = 25;
        for safe in [false, true] {
            let mut q = Queue::default();
            q.outstanding += 100;
            q.hint(&[7]);
            q.ready.entry(9).or_default().push(Ready {
                staging: None,
                first: 0,
                bytes: 100,
                at: 10,
                slots: vec![7],
            });
            for now in 11..=40 {
                let (_, gone) = q.take(9, 0, Some(8), true, safe.then_some(now)).unwrap();
                assert!(gone.is_empty());
            }
            let (gone, _) = q.stale(|at| 40 - at >= TWO_SECONDS, |_, _| false);
            if safe {
                assert!(gone.is_empty());
                assert_eq!(q.held(9, 0), Held::Ready);
                assert_eq!(q.outstanding, 100);
            } else {
                assert_eq!(gone.len(), 1);
                assert_eq!(q.held(9, 0), Held::Nothing);
                assert_eq!(q.outstanding, 0);
                assert!(q.ready_slots.is_empty());
            }
            // Not taken for two seconds: let go either way.
            let (gone, _) = q.stale(|at| 80 - at >= TWO_SECONDS, |_, _| false);
            assert_eq!(gone.len(), usize::from(safe));
            assert!(q.ready.is_empty());
        }
    }

    /// The prune counts what it let go whose texture's host copy is filled
    /// and unstaged (as its look says), by resource and first mip.
    #[test]
    fn the_prune_counts_the_risky_entries() {
        let mut q = Queue::default();
        ready(&mut q, 9, 0, 10, &[1]);
        ready(&mut q, 9, 2, 10, &[2]);
        ready(&mut q, 5, 0, 10, &[]);
        let (gone, risky) = q.stale(|_| true, |resource, first| resource == 9 && first == 2);
        assert_eq!((gone.len(), risky), (3, 1));
        assert!(prune_line(&gone, risky).starts_with("prepare prune: 3 entries let go"));
        assert_eq!(q.outstanding, 0);
    }

    #[test]
    fn prune_safe_is_live_and_off_unless_set() {
        assert_eq!(super::prune_safe_level(None), 0);
        assert_eq!(super::prune_safe_level(Some("1")), 1);
        assert_eq!(super::prune_safe_level(Some("7")), 2);
        assert_eq!(super::prune_safe_level(Some("x")), 0);
        victory_core::settings::check_live("VICTORY_PREPARE_PRUNE_SAFE", "1", super::prune_safe, 0, 1);
    }

    /// An urgent look (a table's texture) ignores the frame's bytes and is
    /// held only by what is out, at twice the budget.
    #[test]
    fn urgent_looks_pass_the_frame_budget_but_not_twice_the_outstanding() {
        assert!(over_budget(100, 0, 100, false));
        assert!(over_budget(0, 100, 100, false));
        assert!(!over_budget(99, 99, 100, false));
        assert!(!over_budget(500, 199, 100, true));
        assert!(over_budget(0, 200, 100, true));
    }

    /// (bootfast) A texture is made ready ahead only when nothing is handed
    /// out for it from that mip or a larger one, and nothing made ready
    /// serves it; what is filed ahead serves the mip it was made from on.
    #[test]
    fn ready_ahead_only_when_nothing_serves_it() {
        let mut q = Queue::default();
        assert!(q.free_ahead(7, 2));
        q.pending.insert((7, 1), vec![3]);
        assert!(!q.free_ahead(7, 2));
        assert!(q.free_ahead(7, 0));
        assert!(q.free_ahead(8, 2));
        q.put_ahead(
            8,
            Ready {
                staging: None,
                first: 1,
                bytes: 0,
                at: 0,
                slots: Vec::new(),
            },
        );
        assert!(!q.free_ahead(8, 1));
        assert!(!q.free_ahead(8, 3));
        assert!(q.free_ahead(8, 0));
        // Filed ahead, it is no slot's: nothing is hinted to the pass.
        assert!(q.ready_slots.is_empty());
        assert_eq!(q.outstanding, 0);
    }

    /// The switch is off unless set: the pass makes its textures itself.
    #[test]
    fn prepare_threads_are_off_unless_set() {
        if std::env::var_os("VICTORY_TEXTURE_PREPARE_THREADS").is_none() {
            assert_eq!(super::threads(), 0);
            assert!(super::take_ready(std::ptr::null(), 0, false).is_none());
        }
    }

    const DEDUP_CHILD: &str = "VICTORY_PREPARE_DEDUP_CHILD";
    const DEDUP_TEST: &str =
        "texture::prepare::tests::replacing_ready_deduplicates_slots_and_releases_staging";

    /// Reproduces two streaming leaks in `texture::prepare`:
    /// 1. When `finish()` replaces an existing `Ready` entry for `(resource, first)`
    ///    and shares a waiting slot with it (e.g. a second prepare resolving to the
    ///    same `staging.first`), appending `old.slots` without deduplicating left the
    ///    slot twice in `Ready.slots` and `ready_slots[slot] == 2`. Serving the slot
    ///    only removed one copy, pinning the `Arc<Staging>` upload buffer, keeping
    ///    `q.outstanding` inflated, and leaving `prepare::slot(slot) == Slot::Ready`
    ///    for 2+ seconds until `prune()`.
    /// 2. When a slot waited on two pending jobs (e.g. descriptor slot rewritten
    ///    while an earlier job was in flight), finishing the first job cleared
    ///    `q.waiting`, reporting `Slot::Other` while the second job was still pending
    ///    and allowing `look()` to push a duplicate slot into `q.pending`.
    #[test]
    fn replacing_ready_deduplicates_slots_and_releases_staging() {
        if std::env::var_os(DEDUP_CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([DEDUP_TEST, "--exact", "--nocapture", "--test-threads=1"])
                .env(DEDUP_CHILD, "1")
                .env("VICTORY_TEXTURE_PREPARE_THREADS", "1")
                .output()
                .unwrap();
            print!("{}", String::from_utf8_lossy(&output.stdout));
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            assert!(
                output.status.success(),
                "prepare dedup probe failed: {}",
                output.status
            );
            return;
        }
        assert_eq!(super::threads(), 1);
        let Some(_host) = crate::host::host() else {
            eprintln!("Skipped prepare dedup probe: no D3D12 device");
            return;
        };
        if let Some((name, vram)) = crate::host::adapter_summary() {
            eprintln!("adapter: {name} ({} MiB VRAM)", vram >> 20);
        }

        // Create a real 1024x1024 RGBA8 committed title texture (4 MiB) so we
        // measure real `Arc<Staging>` upload-buffer retention and `STAGED` RAM.
        const SIDE: u32 = 1024;
        const RGBA8: u32 = 28;
        const DESC_WORDS: [u32; 14] = [
            3,
            0,
            0,
            0,
            SIDE,
            0,
            SIDE,
            (1 << 16) | 1,
            RGBA8,
            1,
            0,
            0x105,
            0,
            0,
        ];
        const HEAP_WORDS: [u32; 5] = [1, 0, 0, 1, 1];
        let resource_iid = crate::object::guid(
            0x696442be,
            0xa72e,
            0x4059,
            [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
        );
        // SAFETY: plain integer structs with matching `#[repr(C)]` layouts.
        let desc: crate::resource::ResourceDesc = unsafe { std::mem::transmute(DESC_WORDS) };
        let props: crate::resource::HeapProperties = unsafe { std::mem::transmute(HEAP_WORDS) };
        let mut res: This = std::ptr::null();
        let hr = crate::resource::create_committed(
            std::ptr::null(),
            &props,
            0,
            &desc,
            0,
            std::ptr::null(),
            &resource_iid,
            &mut res,
        );
        assert_eq!(hr, crate::object::S_OK, "create_committed failed");
        let host_copy = crate::resource::host_copy(res).expect("host texture copy");

        let staged_count_before = super::super::STAGED[0].load(Relaxed);
        let staged_bytes_before = super::super::STAGED[1].load(Relaxed);

        let staging_a =
            super::super::prepared_staging(res, &host_copy.texture, 0).expect("staging_a");
        let staging_b =
            super::super::prepared_staging(res, &host_copy.texture, 0).expect("staging_b");
        let stage_bytes = staging_b.bytes;
        assert!(
            stage_bytes >= 4 << 20,
            "expected >= 4 MiB staging buffer, got {stage_bytes}"
        );
        let weak_b = Arc::downgrade(&staging_b);

        // Scenario 1: Job 1 finishes for slot 10, then Job 2 replaces the same
        // `(res, first=0)` entry for overlapping slots `[10, 11]`.
        let key = res as usize;
        crate::resource::keep_alive(res);
        {
            let mut q = queue();
            q.outstanding += stage_bytes;
            q.pending.insert((key, 0), vec![10]);
            q.wait(10);
        }
        finish(
            Job {
                resource: key,
                first: 0,
                bytes: stage_bytes,
            },
            Outcome::Done(Some(staging_a)),
        );

        crate::resource::keep_alive(res);
        {
            let mut q = queue();
            q.outstanding += stage_bytes;
            q.pending.insert((key, 0), vec![10, 11]);
            q.wait(10);
            q.wait(11);
        }
        finish(
            Job {
                resource: key,
                first: 0,
                bytes: stage_bytes,
            },
            Outcome::Done(Some(staging_b)),
        );

        // Serve slot 10 (via `take_ready`) and slot 11 (via `served`).
        serving(Some(10));
        let taken = take_ready(res, 0, false).expect("take_ready must return staging_b");
        serving(None);
        drop(taken);
        served(11, res);

        let census_after = census();
        let slot10_after = slot(10);
        let slot11_after = slot(11);
        let weak_strong_after = weak_b.strong_count();
        let leaked_staged_count = super::super::STAGED[0]
            .load(Relaxed)
            .saturating_sub(staged_count_before);
        let leaked_staged_bytes = super::super::STAGED[1]
            .load(Relaxed)
            .saturating_sub(staged_bytes_before);

        eprintln!(
            "prepare_dedup metrics: census={census_after:?} slot10={slot10_after:?} slot11={slot11_after:?} \
             staging_strong={weak_strong_after} leaked_staged_count={leaked_staged_count} \
             leaked_staged_bytes={leaked_staged_bytes}"
        );

        assert_eq!(
            census_after,
            [0, 0, 0],
            "ready entry and outstanding budget must drain after serving all slots"
        );
        assert_eq!(
            slot10_after,
            Slot::Other,
            "slot 10 must no longer report Slot::Ready after being served"
        );
        assert_eq!(
            slot11_after,
            Slot::Other,
            "slot 11 must no longer report Slot::Ready after being served"
        );
        assert_eq!(
            weak_strong_after, 0,
            "Arc<Staging> upload buffer must drop immediately when all slots are served"
        );
        assert_eq!(
            leaked_staged_count, 0,
            "STAGED buffer count must return to baseline"
        );
        assert_eq!(
            leaked_staged_bytes, 0,
            "STAGED bytes must return to baseline"
        );

        // Scenario 2: Slot 20 waits on two pending jobs (`res` at mip 1 and mip 0).
        // Finishing the first job must keep slot 20 in `Slot::Waiting` until the
        // second job also finishes, and attaching the same slot twice via `look`
        // must never create duplicate slot entries.
        crate::resource::keep_alive(res);
        crate::resource::keep_alive(res);
        {
            let mut q = queue();
            q.outstanding += stage_bytes * 2;
            q.pending.insert((key, 1), vec![20]);
            q.pending.insert((key, 0), vec![20]);
            q.wait(20);
            q.wait(20);
        }
        finish(
            Job {
                resource: key,
                first: 1,
                bytes: stage_bytes,
            },
            Outcome::NoRoom,
        );
        assert_eq!(
            slot(20),
            Slot::Waiting,
            "slot 20 is still pending on (res, 0) and must remain Slot::Waiting"
        );
        // Re-running `look()` for slot 20 on `(res, 0)` while pending must not duplicate slot 20.
        let recorded = HashSet::new();
        assert_eq!(look(20, res, 0, &recorded, 64 << 20, 1), Look::Wait);
        let staging_c =
            super::super::prepared_staging(res, &host_copy.texture, 0).expect("staging_c");
        let weak_c = Arc::downgrade(&staging_c);
        finish(
            Job {
                resource: key,
                first: 0,
                bytes: stage_bytes,
            },
            Outcome::Done(Some(staging_c)),
        );
        served(20, res);
        assert_eq!(
            census(),
            [0, 0, 0],
            "second scenario must leave queue completely drained"
        );
        assert_eq!(
            slot(20),
            Slot::Other,
            "slot 20 must return to Slot::Other after served()"
        );
        assert_eq!(
            weak_c.strong_count(),
            0,
            "staging_c must drop immediately after slot 20 is served"
        );

        // Release synthetic title texture.
        super::super::forget(key);
        crate::resource::let_go_kept(res);
    }
}
