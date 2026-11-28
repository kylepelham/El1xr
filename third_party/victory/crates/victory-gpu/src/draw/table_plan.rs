//! Per-pipeline descriptor table plan (`TablePlan`), grouped once when a
//! shader is translated instead of re-scanning `UsedResource` declarations at
//! every draw or dispatch bind.
//!
//! Adapted from `avatarmanz/VIctory-7900XTX` (`46e3297818`, originally from
//! `codec-d/cool_fox` `641835a4`) with per-slot `half_views` grouping.

use victory_shader::emit::{TABLE_SLOT_KEPT, USER_TABLE_SLOT, UserData};
use victory_shader::resource::RootIndexedView;
use victory_shader::{ResourceType, RootType, UsedResource};

/// (`VICTORY_HEAP_BUFFER_ADDRESS=1`) The table root parameters whose constant buffers the shader
/// reads by title address alone: it asks for the table's title slot
/// ([`USER_TABLE_SLOT`] at dword 0) and reads none of them through its descriptor (no ask at
/// [`TABLE_SLOT_KEPT`]). Sorted.
pub(crate) fn cbvs_by_address(user_data: &[UserData]) -> Vec<u8> {
    let asked = |slot: u8, dword: u32| {
        user_data
            .iter()
            .any(|u| u.root == RootType::Other(USER_TABLE_SLOT) && u.slot == slot && u.dword == dword)
    };
    let mut slots: Vec<u8> = user_data
        .iter()
        .filter(|u| u.root == RootType::Other(USER_TABLE_SLOT) && u.dword == 0)
        .map(|u| u.slot)
        .filter(|&slot| !asked(slot, TABLE_SLOT_KEPT))
        .collect();
    slots.sort_unstable();
    slots.dedup();
    slots
}

/// Shader interface metadata shared by every pipeline variant and draw.
pub(crate) struct TablePlan {
    pub(crate) groups: Vec<TableGroup>,
    /// Direct entries can interleave slots. Preserve their declaration order
    /// when recording resource reads, writes, and transitions.
    pub(crate) direct: Vec<(usize, usize)>,
    /// (`VICTORY_HEAP_BUFFER_ADDRESS=1`) `(root_slot, descriptor_index)` of table constant
    /// buffers lowered to read by title address: their `buffer_ranges` entries and host copies
    /// are ensured before `memory_ready` on each draw (covers per-draw tables in title slots past
    /// the host heap or written by raw CPU copies). Sorted and deduplicated.
    pub(crate) cbv_address_slots: Vec<(u8, u32)>,
}

pub(crate) struct TableGroup {
    pub(crate) slot: u8,
    pub(crate) has_cbv: bool,
    /// (`VICTORY_HEAP_BUFFER_ADDRESS=1`) The shader reads this table's constant buffers by
    /// title address ([`cbvs_by_address`]): they are not among `entries` and need not be made.
    pub(crate) cbv_by_address: bool,
    pub(crate) has_uav: bool,
    pub(crate) max_dword: Option<u32>,
    pub(crate) entries: Vec<UsedResource>,
    pub(crate) indexed: Vec<RootIndexedView>,
    /// Sorted, deduplicated table dword offsets where the shader reads/writes
    /// 16-bit channels as unsigned integers (`HostShader::half_views`).
    pub(crate) half: Vec<u32>,
    /// Descriptor number -> first declaration index in `entries`, matching the
    /// linear bind-time search.
    by_descriptor: Vec<(u32, usize)>,
}

