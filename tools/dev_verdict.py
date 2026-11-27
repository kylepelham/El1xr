"""`dev.py train --verdict`: KEEP / DROP / INCONCLUSIVE suggestions per set (its switches against the baseline).

Noise floors come from same-set repeats: for each figure, the largest spread (max - min) between runs of one set
that ran at least twice (any set: the baseline's repeats are the usual source), never under MIN_FLOOR. Without
any repeat the floors are DEFAULT_FLOOR (batch 3: differences under ~4 fps / ~8 ms prep were noise with builds
running). A set's median beyond the floor in its good direction counts as better, in the other as worse:

- a run of the set failed or was flagged (device removed, DEVICE_HUNG, panicked, WARN rise): DROP;
- worse only: DROP; better only: KEEP; both: INCONCLUSIVE (mixed); neither: INCONCLUSIVE (within noise).

Live arms are judged B against A with their own floor: the same-set spread across their segments.
These are suggestions; the operator still looks at the PNGs and the logs.
"""

from __future__ import annotations

import statistics

import dev_live

# (figure, +1 when higher is better / -1 when lower is)
METRICS = (
    ("fps", 1), ("prep_ms", -1), ("steady_p99_ms", -1), ("world_over250", -1), ("boot_s", -1), ("stream_s", -1),
)
# 0 means "not reached" for these: the figure is left out when either side has none.
ZERO_IS_NONE = {"steady_p99_ms"}
DEFAULT_FLOOR = {"fps": 4.0, "prep_ms": 8.0, "steady_p99_ms": 25.0, "world_over250": 25.0, "boot_s": 15.0,
                 "stream_s": 30.0}
MIN_FLOOR = {"fps": 0.5, "prep_ms": 1.0, "steady_p99_ms": 5.0, "world_over250": 3.0, "boot_s": 3.0, "stream_s": 5.0}
LIVE_DEFAULT_FLOOR = {"fps": 2.0, "prep_ms": 4.0, "p50_ms": 4.0, "p99_ms": 15.0, "over100_min": 3.0, "over250_min": 1.0}
LIVE_MIN_FLOOR = {"fps": 0.3, "prep_ms": 0.5, "p50_ms": 0.5, "p99_ms": 3.0, "over100_min": 0.5, "over250_min": 0.2}


def floors(done: dict[int, dict[str, object]]) -> tuple[dict[str, float], list[str]]:
    """The noise floor per figure and the sets whose repeats gave it (empty: the defaults)."""
    by_set: dict[str, list[dict[str, object]]] = {}
    for rec in done.values():
        if rec.get("ok") and rec.get("kind", "set") == "set":
            by_set.setdefault(str(rec["set"]), []).append(rec["stats"])
    repeated = {name: runs for name, runs in by_set.items() if len(runs) >= 2}
    if not repeated:
        return dict(DEFAULT_FLOOR), []
    out = {}
    for key, _ in METRICS:
        spreads = []
        for runs in repeated.values():
            values = [float(r.get(key, 0)) for r in runs]
            if key in ZERO_IS_NONE:
                values = [v for v in values if v > 0]
            if len(values) >= 2:
                spreads.append(max(values) - min(values))
        out[key] = max([MIN_FLOOR[key], *spreads]) if spreads else DEFAULT_FLOOR[key]
    return out, sorted(repeated)


def switches(sets: dict[str, dict[str, str]], name: str, baseline: str) -> str:
    """The set's values that differ from the baseline set's."""
    mine, base = sets.get(name, {}), sets.get(baseline, {})
    diff = [f"{k}={v}" for k, v in mine.items() if base.get(k) != v]
    diff += [f"{k}=(unset)" for k in base if k not in mine]
    return " ".join(diff) or "(same as baseline)"


