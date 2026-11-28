//! (El1xr) Teleport: saved spots and the native that moves the player there. The set-coords native
//! is not pinned offline; the finder ([`super::finder`]) names it in game by reading the player's
//! position back, or `VICTORY_MOD_MENU_TP_NATIVE=<hash>:<slots>` names it at start. Until then
//! the Go to row says so and calls nothing.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::model::{NATIVE_GET_ENTITY_COORDS, NATIVE_PLAYER_PED_ID};
use super::queue::{Call, Queue};

/// A native call as scripts make it (`None`: not in the table).
pub type Native<'a> = dyn FnMut(u64, &[u64]) -> Option<[u64; 4]> + 'a;

/// The teleport native (0: not known) and its argument slots.
static TP_HASH: AtomicU64 = AtomicU64::new(0);
static TP_SLOTS: AtomicU64 = AtomicU64::new(0);

/// Saved spots, chosen by number (1..=SPOTS).
pub const SPOTS: usize = 3;
/// The Save rows' one value: a button (the row shows the spot, so each press is a change).
pub const SAVE: &[&str] = &["save"];
/// The Go to rows' one value.
pub const GO: &[&str] = &["go"];
static SAVED: Mutex<[Option<[f32; 3]>; SPOTS]> = Mutex::new([None; SPOTS]);
static LAST: Mutex<String> = Mutex::new(String::new());

/// The teleport native, if known: (hash, argument slots).
pub fn native() -> Option<(u64, usize)> {
    let h = TP_HASH.load(Relaxed);
    (h != 0).then(|| (h, TP_SLOTS.load(Relaxed) as usize))
}

pub fn set_native(hash: u64, slots: usize) {
    TP_SLOTS.store(slots as u64, Relaxed);
    TP_HASH.store(hash, Relaxed);
}

/// `VICTORY_MOD_MENU_TP_NATIVE=<hash hex>:<slots>`.
pub fn parse_native(v: &str) -> Option<(u64, usize)> {
    let (h, s) = v.trim().split_once(':')?;
    let h = u64::from_str_radix(
        h.trim().trim_start_matches("0x").replace('_', "").as_str(),
        16,
    )
    .ok()?;
    let s: usize = s.trim().parse().ok()?;
    (h != 0 && (4..=super::natives::MAX_ARGS).contains(&s)).then_some((h, s))
}

/// Four floats packed into two slots, as vector arguments and results are.
pub fn pack([x, y, z]: [f32; 3]) -> [u64; 2] {
    [
        u64::from(x.to_bits()) | u64::from(y.to_bits()) << 32,
        u64::from(z.to_bits()),
    ]
}

pub fn unpack([a, b, ..]: [u64; 4]) -> [f32; 3] {
    let f = |v: u64| f32::from_bits(v as u32);
    [f(a), f(a >> 32), f(b)]
}

pub fn show([x, y, z]: [f32; 3]) -> String {
    format!("{x:.1} {y:.1} {z:.1}")
}

/// The player ped's handle (two slots).
pub fn player(call: &mut Native) -> Option<[u64; 2]> {
    let [lo, hi, ..] = call(NATIVE_PLAYER_PED_ID, &[])?;
    (lo | hi != 0).then_some([lo, hi])
}

pub fn coords(call: &mut Native, ped: [u64; 2]) -> Option<[f32; 3]> {
    call(NATIVE_GET_ENTITY_COORDS, &[ped[0], ped[1], 0]).map(unpack)
}

/// Calls `hash` with `slots` argument slots: the ped, the position, then zeros.
pub fn move_to(call: &mut Native, hash: u64, slots: usize, ped: [u64; 2], to: [f32; 3]) -> bool {
    let mut args = vec![0u64; slots.max(4)];
    args[..2].copy_from_slice(&ped);
    args[2..4].copy_from_slice(&pack(to));
    call(hash, &args).is_some()
}

fn note(s: String) {
    tracing::info!("mod menu: {s}");
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = s;
}

fn slot(args: &[u64]) -> Option<usize> {
    let i = *args.first()? as usize;
    (1..=SPOTS).contains(&i).then(|| i - 1)
}

/// Script: saves the player's position in spot `args[0]`.
pub fn save_script(call: &mut Native, args: &[u64]) -> Option<[u64; 4]> {
    let i = slot(args)?;
    let ped = player(call)?;
    let at = coords(call, ped)?;
    SAVED.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(at);
    note(format!("teleport saved spot {} at {}", i + 1, show(at)));
    Some([1, 0, 0, 0])
}

/// Script: moves the player to spot `args[0]` and reads the position back.
pub fn goto_script(call: &mut Native, args: &[u64]) -> Option<[u64; 4]> {
    let i = slot(args)?;
    let Some(to) = SAVED.lock().unwrap_or_else(|e| e.into_inner())[i] else {
        note(format!("teleport spot {} is empty; save it first", i + 1));
        return None;
    };
    let Some((hash, slots)) = native() else {
        note("teleport native not found yet; run Find teleport".into());
        return None;
    };
    let ped = player(call)?;
    move_to(call, hash, slots, ped, to);
    let now = coords(call, ped)?;
    note(format!(
        "teleport to spot {} {} via {hash:#018x}: now {} (off by {:.2} m)",
        i + 1,
        show(to),
        show(now),
        dist(now, to)
    ));
    Some([1, 0, 0, 0])
}