impl TablePlan {
    /// `by_address`: [`cbvs_by_address`]; those tables' constant buffers are left out (read by
    /// title address, they ask for no table made afresh).
    pub(crate) fn new(
        used: &[UsedResource],
        indexed: &[RootIndexedView],
        half: &[(u8, u32)],
        by_address: &[u8],
    ) -> Self {
        let through_table = |u: &&UsedResource| {
            u.root == RootType::Table
                && !(u.kind == ResourceType::Cbv && by_address.binary_search(&u.slot).is_ok())
        };
        let mut slots: Vec<u8> = used
            .iter()
            .filter(through_table)
            .map(|u| u.slot)
            .chain(half.iter().map(|&(slot, _)| slot))
            .collect();
        slots.sort_unstable();
        slots.dedup();
        let mut groups: Vec<_> = slots
            .iter()
            .map(|&slot| {
                let mut slot_half: Vec<u32> = half
                    .iter()
                    .filter(|&&(s, _)| s == slot)
                    .map(|&(_, at)| at)
                    .collect();
                slot_half.sort_unstable();
                slot_half.dedup();
                TableGroup {
                    slot,
                    has_cbv: false,
                    cbv_by_address: by_address.binary_search(&slot).is_ok(),
                    has_uav: false,
                    max_dword: None,
                    entries: Vec::new(),
                    by_descriptor: Vec::new(),
                    indexed: indexed
                        .iter()
                        .filter(|v| v.table_slot == slot)
                        .copied()
                        .collect(),
                    half: slot_half,
                }
            })
            .collect();
        let mut direct = Vec::new();
        for &u in used.iter().filter(through_table) {
            let i = slots
                .binary_search(&u.slot)
                .expect("table slot collected from these declarations");
            let group = &mut groups[i];
            group.has_cbv |= u.kind == ResourceType::Cbv;
            group.has_uav |= u.kind == ResourceType::Uav;
            group.max_dword = Some(
                group
                    .max_dword
                    .map_or(u.table_offset_dwords, |m| m.max(u.table_offset_dwords)),
            );
            direct.push((i, group.entries.len()));
            group
                .by_descriptor
                .push((u.table_offset_dwords / 8, group.entries.len()));
            group.entries.push(u);
        }
        for group in &mut groups {
            group.by_descriptor.sort_unstable();
            group.by_descriptor.dedup_by_key(|item| item.0);
        }
        let mut cbv_address_slots: Vec<(u8, u32)> = if by_address.is_empty() {
            Vec::new()
        } else {
            used.iter()
                .filter(|u| {
                    u.root == RootType::Table
                        && u.kind == ResourceType::Cbv
                        && by_address.binary_search(&u.slot).is_ok()
                })
                .map(|u| (u.slot, u.table_offset_dwords / 8))
                .collect()
        };
        cbv_address_slots.sort_unstable();
        cbv_address_slots.dedup();
        Self {
            groups,
            direct,
            cbv_address_slots,
        }
    }
}

impl TableGroup {
    pub(crate) fn entry(&self, descriptor: u32) -> Option<&UsedResource> {
        let i = self
            .by_descriptor
            .binary_search_by_key(&descriptor, |item| item.0)
            .ok()?;
        Some(&self.entries[self.by_descriptor[i].1])
    }

    pub(crate) fn has_half(&self, dword: u32) -> bool {
        !self.half.is_empty() && self.half.binary_search(&dword).is_ok()
    }

