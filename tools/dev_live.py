"""Live arms of `dev.py train`: one launch that switches Live settings between two sets, A/B/A/B.

Plan:

    [live.awake]                # in `order` like a set
    a = "fg_rt"                 # two sets of [sets]; the launch uses A's values, the schedule switches every
    b = "fg_rt_awake"           # key whose value differs between them (it must be in both, and Live)
    segment_s = 40              # each segment's length (default 40)
    segments = 6                # segments in all (default 6: A B A B A B)
    start_s = 150               # first segment, seconds after launch (default 150: fast boot settles ~100-150 s)
    drop_s = 5                  # seconds left out after each change (default 5)
    pattern = "AB"              # repeated over the segments (default "AB"; "ABBA" also cancels a linear drift)
    quiet = true                # optional, as a set's

The run gets `VICTORY_SETTINGS_SCHEDULE=<NN_arm>.schedule.txt` (`at <t>s set <NAME>=<value>` lines, one per key per
segment). The settings registry applies it and logs each change as `settings: segment <n> <name>=<value> at <t>`.
Those lines (by their log timestamps) cut the run into segments; lines of one segment share `n` or come within
CLUSTER_S of each other. Every frame line, pacing window and hitch line is attributed to the segment it lies in,
less the first `drop_s` seconds and anything before the world settled. Per set: the median over its segments of
fps, prep ms, p50, p99 and hitches a minute; and a noise flag for every figure whose same-set spread (A vs A,
B vs B) exceeds the A-vs-B difference.
"""

from __future__ import annotations

import math
import re
import statistics
from dataclasses import dataclass, field
from datetime import datetime, timedelta

import dev_stats

LIVE_KEYS = {"a", "b", "segment_s", "segments", "start_s", "drop_s", "pattern", "quiet"}
DEFAULT_SEGMENT_S = 40.0
DEFAULT_SEGMENTS = 6
DEFAULT_START_S = 150.0
DEFAULT_DROP_S = 5.0
TAIL_S = 5.0
CLUSTER_S = 1.0
MIN_KEPT_S = 10.0
EDGE_S = 0.25
WORLD_DRAWS_MIN = 100.0
LOADING_P50_MS = 100.0

ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
TS_RE = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z)\s+")
SEGMENT_RE = re.compile(r"settings: segment (\d+) (\S+?)=(\S*) at ([0-9.]+)")
FRAME_RE = re.compile(r"([0-9.]+) fps, [0-9.]+ ms/frame \(([0-9.]+) preparing.*?([0-9.]+) draws/frame")
PACING_RE = re.compile(
    r"frame pacing: (\d+) frames in ([0-9.]+) s .*?p50 ([0-9.]+) ms .*?p99 ([0-9.]+) ms .*?(\d+) over 100 ms"
    r".*?; ([0-9.]+) draws/frame"
)

# (figure, column, format, +1 when higher is better / -1 when lower is)
METRICS = (
    ("fps", "fps", ".1f", 1),
    ("prep_ms", "prep", ".1f", -1),
    ("p50_ms", "p50", ".1f", -1),
    ("p99_ms", "p99", ".1f", -1),
    ("over100_min", ">100/min", ".1f", -1),
    ("over250_min", ">250/min", ".2f", -1),
)


class LiveError(ValueError):
    pass


@dataclass
class LiveArm:
    name: str
    a: str
    b: str
    keys: dict[str, tuple[str, str]]
    launch: dict[str, str]
    segment_s: float = DEFAULT_SEGMENT_S
    segments: int = DEFAULT_SEGMENTS
    start_s: float = DEFAULT_START_S
    drop_s: float = DEFAULT_DROP_S
    pattern: str = "AB"
    quiet: bool | None = None

    @property
    def seconds(self) -> int:
        return int(math.ceil(self.start_s + self.segments * self.segment_s + TAIL_S))

    def labels(self) -> list[str]:
        return [self.pattern[i % len(self.pattern)] for i in range(self.segments)]


def _short(name: str) -> str:
    return name.removeprefix("VICTORY_")


