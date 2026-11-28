//! (texflicker, #96, b5 part Q `04_face`) `VICTORY_SHADOW_TILE_WATCH=1`
//! (live, diag; needs `VICTORY_FLICKER_CATCH=1`): each frame, the title's
//! sun-shadow tile programs that ran, were dropped or took another indirect
//! path than in the frame before, as one `shadow tiles:` line (nothing when
//! nothing changed).
//!
//! In `train_b5q/04_face` the diner window showed screen-aligned dark
//! squares at 42 s (f782; about 57 px in the 1920x1080 snap, ~115 px at
//! 4K, soft-edged under the depth-of-field blur, gone by f785), and at 66 s
//! (f1179) hard-edged rectangles of sunlight with the blinds' stripes lay on
//! Lucia's hair and hoodie in a room every other run shows dark at that
//! shot. Both look like screen tiles of the sun's shadow that were not
//! resolved that frame: no texture tile, page or BC block lies square on
//! the screen at the same size across depths. The title's shadows are
//! tiled (`CS_ShadowTileClassify`, `CS_ShadowTileBalanceRTCost`,
//! `CS_ShadowRenderTiled*`, `CS_SetShadowMaskIndirectArgs`), part of them
//! traced against the ray-tracing structures (`CS_RTDistantShadowTrace*32_Tiled`
//! with a 4X temporal upsample: 32-texel tiles at a quarter of the half
//! resolution, 128 px at 4K; `CS_TraceShadowRays_*`, `CS_RaytraceShadows_Mixed`),
//! where every ray is a miss under `VICTORY_BVH_NO_HIT=1` (lit).
//!
//! Names watched: any program whose name holds `Shadow`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::Summary;

pub(crate) fn on() -> bool {
    static ON: victory_core::settings::Live =
        victory_core::settings::Live::flag("VICTORY_SHADOW_TILE_WATCH");
    ON.get_bool()
}

fn parts() -> &'static [String] {
    static PARTS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    PARTS.get_or_init(|| vec!["Shadow".to_owned()])
}

/// One watched program in one frame: its name, runs, drops, last drop's
/// reason, indirect paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Seen {
    pub(crate) name: Arc<str>,
    pub(crate) ran: u32,
    pub(crate) dropped: u32,
    pub(crate) why: &'static str,
    pub(crate) paths: u8,
}

/// Per pipeline: its name if watched (`None`: not watched).
static NAMES: Mutex<Option<HashMap<usize, Option<Arc<str>>>>> = Mutex::new(None);
/// The frame before: its number and its watched programs, by name.
static BEFORE: Mutex<Option<(u64, Vec<Seen>)>> = Mutex::new(None);

