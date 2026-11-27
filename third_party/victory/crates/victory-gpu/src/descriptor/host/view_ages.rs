//! How long texture slots wait for the view the title wrote last
//! (`VICTORY_VIEW_AGES=1`; off unless set). A slot written with a view of a
//! texture that has no host copy yet either reads nothing (null: the surface
//! is black) or keeps showing the view it showed (`VICTORY_KEEP_VIEWS`,
//! `VICTORY_KEEP_OTHER_VIEWS`: a smaller or another texture) until a draw or
//! the texture pass makes the new one. A slot that waits long is a surface
//! that is black, blurred or wrong for that long (#55: the prison cell's top
//! bunk black, then back with a flat green block).
//!
//! Logged: `view ages: shown after <ms>` for every slot that waited at least
//! a second, and every five seconds `view ages: waiting` with the counts by
//! kind and the six longest waits, each with the new view and the old one
//! (resource size, mips and format; view format, dimension, channel mapping
//! and mips; address).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::View;

/// The switch (`VICTORY_VIEW_AGES`; on only when `1`).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| switch(std::env::var_os("VICTORY_VIEW_AGES").as_deref()))
}

fn switch(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v == "1")
}

/// What a waiting slot shows meanwhile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Nothing (a null view).
    Null,
    /// The view of the same texture it showed (fewer mips).
    KeptSame,
    /// A view of another texture.
    KeptOther,
    /// Nothing, for good: the host could not make the view.
    Failed,
}

struct Wait {
    kind: Kind,
    since: u64,
    new: String,
    old: String,
}

/// Waiting slots by heap and index.
static WAITS: Mutex<Option<HashMap<(usize, u32), Wait>>> = Mutex::new(None);
/// When the last summary was logged.
static LAST_SUMMARY: AtomicU64 = AtomicU64::new(0);
/// Logged at least so long a wait.
const LONG_MS: u64 = 1000;
const SUMMARY_MS: u64 = 5000;

/// Slot `index` of heap `heap` waits for `new`, showing `kind` meanwhile
/// (`old`: what it showed). Counted from the first time only.
pub(crate) fn waiting(heap: usize, index: u32, kind: Kind, new: &View, old: Option<&View>) {
    let now = crate::host::now_ms();
    {
        let mut waits = WAITS.lock().unwrap_or_else(|e| e.into_inner());
        let entry = waits.get_or_insert_with(HashMap::new).entry((heap, index));
        let wait = entry.or_insert_with(|| Wait {
            kind,
            since: now,
            new: text(Some(new)),
            old: text(old),
        });
        wait.kind = kind;
    }
    summary(now);
}

/// Slot `index` of heap `heap` shows the view it was written with.
pub(crate) fn shown(heap: usize, index: u32) {
    let now = crate::host::now_ms();
    let done = {
        let mut waits = WAITS.lock().unwrap_or_else(|e| e.into_inner());
        waits.as_mut().and_then(|w| w.remove(&(heap, index)))
    };
    if let Some(w) = done {
        let ms = now.saturating_sub(w.since);
        if ms >= LONG_MS {
            tracing::info!(
                ms,
                kind = ?w.kind,
                heap = format_args!("{heap:#x}"),
                index,
                new = %w.new,
                old = %w.old,
                "view ages: shown after"
            );
        }
    }
    summary(now);
}

/// Every [`SUMMARY_MS`]: the waits by kind and the longest ones.
fn summary(now: u64) {
    let last = LAST_SUMMARY.load(Relaxed);
    if now < last + SUMMARY_MS
        || LAST_SUMMARY
            .compare_exchange(last, now, Relaxed, Relaxed)
            .is_err()
    {
        return;
    }
    let waits = WAITS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(waits) = waits.as_ref() else { return };
    let (counts, longest) = tally(waits.iter().map(|(k, w)| (*k, w.kind, w.since)), now);
    tracing::info!(
        null = counts[0],
        kept_same = counts[1],
        kept_other = counts[2],
        failed = counts[3],
        "view ages: waiting"
    );
    for ((heap, index), kind, ms) in longest {
        let w = &waits[&(heap, index)];
        tracing::info!(
            ms,
            ?kind,
            heap = format_args!("{heap:#x}"),
            index,
            new = %w.new,
            old = %w.old,
            "view ages: waiting long"
        );
    }
}

/// Counts by kind (null, kept same, kept other, failed) and the six longest
/// waits of at least [`LONG_MS`], longest first.
fn tally(
    waits: impl Iterator<Item = ((usize, u32), Kind, u64)>,
    now: u64,
) -> ([usize; 4], Vec<((usize, u32), Kind, u64)>) {
    let mut counts = [0; 4];
    let mut all = Vec::new();
    for (key, kind, since) in waits {
        counts[kind as usize] += 1;
        let ms = now.saturating_sub(since);
        if ms >= LONG_MS {
            all.push((key, kind, ms));
        }
    }
    all.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    all.truncate(6);
    (counts, all)
}

/// A view in a few words: for a texture the title placed, the resource's
/// size, mips and format, the view's format, dimension, channel mapping and
/// first mip and mip count, and the address.
pub(crate) fn text(view: Option<&View>) -> String {
    match view {
        None => "none".into(),
        Some(View::Placed(unordered, r, d)) => format!(
            "placed uav={unordered} {}x{}x{} mips {} fmt {} view fmt {} dim {} map {:#x} mip {}+{} at {:#x}",
            u64::from(r[4]) | u64::from(r[5]) << 32,
            r[6],
            r[7] & 0xffff,
            r[7] >> 16,
            r[8],
            d[0],
            d[1],
            d[2],
            d[4],
            d[5],
            u64::from(d[10]) | u64::from(d[11]) << 32
        ),
        Some(View::Shader(r, Some(d))) => format!(
            "shader {r:#x} view fmt {} dim {} map {:#x} mip {}+{}",
            d[0], d[1], d[2], d[4], d[5]
        ),
        Some(v) => format!("{v:?}").chars().take(80).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_on_only_when_one() {
        assert!(!switch(None));
        assert!(!switch(Some("0".as_ref())));
        assert!(switch(Some("1".as_ref())));
    }

    #[test]
    fn tally_counts_kinds_and_keeps_the_six_longest() {
        let now = 20_000;
        let waits = (0..10u32).map(|i| {
            let kind = [Kind::Null, Kind::KeptSame, Kind::KeptOther, Kind::Failed][i as usize % 4];
            ((1usize, i), kind, now - u64::from(i) * 1000)
        });
        let (counts, longest) = tally(waits, now);
        assert_eq!(counts, [3, 3, 2, 2]);
        // 0 waited 0 ms: not long. 9 waited longest.
        let order: Vec<u32> = longest.iter().map(|(k, ..)| k.1).collect();
        assert_eq!(order, vec![9, 8, 7, 6, 5, 4]);
        assert_eq!(longest[0].2, 9000);
    }

    #[test]
    fn a_placed_view_names_size_format_mapping_and_mips() {
        let mut r = [0u32; 14];
        r[0] = 3;
        r[4] = 512;
        r[6] = 256;
        r[7] = 1 | 10 << 16;
        r[8] = 71;
        let mut d = [0u32; 16];
        d[0] = 72;
        d[1] = 4;
        d[2] = 0x1688;
        d[4] = 2;
        d[5] = 8;
        d[10] = 0x1000;
        let t = text(Some(&View::Placed(false, r, d)));
        assert_eq!(
            t,
            "placed uav=false 512x256x1 mips 10 fmt 71 view fmt 72 dim 4 map 0x1688 mip 2+8 at 0x1000"
        );
        assert_eq!(text(None), "none");
    }
}
