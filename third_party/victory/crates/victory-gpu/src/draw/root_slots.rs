/// A draw's root slots are byte-sized. Enumerate distinct slots in root order.
#[derive(Default)]
pub(super) struct RootSlots([u64; 4]);

impl RootSlots {
    pub(super) fn insert(&mut self, slot: u8) {
        self.0[usize::from(slot / 64)] |= 1u64 << (slot % 64);
    }

    pub(super) fn contains(&self, slot: u8) -> bool {
        self.0[usize::from(slot / 64)] & (1u64 << (slot % 64)) != 0
    }

    #[cfg(test)]
    pub(super) fn ordered(self) -> impl Iterator<Item = u8> {
        self.0.into_iter().enumerate().flat_map(|(i, mut bits)| {
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let slot = i as u8 * 64 + bits.trailing_zeros() as u8;
                bits &= bits - 1;
                Some(slot)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::RootSlots;

    #[test]
    fn mixed_roots_match_sorted_unique_slots() {
        let mut slots = RootSlots::default();
        assert_eq!(RootSlots::default().ordered().count(), 0);
        let mut expected = vec![255, 192, 191, 128, 127, 64, 63, 0, 64, 192, 0];
        for &slot in &expected {
            slots.insert(slot);
        }
        expected.sort_unstable();
        expected.dedup();
        for slot in 0..=255 {
            assert_eq!(slots.contains(slot), expected.contains(&slot));
        }
        assert_eq!(slots.ordered().collect::<Vec<_>>(), expected);
    }
}
