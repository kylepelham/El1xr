//! Per-pipeline descriptor table plan (`TablePlan`), grouped once when a
//! shader is translated instead of re-scanning `UsedResource` declarations at
//! every draw or dispatch bind.
//!
//! Adapted from `avatarmanz/VIctory-7900XTX` (`46e3297818`, originally from
//! `codec-d/cool_fox` `641835a4`) with per-slot `half_views` grouping.

use victory_shader::resource::RootIndexedView;
use victory_shader::{ResourceType, RootType, UsedResource};

/// Shader interface metadata shared by every pipeline variant and draw.
pub(crate) struct TablePlan {
    pub(crate) groups: Vec<TableGroup>,
    /// Direct entries can interleave slots. Preserve their declaration order
    /// when recording resource reads, writes, and transitions.
    pub(crate) direct: Vec<(usize, usize)>,
}

pub(crate) struct TableGroup {
    pub(crate) slot: u8,
    pub(crate) has_cbv: bool,
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
    pub(crate) fn new(
        used: &[UsedResource],
        indexed: &[RootIndexedView],
        half: &[(u8, u32)],
    ) -> Self {
        let mut slots: Vec<u8> = used
            .iter()
            .filter(|u| u.root == RootType::Table)
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
        for &u in used.iter().filter(|u| u.root == RootType::Table) {
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
        Self { groups, direct }
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
        let plan = TablePlan::new(&used, &indexed, &half);

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
