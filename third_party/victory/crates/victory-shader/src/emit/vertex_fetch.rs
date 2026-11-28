//! What a program made from loose pieces (`VICTORY_PIPELINE_PIECES=1`,
//! [`crate::pieces`]) adds to the lowering: its own vertex fetches through a
//! table of vertex buffer descriptors, and the tests that tell such a program
//! from a title's. A child module so it shares the emitter's state. Ported
//! from RipcuBAN/victory-fork c590f21 (trustno1-coder/VIctory-NVIDIA #18,
//! Kemerd/VIctory-NVIDIA-RT).

use super::{
    Cast, Fields, Inst, Lower, NULL, Numeric, Operand, Reg, RootType, Source, StageIo, Structured, V,
    Val, Why, buffer_format,
};
use crate::native::NativeProgram;

/// Whether `native` was made from loose pieces ([`Lower::made_from_pieces`]),
/// asked before a [`Lower`] exists.
pub(super) fn from_pieces(native: &NativeProgram) -> bool {
    let range = crate::native::VERTEX_BUFFERS >> 8;
    native
        .layout
        .iter()
        .any(|p| matches!(p.root, RootType::Other(code) if code >> 8 == range))
}

/// A `v_cndmask_b32` whose mask is no lane mask the structuring tracks: a
/// number the code holds (`s_cselect` of all lanes or none, a bit pattern).
/// The select takes this lane's own bit of it, which needs the console's
/// lane numbers. `reads_mask_as_data` leaves selects out on purpose (their
/// masks are mostly tracked ones), so a program whose only such use is this
/// one would be lowered with host lanes.
pub(super) fn selects_by_number(st: &Structured, i: &Inst) -> bool {
    let Fields::Valu(v) = &i.fields else {
        return false;
    };
    i.name() == "v_cndmask_b32"
        && matches!(v.src.get(2), Some(&Operand::Scalar { code, .. })
            if i.read(Reg::S(code)).is_some_and(|m| !st.masks.contains_key(&m)))
}

