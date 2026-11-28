//! `VICTORY_TABLE_VIEWS_ONCE=1` (restart; off unless `1`): a descriptor table a draw makes (or
//! makes ready again) has its slots' views looked up under one hold of the descriptor lookup
//! lock's read side ([`VIEWS`]), not one hold per slot.
//!
//! Batch 5: binds make 4,000-8,700 tables a frame over 30,000-56,000 slots and make 2,000-4,500
//! ready again; each slot's `descriptor::view` took the shared lock's read side, an interlocked
//! write to one cache line that the title's six to eight recording threads (and the heap sync,
//! prepare threads, mirrors) all hit. With the switch a table's `n` slots are read in one hold:
//! each the same function as [`view`] (`view_with`), so each slot gives the view `view` would
//! have given at that moment. The table's records are read once at the start, as the key's hash
//! already reads them.

use super::{VIEW_TAG, View, Views, view_with};

/// The switch, read once.
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TABLE_VIEWS_ONCE").is_some_and(|v| v == "1"))
}

/// The views of one table's slots, looked up together (kept in the draw state, reused).
#[derive(Default)]
pub(crate) struct TableViews {
    table: u64,
    views: Vec<Option<View>>,
    filled: bool,
}

impl TableViews {
    /// With the switch: the views of the `n` slots from `table` (32 bytes apart), in one hold of
    /// the lookup lock. Without it nothing is looked up here and [`TableViews::get`] asks
    /// `view` for each.
    pub(crate) fn fill(&mut self, table: u64, n: u32) {
        self.filled = false;
        if !on() || table == 0 {
            return;
        }
        views_at(table, n, &mut self.views);
        self.table = table;
        self.filled = true;
    }

    /// The view at `handle`: from the table's looked-up views if it is one of its slots, else
    /// looked up alone.
    pub(crate) fn get(&self, handle: u64) -> Option<View> {
        if self.filled
            && let Some(at) = handle.checked_sub(self.table)
            && at % 32 == 0
            && let Some(view) = self.views.get((at / 32) as usize)
        {
            return view.clone();
        }
        super::view(handle)
    }

    /// Done with the table: later asks look up alone.
    pub(crate) fn clear(&mut self) {
        self.filled = false;
    }
}

/// [`super::view`] of each of `n` slots 32 bytes apart from `table`, the read side of the lookup
/// lock taken at most once for them all.
pub(crate) fn views_at(table: u64, n: u32, out: &mut Vec<Option<View>>) {
    out.clear();
    let mut held: Option<std::sync::RwLockReadGuard<'static, Views>> = None;
    for k in 0..u64::from(n) {
        let handle = table + k * 32;
        // SAFETY: a descriptor slot of the table, in title memory (as `view` reads it).
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        out.push(if tag == VIEW_TAG {
            view_with(i, generation, &mut held)
        } else {
            None
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::{View, view, words_for};
    use super::*;

    /// Records as the title's slots hold them: views of resources, a constant buffer, nothing,
    /// a stale generation; each looked up together gives what `view` gives alone.
    #[test]
    fn a_table_looked_up_together_gives_what_each_slot_gives() {
        let views = [
            Some(View::Shader(0x7e57_0001, None)),
            Some(View::Constant(0xd_0000_1000, 256)),
            None,
            Some(View::Unordered(0x7e57_0002, 0, None)),
            Some(View::Shader(
                0x7e57_0003,
                Some([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
            )),
            Some(View::Constant(0xd_0000_2000, 64)),
        ];
        // 8 dwords a slot, as the title's view heaps lay them.
        let mut slots = vec![[0u32; 8]; views.len() + 1];
        for (slot, v) in slots.iter_mut().zip(&views) {
            if let Some(v) = v {
                let [i, generation] = words_for(v.clone());
                slot[..3].copy_from_slice(&[VIEW_TAG, i, generation]);
            }
        }
        // A stale record: the generation of an entry moved on.
        let [i, generation] = words_for(View::Shader(0x7e57_0004, None));
        slots[views.len()][..3].copy_from_slice(&[VIEW_TAG, i, generation.wrapping_add(1)]);
        let table = slots.as_ptr() as u64;
        let n = slots.len() as u32;
        let alone: Vec<Option<View>> = (0..u64::from(n)).map(|k| view(table + k * 32)).collect();
        let mut together = Vec::new();
        views_at(table, n, &mut together);
        assert_eq!(together, alone);
        assert_eq!(alone[0], views[0]);
        assert_eq!(alone[2], None);
        assert_eq!(alone[views.len()], None);
        // Through the kept state: slots of the table, and a handle outside it asked alone.
        let mut tv = TableViews::default();
        tv.views = together.clone();
        tv.table = table;
        tv.filled = true;
        for k in 0..u64::from(n) {
            assert_eq!(tv.get(table + k * 32), alone[k as usize]);
        }
        assert_eq!(tv.get(table + 8), view(table + 8));
        tv.clear();
        assert_eq!(tv.get(table), alone[0]);
    }
}
