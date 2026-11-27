//! (trustno1 `d1c412c`) `VICTORY_KEEP_SHOWN=1|2`: which texture a slot written
//! with a view of another texture may go on showing until the new one is made
//! (`VICTORY_KEEP_OTHER_VIEWS`, Neptune's, on by default, keeps any texture).
//! `1`: only one of the same kind, moved or in another size of the same
//! shape (a title streaming a texture in); `2`: any of the same dimension;
//! unset: any texture, as before. A slot whose old texture may not stand in
//! is made null at once, as with `VICTORY_KEEP_OTHER_VIEWS=0`.
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
            Kin::Other => keep >= 2,
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

/// `VICTORY_KEEP_SHOWN`: 0 unset (any texture stands in).
fn keep_shown() -> u8 {
    static KEEP: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *KEEP.get_or_init(|| parse(std::env::var("VICTORY_KEEP_SHOWN").ok().as_deref()))
}

fn parse(value: Option<&str>) -> u8 {
    value
        .and_then(|v| v.trim().parse::<u8>().ok())
        .map_or(0, |v| v.min(2))
}

/// Since the start, of the slots `VICTORY_KEEP_OTHER_VIEWS` would keep: kept
/// moved, sized, others; made null as their old texture could not stand in.
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

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
    let kin = kin(old, new);
    let kept = kin.kept(keep);
    let at = match (kept, kin) {
        (false, _) => 3,
        (true, Kin::Moved | Kin::Same) => 0,
        (true, Kin::Sized) => 1,
        (true, _) => 2,
    };
    let n = COUNTS[at].fetch_add(1, Relaxed) + 1;
    if n.is_power_of_two() && n >= 64 {
        let [moved, sized, other, refused] = COUNTS.each_ref().map(|c| c.load(Relaxed));
        tracing::info!(
            "slots written with another texture, since the start: kept showing one moved {moved}, sized {sized}, \
             another of the same dimension {other}; made null as the old one could not stand in {refused} (VICTORY_KEEP_SHOWN={keep})"
        );
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

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
