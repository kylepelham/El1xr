"""`dev.py look PLAN ARM` (and the end of `train --quick`): one screen of an arm against the baseline.

The baseline is the median of the baseline set's passed runs (`plan.baseline`); greps and log
flags are set against its latest run. Shown: the figures (fps, prep, gpu, boot, settled, stream,
p99, frames over 100/250 ms per world minute, hitches, the longest hang watch stall, refused,
driver resets during the run), the plan's `[greps]` that matched in either log with the arm's
first matching line, the log's flags and hang lines, the arm's flagged visual sheets and black
frames (`autovis/visual.json`), and the verdict row of its set (`verdict.json`).
"""

from __future__ import annotations

import json
import statistics
from collections.abc import Callable
from pathlib import Path

import dev_autovis
import dev_greps
import dev_train

ANSI_STRIP = dev_greps.ANSI_RE
GREPS_SHOWN = 10
LINE_W = 150
HANG_SHOWN = 3
SHEETS_SHOWN = 6


def per_minute(stats: dict[str, object], key: str) -> float:
    minutes = float(stats.get("world_s", 0.0)) / 60.0
    return float(stats.get(key, 0)) / minutes if minutes > 0 else 0.0


# (label, figure of a record, format)
FIGURES: tuple[tuple[str, Callable[[dict[str, object]], float], str], ...] = (
    ("fps", lambda r: float(r["stats"].get("fps", 0)), ".1f"),
    ("prep ms", lambda r: float(r["stats"].get("prep_ms", 0)), ".1f"),
    ("gpu ms", lambda r: float(r["stats"].get("gpu_ms", 0)), ".1f"),
    ("boot s", lambda r: float(r["stats"].get("boot_s", 0)), ".1f"),
    ("settled s", lambda r: float(r["stats"].get("settled_s", 0)), ".1f"),
    ("stream s", lambda r: float(r["stats"].get("stream_s", 0)), ".1f"),
    ("p99 ms", lambda r: float(r["stats"].get("p99_ms", 0)), ".1f"),
    ("steady p99 ms", lambda r: float(r["stats"].get("steady_p99_ms", 0)), ".1f"),
    (">100 ms /min", lambda r: per_minute(r["stats"], "world_over100"), ".1f"),
    (">250 ms /min", lambda r: per_minute(r["stats"], "world_over250"), ".2f"),
    ("hitches", lambda r: float(r["stats"].get("hitches", 0)), ".0f"),
    ("worst ms", lambda r: float(r["stats"].get("worst_ms", 0)), ".0f"),
    ("hang stall s", lambda r: float(r["stats"].get("hang_stall_s", 0)), ".1f"),
    ("refused", lambda r: float(r["stats"].get("refused", 0)), ".0f"),
    ("driver resets", lambda r: float((r.get("resets_after") or 0) - (r.get("resets_before") or 0)), ".0f"),
)


def baseline_steps(plan: dev_train.Plan, done: dict[int, dict[str, object]]) -> list[int]:
    return [s for s in sorted(done) if done[s]["set"] == plan.baseline and done[s].get("ok")]


def last_baseline(plan: dev_train.Plan, done: dict[int, dict[str, object]]) -> int | None:
    steps = baseline_steps(plan, done)
    return steps[-1] if steps else None


def grep_hits(text: str, greps: dict) -> dict[str, tuple[int, str]]:
    """Per grep: matching lines and the first one."""
    out = {name: [0, ""] for name in greps}
    for raw in text.splitlines():
        line = ANSI_STRIP.sub("", raw)
        for name, pattern in greps.items():
            if pattern.search(line):
                out[name][0] += 1
                if not out[name][1]:
                    out[name][1] = line.strip()
    return {k: (v[0], v[1]) for k, v in out.items()}


def _status(rec: dict[str, object]) -> str:
    if rec.get("stopped"):
        return f"STOPPED ({rec['stopped']})"
    if rec.get("prewarm_failed"):
        return "FAILED (prewarm)"
    ended = rec.get("ended") or {}
    how = f", ended {ended['how']} at {ended['at_s']:.0f}s" if "at_s" in ended else ""
    tries = f", attempt {rec['attempt']}" if rec.get("attempt", 1) != 1 else ""
    return ("ok" if rec.get("ok") else "FAILED") + how + tries


