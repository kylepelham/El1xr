//! (`VICTORY_HEAP_VIEW_ADDRESS=1`) The address of a view the code picks from the view heap at
//! an offset it computes and reads as numbers: the TraceShadowRays programs load their
//! ray-tracing structure's descriptor from the heap at an offset a per-wave record names and
//! take its base from dwords 0 and 1, to walk the structure in title memory. The host keeps
//! each title slot's view address in a table (`shader::SLOT_VIEW_ADDRESSES`); the load's
//! dwords 0 and 1 read the slot's pair, as the console's descriptor holds the address there
//! (dword 1 also holds the stride from bit 16 up, which code that takes the address masks
//! away). The other dwords have no value.

use super::{Attr, Bin, Fields, Lower, Operand, Reg, RootType, Site, Tag, V};

#[cfg(test)]
thread_local! {
    /// Tests: the switch on for this thread.
    pub(in crate::emit) static FORCED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether heap views read as numbers get their address (`VICTORY_HEAP_VIEW_ADDRESS=1`).
pub fn on() -> bool {
    #[cfg(test)]
    if FORCED.get() {
        return true;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_VIEW_ADDRESS").is_some_and(|v| v == "1"))
}

impl<'a> Lower<'a> {
    /// The title address (low, high) of the view the scalar load at `site` reads from the view
    /// heap (root type `ResourceHeap`) at the slot its address names: the host's pair for that
    /// slot. `None` with the switch off or for a load from anywhere else.
    pub(super) fn heap_view_address(&mut self, site: Site) -> Option<(V, V)> {
        use super::super::shader::{SLOT_VIEW_ADDRESSES, VIEW_ADDRESSES_COUNT};
        if !on() {
            return None;
        }
        let a = self.scalar.get(&site).copied()?;
        if self.x.native.layout.get(a.param)?.root != RootType::ResourceHeap {
            return None;
        }
        let li = &self.x.p.blocks[site.0].insts[site.1];
        let Fields::Smem {
            base: Operand::Scalar { code, .. },
            offset,
            soffset,
            ..
        } = li.fields
        else {
            return None;
        };
        let base = li.read(Reg::S(code))?;
        let so = (!matches!(soffset, Operand::Scalar { code: super::NULL, .. })).then_some(soffset);
        let c = self.cur;
        let mut byte = self.i32_of(base).ok()?;
        let k = self.s.m.i32(offset as u32);
        byte = self.s.m.bin(c, Bin::Add, byte, k);
        if let Some(so) = so {
            let so = self.operand(li, &so).ok()?;
            byte = self.s.m.bin(c, Bin::Add, byte, so);
        }
        // Views are 32 bytes; inside the table whatever the slot (a root view has no bounds).
        let i32t = self.s.t.i32;
        let five = self.s.m.i32(5);
        let slot = self.s.m.bin(c, Bin::LShr, byte, five);
        let last = self.s.m.i32(VIEW_ADDRESSES_COUNT - 1);
        let slot = self
            .s
            .op(c, "binary.i32", Attr::ReadNone, i32t, 40, &[slot, last]);
        self.s.m.tag(slot, Tag::Clamp);
        let three = self.s.m.i32(3);
        let at = self.s.m.bin(c, Bin::Shl, slot, three);
        let h = self.s.root_srv_handle(SLOT_VIEW_ADDRESSES);
        let (iu, both, align) = (self.s.m.undef(i32t), self.s.m.i8(3), self.s.m.i32(4));
        let r = self.s.op(
            c,
            "rawBufferLoad.f32",
            Attr::ReadOnly,
            self.s.t.res_ret,
            139,
            &[h, at, iu, both, align],
        );
        let (lo, hi) = (self.s.m.extract(c, r, 0), self.s.m.extract(c, r, 1));
        Some((self.as_int(lo), self.as_int(hi)))
    }
}

#[cfg(test)]
#[path = "heap_address_tests.rs"]
mod tests;
