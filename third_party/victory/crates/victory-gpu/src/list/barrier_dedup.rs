//! (portscout, deepscout b #19) `VICTORY_BARRIER_DEDUP=1`: the transitions
//! one title `ResourceBarrier` call makes on the same host texture are
//! collapsed into one.
//!
//! Ported by hand from LooperSalty/AnyXboxX `75296ed` (Mauro, GPL-3.0).
//! The list tracks whole textures (`HostList::transition`), so a call that
//! moves two subresources of one texture (a mip chain: level 0 to shader
//! read, level 1 to unordered access) comes out as two
//! `ALL_SUBRESOURCES` transitions of that texture in one host call, the
//! second starting where the first ends. D3D12 forbids that
//! (`RESOURCE_BARRIER_DUPLICATE_SUBRESOURCE_TRANSITIONS`); what a driver
//! does with it is its own. With the switch they become one transition
//! from the first's before to the last's after, at the first's place; a
//! round trip that ends where it began is dropped, or becomes an
//! unordered access barrier on the texture when it began and ends in
//! unordered access (the writes before still finish before the reads
//! after), unless the call already has such a barrier on it or on every
//! resource. The texture ends in the same state as without the switch.
//!
//! `VICTORY_BARRIER_DEDUP=count` changes nothing and counts the calls that
//! hold duplicates. Line (both modes, at powers of two of such calls):
//! `title barriers: duplicate transitions`.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::Barrier;

/// `D3D12_RESOURCE_STATE_UNORDERED_ACCESS`.
const UNORDERED_ACCESS: u32 = 0x8;
const TRANSITION: u32 = 0;
const UAV: u32 = 2;

/// 0 off (default), 1 collapse, 2 count only.
fn mode() -> u8 {
    static MODE: OnceLock<u8> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_BARRIER_DEDUP").as_deref() {
        Ok("1") => {
            tracing::info!(
                "duplicate transitions of one texture in a title barrier call are collapsed (VICTORY_BARRIER_DEDUP; LooperSalty 75296ed)"
            );
            1
        }
        Ok("count") => 2,
        _ => 0,
    })
}

/// What a collapse did: transitions merged into an earlier one, round
/// trips dropped, round trips made an unordered access barrier.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Done {
    merged: u64,
    dropped: u64,
    to_uav: u64,
}

/// The host barriers of one call with duplicate transitions collapsed.
fn collapse(host: &mut Vec<Barrier>) -> Done {
    let mut done = Done::default();
    let mut out: Vec<Barrier> = Vec::with_capacity(host.len());
    for b in host.drain(..) {
        if b.kind == TRANSITION
            && let Some(earlier) = out
                .iter_mut()
                .find(|h| h.kind == TRANSITION && h.first == b.first)
        {
            earlier.last = b.last;
            done.merged += 1;
            continue;
        }
        out.push(b);
    }
    if done.merged > 0 {
        let mut i = 0;
        while i < out.len() {
            let b = out[i];
            if b.kind == TRANSITION && (b.second >> 32) as u32 == b.last as u32 {
                let uav_already = out
                    .iter()
                    .any(|h| h.kind == UAV && (h.first.is_null() || h.first == b.first));
                if b.last as u32 == UNORDERED_ACCESS && !uav_already {
                    out[i] = Barrier {
                        kind: UAV,
                        flags: 0,
                        first: b.first,
                        second: 0,
                        last: 0,
                    };
                    done.to_uav += 1;
                    i += 1;
                } else {
                    out.remove(i);
                    done.dropped += 1;
                }
                continue;
            }
            i += 1;
        }
    }
    *host = out;
    done
}

/// Whether two transitions in `host` name the same texture.
fn has_duplicates(host: &[Barrier]) -> bool {
    host.iter().enumerate().any(|(i, b)| {
        b.kind == TRANSITION
            && host[..i]
                .iter()
                .any(|h| h.kind == TRANSITION && h.first == b.first)
    })
}

static CALLS: AtomicU64 = AtomicU64::new(0);
static MERGED: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static TO_UAV: AtomicU64 = AtomicU64::new(0);

