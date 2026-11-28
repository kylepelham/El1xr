//! Synthetic builder dispatches against `crate::bvh_declare`'s marks.

use super::{CELL, Cells, HOOK_READ, HOOK_WRITTEN, ROOT_OUT, pick_check, to_declare, written};

/// Title addresses, each at the start of its own 4 MiB cell, two cells
/// apart (a mark also takes the cell after).
const DEST: u64 = 0x10_0000_0000;
const SCRATCH: u64 = DEST + 2 * CELL;
const SOURCE: u64 = DEST + 4 * CELL;
const VERTICES: u64 = DEST + 6 * CELL;
const INDICES: u64 = DEST + 8 * CELL;
const TRANSFORM: u64 = DEST + 10 * CELL;
const HEADERS: u64 = DEST + 12 * CELL;
const FAULTED: u64 = DEST + 14 * CELL;

/// The copies a list holds (windows of one cell each), and per copy the
/// generation it became written in, as `crate::memory` keeps them.
fn judge(cells: &Cells, copies: &[u64], written_at: &mut [u64], generation: &mut u64) {
    for (at, &start) in written_at.iter_mut().zip(copies) {
        if *at == 0 && cells.span_written(start, CELL) {
            *generation += 1;
            *at = *generation;
        }
    }
}

/// A batched bottom-level build: the hook names destination, source,
/// scratch, transform, indices and vertices, of which destination and
/// scratch are written; the builder dispatch's root arguments name the
/// destination, the vertices and a table of headers the hook never named.
/// Written: destination and scratch with the cell after each, and the
/// headers' (a root output); read only: source, vertices, indices,
/// transform; the copy only a fault made is not declared.
#[test]
fn a_builder_dispatch_declares_only_its_outputs() {
    let cells = Cells::new();
    let named = [DEST, SOURCE, SCRATCH, TRANSFORM, INDICES, VERTICES];
    cells.hook(&[DEST, SCRATCH], &named);
    assert!(cells.root_output(DEST + 0x100));
    assert!(!cells.root_output(VERTICES + 0x40));
    assert!(cells.root_output(HEADERS));
    let copies = [
        DEST,
        DEST + CELL,
        SCRATCH,
        SCRATCH + CELL,
        SOURCE,
        VERTICES,
        INDICES,
        TRANSFORM,
        HEADERS,
        HEADERS + CELL,
        FAULTED,
    ];
    let (mut written_at, mut generation) = (vec![0; copies.len()], 0);
    judge(&cells, &copies, &mut written_at, &mut generation);
    let declared: Vec<u64> = to_declare(&written_at, None)
        .into_iter()
        .map(|n| copies[n])
        .collect();
    assert_eq!(
        declared,
        [DEST, DEST + CELL, SCRATCH, SCRATCH + CELL, HEADERS, HEADERS + CELL]
    );
    // Without the switch every numbered copy was declared: 11, not 6.
    assert!(declared.len() < copies.len());
    for read_only in [SOURCE, VERTICES, INDICES, TRANSFORM, FAULTED] {
        assert!(!cells.span_written(read_only, CELL), "{read_only:#x}");
    }
}

/// A later dispatch of the same list names a new output: only that copy is
/// declared then; a list that has declared at the newest generation
/// declares nothing more.
#[test]
fn a_list_declares_each_written_copy_once() {
    let cells = Cells::new();
    cells.hook(&[DEST], &[DEST, VERTICES]);
    let copies = [DEST, VERTICES, FAULTED];
    let (mut written_at, mut generation) = (vec![0; 3], 0);
    judge(&cells, &copies, &mut written_at, &mut generation);
    assert_eq!(to_declare(&written_at, None), [0]);
    let seen = generation;
    assert!(to_declare(&written_at, Some(seen)).is_empty());
    assert!(cells.root_output(FAULTED + 8));
    judge(&cells, &copies, &mut written_at, &mut generation);
    assert_eq!(to_declare(&written_at, Some(seen)), [2]);
    // The vertices stay read only even when a root argument names them.
    assert!(!cells.root_output(VERTICES));
    judge(&cells, &copies, &mut written_at, &mut generation);
    assert_eq!(written_at[1], 0);
}

/// An update builds in place: its source is its destination, written; a
/// range the hook names as both is written; a root output that later turns
/// out to be named only as an input is no longer written by its cell.
#[test]
fn outputs_win_over_inputs() {
    assert!(written(HOOK_WRITTEN | HOOK_READ));
    assert!(written(ROOT_OUT));
    assert!(!written(ROOT_OUT | HOOK_READ));
    assert!(!written(HOOK_READ));
    assert!(!written(0));
    let cells = Cells::new();
    cells.hook(&[DEST], &[DEST, DEST]);
    assert!(cells.span_written(DEST, CELL));
    assert!(cells.root_output(DEST));
    // A window that crosses into a written cell is written.
    assert!(cells.span_written(DEST - 0x1000, 0x2000));
    assert!(!cells.span_written(DEST - CELL, CELL));
    // Past the 40-bit space: nothing marked, nothing panics.
    cells.mark(1 << 41, HOOK_WRITTEN);
    assert_eq!(cells.get(1 << 41), 0);
}

/// The check goes round the copies that are not written.
#[test]
fn the_check_goes_round_the_undeclared_copies() {
    let written_at = [3, 0, 5, 0, 0];
    assert_eq!(pick_check(&written_at, 0), Some(1));
    assert_eq!(pick_check(&written_at, 1), Some(3));
    assert_eq!(pick_check(&written_at, 3), Some(4));
    assert_eq!(pick_check(&written_at, 4), Some(1));
    assert_eq!(pick_check(&[1, 2], 0), None);
    assert_eq!(pick_check(&[], 0), None);
}

/// A landing in the copy under check is counted once; in another copy, or
/// of no bytes, not at all.
#[test]
fn a_landing_in_the_checked_copy_is_counted() {
    let [armed, found] = super::checks();
    super::arm(77, 9, false);
    super::landed(78, 0, CELL, 0, 64);
    super::landed(77, 0, CELL, 0, 0);
    assert_eq!(super::checks(), [armed + 1, found]);
    super::landed(77, 0, CELL, 0x40, 64);
    super::landed(77, 0, CELL, 0x80, 64);
    assert_eq!(super::checks(), [armed + 1, found + 1]);
}

/// The addresses a builder dispatch's root arguments name, as
/// `crate::memory` names them: views, a pair of constants, 256-byte units.
#[test]
fn root_arguments_name_views_pairs_and_units() {
    let named = crate::memory::named(&[DEST], &[0x2000_0000, 0x10, 0x5]);
    assert_eq!(named, [DEST, 0x10_2000_0000, 0x20_0000_0000]);
}
