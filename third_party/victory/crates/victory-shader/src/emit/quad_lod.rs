//! `VICTORY_COMPUTE_QUAD_LOD=1` (off by default): a compute program's samples that take an
//! implicit level (`image_sample`, `_o`, `_cl`, `_b`, `_b_cl`) get it from the console's lane
//! quads, as the hardware does, instead of reading level 0 (a bias: that level). The DXIL
//! validator grants no derivatives to these programs, so the gradients are read from the quad
//! (lane `4k + 1` less lane `4k`, lane `4k + 2` less lane `4k`: coarse derivatives) and the
//! sample takes them explicitly; a bias scales them by `2^bias`. Cubes keep level 0.
//!
//! The console's quads are lanes `4k..4k + 3` of its wave, which hold consecutive flat thread
//! ids. With console lanes (cross-lane code) the thread relabelling makes the host lane the
//! console lane, in a half of a 64-lane wave too; in the paired form each twin reads its own
//! values from the quad's host lanes (lane `l` and `l + 32` share a host lane). Otherwise the
//! host's waves hold consecutive flat ids in order, as NVIDIA's do (no regrouping into one row
//! as `78bcfdc3` does for hosts that tile 2x2).
//! shortcut: in the halves and ticket forms a console lane is its rank among the lanes active
//! at entry, taken as its host lane (they are the same while a wave's lanes start together).
//!
//! A quad with a lane that is not here (a branch some lanes left, the tail of a group that is
//! not whole quads) takes gradients of 0, level 0 as before: the host reads nothing defined
//! from an inactive lane, where the console reads whatever that lane's register holds.
//!
//! Source: hello3928/Project-VI `653b1197`, `78bcfdc3` (Marvisak/VIctory PR #2999).

use super::isa_fixes::Fix;
use super::{Attr, Bin, Lower, V, flags, pred};

impl Lower<'_> {
    /// Whether an implicit-level sample of image kind `kind` takes its gradients from the
    /// quad: asked for, a compute program (not mesh), not a cube.
    pub(super) fn quad_lod(&self, kind: u32) -> bool {
        self.s.threads.is_some() && self.s.mesh.is_none() && kind != 5 && Fix::ComputeQuadLod.on()
    }

    /// The x then y gradients (three each, undefined past `coords`) of float coordinates
    /// `coords` across this lane's quad, each times `scale` if given; 0 where a lane of the
    /// quad is not here.
    pub(super) fn quad_gradients(&mut self, coords: &[V], scale: Option<V>) -> Vec<V> {
        self.s.flags |= flags::WAVE_OPS;
        let c = self.cur;
        let (i32t, f32t) = (self.s.t.i32, self.s.t.f32);
        let fu = self.s.m.undef(f32t);
        let lane = self.lane_index();
        let (not3, one, two) = (self.s.m.i32(!3), self.s.m.i32(1), self.s.m.i32(2));
        let base = self.s.m.bin(c, Bin::And, lane, not3);
        let right = self.s.m.bin(c, Bin::Or, base, one);
        let below = self.s.m.bin(c, Bin::Or, base, two);
        // The quad's lanes 0, 1 and 2 here (the ballot's dword of the quad).
        let yes = self.s.m.bool(true);
        let four = self.s.m.named_struct("dx.types.fouri32", vec![i32t; 4]);
        let ballot = self
            .s
            .op(c, "waveActiveBallot", Attr::NoUnwind, four, 116, &[yes]);
        let (lo, hi) = (
            self.s.m.extract(c, ballot, 0),
            self.s.m.extract(c, ballot, 1),
        );
        let k32 = self.s.m.i32(32);
        let upper = self.s.m.cmp(c, pred::IUGE, base, k32);
        let word = self.s.m.select(c, upper, hi, lo);
        let k31 = self.s.m.i32(31);
        let at = self.s.m.bin(c, Bin::And, base, k31);
        let bits = self.s.m.bin(c, Bin::LShr, word, at);
        let seven = self.s.m.i32(7);
        let bits = self.s.m.bin(c, Bin::And, bits, seven);
        let whole = self.s.m.cmp(c, pred::EQ, bits, seven);
        let zero = self.s.m.f32(0);
        let mut g = vec![fu; 6];
        for (k, &x) in coords.iter().enumerate().take(3) {
            let xi = self.as_int(x);
            let read = |l: &mut Self, lane: V| {
                let v = l.read_lane(xi, lane);
                l.as_float(v)
            };
            let (x0, x1, x2) = (read(self, base), read(self, right), read(self, below));
            for (slot, other) in [(k, x1), (3 + k, x2)] {
                let mut d = self.s.m.bin(c, Bin::Sub, other, x0);
                if let Some(s) = scale {
                    d = self.s.m.bin(c, Bin::Mul, d, s);
                }
                g[slot] = self.s.m.select(c, whole, d, zero);
            }
        }
        g
    }
}
