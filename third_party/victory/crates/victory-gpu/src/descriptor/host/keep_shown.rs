//! (trustno1 `d1c412c`) `VICTORY_KEEP_SHOWN=1|2`: which texture a slot written
//! with a view of another texture may go on showing until the new one is made
//! (`VICTORY_KEEP_OTHER_VIEWS`, Neptune's, on by default, keeps any texture).
//! `1`: only one of the same kind, moved or in another size of the same
//! shape (a title streaming a texture in); `2`: any of the same dimension;
//! unset: any texture, as before. A slot whose old texture may not stand in
//! is made null at once, as with `VICTORY_KEEP_OTHER_VIEWS=0`.
//!
//! (trustno1 PR #280) `checked` (or `3`): as `1`, except that a texture of
//! the same description placed elsewhere stands in only where the memories
//! of the two views begin and end with the same 4 KiB (the title moved it);
//! otherwise the slot is made null until the new one is uploaded. "The same
//! kind elsewhere" also matches a streamer giving the slot another texture
//! of that size and format (an albedo where a mask or another character's
//! map was): rainbow or garbage texels on a face until the upload lands.
//! (trustno1 shows a grey 1x1 texture meanwhile; our tree has no such
//! stand-in, so it reads as nothing, as every slot refused here does.)
//!
//! What a shader reads through a view of another dimension (a cube for a
//! plane, a volume for an array) is undefined in D3D12: such a stand-in may
//! show garbage where null shows black for a frame.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::View;

/// What the texture view a slot is written with has to do with the one it
/// shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kin {
    /// A view of the same placed texture.
    Same,
    /// A texture of the same description placed elsewhere.
    Moved,
    /// One of the same format and shape in another size.
    Sized,
    /// A view of the same dimension, of another texture.
    Other,
    /// A view no shader that reads the one can read.
    None,
}

impl Kin {
    fn kept(self, keep: u8) -> bool {
        match self {
            Kin::Same => true,
            Kin::Moved | Kin::Sized => keep >= 1,
            Kin::Other => keep == 2,
            Kin::None => false,
        }
    }
}

fn kin(old: &View, new: &View) -> Kin {
    if super::same_placed_texture(old, new) {
        return Kin::Same;
    }
    if let (View::Placed(false, a, from), View::Placed(false, b, to)) = (old, new) {
        if a[0] == 1 || b[0] == 1 || from[1] != to[1] {
            return Kin::None;
        }
        // (Width, 64 bits, height, depth and levels, format: words 4 to 8.)
        if a[0] == b[0] && a[2..13] == b[2..13] {
            return Kin::Moved;
        }
        let (wide, high) = ([a[4], b[4]].map(u64::from), [a[6], b[6]].map(u64::from));
        return if from[0] == to[0] && a[0] == b[0] && wide[0] * high[1] == wide[1] * high[0] {
            Kin::Sized
        } else {
            Kin::Other
        };
    }
    let dimension = |view: &View| match view {
        View::Placed(false, resource, desc) if resource[0] != 1 => desc[1],
        View::Shader(r, Some(desc))
            if crate::resource::buffer_bytes(*r as crate::object::This).is_none() =>
        {
            desc[1]
        }
        _ => 0,
    };
    match (dimension(old), dimension(new)) {
        (0, _) | (_, 0) => Kin::None,
        (a, b) if a == b => Kin::Other,
        _ => Kin::None,
    }
}

/// `VICTORY_KEEP_SHOWN=checked` (or `3`).
const CHECKED: u8 = 3;

/// `VICTORY_KEEP_SHOWN`: 0 unset (any texture stands in), 1, 2, or
/// [`CHECKED`].
fn keep_shown() -> u8 {
    static KEEP: victory_core::settings::Live =
        victory_core::settings::Live::new("VICTORY_KEEP_SHOWN", |v| u64::from(parse(v)));
    KEEP.get() as u8
}

fn parse(value: Option<&str>) -> u8 {
    match value.map(str::trim) {
        Some("checked" | "3") => CHECKED,
        v => v.and_then(|v| v.parse::<u8>().ok()).map_or(0, |v| v.min(2)),
    }
}

/// Since the start, of the slots `VICTORY_KEEP_OTHER_VIEWS` would keep: kept
/// moved, sized, others; made null as their old texture could not stand in.
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Since the start, under [`CHECKED`]: moved textures whose memories began
/// and ended alike (kept), and those whose memories differed or could not
/// be read (made null).
static MOVED_CHECKED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Whether a slot showing `old`, written with `new` (another texture), may
/// go on showing it: always unless `VICTORY_KEEP_SHOWN` is set.
pub(super) fn may_stand_in(old: &View, new: &View) -> bool {
    let keep = keep_shown();
    if keep == 0 {
        return true;
    }
    stand_in_as(old, new, keep)
}