impl Lower<'_> {
    /// Whether the program was made from loose pieces ([`crate::pieces`])
    /// and not passed by a title in a container. Those programs carry a
    /// layout entry whose `RootType::Other` code is in the 0x200 range (the
    /// vertex buffer table, [`crate::native::VERTEX_BUFFERS`], is the
    /// first); a title's programs never do, the codes the host computes
    /// for them lie in the 0x100 range. What the pieces added to the
    /// translation is taken only where this holds, so a title's shaders
    /// translate as they did before the pieces existed.
    pub(super) fn made_from_pieces(&self) -> bool {
        from_pieces(self.x.native)
    }

    /// Which table entry `src` is loaded from: its root parameter and byte.
    fn entry(&self, src: &Source) -> Option<(usize, u32)> {
        let Source::Loaded { load, first } = src else {
            return None;
        };
        let a = self.scalar.get(load).filter(|a| a.dynamic.is_none())?;
        Some((a.param, a.offset + 4 * u32::from(*first)))
    }

    /// Whether layout entry `param` is the table of vertex buffer
    /// descriptors of a program that fetches its own attributes.
    pub(super) fn vertex_table(&self, param: usize) -> bool {
        let entry = self.x.native.layout.get(param);
        entry.is_some_and(|p| p.root == RootType::Other(crate::native::VERTEX_BUFFERS))
    }

    /// The byte of the vertex buffer table a buffer access at `site` takes
    /// its descriptor from, when it is one of the program's own vertex
    /// fetches.
    pub(super) fn vertex_fetch_entry(&self, site: (usize, usize)) -> Option<u32> {
        let &src = self.descriptors.get(&(site, false))?;
        let (param, byte) = self.entry(src)?;
        self.vertex_table(param).then_some(byte)
    }

    /// A vertex program's own fetch of an attribute: a buffer load through
    /// the descriptor at byte `entry` of the vertex buffer table (16 bytes
    /// a slot), at the vertex index and a constant byte of the vertex. It
    /// becomes a read of the input signature, so the host's input assembler
    /// fetches (and adds the base vertex, as the code does with s2).
    ///
    /// Only the forms seen are taken: whole dwords of a 32-bit float
    /// element, and an `R8G8B8A8_UNORM` element through a typed load of
    /// that format or of one of its bytes. Anything else is refused: a
    /// fetch that reads what the element does not hold would draw wrong.
    pub(super) fn vertex_fetch(
        &mut self,
        inst: &Inst,
        entry: u32,
        guard: Option<V>,
    ) -> Result<(), Why> {
        let Fields::Buffer {
            data,
            addr,
            soffset,
            offset,
            offen,
            idxen,
            format,
            lds,
            tfe,
            ..
        } = &inst.fields
        else {
            unreachable!()
        };
        let name = inst.name();
        let plain = matches!(
            soffset,
            Operand::Scalar { code: NULL, .. } | Operand::Const(128)
        );
        if !*idxen || *offen || !plain || *lds || *tfe || !entry.is_multiple_of(16) {
            return Err("a vertex fetch that is not by vertex index at a constant byte");
        }
        // The index must be the vertex's own: v0 as it arrives plus s2 as
        // it arrives (the base vertex), which is what the host adds too.
        let Operand::Vector { reg: areg, .. } = *addr else {
            return Err("a buffer address");
        };
        let index = inst.read(Reg::V(areg)).ok_or("a buffer address")?;
        let p = self.x.p;
        let arrives = |reg: Reg| p.inputs.iter().find(|i| i.0 == reg).map(|i| i.1);
        let own = p.blocks.iter().flat_map(|b| &b.insts).any(|i| {
            i.name() == "v_add_nc_u32"
                && i.writes.iter().any(|w| w.1 == index)
                && i.read(Reg::S(2))
                    .is_some_and(|v| Some(v) == arrives(Reg::S(2)))
                && i.read(Reg::V(0))
                    .is_some_and(|v| Some(v) == arrives(Reg::V(0)))
                && matches!(&i.fields, Fields::Valu(f) if f.src.len() == 2
                    && f.sdwa.is_none()
                    && f.dpp.is_none()
                    && f.src.iter().any(|s| matches!(s, Operand::Scalar { code: 2, .. }))
                    && f.src.iter().any(|s| matches!(s, Operand::Vector { reg: 0, .. })))
        });
        if !own {
            return Err("a vertex fetch at an index that is not the vertex's own");
        }
        // The element of the input layout that holds the byte.
        let StageIo::Vs { fetch, .. } = &self.x.native.io else {
            return Err("a vertex fetch outside a vertex program");
        };
        let (slot, byte) = (entry / 16, u32::from(*offset));
        let e = fetch
            .iter()
            .find(|e| e.slot == slot && byte >= e.offset && byte - e.offset < e.bytes)
            .ok_or("a vertex fetch outside the elements whose format is handled")?;
        let within = byte - e.offset;
        let channels = crate::pieces::element_shape(e.format).map_or(0, |s| u32::from(s.1));
        // The first channel read and how many, by the load and the format.
        let dwords = match name {
            "buffer_load_dword" => Some(1),
            "buffer_load_dwordx2" => Some(2),
            "buffer_load_dwordx3" => Some(3),
            "buffer_load_dwordx4" => Some(4),
            _ => None,
        };
        let typed = match name.strip_prefix("tbuffer_load_format_") {
            Some("x") => Some(1),
            Some("xy") => Some(2),
            Some("xyz") => Some(3),
            Some("xyzw") => Some(4),
            _ => None,
        };
        let bytes = e.format == crate::pieces::R8G8B8A8_UNORM;
        let (first, count) = match (dwords, typed, format.and_then(buffer_format)) {
            // Whole dwords of a float element.
            (Some(n), None, None) if !bytes && within.is_multiple_of(4) => (within / 4, n),
            // All four bytes as normalized values, or some of the first.
            (None, Some(n), Some((&[8, 8, 8, 8], Numeric::Unorm))) if bytes && within == 0 => {
                (0, n)
            }
            // One byte as a normalized value.
            (None, Some(1), Some((&[8], Numeric::Unorm))) if bytes => (within, 1),
            _ => return Err("a vertex fetch of a form or format that is not handled"),
        };
        if first + count > channels {
            return Err("a vertex fetch past its element");
        }
        let Some(Operand::Vector { reg, .. }) = *data else {
            return Err("a buffer result");
        };
        for k in 0..count {
            let (id, col) = self
                .find_input(e.row, (first + k) as u8)
                .ok_or("a vertex input the signature lacks")?;
            // (float elements arrive as their bits, normalized ones as the
            // float the typed load gives)
            let x = match self.load_input(id, col) {
                Val::I32(x) => x,
                Val::Bit(x) => self.s.m.cast(self.cur, Cast::ZExt, x, self.s.t.i32),
                Val::Missing(w) => return Err(w),
            };
            self.write(inst, Reg::V(reg + k as u8), x, guard);
        }
        Ok(())
    }
}
