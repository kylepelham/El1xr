//! `VICTORY_CLAMP_AFTER_WRITER`: what a list knows of its indirect records (`San`) and the
//! records the title was seen to read (`learn`). The passes themselves need a card.

use super::{LEARNED, San, WHY, after_writer, learn, min_stride};

const DRAW: u32 = 0;
const INDEXED: u32 = 1;
const MESH: u32 = 10;

fn covered(san: &mut San, at: usize, kind: u32) {
    san.passed.insert(1, san.seq);
    san.clean.insert(at, (kind, san.seq));
}

#[test]
fn off_unless_asked_for() {
    assert!(std::env::var_os("VICTORY_CLAMP_AFTER_WRITER").is_some() || !after_writer());
}

#[test]
fn a_record_never_covered_is_not_clean() {
    let san = San::default();
    assert!(!san.is_clean(0x1000, DRAW, 16));
}

#[test]
fn a_covered_record_is_clean_until_a_later_write_overlaps_it() {
    let mut san = San::default();
    san.wrote(0x1000, 0x2000);
    covered(&mut san, 0x1100, DRAW);
    assert!(san.is_clean(0x1100, DRAW, 16));
    san.wrote(0x1108, 0x1110);
    assert!(!san.is_clean(0x1100, DRAW, 16));
}

#[test]
fn a_write_declared_before_the_pass_does_not_dirty_it() {
    let mut san = San::default();
    san.wrote(0x1100, 0x1110);
    covered(&mut san, 0x1100, DRAW);
    assert!(san.is_clean(0x1100, DRAW, 16));
}

#[test]
fn writes_beside_a_record_leave_it_clean() {
    let mut san = San::default();
    covered(&mut san, 0x1100, INDEXED);
    san.wrote(0x1000, 0x1100);
    san.wrote(0x1114, 0x1200);
    assert!(san.is_clean(0x1100, INDEXED, 20));
    san.wrote(0x1113, 0x1114);
    assert!(!san.is_clean(0x1100, INDEXED, 20));
}

#[test]
fn a_record_is_clean_only_as_the_kind_it_was_covered_as() {
    let mut san = San::default();
    covered(&mut san, 0x1100, DRAW);
    assert!(!san.is_clean(0x1100, INDEXED, 20));
    assert!(!san.is_clean(0x1100, MESH, 12));
}

#[test]
fn the_log_names_at_most_eight_ranges() {
    let mut san = San::default();
    for i in 0..20 {
        san.wrote(i * 0x100, i * 0x100 + 0x10);
    }
    assert_eq!(san.ranges().len(), 8);
}

#[test]
fn every_kind_the_title_reads_has_a_record_size() {
    assert_eq!(min_stride(DRAW), Some(16));
    assert_eq!(min_stride(INDEXED), Some(20));
    assert_eq!(min_stride(MESH), Some(12));
    assert_eq!(min_stride(7), None);
}

#[test]
fn learning_a_record_counts_it_unseen_once() {
    let at = 0x7fff_0000_usize;
    LEARNED.lock().unwrap().1.remove(&at);
    let before = WHY[0].load(std::sync::atomic::Ordering::Relaxed);
    learn(at, DRAW);
    learn(at, DRAW);
    let after = WHY[0].load(std::sync::atomic::Ordering::Relaxed);
    assert!(after > before, "the first sight is counted");
    assert!(LEARNED.lock().unwrap().1.get(&at).is_some_and(|v| v.0 == DRAW));
}