fn stand_in_as(old: &View, new: &View, keep: u8) -> bool {
    stand_in_reading(old, new, keep, |at, out| crate::host::read_into(at as *const u8, out))
}

fn stand_in_reading(old: &View, new: &View, keep: u8, read: impl Fn(u64, &mut [u8]) -> bool) -> bool {
    let kin = kin(old, new);
    let mut kept = kin.kept(keep);
    if kept && kin == Kin::Moved && keep == CHECKED {
        kept = moved_whole(old, new, read);
        MOVED_CHECKED[usize::from(!kept)].fetch_add(1, Relaxed);
    }
    let at = match (kept, kin) {
        (false, _) => 3,
        (true, Kin::Moved | Kin::Same) => 0,
        (true, Kin::Sized) => 1,
        (true, _) => 2,
    };
    let n = COUNTS[at].fetch_add(1, Relaxed) + 1;
    if n.is_power_of_two() && n >= 64 {
        let [moved, sized, other, refused] = COUNTS.each_ref().map(|c| c.load(Relaxed));
        let [alike, unlike] = MOVED_CHECKED.each_ref().map(|c| c.load(Relaxed));
        tracing::info!(
            "slots written with another texture, since the start: kept showing one moved {moved}, sized {sized}, \
             another of the same dimension {other}; made null as the old one could not stand in {refused} (VICTORY_KEEP_SHOWN={keep}); \
             moved ones checked: {alike} with the same ends kept, {unlike} with other ends made null"
        );
    }
    kept
}

/// Whether the memories of two views of one texture description placed in
/// two places begin and end with the same bytes (4 KiB at each end, all of
/// it under 8 KiB), of the mips both views show. False where either cannot
/// be read.
fn moved_whole(old: &View, new: &View, read: impl Fn(u64, &mut [u8]) -> bool) -> bool {
    let (View::Placed(false, resource, from), View::Placed(false, _, to)) = (old, new) else {
        return false;
    };
    let at = |d: &[u32; 16]| u64::from(d[10]) | u64::from(d[11]) << 32;
    let first = crate::resource::first_mip(from).max(crate::resource::first_mip(to));
    let Some(bytes) = crate::resource::placed_bytes_from(resource, first) else {
        return false;
    };
    same_ends(at(from), at(to), bytes, read)
}