/// The switch on the host barriers of one title call (in place).
pub(super) fn apply(host: &mut Vec<Barrier>) {
    let mode = mode();
    if mode == 0 || host.len() < 2 {
        return;
    }
    let done = if mode == 1 {
        collapse(host)
    } else if has_duplicates(host) {
        let mut copy = host.clone();
        collapse(&mut copy)
    } else {
        Done::default()
    };
    if done.merged == 0 {
        return;
    }
    let calls = CALLS.fetch_add(1, Relaxed) + 1;
    MERGED.fetch_add(done.merged, Relaxed);
    DROPPED.fetch_add(done.dropped, Relaxed);
    TO_UAV.fetch_add(done.to_uav, Relaxed);
    if calls.is_power_of_two() {
        tracing::info!(
            collapsed = mode == 1,
            calls,
            merged = MERGED.load(Relaxed),
            round_trips_dropped = DROPPED.load(Relaxed),
            round_trips_to_uav = TO_UAV.load(Relaxed),
            "title barriers: duplicate transitions of one texture in a call (VICTORY_BARRIER_DEDUP)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::This;

    fn t(texture: usize, before: u32, after: u32) -> Barrier {
        Barrier {
            kind: TRANSITION,
            flags: 0,
            first: texture as This,
            second: 0xffff_ffff | (u64::from(before) << 32),
            last: u64::from(after),
        }
    }

    fn uav(texture: usize) -> Barrier {
        Barrier {
            kind: UAV,
            flags: 0,
            first: texture as This,
            second: 0,
            last: 0,
        }
    }

    fn view(b: &[Barrier]) -> Vec<(u32, usize, u32, u32)> {
        b.iter()
            .map(|b| {
                (
                    b.kind,
                    b.first as usize,
                    (b.second >> 32) as u32,
                    b.last as u32,
                )
            })
            .collect()
    }

    #[test]
    fn two_transitions_of_one_texture_become_one() {
        let mut host = vec![t(0x10, 0x4, 0x80), t(0x10, 0x80, 0x8)];
        assert!(has_duplicates(&host));
        let done = collapse(&mut host);
        assert_eq!(
            done,
            Done {
                merged: 1,
                dropped: 0,
                to_uav: 0
            }
        );
        assert_eq!(view(&host), [(TRANSITION, 0x10, 0x4, 0x8)]);
    }

    #[test]
    fn interleaved_textures_collapse_each_on_its_own() {
        let mut host = vec![t(0x10, 0x4, 0x80), t(0x20, 0x4, 0x8), t(0x10, 0x80, 0x8)];
        collapse(&mut host);
        assert_eq!(
            view(&host),
            [(TRANSITION, 0x10, 0x4, 0x8), (TRANSITION, 0x20, 0x4, 0x8)]
        );
    }

    #[test]
    fn a_round_trip_through_unordered_access_keeps_its_wait() {
        let mut host = vec![t(0x10, 0x8, 0x80), t(0x10, 0x80, 0x8)];
        let done = collapse(&mut host);
        assert_eq!(
            done,
            Done {
                merged: 1,
                dropped: 0,
                to_uav: 1
            }
        );
        assert_eq!(view(&host), [(UAV, 0x10, 0, 0)]);
        // Not twice when the call already waits on it or on everything.
        let mut host = vec![t(0x10, 0x8, 0x80), uav(0x10), t(0x10, 0x80, 0x8)];
        collapse(&mut host);
        assert_eq!(view(&host), [(UAV, 0x10, 0, 0)]);
        let mut host = vec![t(0x10, 0x8, 0x80), t(0x10, 0x80, 0x8), uav(0)];
        collapse(&mut host);
        assert_eq!(view(&host), [(UAV, 0, 0, 0)]);
    }

    #[test]
    fn any_other_round_trip_is_dropped() {
        let mut host = vec![t(0x10, 0x80, 0x4), t(0x10, 0x4, 0x80), t(0x20, 0x4, 0x80)];
        let done = collapse(&mut host);
        assert_eq!(
            done,
            Done {
                merged: 1,
                dropped: 1,
                to_uav: 0
            }
        );
        assert_eq!(view(&host), [(TRANSITION, 0x20, 0x4, 0x80)]);
    }

    #[test]
    fn a_call_without_duplicates_is_left_alone() {
        let before = vec![t(0x10, 0x4, 0x80), uav(0x20), t(0x20, 0x8, 0x80), uav(0)];
        let mut host = before.clone();
        assert!(!has_duplicates(&host));
        assert_eq!(collapse(&mut host), Done::default());
        assert_eq!(view(&host), view(&before));
    }
}