    #[cfg(windows)]
    pub(crate) fn needs_snapshot(
        &self,
        heap: &crate::descriptor::Heap,
        host_count: u32,
        table: u64,
    ) -> bool {
        let Some(max_dword) = self.max_dword else {
            return false;
        };
        let index_at = |dword: u32| {
            table
                .checked_add(u64::from(dword) * 4)
                .and_then(|end| heap.index(end))
        };
        match index_at(max_dword) {
            Some(i) => i >= host_count,
            None => self
                .entries
                .iter()
                .any(|u| index_at(u.table_offset_dwords).is_some_and(|i| i >= host_count)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_used(slot: u8, dword: u32, kind: ResourceType) -> UsedResource {
        UsedResource {
            kind,
            root: RootType::Table,
            table_offset_dwords: dword,
            lower_bound: 0,
            stride: 0,
            dwords: 8,
            user_data: 0,
            slot,
        }
    }

    fn asks(slot: u8, dword: u32) -> UserData {
        UserData {
            index: 0,
            slot,
            root: RootType::Other(USER_TABLE_SLOT),
            dword,
        }
    }

    #[test]
    fn tables_whose_constant_buffers_are_read_by_address_are_told_by_the_hosts_words() {
        let plain = UserData {
            index: 1,
            slot: 2,
            root: RootType::Table,
            dword: 0,
        };
        // 4 asks for its title slot alone; 1 also reads a constant buffer through its descriptor
        let words = [asks(4, 0), plain, asks(1, 0), asks(1, TABLE_SLOT_KEPT), asks(4, 0)];
        assert_eq!(cbvs_by_address(&words), vec![4]);
        assert!(cbvs_by_address(&[plain, asks(3, TABLE_SLOT_KEPT)]).is_empty());
    }

    #[test]
    fn a_constant_buffer_read_by_address_no_longer_makes_its_table_afresh() {
        let used = [
            table_used(1, 0, ResourceType::Cbv),
            table_used(1, 8, ResourceType::Srv),
            table_used(2, 0, ResourceType::Cbv),
            table_used(3, 0, ResourceType::Cbv),
        ];
        // Off (or the shader read them through descriptors): every table with a constant buffer
        // is made afresh for each draw (`State::tables`: `needs_snapshot = group.has_cbv`).
        let before = TablePlan::new(&used, &[], &[], &cbvs_by_address(&[]));
        assert!(before.groups.iter().all(|g| g.has_cbv && !g.cbv_by_address));
        // By address for tables 1 and 2: their constant buffers leave the plan; table 1 keeps
        // its texture (bound in place unless something else asks), table 2 has nothing left to
        // bind, table 3 (read through its descriptor) is unchanged.
        let words = [asks(1, 0), asks(2, 0), asks(3, 0), asks(3, TABLE_SLOT_KEPT)];
        let after = TablePlan::new(&used, &[], &[], &cbvs_by_address(&words));
        assert_eq!(after.groups.iter().map(|g| g.slot).collect::<Vec<_>>(), vec![1, 3]);
        let g1 = &after.groups[0];
        assert!(!g1.has_cbv && g1.cbv_by_address);
        assert_eq!(g1.entries.len(), 1);
        assert_eq!(g1.entries[0].kind, ResourceType::Srv);
        assert_eq!(g1.max_dword, Some(8));
        assert!(g1.entry(0).is_none() && g1.entry(1).is_some());
        let g3 = &after.groups[1];
        assert!(g3.has_cbv && !g3.cbv_by_address);
        assert_eq!(after.direct.len(), 2);
        assert_eq!(after.cbv_address_slots, vec![(1, 0), (2, 0)]);
    }

    #[test]
    fn groups_by_slot_preserves_direct_order_and_first_descriptor_match() {
        let used = [
            // Non-table entries are ignored.
            UsedResource {
                kind: ResourceType::Cbv,
                root: RootType::Descriptor,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 0,
                dwords: 4,
                user_data: 0,
                slot: 0,
            },
            table_used(3, 16, ResourceType::Srv),
            table_used(1, 0, ResourceType::Cbv),
            table_used(3, 16, ResourceType::Uav), // duplicate descriptor 2: first wins
            table_used(3, 8, ResourceType::Uav),
        ];
        let indexed = [RootIndexedView {
            table_slot: 3,
            constant_slot: 2,
            constant_dword: 1,
            byte_offset: 32,
            shift: 5,
        }];
        let half = [(3, 16), (5, 8), (3, 16)];
        let plan = TablePlan::new(&used, &indexed, &half, &[]);

        // Groups sorted by slot, including half-only slot 5.
        assert_eq!(
            plan.groups.iter().map(|g| g.slot).collect::<Vec<_>>(),
            vec![1, 3, 5]
        );
        // Direct order matches original declaration order of table entries.
        let direct_pairs: Vec<(u8, u32, ResourceType)> = plan
            .direct
            .iter()
            .map(|&(g, e)| {
                let u = &plan.groups[g].entries[e];
                (u.slot, u.table_offset_dwords, u.kind)
            })
            .collect();
        assert_eq!(
            direct_pairs,
            vec![
                (3, 16, ResourceType::Srv),
                (1, 0, ResourceType::Cbv),
                (3, 16, ResourceType::Uav),
                (3, 8, ResourceType::Uav),
            ]
        );

        let g1 = &plan.groups[0];
        assert!(g1.has_cbv && !g1.has_uav && g1.max_dword == Some(0));
        assert!(g1.indexed.is_empty() && g1.half.is_empty());

        let g3 = &plan.groups[1];
        assert!(!g3.has_cbv && g3.has_uav && g3.max_dword == Some(16));
        assert_eq!(g3.indexed.len(), 1);
        assert_eq!(g3.half, vec![16]);
        assert!(g3.has_half(16) && !g3.has_half(8));
        // Descriptor 2 maps to the first declaration (Srv), matching linear find().
        assert_eq!(g3.entry(2).unwrap().kind, ResourceType::Srv);
        assert_eq!(g3.entry(1).unwrap().kind, ResourceType::Uav);
        assert!(g3.entry(0).is_none());

        let g5 = &plan.groups[2];
        assert!(!g5.has_cbv && !g5.has_uav && g5.max_dword.is_none());
        assert_eq!(g5.half, vec![8]);
        assert!(g5.has_half(8));
    }
}
