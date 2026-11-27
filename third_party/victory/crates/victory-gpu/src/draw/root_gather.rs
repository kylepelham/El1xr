//! The root views and root constants a shader that reads title memory by
//! address is given (`State::memory_ready`), gathered on the stack: no
//! allocation per draw while they fit, as most do.
//!
//! Source: fxceless/victory-fork #1443 (`ff63c74`, "reuse descriptor table
//! scratch buffers and stack roots in bind"). Its table scratch reuse was
//! here already (`State::scratch_made`, `scratch_selected`, `TablePlan`);
//! this is its stack roots part.

use super::Root;

/// Values kept on the stack before a gather spills to the heap.
const STACK: usize = 64;

/// A list of `T` that lives on the stack up to [`STACK`] values and on the
/// heap past that, in the order pushed.
pub(super) struct StackList<T: Copy + Default> {
    stack: [T; STACK],
    len: usize,
    spilled: Vec<T>,
}

impl<T: Copy + Default> StackList<T> {
    pub(super) fn new() -> Self {
        Self {
            stack: [T::default(); STACK],
            len: 0,
            spilled: Vec::new(),
        }
    }

    pub(super) fn extend_from_slice(&mut self, values: &[T]) {
        if self.spilled.is_empty() && self.len + values.len() <= STACK {
            self.stack[self.len..self.len + values.len()].copy_from_slice(values);
            self.len += values.len();
            return;
        }
        if self.spilled.is_empty() {
            self.spilled.extend_from_slice(&self.stack[..self.len]);
        }
        self.spilled.extend_from_slice(values);
    }

    pub(super) fn as_slice(&self) -> &[T] {
        if self.spilled.is_empty() {
            &self.stack[..self.len]
        } else {
            &self.spilled
        }
    }
}

/// The root views' addresses and the root constants of `roots`, in root
/// order, as `crate::memory::ready` takes them.
pub(super) fn gather(roots: &[Root]) -> (StackList<u64>, StackList<u32>) {
    let mut addresses = StackList::new();
    let mut constants = StackList::new();
    for root in roots {
        match root {
            Root::View(address) => addresses.extend_from_slice(std::slice::from_ref(address)),
            Root::Constants(values) => constants.extend_from_slice(values),
            _ => {}
        }
    }
    (addresses, constants)
}

#[cfg(test)]
mod tests {
    use super::{Root, STACK, StackList, gather};

    /// What the gather did before: two vectors.
    fn gather_vec(roots: &[Root]) -> (Vec<u64>, Vec<u32>) {
        let (mut addresses, mut constants) = (Vec::new(), Vec::new());
        for root in roots {
            match root {
                Root::View(address) => addresses.push(*address),
                Root::Constants(values) => constants.extend_from_slice(values),
                _ => {}
            }
        }
        (addresses, constants)
    }

    fn same(roots: &[Root]) {
        let (addresses, constants) = gather(roots);
        let (want_addresses, want_constants) = gather_vec(roots);
        assert_eq!(addresses.as_slice(), &want_addresses[..]);
        assert_eq!(constants.as_slice(), &want_constants[..]);
    }

    #[test]
    fn the_stack_gather_equals_the_vector_gather() {
        same(&[]);
        same(&[Root::None, Root::Table(0x40)]);
        same(&[
            Root::View(0x1000),
            Root::Constants(vec![1, 2, 3]),
            Root::Table(8),
            Root::View(0x2000),
            Root::Constants(vec![]),
            Root::Constants(vec![4]),
        ]);
    }

    #[test]
    fn past_the_stack_it_spills_in_order() {
        // Constants exactly filling the stack, then one more set.
        let full: Vec<u32> = (0..STACK as u32).collect();
        same(&[Root::Constants(full.clone())]);
        same(&[Root::Constants(full.clone()), Root::Constants(vec![7, 8])]);
        // One set larger than the stack on its own.
        let big: Vec<u32> = (0..(STACK as u32 * 3)).collect();
        same(&[
            Root::Constants(vec![1]),
            Root::Constants(big),
            Root::Constants(vec![9]),
        ]);
        // More views than the stack holds.
        let views: Vec<Root> = (0..(STACK as u64 + 5))
            .map(|i| Root::View(i * 256))
            .collect();
        same(&views);
    }

    #[test]
    fn a_list_on_the_stack_does_not_allocate() {
        let mut list: StackList<u32> = StackList::new();
        list.extend_from_slice(&[1, 2, 3]);
        assert_eq!(list.spilled.capacity(), 0);
        assert_eq!(list.as_slice(), &[1, 2, 3]);
    }
}