def parse_live(raw: object, sets: dict[str, dict[str, str]], kinds: dict[str, str] | None) -> dict[str, LiveArm]:
    """The plan's `[live.<name>]` arms. `kinds` (the registry's apply kind per setting, None without a registry)
    refuses scheduling a setting that is not Live."""
    if not isinstance(raw, dict):
        raise LiveError("live: a table of [live.<name>] arms")
    arms = {}
    for name, table in raw.items():
        where = f"live.{name}"
        if not isinstance(table, dict):
            raise LiveError(f"{where}: a table")
        unknown = set(table) - LIVE_KEYS
        if unknown:
            raise LiveError(f"{where}: unknown keys {', '.join(sorted(unknown))}")
        if name in sets:
            raise LiveError(f"{where}: a set has that name too")
        a, b = str(table.get("a", "")), str(table.get("b", ""))
        for side in (a, b):
            if side not in sets:
                raise LiveError(f"{where}: '{side}' is not a set (a and b name two sets)")
        va, vb = sets[a], sets[b]
        keys = {}
        for key in sorted(set(va) | set(vb)):
            if va.get(key) == vb.get(key):
                continue
            if key not in va or key not in vb:
                raise LiveError(f"{where}: {key} differs but only one set gives it; give it in both (e.g. its default)")
            if kinds is not None and kinds.get(key) != "Live":
                kind = kinds.get(key, "not in the settings registry")
                raise LiveError(f"{where}: {key} is {kind}, not Live: it needs its own boot (a set arm)")
            keys[key] = (va[key], vb[key])
        if not keys:
            raise LiveError(f"{where}: sets '{a}' and '{b}' do not differ")
        pattern = str(table.get("pattern", "AB")).upper()
        if set(pattern) != {"A", "B"}:
            raise LiveError(f"{where}: pattern uses A and B, both (e.g. AB, ABBA)")
        arm = LiveArm(
            name=name, a=a, b=b, keys=keys, launch=dict(va), pattern=pattern,
            segment_s=float(table.get("segment_s", DEFAULT_SEGMENT_S)),
            segments=int(table.get("segments", DEFAULT_SEGMENTS)),
            start_s=float(table.get("start_s", DEFAULT_START_S)),
            drop_s=float(table.get("drop_s", DEFAULT_DROP_S)),
            quiet=bool(table["quiet"]) if "quiet" in table else None,
        )
        if arm.segments < 2 or arm.segment_s <= arm.drop_s + MIN_KEPT_S:
            raise LiveError(f"{where}: at least 2 segments, each longer than drop_s + {MIN_KEPT_S:g} s")
        arms[name] = arm
    return arms


def schedule_text(arm: LiveArm) -> str:
    """The VICTORY_SETTINGS_SCHEDULE file: every differing key set at the start of every segment."""
    lines = [f"# dev.py train live arm {arm.name}: A={arm.a} B={arm.b}, {arm.segments} x {arm.segment_s:g} s "
             f"from {arm.start_s:g} s ({''.join(arm.labels())})"]
    for i, label in enumerate(arm.labels()):
        at = arm.start_s + i * arm.segment_s
        for key, (va, vb) in arm.keys.items():
            lines.append(f"at {at:g}s set {key}={va if label == 'A' else vb}")
    return "\n".join(lines) + "\n"


@dataclass
class LiveLog:
    changes: list[tuple[datetime, int, str, str]] = field(default_factory=list)
    frames: list[tuple[datetime, float, float, float]] = field(default_factory=list)
    pacing: list[tuple[datetime, float, float, float, float, float, float]] = field(default_factory=list)
    hitches: list[tuple[datetime, float]] = field(default_factory=list)
    end: datetime | None = None


