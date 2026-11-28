//! (`VICTORY_HEAP_BUFFER_ADDRESS=1`) Buffers the code picks from the view heap by an index it
//! computes (root type `ResourceHeap`) are read by their title address, not through a host
//! descriptor: the host keeps each title slot's buffer range in a table
//! (`shader::SLOT_BUFFER_RANGES`: four dwords a slot, the address's low and high dwords, the
//! size in bytes and the element stride; all 0 for a slot with no buffer view), and the load
//! reads title memory there through the memory directory (`shader::SLOT_MEMORY`), as
//! raw-address code does. A buffer view then needs no slot in the host's view heap (capped at
//! 1,000,000 on the RTX 5090, under the title's 1,245,184), and a title slot past the host's
//! reads its own buffer instead of another view or null.
//!
//! Read-only raw and structured loads of whole dwords only (`buffer_load_dword*` with no
//! format, and scalar `s_buffer_load_dword*`, which also reach constant buffers this way):
//! typed loads need the format's decode, stores and atomics go through the host's copies of
//! title memory only once a write path there is proven, byte and short loads keep
//! their descriptor. Out of bounds as a raw view: each dword past the size reads 0.

use super::{Attr, Bin, Cast, Fields, Lower, NULL, Operand, Reg, RootType, Tag, V, Why, pred};
use crate::resource::Source;

#[cfg(test)]
thread_local! {
    /// Tests: the switch for this thread.
    pub(in crate::emit) static FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether heap-picked buffers are read by address (`VICTORY_HEAP_BUFFER_ADDRESS=1`).
pub fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = FORCED.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_BUFFER_ADDRESS").is_some_and(|v| v == "1"))
}

impl Lower<'_> {
    /// The title slot (not the host's: the table is by title slot) of the
    /// descriptor `src` loads from the view heap, if it does.
    fn heap_buffer_slot(&mut self, src: &Source) -> Result<Option<V>, Why> {
        let Source::Loaded { load, first } = *src else {
            return Ok(None);
        };
        let Some(a) = self.scalar.get(&load).copied() else {
            return Ok(None);
        };
        if self.x.native.layout.get(a.param).map(|p| p.root) != Some(RootType::ResourceHeap) {
            return Ok(None);
        }
        let li = &self.x.p.blocks[load.0].insts[load.1];
        let Fields::Smem {
            base: Operand::Scalar { code, .. },
            offset,
            soffset,
            ..
        } = li.fields
        else {
            return Ok(None);
        };
        let c = self.cur;
        let base = li.read(Reg::S(code)).ok_or("a descriptor load")?;
        let mut addr = self.i32_of(base)?;
        let k = self
            .s
            .m
            .i32((offset as u32).wrapping_add(4 * u32::from(first)));
        addr = self.s.m.bin(c, Bin::Add, addr, k);
        if !matches!(soffset, Operand::Scalar { code: NULL, .. }) {
            let so = self.operand(li, &soffset)?;
            addr = self.s.m.bin(c, Bin::Add, addr, so);
        }
        let five = self.s.m.i32(5);
        Ok(Some(self.s.m.bin(c, Bin::LShr, addr, five)))
    }

    /// A read-only load of `count` dwords from the buffer `src` picks from
    /// the view heap, element `index` (if indexed) at byte `off`, by
    /// address; `None` when `src` is not such a buffer or the switch is off.
    pub(super) fn heap_buffer_load(
        &mut self,
        src: &Source,
        index: Option<V>,
        off: V,
        count: u32,
        guard: Option<V>,
    ) -> Result<Option<Vec<V>>, Why> {
        use super::super::shader::{BUFFER_RANGES_COUNT, SLOT_BUFFER_RANGES};
        if !on() {
            return Ok(None);
        }
        let Some(slot) = self.heap_buffer_slot(src)? else {
            return Ok(None);
        };
        let c = self.cur;
        let i32t = self.s.t.i32;
        // Inside the table whatever the slot (a root view has no bounds).
        let last = self.s.m.i32(BUFFER_RANGES_COUNT - 1);
        let slot = self
            .s
            .op(c, "binary.i32", Attr::ReadNone, i32t, 40, &[slot, last]);
        self.s.m.tag(slot, Tag::Clamp);
        let four = self.s.m.i32(4);
        let at = self.s.m.bin(c, Bin::Shl, slot, four);
        let h = self.s.root_srv_handle(SLOT_BUFFER_RANGES);
        let (iu, all, align) = (self.s.m.undef(i32t), self.s.m.i8(0xF), self.s.m.i32(4));
        let r = self.s.op(
            c,
            "rawBufferLoad.f32",
            Attr::ReadOnly,
            self.s.t.res_ret,
            139,
            &[h, at, iu, all, align],
        );
        let words: Vec<V> = (0..4)
            .map(|k| {
                let x = self.s.m.extract(c, r, k);
                self.as_int(x)
            })
            .collect();
        let (lo, hi, size, stride) = (words[0], words[1], words[2], words[3]);
        let byte = match index {
            Some(element) => {
                let at = self.s.m.bin(c, Bin::Mul, element, stride);
                self.s.m.bin(c, Bin::Add, at, off)
            }
            None => off,
        };
        // the address: the range's start plus the byte, with the carry
        let sum = self.s.m.bin(c, Bin::Add, lo, byte);
        let wrapped = self.s.m.cmp(c, pred::IULT, sum, lo);
        let one = self.s.m.cast(c, Cast::ZExt, wrapped, i32t);
        let high = self.s.m.bin(c, Bin::Add, hi, one);
        // a load wholly past the end touches no memory (no fault for it)
        let inside = self.s.m.cmp(c, pred::IULT, byte, size);
        let guard = match guard {
            Some(g) => Some(self.s.m.bin(c, Bin::And, g, inside)),
            None => Some(inside),
        };
        // past the end: the range's own first page is looked up, not wherever the sum points
        let sum = self.s.m.select(c, inside, sum, lo);
        let high = self.s.m.select(c, inside, high, hi);
        let (h, at) = self.memory(sum, high, guard);
        let zero = self.s.m.i32(0);
        let mut out = Vec::new();
        // four dwords a load, on in the same copy (a buffer's copy holds all of it)
        for first in (0..count).step_by(4) {
            let n = (count - first).min(4);
            let at = if first == 0 {
                at
            } else {
                let k = self.s.m.i32(4 * first);
                self.s.m.bin(c, Bin::Add, at, k)
            };
            let mask = self.s.m.i8(((1u32 << n) - 1) as u8);
            let r = self.s.op(
                c,
                "rawBufferLoad.f32",
                Attr::ReadOnly,
                self.s.t.res_ret,
                139,
                &[h, at, iu, mask, align],
            );
            for j in 0..n {
                let k = first + j;
                let x = self.s.m.extract(c, r, j);
                let x = self.as_int(x);
                // dword k in bounds: byte + 4k + 4 <= size, without wrapping
                let end = self.s.m.i32(4 * k + 4);
                let end = self.s.m.bin(c, Bin::Add, byte, end);
                let fits = self.s.m.cmp(c, pred::IULE, end, size);
                let no_wrap = self.s.m.cmp(c, pred::IUGE, end, byte);
                let ok = self.s.m.bin(c, Bin::And, fits, no_wrap);
                out.push(self.s.m.select(c, ok, x, zero));
            }
        }
        Ok(Some(out))
    }
}
