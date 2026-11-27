//! Where a program's code table starts (`VICTORY_CONST_TABLE_TRIM=1`).
//!
//! Ported from Marvisak/VIctory#2143 (desplmfao), as taken by fxceless/victory-fork PR #1006 and,
//! behind this switch, by 3cvil/M1LK `de0bc3e5e0`; GPL-3.0.

use crate::ir::Program;
use crate::resource::Address;

/// `VICTORY_CONST_TABLE_TRIM=1`: the constant table [`super::Lower::code_dword`] reads starts
/// where the program's instructions end ([`code_table_start`]) rather than at the lowest offset
/// any load names. A load at a computed offset names offset 0, so the table was the whole
/// program (16-18 KiB in the title's lighting computes reached at world entry), and NVIDIA's
/// compiler keeps an array over 16 KiB in every thread's local memory (Xid 13 MMU faults on an
/// RTX 5090 under vkd3d, GBs of video memory reserved on an RTX 5080). Off by default (the old
/// start). Part of the shader cache key (the build script lists every switch it finds here).
pub(super) fn const_table_trim() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_CONST_TABLE_TRIM.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_CONST_TABLE_TRIM").is_ok_and(|v| v.trim() == "1"))
}

#[cfg(test)]
thread_local! {
    /// A test's choice for [`const_table_trim`], for its thread only.
    pub(super) static TEST_CONST_TABLE_TRIM: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Where a program's constant table starts in its code: where its instructions end, unless a
/// load at a fixed offset reads below that (then at `lowest`, the lowest offset any load or
/// descriptor names). The program's constants follow its instructions, and a load at a computed
/// offset reads them, not instruction bytes. A computed index below the start is clamped as one
/// past the end is.
pub(super) fn code_table_start(p: &Program, addresses: &[&Address], lowest: u32) -> u32 {
    let code_end = p
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .map(|i| i.machine.offset + 4 * i.machine.words.len())
        .max()
        .unwrap_or(0) as u32;
    let fixed_below = addresses
        .iter()
        .any(|a| a.dynamic.is_none() && a.offset < code_end);
    if fixed_below {
        lowest
    } else {
        lowest.max(code_end & !3)
    }
}

#[cfg(test)]
mod tests {
    use super::{TEST_CONST_TABLE_TRIM, code_table_start, const_table_trim};
    use crate::{ir, resource};

    const S_ENDPGM: u32 = 0xBF81_0000;

    /// A program's constant table starts after its instructions, unless a load at a fixed
    /// offset reads below that.
    #[test]
    fn the_code_table_starts_after_the_instructions() {
        // s_nop 0, s_endpgm, then 1,000 dwords of constants.
        let mut words = vec![0xBF80_0000, S_ENDPGM];
        words.extend(0..1000u32);
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let p = ir::build(&code, false, 256).unwrap();
        let at = |offset: u32, dynamic: bool| resource::Address {
            param: resource::CODE,
            offset,
            dynamic: dynamic.then_some(ir::Value(0)),
        };
        let (computed, fixed_after, fixed_inside) = (at(0, true), at(12, false), at(4, false));
        // A load at a computed offset from the code's start: the table is the constants only.
        assert_eq!(code_table_start(&p, &[&computed], 0), 8);
        assert_eq!(code_table_start(&p, &[&computed, &fixed_after], 0), 8);
        // A load at a fixed offset inside the instructions keeps the start.
        assert_eq!(code_table_start(&p, &[&computed, &fixed_inside], 0), 0);
        // Loads past the instructions only: from the lowest of them.
        assert_eq!(code_table_start(&p, &[&fixed_after], 12), 12);
    }

    /// The switch reads its test override, so a test can turn the trim on for its thread.
    #[test]
    fn the_code_table_trim_follows_its_switch() {
        let was = TEST_CONST_TABLE_TRIM.replace(Some(true));
        assert!(const_table_trim());
        TEST_CONST_TABLE_TRIM.set(Some(false));
        assert!(!const_table_trim());
        TEST_CONST_TABLE_TRIM.set(was);
    }
}