def parse_log(text: str) -> LiveLog:
    log = LiveLog()
    for raw in text.splitlines():
        line = ANSI_RE.sub("", raw)
        m = TS_RE.match(line)
        if not m:
            continue
        ts = datetime.fromisoformat(m.group(1).replace("Z", "+00:00"))
        log.end = ts
        if s := SEGMENT_RE.search(line):
            log.changes.append((ts, int(s.group(1)), s.group(2), s.group(3)))
        elif f := FRAME_RE.search(line):
            log.frames.append((ts, float(f.group(1)), float(f.group(2)), float(f.group(3))))
        elif p := PACING_RE.search(line):
            frames, secs, p50, p99, over100, draws = (float(v) for v in p.groups())
            log.pacing.append((ts, secs, frames, p50, p99, over100, draws))
        elif h := dev_stats.parse_hitch_line(line):
            log.hitches.append((ts, h[1]))
    return log


def settled_at(log: LiveLog) -> datetime | None:
    """End of the last world pacing window still loading (median over LOADING_P50_MS), else the start of the
    first world window; None before any world window."""
    world = [w for w in log.pacing if w[6] >= WORLD_DRAWS_MIN]
    if not world:
        return None
    loading = [w for w in world if w[3] > LOADING_P50_MS]
    if loading:
        return loading[-1][0]
    first = world[0]
    return first[0] - timedelta(seconds=first[1])


def cluster(changes: list[tuple[datetime, int, str, str]]) -> list[tuple[datetime, int, dict[str, str]]]:
    """The change lines grouped into segments: (first line's time, n, {short name: value})."""
    groups: list[tuple[datetime, int, dict[str, str]]] = []
    for ts, n, name, value in changes:
        if groups and (groups[-1][1] == n or (ts - groups[-1][0]).total_seconds() <= CLUSTER_S):
            groups[-1][2][_short(name)] = value
        else:
            groups.append((ts, n, {_short(name): value}))
    return groups


def label_of(arm: LiveArm, values: dict[str, str]) -> str:
    for label, idx in (("A", 0), ("B", 1)):
        if values and all(_short(k) not in values or values[_short(k)] == v[idx] for k, v in arm.keys.items()):
            return label
    return "?"


def _figures(log: LiveLog, lo: datetime, hi: datetime) -> dict[str, float] | None:
    def inside(start: float, end: float) -> bool:
        return start >= lo.timestamp() - EDGE_S and end <= hi.timestamp() + EDGE_S

    frames, prev = [], None
    for ts, fps, prep, draws in log.frames:
        start = prev if prev is not None else ts.timestamp() - 5.0
        prev = ts.timestamp()
        if draws >= WORLD_DRAWS_MIN and inside(start, ts.timestamp()):
            frames.append((fps, prep))
    windows = [w for w in log.pacing if w[6] >= WORLD_DRAWS_MIN and inside(w[0].timestamp() - w[1], w[0].timestamp())]
    if not frames and not windows:
        return None
    secs = sum(w[1] for w in windows)
    kept = (hi - lo).total_seconds()
    over250 = sum(1 for ts, ms in log.hitches if lo < ts <= hi and ms >= dev_stats.OVER250_MS)
    return {
        "fps": statistics.median(f[0] for f in frames) if frames else 0.0,
        "prep_ms": statistics.median(f[1] for f in frames) if frames else 0.0,
        "p50_ms": statistics.median(w[3] for w in windows) if windows else 0.0,
        "p99_ms": statistics.median(w[4] for w in windows) if windows else 0.0,
        "over100_min": sum(w[5] for w in windows) / secs * 60.0 if secs else 0.0,
        "over250_min": over250 / kept * 60.0 if kept else 0.0,
        "frame_lines": float(len(frames)),
        "windows": float(len(windows)),
    }