def judge(deltas: dict[str, float], floor: dict[str, float], metrics) -> tuple[str, list[str]]:
    better, worse = [], []
    for key, sign in metrics:
        if key not in deltas:
            continue
        d = deltas[key]
        if sign * d > floor[key]:
            better.append(f"{key} {d:+.1f} (floor {floor[key]:.1f})")
        elif sign * d < -floor[key]:
            worse.append(f"{key} {d:+.1f} (floor {floor[key]:.1f})")
    if better and worse:
        return "INCONCLUSIVE", ["mixed: better " + ", ".join(better) + "; worse " + ", ".join(worse)]
    if worse:
        return "DROP", ["worse " + ", ".join(worse)]
    if better:
        return "KEEP", ["better " + ", ".join(better)]
    return "INCONCLUSIVE", ["within noise"]


def set_verdicts(
    sets: dict[str, dict[str, str]], baseline: str, done: dict[int, dict[str, object]], flagged: dict[str, list[str]]
) -> tuple[list[dict[str, object]], dict[str, float], list[str]]:
    floor, sources = floors(done)
    by_set: dict[str, list[dict[str, object]]] = {}
    for step in sorted(done):
        rec = done[step]
        if rec.get("kind", "set") == "set":
            by_set.setdefault(str(rec["set"]), []).append(rec)
    base_runs = [r["stats"] for r in by_set.get(baseline, []) if r.get("ok")]
    rows = []
    for name, recs in by_set.items():
        if name == baseline:
            continue
        row = {"set": name, "switches": switches(sets, name, baseline), "runs": len(recs)}
        failed = [f"{r['step']:02d} failed" for r in recs if not r.get("ok")] + flagged.get(name, [])
        good = [r["stats"] for r in recs if r.get("ok")]
        if failed:
            row["verdict"], row["why"] = "DROP", ["; ".join(failed)]
        elif not base_runs:
            row["verdict"], row["why"] = "INCONCLUSIVE", [f"no passed run of the baseline '{baseline}'"]
        else:
            deltas = {}
            for key, _ in METRICS:
                mine = [float(s.get(key, 0)) for s in good]
                base = [float(s.get(key, 0)) for s in base_runs]
                if key in ZERO_IS_NONE:
                    mine, base = [v for v in mine if v > 0], [v for v in base if v > 0]
                if mine and base:
                    deltas[key] = statistics.median(mine) - statistics.median(base)
            row["deltas"] = deltas
            row["verdict"], row["why"] = judge(deltas, floor, METRICS)
            if len(good) == 1:
                row["why"].append("1 run")
        rows.append(row)
    return rows, floor, sources


def live_verdict(result: dict[str, object]) -> dict[str, object]:
    spread = result.get("spread") or {}
    floor = {k: max(spread[k], LIVE_MIN_FLOOR[k]) if k in spread else LIVE_DEFAULT_FLOOR[k] for k, *_ in dev_live.METRICS}
    keys = " ".join(f"{k}={b}" for k, (_, b) in result["keys"].items())
    row = {"set": f"{result['b']} (live vs {result['a']})", "switches": keys,
           "runs": sum(1 for s in result["segments"] if "figures" in s)}
    if result["problems"]:
        row["verdict"], row["why"] = "INCONCLUSIVE", list(result["problems"])
    elif not result["delta"]:
        row["verdict"], row["why"] = "INCONCLUSIVE", ["no usable segment of one of the sets"]
    else:
        row["verdict"], row["why"] = judge(result["delta"], floor, [(k, s) for k, _, _, s in dev_live.METRICS])
        if not spread:
            row["why"].append("no same-set repeat: default floors")
    row["deltas"] = result["delta"]
    return row


def verdict_lines(
    rows: list[dict[str, object]], floor: dict[str, float], sources: list[str], plan_name: str, baseline: str
) -> list[str]:
    src = f"same-set repeats of {', '.join(sources)}" if sources else "defaults (no set ran twice)"
    lines = [f"# train {plan_name}: verdicts against {baseline} (suggestions)", "",
             "Noise floors (" + src + "): " + ", ".join(f"{k} {v:.1f}" for k, v in floor.items()), "",
             "| set | switches | runs | verdict | why |", "|---|---|---|---|---|"]
    for row in rows:
        why = "; ".join(row["why"]).replace("|", "\\|")
        lines.append(f"| {row['set']} | `{row['switches']}` | {row['runs']} | **{row['verdict']}** | {why} |")
    return lines
