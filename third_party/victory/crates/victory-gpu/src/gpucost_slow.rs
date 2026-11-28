//! Slow list runs, one by one (`VICTORY_GPU_COST_SLOW_MS=<ms>`, with
//! `VICTORY_GPU_COST`; off by default). The cost table sums labels over
//! seconds and leaves out a list run of over a second ("over a second" in
//! its runs not counted), so the run that holds the card's fence for
//! seconds at world entry (issue #78) never shows its commands. With the
//! switch, every run longer than the value is said on a line of its own
//! when its stamps are read (at the list's next reset): its length, our
//! list number (`victory list #N`, as the hang watch names it), and its
//! slowest stretches, each with its place among the run's stamps and its
//! label (at `VICTORY_GPU_COST=2` the title command's kind and shader
//! hashes). Every line starts with `gpu cost slow run:`.


/// Stretches named per slow run.
const TOP: usize = 8;

/// `VICTORY_GPU_COST_SLOW_MS`: the run length past which a run is said, in
/// ticks of a card whose timestamps tick `frequency` times a second.
pub(crate) fn threshold(frequency: u64) -> Option<u64> {
    let ms = slow_ms()?;
    Some((ms * frequency as f64 / 1e3) as u64)
}

/// Live (0 bits: unset; a set one is above 0).
fn slow_ms() -> Option<f64> {
    static MS: victory_core::settings::Live =
        victory_core::settings::Live::new("VICTORY_GPU_COST_SLOW_MS", |v| parse(v).map_or(0, f64::to_bits));
    let bits = MS.get();
    (bits != 0).then(|| f64::from_bits(bits))
}

fn parse(value: Option<&str>) -> Option<f64> {
    value?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|ms| ms.is_finite() && *ms > 0.0)
}

/// The `TOP` longest stretches of a run: (index of the stretch's first
/// stamp, its label, its ticks), longest first. `stamps` has one more entry
/// than `labels` and is in order.
fn slowest<'a, L: AsRef<str>>(labels: &'a [L], stamps: &[u64]) -> Vec<(usize, &'a str, u64)> {
    let mut rows: Vec<(usize, &str, u64)> = labels
        .iter()
        .zip(stamps.windows(2))
        .enumerate()
        .map(|(i, (l, w))| (i, l.as_ref(), w[1] - w[0]))
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.2));
    rows.truncate(TOP);
    rows
}

/// The line for a run of list `id` (0: one of ours without a number).
fn line<L: AsRef<str>>(id: u64, labels: &[L], stamps: &[u64], frequency: u64) -> String {
    let ms = |ticks: u64| ticks as f64 * 1e3 / frequency.max(1) as f64;
    let span = stamps[stamps.len() - 1] - stamps[0];
    let list = if id == 0 {
        "a list of ours".to_owned()
    } else {
        format!("victory list #{id}")
    };
    let rows = slowest(labels, stamps)
        .into_iter()
        .map(|(i, l, t)| {
            format!(
                "{:.2} ms at stamp {}/{}: {}",
                ms(t),
                i + 1,
                labels.len(),
                l.chars().take(200).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join(" | ");
    format!(
        "gpu cost slow run: {list} ran {:.2} ms on the card ({} stamps, first label {}); the slowest: {rows}",
        ms(span),
        labels.len(),
        labels[0].as_ref().chars().take(80).collect::<String>()
    )
}

/// Says a run of list `id` if it is longer than the switch's value.
/// `stamps` are all written and in order, one more than `labels`.
pub(crate) fn note<L: AsRef<str>>(id: u64, labels: &[L], stamps: &[u64], frequency: u64) {
    let Some(limit) = threshold(frequency) else {
        return;
    };
    if labels.is_empty() || stamps.len() != labels.len() + 1 {
        return;
    }
    if stamps[stamps.len() - 1] - stamps[0] > limit {
        tracing::warn!("{}", line(id, labels, stamps, frequency));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_threshold_is_live() {
        victory_core::settings::check_live("VICTORY_GPU_COST_SLOW_MS", "2.5", slow_ms, None, Some(2.5));
    }

    #[test]
    fn reads_the_milliseconds() {
        assert_eq!(parse(Some("500")), Some(500.0));
        assert_eq!(parse(Some(" 2.5 ")), Some(2.5));
        assert_eq!(parse(Some("0")), None);
        assert_eq!(parse(Some("-3")), None);
        assert_eq!(parse(Some("x")), None);
        assert_eq!(parse(None), None);
    }

    #[test]
    fn names_the_longest_stretches_first_with_their_place() {
        let labels = [
            "title passes",
            "indirect [vertex a, pixel b]",
            "draw [vertex c]",
        ];
        let stamps = [0, 10, 5_010, 5_030];
        let rows = slowest(&labels, &stamps);
        assert_eq!(rows[0], (1, "indirect [vertex a, pixel b]", 5_000));
        assert_eq!(rows[1], (2, "draw [vertex c]", 20));
        assert_eq!(rows[2], (0, "title passes", 10));
    }

    #[test]
    fn keeps_only_the_top_stretches() {
        let labels: Vec<String> = (0..20).map(|i| format!("l{i}")).collect();
        let stamps: Vec<u64> = (0..21).map(|i| i * i).collect();
        let rows = slowest(&labels, &stamps);
        assert_eq!(rows.len(), TOP);
        assert_eq!(rows[0].1, "l19");
    }

    #[test]
    fn the_line_says_the_list_and_milliseconds() {
        let labels = ["title passes", "indirect [vertex a, pixel b]"];
        // A card that ticks a million times a second: 3 s in the draw.
        let stamps = [0, 1_000, 3_001_000];
        let text = line(641, &labels, &stamps, 1_000_000);
        assert!(
            text.starts_with("gpu cost slow run: victory list #641 ran 3001.00 ms"),
            "{text}"
        );
        assert!(
            text.contains("3000.00 ms at stamp 2/2: indirect [vertex a, pixel b]"),
            "{text}"
        );
        assert!(line(0, &labels, &stamps, 1_000_000).contains("a list of ours"));
    }
}