def analyse(arm: LiveArm, text: str) -> dict[str, object]:
    """Segments, per-set medians, B-A deltas, noisy figures and problems of a live arm's log."""
    log = parse_log(text)
    groups = cluster(log.changes)
    settle = settled_at(log)
    problems = []
    if not groups:
        problems.append("no `settings: segment` lines: the schedule was not applied (no settings registry?)")
    elif len(groups) != arm.segments:
        problems.append(f"{len(groups)} segments logged, {arm.segments} scheduled")
    segments = []
    for i, (ts, n, values) in enumerate(groups):
        end = groups[i + 1][0] if i + 1 < len(groups) else (log.end or ts)
        label = label_of(arm, values)
        lo = ts + timedelta(seconds=arm.drop_s)
        if settle is not None and settle > lo:
            lo = settle
        seg: dict[str, object] = {"index": i + 1, "n": n, "label": label, "values": values,
                                  "kept_s": max(0.0, (end - lo).total_seconds())}
        if label == "?":
            problems.append(f"segment {i + 1} (n={n}) matches neither set: {values}")
        if settle is None or seg["kept_s"] < MIN_KEPT_S:
            seg["skipped"] = "before the world settled" if settle is None or settle >= end else "too short"
        else:
            figures = _figures(log, lo, end)
            if figures is None:
                seg["skipped"] = "no frame lines"
            else:
                seg["figures"] = figures
        segments.append(seg)
    expected = arm.labels()
    seen = [str(s["label"]) for s in segments]
    if groups and seen != expected[: len(seen)]:
        problems.append(f"segment order {''.join(seen)}, scheduled {''.join(expected)}")
    sets, spreads = {}, {}
    for label in ("A", "B"):
        rows = [s["figures"] for s in segments if s["label"] == label and "figures" in s]
        if rows:
            sets[label] = {"segments": float(len(rows)),
                           **{k: statistics.median(r[k] for r in rows) for k, *_ in METRICS}}
            if len(rows) >= 2:
                spreads[label] = {k: max(r[k] for r in rows) - min(r[k] for r in rows) for k, *_ in METRICS}
    delta, noisy, spread = {}, [], {}
    if "A" in sets and "B" in sets:
        for k, *_ in METRICS:
            delta[k] = sets["B"][k] - sets["A"][k]
            if spreads:
                spread[k] = max(s[k] for s in spreads.values())
                if spread[k] > abs(delta[k]):
                    noisy.append(k)
    return {"a": arm.a, "b": arm.b, "keys": {k: list(v) for k, v in arm.keys.items()}, "segments": segments,
            "sets": sets, "delta": delta, "spread": spread, "noisy": noisy, "problems": problems}


def report_lines(rid: str, result: dict[str, object]) -> list[str]:
    """A markdown section for one live run."""
    keys = ", ".join(f"{k} {a}->{b}" for k, (a, b) in result["keys"].items())
    lines = ["", f"### {rid}: live A={result['a']} vs B={result['b']} ({keys})", ""]
    for p in result["problems"]:
        lines.append(f"- **problem**: {p}")
    cols = [c for _, c, _, _ in METRICS]
    lines += ["", "| set | segments | " + " | ".join(cols) + " |", "|" + "---|" * (len(cols) + 2)]
    sets, delta, spread = result["sets"], result["delta"], result["spread"]
    for label in ("A", "B"):
        if label in sets:
            name = result["a"] if label == "A" else result["b"]
            cells = [f"{sets[label][k]:{spec}}" for k, _, spec, _ in METRICS]
            lines.append(f"| {label} {name} | {int(sets[label]['segments'])} | " + " | ".join(cells) + " |")
    if delta:
        cells = []
        for k, _, spec, _ in METRICS:
            cell = f"{delta[k]:+{spec}}"
            cells.append(f"**{cell} noisy**" if k in result["noisy"] else cell)
        lines.append("| B-A | | " + " | ".join(cells) + " |")
        if spread:
            lines.append("| same-set spread | | " + " | ".join(f"{spread[k]:{spec}}" for k, _, spec, _ in METRICS) + " |")
    if result["noisy"]:
        lines += ["", f"A-vs-A spread exceeds A-vs-B for: {', '.join(result['noisy'])} (noise, not an effect)."]
    skipped = [f"{s['index']}{s['label']} ({s['skipped']})" for s in result["segments"] if "skipped" in s]
    if skipped:
        lines += ["", f"Segments left out: {', '.join(skipped)}."]
    return lines