def look(
    plan: dev_train.Plan, done: dict[int, dict[str, object]], train_dir: Path, rec: dict[str, object], rid: str,
    read_text: Callable[[Path], str], visual: dict[str, object] | None = None, base_step: int | None = None,
) -> list[str]:
    name = str(rec["set"])
    switches = " ".join(f"{k}={v}" for k, v in plan.sets.get(name, {}).items()
                        if plan.sets.get(plan.baseline, {}).get(k) != v) or "(baseline values)"
    bases = baseline_steps(plan, done)
    base_ids = [dev_train.run_id(s, plan.baseline) for s in bases]
    lines = [
        f"look {rid} (set {name}: {switches}): {_status(rec)}",
        f"baseline {plan.baseline}: median of {len(bases)} runs ({' '.join(base_ids) or 'none'})",
        "",
        f"{'':16}{'arm':>10}{'baseline':>10}{'delta':>10}",
    ]
    for label, get, spec in FIGURES:
        mine = get(rec)
        cells = f"{mine:>10{spec}}"
        if bases:
            base = statistics.median(get(done[s]) for s in bases)
            cells += f"{base:>10{spec}}{mine - base:>+10{spec}}"
        lines.append(f"{label:16}{cells}")
    arm_text = read_text(Path(str(rec["log"]))) if rec.get("log") else ""
    base_step = base_step if base_step is not None else (bases[-1] if bases else None)
    base_text = ""
    if base_step is not None and done[base_step].get("log"):
        base_text = read_text(Path(str(done[base_step]["log"])))
    lines += _grep_lines(plan, arm_text, base_text, base_step, done)
    scan = dev_greps.scan_log(arm_text, {})
    base_scan = dev_greps.scan_log(base_text, {}) if base_text else None
    base_warns = dev_greps.baseline_warns([base_scan]) if base_scan else None
    found = dev_greps.flags(scan, base_warns, name == plan.baseline)
    lines += ["", "flags: " + (" / ".join(found) if found else "none")]
    for line in scan.hang[:HANG_SHOWN]:
        lines.append(f"  {line[:LINE_W]}")
    lines += _visual_lines(train_dir, rid, visual)
    lines += _verdict_lines(train_dir, name, plan.baseline)
    return lines


def _grep_lines(plan, arm_text: str, base_text: str, base_step: int | None, done) -> list[str]:
    if not plan.greps:
        return []
    arm_hits = grep_hits(arm_text, plan.greps)
    base_hits = grep_hits(base_text, plan.greps) if base_text else {}
    base_id = dev_train.run_id(base_step, str(done[base_step]["set"])) if base_step is not None else "-"
    hit = [n for n in plan.greps if arm_hits[n][0] or base_hits.get(n, (0, ""))[0]]
    lines = ["", f"greps ({len(hit)} of {len(plan.greps)} matched; arm / {base_id}):"]
    for name in hit[:GREPS_SHOWN]:
        count, first = arm_hits[name]
        lines.append(f"  {name}: {count} / {base_hits.get(name, (0, ''))[0]}")
        if first:
            lines.append(f"    {first[:LINE_W]}")
    if len(hit) > GREPS_SHOWN:
        lines.append(f"  ... {len(hit) - GREPS_SHOWN} more in report.md")
    return lines


def _visual_lines(train_dir: Path, rid: str, visual: dict[str, object] | None) -> list[str]:
    if visual is None:
        path = train_dir / "autovis" / "visual.json"
        if not path.exists():
            return ["", "visual: no autovis/visual.json yet (train --autovis)"]
        visual = json.loads(path.read_text(encoding="utf-8"))
        flagged, black = dev_autovis.arm_entries(visual, rid)
        pairs = [p for p in visual.get("pairs", []) if p["arm"] == rid]
    else:
        pairs = visual["pairs"]
        flagged = sorted((p for p in pairs if p["flagged"]), key=lambda p: -p["score"])
        black = visual["black"]
    worst = max((p["score"] for p in pairs), default=0.0)
    head = f"visual: {len(flagged)} of {len(pairs)} snap pairs flagged (worst score {worst:.1f}); {len(black)} black frames"
    lines = ["", head]
    for p in flagged[:SHEETS_SHOWN]:
        lines.append(f"  {p['t_arm']}s score {p['score']:.1f} {p['flagged_blocks']} blocks: {p['sheet']}")
    if len(flagged) > SHEETS_SHOWN:
        lines.append(f"  ... {len(flagged) - SHEETS_SHOWN} more in autovis/visual.json")
    for b in black[:3]:
        lines.append(f"  black {b['t']}s: {b['kind']} {b['share']:.0%}")
    return lines


def _verdict_lines(train_dir: Path, name: str, baseline: str) -> list[str]:
    if name == baseline:
        return ["", "verdict: (baseline set)"]
    path = train_dir / "verdict.json"
    if not path.exists():
        return ["", "verdict: none yet (train --verdict)"]
    rows = json.loads(path.read_text(encoding="utf-8")).get("rows", [])
    row = next((r for r in rows if r["set"] == name), None)
    if row is None:
        return ["", f"verdict: no row for {name}"]
    return ["", f"verdict: {row['verdict']} ({'; '.join(row['why'])})"]