/// Whether `[a, a + bytes)` and `[b, b + bytes)` hold the same first and
/// last 4 KiB (all of them under 8 KiB), read by `read` (false: unreadable).
fn same_ends(a: u64, b: u64, bytes: u64, read: impl Fn(u64, &mut [u8]) -> bool) -> bool {
    const END: u64 = 4096;
    if a == 0 || b == 0 || bytes == 0 {
        return false;
    }
    let ends = if bytes <= 2 * END { [(0, bytes), (0, 0)] } else { [(0, END), (bytes - END, END)] };
    let (mut x, mut y) = ([0u8; 2 * END as usize], [0u8; 2 * END as usize]);
    ends.into_iter().filter(|&(_, len)| len > 0).all(|(offset, len)| {
        let len = len as usize;
        read(a + offset, &mut x[..len]) && read(b + offset, &mut y[..len]) && x[..len] == y[..len]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_live() {
        victory_core::settings::check_live("VICTORY_KEEP_SHOWN", "2", keep_shown, 0, 2);
        victory_core::settings::check_live("VICTORY_KEEP_SHOWN", "checked", keep_shown, 0, CHECKED);
    }

    fn placed(resource: [u32; 14], format: u32, dimension: u32, address: u32) -> View {
        let mut desc = [0u32; 16];
        (desc[0], desc[1], desc[10], desc[11]) = (format, dimension, address, 0xb);
        View::Placed(false, resource, desc)
    }

    #[test]
    fn the_switch_is_off_unless_one_or_two() {
        assert_eq!(parse(None), 0);
        assert_eq!(parse(Some("x")), 0);
        assert_eq!(parse(Some("1")), 1);
        assert_eq!(parse(Some(" 2")), 2);
        assert_eq!(parse(Some("9")), 2);
        assert_eq!(parse(Some("checked")), CHECKED);
        assert_eq!(parse(Some(" 3")), CHECKED);
    }

    /// (trustno1 PR #280) A texture of the same description elsewhere stands
    /// for the new one only where both memories begin and end alike, not
    /// where a streamer gave the slot another texture of that size and
    /// format, nor where a memory cannot be read.
    #[test]
    fn a_moved_texture_is_kept_only_with_the_same_ends() {
        // One texture's memory, the same moved (its middle may still
        // differ: only the ends are compared), another of that size whose
        // last byte differs.
        let memory: Vec<u8> = (0..64 * 1024u32).map(|i| (i * 31 + i / 4096) as u8).collect();
        let mut moved = memory.clone();
        let mut other = memory.clone();
        moved[20_000] ^= 0xff;
        other[64 * 1024 - 1] ^= 1;
        let all = [memory, moved, other];
        let read = |at: u64, out: &mut [u8]| {
            let Some(bytes) = all.get(((at >> 32) as usize).wrapping_sub(1)) else {
                return false;
            };
            let offset = (at & 0xffff_ffff) as usize;
            bytes.get(offset..offset + out.len()).map(|src| out.copy_from_slice(src)).is_some()
        };
        let base = |i: u64| (i + 1) << 32;
        let len = 64 * 1024;
        assert!(same_ends(base(0), base(1), len, read));
        assert!(!same_ends(base(0), base(2), len, read));
        assert!(!same_ends(base(0), base(7), len, read));
        assert!(!same_ends(0, base(1), len, read));
        assert!(same_ends(base(0), base(1), 6000, read));
        assert!(!same_ends(base(0), base(2), 64 * 1024 - 1 + 8000, read), "past the end: unreadable");
    }

    /// `checked` asks the memories of a moved texture only; sized ones stand
    /// in as at 1, others of the same dimension do not (as at 1), and 0, 1
    /// and 2 never read memory.
    #[test]
    fn checked_reads_the_memory_of_moved_textures_alone() {
        let texture = [3, 0, 0, 0, 64, 0, 64, 0x1_0001, 28, 1, 0, 0x105, 0, 0];
        let bytes = crate::resource::placed_bytes_from(&texture, 0).expect("a 64x64 texture's size");
        let old = placed(texture, 28, 4, 0x1000_0000);
        let moved = placed(texture, 28, 4, 0x2000_0000);
        let mut half = texture;
        (half[4], half[6]) = (32, 32);
        let sized = placed(half, 28, 4, 0x2000_0000);
        let mut wide = texture;
        wide[4] = 128;
        let other = placed(wide, 28, 4, 0x2000_0000);
        let asked = std::cell::Cell::new(0u32);
        let alike = |_: u64, out: &mut [u8]| {
            asked.set(asked.get() + 1);
            out.fill(7);
            true
        };
        assert!(stand_in_reading(&old, &moved, CHECKED, alike));
        assert!(asked.get() > 0 && bytes > 0);
        asked.set(0);
        let n = std::cell::Cell::new(0u8);
        let unlike = |_: u64, out: &mut [u8]| {
            n.set(n.get().wrapping_add(1));
            out.fill(n.get());
            true
        };
        assert!(!stand_in_reading(&old, &moved, CHECKED, unlike));
        assert!(!stand_in_reading(&old, &moved, CHECKED, |_, _| false), "unreadable: not kept");
        assert!(stand_in_reading(&old, &sized, CHECKED, alike));
        assert!(!stand_in_reading(&old, &other, CHECKED, alike));
        for keep in 0..=2 {
            assert_eq!(stand_in_reading(&old, &moved, keep, alike), keep >= 1);
        }
        assert_eq!(asked.get(), 0, "only a moved texture under checked reads memory");
    }

    /// What may stand for a texture that is not there yet: one of its kind
    /// in another place or size; another one only at 2; nothing a shader
    /// cannot read in its place.
    #[test]
    fn a_slot_shows_on_what_can_stand_for_its_new_texture() {
        let texture = [3, 0, 0, 0, 512, 0, 256, 0x8_0001, 98, 1, 0, 0x105, 0, 0];
        let old = placed(texture, 98, 4, 0x1000_0000);
        let moved = placed(texture, 98, 4, 0x2000_0000);
        let mut half = texture;
        (half[4], half[6], half[7]) = (256, 128, 0x7_0001);
        let sized = placed(half, 98, 4, 0x2000_0000);
        let mut square = texture;
        square[6] = 512;
        let other = placed(square, 98, 4, 0x2000_0000);
        let cube = placed(texture, 98, 9, 0x2000_0000);
        assert_eq!(kin(&old, &moved), Kin::Moved);
        assert_eq!(kin(&old, &sized), Kin::Sized);
        assert_eq!(kin(&old, &other), Kin::Other);
        assert_eq!(kin(&old, &cube), Kin::None);
        assert!(stand_in_as(&old, &moved, 1) && stand_in_as(&old, &sized, 1));
        assert!(!stand_in_as(&old, &other, 1) && stand_in_as(&old, &other, 2));
        assert!(!stand_in_as(&old, &cube, 2));
    }
}
