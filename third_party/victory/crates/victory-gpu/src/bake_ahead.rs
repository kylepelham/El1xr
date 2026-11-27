//! (bootfast) A bake draw's texture pass makes the host copies of its due
//! textures on threads of its own first (`VICTORY_BAKE_PARALLEL=N`, N of 2
//! to 32; unset, 0 or 1: off, as before).
//!
//! While the title loads it bakes its characters' skin and decorations with
//! a pixel shader that picks its sources by index, and before every such
//! draw the pass over the heap's texture views uploads what is due, there
//! and then, on the thread that records the frame (`State::early_textures`,
//! up to 10 s a frame; not handed to the prepare threads, as what a bake
//! reads must be there now). Of a fast boot's 38-46 s to the first world
//! frame that pass took 12.6-18.3 s ("early/bake textures" in the frame
//! phases), with the process at 1-2 cores busy; most of a texture's time
//! there is making its host copy (a `D3D12` committed texture: 1-3 ms each
//! on the prepare threads at the same moments, thousands of them).
//!
//! With the switch the pass first looks at its due slots, takes each title
//! texture whose host copy is not made, and makes those copies on N
//! threads, within the pass's own time, while the card has room. It then
//! goes on as before: it finds the copies made and stages and records the
//! uploads in the same order. Only the making of host copies moves, which
//! the prepare threads already do off the recording thread for the other
//! passes (`crate::texture::prepare`); nothing is staged or recorded here.
//!
//! With `VICTORY_BAKE_STAGE_AHEAD=1` as well (and the prepare threads'
//! table, `VICTORY_TEXTURE_PREPARE_THREADS`) the threads make each texture
//! ready as a prepare thread does: host copy, staging buffer, the title's
//! memory detiled into it, filed where the pass's `staging` takes it
//! (`crate::texture::prepare::make_ready_ahead`). The pass still records
//! every upload into the list before the draw; the list's submission still
//! reads the texture again if the title wrote it since (write watch,
//! guards), so the upload-before-draw guarantee is unchanged.

use std::collections::HashSet;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use crate::descriptor::View;
use crate::object::This;

/// The threads a bake draw's pass makes host copies on
/// (`VICTORY_BAKE_PARALLEL`, at most 32; 0 when unset or under 2).
pub(crate) fn threads() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| parse_threads(std::env::var("VICTORY_BAKE_PARALLEL").ok().as_deref()))
}

fn parse_threads(value: Option<&str>) -> usize {
    match value.and_then(|v| v.trim().parse::<usize>().ok()) {
        Some(n) if n >= 2 => n.min(32),
        _ => 0,
    }
}

/// Textures to make at least for the threads to start: fewer are left to
/// the pass.
const AT_LEAST: usize = 8;

/// Since the start: passes that made copies here, host copies made,
/// textures looked for (made or not), and the passes' milliseconds here.
pub(crate) static STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The threads that make `n` jobs, `n` of them at most.
fn workers(n: usize, threads: usize) -> usize {
    threads.min(n)
}

/// Runs `make` for each of `jobs` on `threads` threads, each job once,
/// none started after `deadline`: how many returned true.
fn run_all<T: Sync>(jobs: &[T], threads: usize, deadline: Instant, make: impl Fn(&T) -> bool + Sync) -> u64 {
    let next = AtomicUsize::new(0);
    let made = AtomicU64::new(0);
    std::thread::scope(|s| {
        for _ in 0..workers(jobs.len(), threads) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Relaxed);
                    let Some(job) = jobs.get(i) else { break };
                    if Instant::now() >= deadline {
                        break;
                    }
                    if make(job) {
                        made.fetch_add(1, Relaxed);
                    }
                }
            });
        }
    });
    made.load(Relaxed)
}

/// (bootfast) Whether the threads also make the textures ready, as the
/// prepare threads do (staging buffer made, the title's memory detiled into
/// it), for the pass to record their uploads
/// (`VICTORY_BAKE_STAGE_AHEAD=1`, with `VICTORY_BAKE_PARALLEL` and
/// `VICTORY_TEXTURE_PREPARE_THREADS`; off by default).
fn stage_ahead() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BAKE_STAGE_AHEAD").is_some_and(|v| v == "1"))
}

/// The most bytes of staging buffers made ready ahead for one pass: what
/// the pass does not take in its time is let go two seconds later
/// (`crate::texture::prepare::prune`).
const STAGE_AHEAD_MOST: u64 = 512 << 20;

