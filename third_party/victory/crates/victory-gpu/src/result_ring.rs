//! (entryhitch, #16) `VICTORY_RESULT_RING=1` (Live, off by default): the
//! card compare's result area (`CardWork::differs`, 262,144 dwords) as a
//! ring whose stretches come back as their plans land (idea of Skyfrye's
//! LooperSalty/AnyXboxX `3f7a8c0434` + `5c5d6c9970`; written here for our
//! `card_ranges`, which places ranges one by one and shares slots with
//! `VICTORY_CARD_COMPARE_SHARE`).
//!
//! Off, the area is reused from its start only when no plan is out, so with
//! the relay (nearly always a plan in the air) it only fills, and
//! `Host::card_results_low` makes the submitting thread land every flight
//! (a wait for the card, then every flight's copy-back) each time it is
//! half taken: "lists in the air" on the why-settle line. On, a plan takes
//! one stretch after the newest one's, wrapping to the start, and gives it
//! back in `Host::card_done`; the flights are made to land only when the
//! area is three quarters taken by plans still in the air.
//!
//! The compare stores a flag in every dword of its plan's stretch, and the
//! plan's results are read before it gives the stretch back, so no two
//! plans in the air share a dword either way. A range with no room left is
//! not compared, which fetches it whole, as when the area ran out before.
//!
//! The switch takes effect only when no plan is out: a flip asks for one
//! landing ([`Ring::wants_landing`]), so the two ways never share the area.

use std::collections::VecDeque;

use victory_core::settings::Live;

static ON: Live = Live::flag("VICTORY_RESULT_RING");

pub(crate) fn on() -> bool {
    ON.get_bool()
}

/// The area's dwords taken by plans out, as a ring or the old way.
#[derive(Debug)]
pub(crate) struct Ring {
    /// The area's size, in dwords.
    size: u32,
    /// Whether the area is a ring now (latched while plans are out).
    ring: bool,
    /// Old way: dwords taken since the area was last empty. Ring: where the
    /// next stretch starts if there is room after it.
    head: u32,
    /// Ring: the stretches of plans out (start, end, given back), oldest
    /// first.
    out: VecDeque<(u32, u32, bool)>,
    /// Ring: their dwords not given back.
    used: u32,
}

impl Ring {
    pub(crate) fn new(size: u32) -> Ring {
        Ring {
            size,
            ring: false,
            head: 0,
            out: VecDeque::new(),
            used: 0,
        }
    }

    /// Before a plan's ranges are placed, with `plans_out` plans not done
    /// with: the first dword to place from and the dword limit (absolute)
    /// to stay below. Takes up `want` (the switch) if no plan is out.
    pub(crate) fn room(&mut self, plans_out: u32, want: bool) -> (u32, u32) {
        if plans_out == 0 {
            self.ring = want;
            self.empty();
        }
        if !self.ring {
            return (self.head, self.size);
        }
        let Some(&(oldest, _, _)) = self.out.front() else {
            self.head = 0;
            return (0, self.size);
        };
        if self.head > oldest {
            // Taken: [oldest, head). Free: [head, size) and [0, oldest).
            if self.size - self.head >= oldest {
                (self.head, self.size)
            } else {
                (0, oldest)
            }
        } else {
            // Taken: [oldest, size) and [0, head). Free: [head, oldest).
            (self.head, oldest)
        }
    }

    /// The plan placed its ranges in `[start, end)` (from [`Ring::room`]):
    /// the stretch it gives back with [`Ring::give`].
    pub(crate) fn took(&mut self, start: u32, end: u32) -> (u32, u32) {
        self.head = end;
        if self.ring && end > start {
            self.out.push_back((start, end, false));
            self.used += end - start;
        }
        (start, end)
    }

    /// A plan is done with (its results read). `none_out`: no plan is out
    /// after it, or none can be (no relay): the area is free from its start.
    pub(crate) fn give(&mut self, stretch: (u32, u32), none_out: bool) {
        if none_out {
            self.empty();
            return;
        }
        if !self.ring || stretch.1 <= stretch.0 {
            return;
        }
        if let Some(s) = self.out.iter_mut().find(|s| !s.2 && (s.0, s.1) == stretch) {
            s.2 = true;
            self.used -= stretch.1 - stretch.0;
        }
        while self.out.front().is_some_and(|s| s.2) {
            self.out.pop_front();
        }
        if self.out.is_empty() {
            self.head = 0;
        }
    }

    /// No plan is out: the area is free from its start.
    pub(crate) fn empty(&mut self) {
        self.head = 0;
        self.out.clear();
        self.used = 0;
    }

    /// Whether the flights in the air are to land before the next plan:
    /// the old way when the area is half taken; as a ring when three
    /// quarters are; and once when the switch was flipped, so the next plan
    /// starts the new way.
    pub(crate) fn wants_landing(&self, plans_out: u32, want: bool) -> bool {
        if plans_out == 0 {
            return false;
        }
        if want != self.ring {
            return true;
        }
        if self.ring {
            u64::from(self.used) * 4 > u64::from(self.size) * 3
        } else {
            u64::from(self.head) * 2 > u64::from(self.size)
        }
    }

