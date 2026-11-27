//! `VICTORY_MATERIAL_TEXTURE_DEMAND=1` (`crate::material_demand`): a deferred character draw's
//! material textures, read from its record at bind time, for the front of the next pass.

use super::*;
use crate::material_demand::{
    C, RECORD_USER_WORD, TABLE_BYTES, TABLE_USER_WORD, add, byte_range, fields, indices, want,
};

impl State {
    /// For a draw whose pixel shader is a character material one: its textures wanted.
    pub(super) fn material_textures(&self, p: &HostPipeline) {
        let Some((stage, fields)) = p
            .stages
            .iter()
            .find_map(|s| fields(s.shader.hash.as_str()).map(|f| (s, f)))
        else {
            return;
        };
        match self.material_slots(stage, fields) {
            Some(slots) => {
                add(C::Slots, slots.len() as u64);
                want(&slots);
            }
            None => add(C::Unread, 1),
        }
        add(C::Draws, 1);
    }

    /// The heap slots the draw's material record names at `fields`.
    fn material_slots(
        &self,
        stage: &crate::pipeline::host::Stage,
        fields: &[u32],
    ) -> Option<Vec<u32>> {
        let t = stage.translated();
        let word = |index: u32| {
            let u = t.host.user_data.iter().find(|u| u.index == index)?;
            if matches!(u.root, RootType::Other(_)) {
                return None;
            }
            Some(self.user_value(false, u.slot, u.root, u.dword))
        };
        let table = word(TABLE_USER_WORD)?;
        let record = word(RECORD_USER_WORD)?;
        let (heap, _) = self.view_heap()?;
        let view = crate::descriptor::view(heap.handle(table.wrapping_add(TABLE_BYTES) >> 5)?)?;
        let (base, len) = match view {
            View::Shader(object, None) => {
                let (at, len) = crate::resource::buffer_bytes(object as This)?;
                (at as u64, len)
            }
            _ => crate::descriptor::host::buffer_range(&view)?,
        };
        let (at, bytes) = byte_range(record, fields, len)?;
        let words = super::bake_pass::words(base.checked_add(at)?, bytes / 4)?;
        Some(indices(&words, fields))
    }
}