/// Before a bake draw's pass over the heap's due slots `due` of `heap`:
/// the host copies of their title textures not made yet, made on
/// [`threads`] threads within `budget`; with [`stage_ahead`] the textures
/// made ready too.
#[cfg(windows)]
pub(crate) fn host_copies(heap: &crate::descriptor::Heap, due: &[(u32, u64)], budget: Duration) {
    let threads = threads();
    if threads == 0 {
        return;
    }
    let staging = stage_ahead() && crate::texture::prepare::threads() > 0;
    let began = Instant::now();
    let mut seen = HashSet::new();
    let mut todo: Vec<(usize, u32)> = Vec::new();
    for &(index, _) in due {
        let Some(view) = heap.handle(index).and_then(crate::descriptor::view) else {
            continue;
        };
        // A placed texture whose memory is not mapped gets no host copy in
        // the pass either.
        let mut first = 0;
        if let View::Placed(false, resource, desc) = &view {
            first = crate::resource::first_mip(desc);
            let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            let len = crate::resource::placed_bytes_from(resource, first).unwrap_or(1);
            if crate::texture::mapped_bytes(address as *const u8, len as usize) == 0 {
                continue;
            }
        }
        let Some(View::Shader(resource, _)) =
            crate::descriptor::host::placed_texture(&view).or(Some(view))
        else {
            continue;
        };
        let resource = resource as This;
        if resource.is_null()
            || !crate::resource::title_fills(resource)
            || crate::resource::host_refused(resource)
            || !seen.insert((resource as usize, first))
        {
            continue;
        }
        let made = crate::resource::host_copy_made(resource);
        let wanted = if staging {
            !made || crate::resource::staged_first(resource) > first
        } else {
            !made
        };
        if wanted {
            todo.push((resource as usize, first));
        }
    }
    if todo.len() < AT_LEAST {
        return;
    }
    // Kept alive while the threads make them: the title may release one
    // meanwhile (as for the prepare threads).
    for &(r, _) in &todo {
        crate::resource::keep_alive(r as This);
    }
    let deadline = began + budget;
    let staged = AtomicU64::new(0);
    let made = run_all(&todo, threads, deadline, |&(r, first)| {
        let r = r as This;
        if staging && staged.load(Relaxed) < STAGE_AHEAD_MOST {
            match crate::texture::prepare::make_ready_ahead(r, first) {
                Some(bytes) => {
                    staged.fetch_add(bytes, Relaxed);
                    true
                }
                None => false,
            }
        } else {
            !crate::resource::host_copy_made(r)
                && crate::resource::room_lately()
                && crate::resource::host_copy(r).is_some()
        }
    });
    for &(r, _) in &todo {
        crate::resource::let_go_kept(r as This);
    }
    let ms = began.elapsed().as_millis() as u64;
    let passes = STATS[0].fetch_add(1, Relaxed) + 1;
    STATS[1].fetch_add(made, Relaxed);
    STATS[2].fetch_add(todo.len() as u64, Relaxed);
    STATS[3].fetch_add(ms, Relaxed);
    if passes.is_power_of_two() {
        tracing::info!(
            threads,
            staging,
            passes,
            looked_for = todo.len(),
            made,
            staged_mb = staged.load(Relaxed) >> 20,
            ms,
            made_so_far = STATS[1].load(Relaxed),
            ms_so_far = STATS[3].load(Relaxed),
            "a bake draw's host copies (and with VICTORY_BAKE_STAGE_AHEAD its textures made ready) made on threads of their own first (VICTORY_BAKE_PARALLEL)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_off_unless_two_or_more() {
        assert_eq!(parse_threads(None), 0);
        assert_eq!(parse_threads(Some("")), 0);
        assert_eq!(parse_threads(Some("0")), 0);
        assert_eq!(parse_threads(Some("1")), 0);
        assert_eq!(parse_threads(Some("x")), 0);
        assert_eq!(parse_threads(Some(" 8 ")), 8);
        assert_eq!(parse_threads(Some("99")), 32);
    }

    /// Every job is made once, on no more threads than jobs.
    #[test]
    fn each_job_runs_once() {
        let jobs: Vec<usize> = (0..1000).collect();
        let hits: Vec<AtomicU64> = (0..1000).map(|_| AtomicU64::new(0)).collect();
        let deadline = Instant::now() + Duration::from_secs(60);
        let made = run_all(&jobs, 8, deadline, |&j| {
            hits[j].fetch_add(1, Relaxed);
            j % 2 == 0
        });
        assert_eq!(made, 500);
        assert!(hits.iter().all(|h| h.load(Relaxed) == 1));
        assert_eq!(workers(3, 8), 3);
        assert_eq!(workers(100, 8), 8);
    }

    /// Nothing is started past the deadline.
    #[test]
    fn nothing_starts_after_the_deadline() {
        let jobs: Vec<usize> = (0..100).collect();
        let made = run_all(&jobs, 4, Instant::now(), |_| true);
        assert_eq!(made, 0);
    }
}