    #[cfg(test)]
    fn used(&self) -> u32 {
        self.used
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plan taking up to `need` dwords, as `card_ranges` does: as much as
    /// the room allows.
    fn plan(r: &mut Ring, out: u32, want: bool, need: u32) -> (u32, u32) {
        let (start, limit) = r.room(out, want);
        let end = start + need.min(limit - start);
        r.took(start, end)
    }

    #[test]
    fn off_it_is_the_old_counter() {
        let mut r = Ring::new(100);
        assert_eq!(plan(&mut r, 0, false, 30), (0, 30));
        assert_eq!(plan(&mut r, 1, false, 30), (30, 60));
        assert!(r.wants_landing(2, false));
        r.give((0, 30), false);
        assert_eq!(plan(&mut r, 1, false, 50), (60, 100));
        assert_eq!(plan(&mut r, 2, false, 10), (100, 100));
        r.give((30, 60), false);
        r.give((60, 100), false);
        r.give((100, 100), true);
        assert_eq!(plan(&mut r, 0, false, 10), (0, 10));
    }

    #[test]
    fn stretches_come_back_and_wrap() {
        let mut r = Ring::new(100);
        let a = plan(&mut r, 0, true, 40);
        let b = plan(&mut r, 1, true, 40);
        assert_eq!((a, b), ((0, 40), (40, 80)));
        assert!(r.wants_landing(2, true));
        r.give(a, false);
        assert!(!r.wants_landing(1, true));
        // 20 left after, 40 before: the start.
        let c = plan(&mut r, 1, true, 50);
        assert_eq!(c, (0, 40));
        r.give(b, false);
        let d = plan(&mut r, 1, true, 30);
        assert_eq!(d, (40, 70));
        r.give(c, false);
        r.give(d, true);
        assert_eq!(r.used(), 0);
        assert_eq!(plan(&mut r, 0, true, 10), (0, 10));
    }

    #[test]
    fn a_stretch_given_back_out_of_order_waits_for_the_older_one() {
        let mut r = Ring::new(100);
        let a = plan(&mut r, 0, true, 30);
        let b = plan(&mut r, 1, true, 30);
        let c = plan(&mut r, 2, true, 30);
        r.give(b, false);
        assert_eq!(r.used(), 60);
        // b's dwords are not handed out while a is out ahead of it.
        assert_eq!(plan(&mut r, 2, true, 30), (90, 100));
        r.give(a, false);
        let e = plan(&mut r, 2, true, 70);
        assert_eq!(e, (0, 60));
        let _ = c;
    }

    #[test]
    fn a_flip_waits_for_no_plan_out() {
        let mut r = Ring::new(100);
        let a = plan(&mut r, 0, false, 20);
        assert!(r.wants_landing(1, true));
        // Still the old way while a is out.
        assert_eq!(plan(&mut r, 1, true, 20), (20, 40));
        r.give(a, false);
        r.give((20, 40), true);
        assert!(!r.wants_landing(0, true));
        let b = plan(&mut r, 0, true, 20);
        assert_eq!(b, (0, 20));
        assert!(r.wants_landing(1, false));
        assert!(!r.wants_landing(1, true));
    }

    /// Random plans landing in order, several in the air: no dword is ever
    /// held by two plans out, and the old way never moves past the area.
    #[test]
    fn no_dword_is_held_twice() {
        for want in [false, true] {
            let mut r = Ring::new(1000);
            let mut owner = vec![0u32; 1000];
            let mut air: VecDeque<((u32, u32), u32)> = VecDeque::new();
            let mut seed = 0x9e37_79b9_7f4a_7c15u64;
            let mut next = |m: u64| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed % m
            };
            let (mut placed, mut landings) = (0u64, 0u64);
            for id in 1..5000u32 {
                if r.wants_landing(air.len() as u32, want) || air.len() > 6 || next(3) == 0 {
                    while let Some((s, plan_id)) = air.pop_front() {
                        for d in s.0..s.1 {
                            assert_eq!(owner[d as usize], plan_id);
                            owner[d as usize] = 0;
                        }
                        r.give(s, air.is_empty());
                        if next(2) == 0 {
                            break;
                        }
                    }
                    landings += 1;
                }
                let s = plan(&mut r, air.len() as u32, want, next(200) as u32);
                assert!(s.1 <= 1000);
                for d in s.0..s.1 {
                    assert_eq!(owner[d as usize], 0, "dword {d} held twice");
                    owner[d as usize] = id;
                }
                placed += u64::from(s.1 - s.0);
                air.push_back((s, id));
            }
            assert!(placed > 0 && landings > 0);
        }
    }
}