/// The watched programs of `s`, by name (`name` names a pipeline).
pub(crate) fn seen(s: &Summary, name: &dyn Fn(usize) -> String, parts: &[String]) -> Vec<Seen> {
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    let names = names.get_or_insert_with(HashMap::new);
    let mut out: Vec<Seen> = s
        .programs
        .iter()
        .filter_map(|&(p, ran, dropped)| {
            let n = names
                .entry(p)
                .or_insert_with(|| {
                    let n = name(p);
                    parts
                        .iter()
                        .any(|w| n.contains(w.as_str()))
                        .then(|| Arc::from(n))
                })
                .clone()?;
            Some(Seen {
                name: n,
                ran,
                dropped,
                why: if dropped > 0 { s.why_of(p) } else { "" },
                paths: s.paths_of(p),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// What changed from `before` to `now`: `name ran B->N dropped B->N (why)
/// paths B->N`, `+name` new, `-name` gone; at most `most` named.
pub(crate) fn changes(before: &[Seen], now: &[Seen], most: usize) -> Vec<String> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < before.len() || j < now.len() {
        let order = match (before.get(i), now.get(j)) {
            (Some(b), Some(n)) => b.name.cmp(&n.name),
            (Some(_), None) => std::cmp::Ordering::Less,
            _ => std::cmp::Ordering::Greater,
        };
        match order {
            std::cmp::Ordering::Less => {
                let b = &before[i];
                out.push(format!("-{} (ran {} dropped {})", b.name, b.ran, b.dropped));
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                let n = &now[j];
                out.push(format!("+{} {}", n.name, counts(None, n)));
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                let (b, n) = (&before[i], &now[j]);
                if (b.ran, b.dropped, b.paths) != (n.ran, n.dropped, n.paths) {
                    out.push(format!("{} {}", n.name, counts(Some(b), n)));
                }
                i += 1;
                j += 1;
            }
        }
    }
    if out.len() > most {
        let more = out.len() - most;
        out.truncate(most);
        out.push(format!("+{more} more"));
    }
    out
}

fn counts(before: Option<&Seen>, n: &Seen) -> String {
    let why = if n.dropped > 0 {
        format!(" ({})", n.why)
    } else {
        String::new()
    };
    match before {
        Some(b) => format!(
            "ran {}->{} dropped {}->{}{why} paths {}->{}",
            b.ran,
            n.ran,
            b.dropped,
            n.dropped,
            super::path_names(b.paths),
            super::path_names(n.paths)
        ),
        None => format!(
            "ran {} dropped {}{why} paths {}",
            n.ran,
            n.dropped,
            super::path_names(n.paths)
        ),
    }
}

/// Frame `s` is presented: its line, if a watched program changed.
pub(super) fn frame(s: &Summary) {
    if !on() {
        return;
    }
    let now = seen(s, &super::named, parts());
    let before = BEFORE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace((s.frame, now.clone()));
    let Some((was, before)) = before else {
        return;
    };
    let changed = changes(&before, &now, 16);
    if changed.is_empty() {
        return;
    }
    tracing::info!(
        "shadow tiles: frame {} vs {was}: {} changed [{}]; watched now={} ran={} dropped={}",
        s.frame,
        changed.len(),
        changed.join("; "),
        now.len(),
        now.iter().map(|n| u64::from(n.ran)).sum::<u64>(),
        now.iter().map(|n| u64::from(n.dropped)).sum::<u64>()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(name: &str, ran: u32, dropped: u32, paths: u8) -> Seen {
        Seen {
            name: Arc::from(name),
            ran,
            dropped,
            why: if dropped > 0 { "no pipeline" } else { "" },
            paths,
        }
    }

    #[test]
    fn the_switch_is_off_unless_set() {
        assert!(!on());
    }

    #[test]
    fn only_what_changed_is_named() {
        let before = vec![
            s("CS_RTDistantShadowTrace32_Tiled", 1, 0, 1),
            s("CS_ShadowRenderTiledCascades", 4, 0, 0),
            s("CS_ShadowTileClassify", 1, 0, 0),
        ];
        let now = vec![
            s("CS_RTDistantShadowTrace32_Tiled", 0, 1, 0),
            s("CS_SetShadowMaskIndirectArgs", 1, 0, 0),
            s("CS_ShadowTileClassify", 1, 0, 0),
        ];
        assert_eq!(
            changes(&before, &now, 16),
            vec![
                "CS_RTDistantShadowTrace32_Tiled ran 1->0 dropped 0->1 (no pipeline) paths raw->-",
                "+CS_SetShadowMaskIndirectArgs ran 1 dropped 0 paths -",
                "-CS_ShadowRenderTiledCascades (ran 4 dropped 0)",
            ]
        );
        assert!(changes(&now, &now, 16).is_empty());
    }

    #[test]
    fn the_changes_are_capped() {
        let before: Vec<Seen> = (0..20)
            .map(|i| s(&format!("CS_Shadow{i:02}"), 1, 0, 0))
            .collect();
        let now: Vec<Seen> = (0..20)
            .map(|i| s(&format!("CS_Shadow{i:02}"), 2, 0, 0))
            .collect();
        let c = changes(&before, &now, 4);
        assert_eq!(c.len(), 5);
        assert_eq!(c[4], "+16 more");
    }

    #[test]
    fn programs_are_watched_by_name() {
        let summary = Summary {
            frame: 9,
            programs: vec![(0x10, 2, 0), (0x20, 0, 3), (0x30, 1, 0)],
            why: vec![(0x20, "no pipeline")],
            ..Summary::default()
        };
        let name = |p: usize| match p {
            0x10 => "[compute 01 CS_ShadowTileClassify_1_Wrapped]".to_owned(),
            0x20 => "[compute 02 CS_TraceShadowRays_Full_2_Wrapped]".to_owned(),
            _ => "[compute 03 CS_MainBlur_3_Wrapped]".to_owned(),
        };
        let parts = vec!["Shadow".to_owned()];
        let seen = seen(&summary, &name, &parts);
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[0].name.as_ref(),
            "[compute 01 CS_ShadowTileClassify_1_Wrapped]"
        );
        assert_eq!(
            (seen[1].ran, seen[1].dropped, seen[1].why),
            (0, 3, "no pipeline")
        );
    }
}
