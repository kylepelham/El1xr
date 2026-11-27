//! (bootfast) `VICTORY_DSTORAGE_REUSE=1` (off by default): a worker keeps
//! its buffers from one request to the next instead of allocating and
//! zeroing new ones for each (the compressed source read from the file, the
//! zlib output BCPack then unpacks, the bounce buffer of a plain read, the
//! buffer of a write into pages that are not one run). At 5,000-11,000
//! requests a second that is 10,000-30,000 allocations a second, the larger
//! ones fresh pages from the system that fault in on their first write.
//! The bytes are the same: each buffer is cut to the request's size and
//! zeroed as before, only its memory is not new.

use std::cell::RefCell;
use std::sync::OnceLock;

/// The buffers a request uses at once, one slot each.
#[derive(Clone, Copy)]
pub(super) enum Slot {
    Source = 0,
    Packed,
    Bounce,
}

/// A buffer larger than this goes back to the system after its request.
const KEEP_AT_MOST: usize = 64 << 20;

pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DSTORAGE_REUSE").is_some_and(|v| v == "1"))
}

thread_local! {
    static KEPT: RefCell<[Vec<u8>; 3]> = const { RefCell::new([Vec::new(), Vec::new(), Vec::new()]) };
}

/// `size` zeroed bytes for `slot`: this worker's kept buffer with the switch,
/// else a new one.
pub(super) fn take(slot: Slot, size: usize) -> Vec<u8> {
    if !on() {
        return vec![0; size];
    }
    let mut buffer = KEPT.with_borrow_mut(|kept| std::mem::take(&mut kept[slot as usize]));
    buffer.clear();
    buffer.resize(size, 0);
    buffer
}

/// Gives `buffer` back to `slot` for the next request (with the switch).
pub(super) fn give(slot: Slot, buffer: Vec<u8>) {
    if on() && buffer.capacity() <= KEEP_AT_MOST {
        KEPT.with_borrow_mut(|kept| kept[slot as usize] = buffer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_buffer_taken_is_zeroed_and_of_its_size() {
        let mut b = take(Slot::Source, 10);
        assert_eq!(b, vec![0; 10]);
        b.fill(7);
        give(Slot::Source, b);
        // Whether kept or not, the next one is zeroed again.
        assert_eq!(take(Slot::Source, 4), vec![0; 4]);
        assert_eq!(take(Slot::Packed, 0), Vec::<u8>::new());
    }
}