pub fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(p, q)| (p - q) * (p - q))
        .sum::<f32>()
        .sqrt()
}

/// Queues `script` for spot `spot` (1-based) on the game thread.
pub fn push(queue: &Queue, name: &'static str, script: super::queue::Script, spot: u64) -> bool {
    queue.push(Call {
        name,
        hash: NATIVE_PLAYER_PED_ID,
        args: vec![spot],
        script: Some(script),
    })
}

/// Spot `spot` (1-based) as its rows show it.
pub fn spot(spot: usize) -> String {
    let saved = SAVED.lock().unwrap_or_else(|e| e.into_inner())[spot - 1];
    saved.map_or_else(|| "empty".into(), show)
}

macro_rules! spot_rows {
    ($($n:literal: $save:ident $goto:ident $get:ident),*) => {$(
        pub fn $save(_: &str) {
            push(&super::QUEUE, "tp save", save_script, $n);
        }
        pub fn $goto(_: &str) {
            push(&super::QUEUE, "tp goto", goto_script, $n);
        }
        pub fn $get() -> String {
            spot($n)
        }
    )*};
}

spot_rows!(1: save_1 goto_1 spot_1, 2: save_2 goto_2 spot_2, 3: save_3 goto_3 spot_3);

/// What the last save or teleport did.
pub fn last_get() -> String {
    let last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last.is_empty() {
        "-".into()
    } else {
        last.clone()
    }
}

/// The teleport native, for its status row.
pub fn native_status() -> String {
    match native() {
        Some((h, s)) => format!("{h:#018x} ({s} slots)"),
        None => "not found yet: run Find teleport".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natives_parse_with_their_slot_count() {
        assert_eq!(
            parse_native("0x4a65_60c6_18a1_1572:7"),
            Some((0x4a65_60c6_18a1_1572, 7))
        );
        assert_eq!(
            parse_native("4a6560c618a11572:4"),
            Some((0x4a65_60c6_18a1_1572, 4))
        );
        assert_eq!(
            parse_native("4a6560c618a11572:3"),
            None,
            "a handle and a vector at least"
        );
        assert_eq!(parse_native("4a6560c618a11572"), None);
        assert_eq!(parse_native("zz:5"), None);
    }

    #[test]
    fn vectors_pack_as_the_coords_native_returns_them() {
        let v = [-1293.5, 400.25, 1.9];
        let [a, b] = pack(v);
        assert_eq!(unpack([a, b, 0, 0]), v);
        assert_eq!(a as u32, (-1293.5f32).to_bits());
    }

    #[test]
    fn saved_spots_teleport_through_the_found_native_and_read_back() {
        let mut pos = [10.0f32, 20.0, 5.0];
        let ped = [0x11, 0x22];
        let mut seen = Vec::new();
        let mut call = |h: u64, a: &[u64]| -> Option<[u64; 4]> {
            seen.push((h, a.to_vec()));
            match h {
                NATIVE_PLAYER_PED_ID => Some([ped[0], ped[1], 0, 0]),
                NATIVE_GET_ENTITY_COORDS => {
                    let [x, y] = pack(pos);
                    Some([x, y, 0, 0])
                }
                0x77 => {
                    pos = unpack([a[2], a[3], 0, 0]);
                    Some([0; 4])
                }
                _ => None,
            }
        };
        assert!(save_script(&mut call, &[1]).is_some());
        assert!(save_script(&mut call, &[4]).is_none(), "spots 1-3");
        assert!(goto_script(&mut call, &[2]).is_none(), "spot 2 is empty");
        TP_HASH.store(0, Relaxed);
        assert!(goto_script(&mut call, &[1]).is_none(), "no native yet");
        assert!(last_get().contains("not found"));
        set_native(0x77, 6);
        drop(call);
        pos = [99.0, 99.0, 99.0];
        let mut seen2 = Vec::new();
        let mut call = |h: u64, a: &[u64]| -> Option<[u64; 4]> {
            seen2.push((h, a.to_vec()));
            match h {
                NATIVE_PLAYER_PED_ID => Some([ped[0], ped[1], 0, 0]),
                NATIVE_GET_ENTITY_COORDS => {
                    let [x, y] = pack(pos);
                    Some([x, y, 0, 0])
                }
                0x77 => {
                    pos = unpack([a[2], a[3], 0, 0]);
                    Some([0; 4])
                }
                _ => None,
            }
        };
        assert!(goto_script(&mut call, &[1]).is_some());
        assert_eq!(pos, [10.0, 20.0, 5.0], "back at the saved spot");
        let tp = seen2.iter().find(|(h, _)| *h == 0x77).unwrap();
        assert_eq!(tp.1.len(), 6, "the native's slot count");
        assert_eq!(&tp.1[..2], &ped, "the ped first");
        assert_eq!(&tp.1[4..], &[0, 0], "zero flags");
        assert!(last_get().contains("off by 0.00 m"), "{}", last_get());
        assert_eq!(spot_1(), "10.0 20.0 5.0");
        assert_eq!(spot(2), "empty");
        assert!(!seen.is_empty());
    }
}
