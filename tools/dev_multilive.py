"""Multi-switch live launches of `dev.py train`: one launch A/Bs several Live switches, one arm after another.

Plan:

    [multilive.feed]                # in `order` like a set; named twice = up to two launches (early stop per arm)
    base = "base"                   # the set the launch uses (default: the plan's baseline)
    arms = ["GUARD_4K_JOIN=1", "REFRESH_STAMP=1", "KEEP_SHOWN=2 TABLED_REREAD=1",
            { set = "SKIN_FILL=1", segment_s = 15, at_s = 40 }]
    segment_s = 24                  # each segment (default 24); an arm table may give its own
    pattern = "ABBA"                # per arm (default "ABBA"); `segments` (default: the pattern's length)
    start_s = 130                   # the first arm's first change, seconds after launch (default 130)
    drop_s = 5                      # seconds left out after each change (default 5)
    quiet = false                   # optional, as a set's
    count = { guards = '4 KiB guards per frame' }        # lines per segment, summed per side
    num = { flush = 'batch_flush ([0-9.]+) ms' }         # the captured numbers of a line (summed), median per segment

An arm is `KEY=B [KEY=B ...]` (VICTORY_ optional), every key Live: B is what it switches to; A is the base set's
value, else empty (the launch's own environment: the default). Arms run back to back (an arm's `at_s` holds it back
to that time); after an arm its keys go back to A at the next change.

The registry numbers the schedule's distinct times 1, 2, ... and logs each change as `settings: segment <n> ...`;
this module wrote the schedule, so `n` names the arm and side. Figures per segment are dev_live's (fps, prep, p50,
p99, >100/min, >250/min) plus pacing jitter, less `drop_s` after each change and anything before the world
settled; per arm: A and B medians, B-A, the same-side spread and noisy figures (dev_live.summarise), and the
`count`/`num` greps per side. Each segment also lists its snaps (by the log's `snap saved` lines).

Early stop per arm: a launch that ends before its schedule (a stop of `--all`, a crash) leaves each arm complete,
cut or not reached; a cut arm of a stopped launch is the suspect. The next launch of the same entry (a retry of
the step, or the entry named again in `order`) schedules only the arms still owed: not the suspects, not the arms
complete in that step's earlier attempt (their results are carried), and not the arms another launch already found
clearly KEEP or DROP (dev_verdict.live_verdict, with a figure beyond CLEAR times its noise floor). A launch with
nothing owed is skipped.

Folds in the batch 5 operator's `AnyXboxX-coord/train/b5/tools/multilive.py` (per-switch tables of a multi-switch
live launch, with its `--count`/`--num` greps).
"""

from __future__ import annotations

import bisect
import math
import re
import statistics
from dataclasses import dataclass, field
from datetime import datetime, timedelta

import dev_live
import dev_verdict

ML_KEYS = {"base", "arms", "segment_s", "segments", "start_s", "drop_s", "pattern", "quiet", "count", "num"}
ARM_KEYS = {"set", "segment_s", "segments", "pattern", "at_s"}
DEFAULT_SEGMENT_S = 24.0
DEFAULT_PATTERN = "ABBA"
DEFAULT_START_S = 130.0
METRICS = (*dev_live.METRICS, ("jitter_ms", "jitter", ".1f", -1))
JITTER_RE = re.compile(r"frame pacing: \d+ frames in ([0-9.]+) s .*?jitter ([0-9.]+) ms.*?; ([0-9.]+) draws/frame")
SNAP_RE = re.compile(r"snap saved .*?(snap-\d+s-f\d+\.png)")
DECIDED = ("KEEP", "DROP")
CLEAR = 2.0


class MultiLiveError(ValueError):
    pass


@dataclass
class Arm:
    name: str
    keys: dict[str, tuple[str, str]]
    segment_s: float
    labels: list[str]
    at_s: float | None = None


@dataclass
class MultiLive:
    name: str
    base: str
    launch: dict[str, str]
    arms: list[Arm]
    start_s: float = DEFAULT_START_S
    drop_s: float = dev_live.DEFAULT_DROP_S
    quiet: bool | None = None
    counts: dict[str, re.Pattern[str]] = field(default_factory=dict)
    nums: dict[str, re.Pattern[str]] = field(default_factory=dict)

    def arm(self, name: str) -> Arm:
        return next(a for a in self.arms if a.name == name)


@dataclass
class Segment:
    arm: str
    index: int
    label: str
    start: float
    end: float


def _key(word: str) -> str:
    return word if word.startswith("VICTORY_") else f"VICTORY_{word}"


def _regexes(where: str, raw: object) -> dict[str, re.Pattern[str]]:
    if not isinstance(raw, dict):
        raise MultiLiveError(f"{where}: a table of name = 'regex'")
    try:
        return {str(k): re.compile(str(v)) for k, v in raw.items()}
    except re.error as exc:
        raise MultiLiveError(f"{where}: {exc}") from exc


def _arm(where: str, raw: object, launch: dict[str, str], kinds: dict[str, str] | None, table: dict) -> Arm:
    spec = raw if isinstance(raw, dict) else {"set": raw}
    unknown = set(spec) - ARM_KEYS
    if unknown:
        raise MultiLiveError(f"{where}: unknown keys {', '.join(sorted(unknown))}")
    words = str(spec.get("set", "")).split()
    if not words:
        raise MultiLiveError(f"{where}: an arm is 'KEY=value [KEY=value ...]'")
    keys = {}
    for word in words:
        name, sep, b = word.partition("=")
        key = _key(name)
        if not sep or not re.fullmatch(r"VICTORY_[A-Z0-9_]+", key):
            raise MultiLiveError(f"{where}: '{word}' is not KEY=value")
        if kinds is not None and kinds.get(key) != "Live":
            kind = kinds.get(key, "not in the settings registry")
            raise MultiLiveError(f"{where}: {key} is {kind}, not Live: it needs its own boot (a set arm)")
        a = launch.get(key, "")
        if a == b:
            raise MultiLiveError(f"{where}: {key}={b} is what the launch has already")
        keys[key] = (a, b)
    pattern = str(spec.get("pattern", table.get("pattern", DEFAULT_PATTERN))).upper()
    if set(pattern) != {"A", "B"}:
        raise MultiLiveError(f"{where}: pattern uses A and B, both (e.g. AB, ABBA)")
    count = int(spec.get("segments", table.get("segments", len(pattern))))
    seg = float(spec.get("segment_s", table.get("segment_s", DEFAULT_SEGMENT_S)))
    drop = float(table.get("drop_s", dev_live.DEFAULT_DROP_S))
    if count < 2 or seg < drop + 8.0:
        raise MultiLiveError(f"{where}: at least 2 segments, each at least drop_s + 8 s")
    labels = [pattern[i % len(pattern)] for i in range(count)]
    at = float(spec["at_s"]) if "at_s" in spec else None
    return Arm(name=" ".join(f"{k.removeprefix('VICTORY_')}={b}" for k, (_, b) in keys.items()), keys=keys,
               segment_s=seg, labels=labels, at_s=at)


def parse_multilive(
    raw: object, sets: dict[str, dict[str, str]], baseline: str, kinds: dict[str, str] | None
) -> dict[str, MultiLive]:
    """The plan's `[multilive.<name>]` entries. `kinds` (the registry's apply kind per setting) refuses a key that
    is not Live."""
    if not isinstance(raw, dict):
        raise MultiLiveError("multilive: a table of [multilive.<name>] entries")
    out = {}
    for name, table in raw.items():
        where = f"multilive.{name}"
        if not isinstance(table, dict):
            raise MultiLiveError(f"{where}: a table")
        unknown = set(table) - ML_KEYS
        if unknown:
            raise MultiLiveError(f"{where}: unknown keys {', '.join(sorted(unknown))}")
        if name in sets:
            raise MultiLiveError(f"{where}: a set has that name too")
        base = str(table.get("base", baseline))
        if base not in sets:
            raise MultiLiveError(f"{where}: base '{base}' is not a set")
        arms_raw = table.get("arms")
        if not isinstance(arms_raw, list) or not arms_raw:
            raise MultiLiveError(f"{where}: arms = [\"KEY=value\", ...]")
        launch = dict(sets[base])
        arms = [_arm(f"{where}.arms[{i}]", a, launch, kinds, table) for i, a in enumerate(arms_raw)]
        names = [a.name for a in arms]
        if len(set(names)) != len(names):
            raise MultiLiveError(f"{where}: an arm is given twice")
        ml = MultiLive(
            name=name, base=base, launch=launch, arms=arms,
            start_s=float(table.get("start_s", DEFAULT_START_S)),
            drop_s=float(table.get("drop_s", dev_live.DEFAULT_DROP_S)),
            quiet=bool(table["quiet"]) if "quiet" in table else None,
            counts=_regexes(f"{where}.count", table.get("count", {})),
            nums=_regexes(f"{where}.num", table.get("num", {})),
        )
        timeline(ml, names)
        out[name] = ml
    return out


def timeline(ml: MultiLive, active: list[str]) -> list[Segment]:
    """The segments of the active arms (in plan order), back to back from start_s (an arm's at_s holds it back)."""
    segs, t = [], ml.start_s
    for arm in ml.arms:
        if arm.name not in active:
            continue
        if arm.at_s is not None:
            if arm.at_s < t:
                raise MultiLiveError(f"multilive.{ml.name}: arm {arm.name} at_s {arm.at_s:g} is before {t:g}")
            t = arm.at_s
        for i, label in enumerate(arm.labels):
            segs.append(Segment(arm.name, i, label, t, t + arm.segment_s))
            t += arm.segment_s
    return segs


def seconds(ml: MultiLive, active: list[str]) -> int:
    segs = timeline(ml, active)
    end = segs[-1].end if segs else ml.start_s
    return int(math.ceil(end + dev_live.TAIL_S))


def _changes(ml: MultiLive, active: list[str]) -> list[tuple[float, str, str]]:
    """(t, key, value) in the order written: each segment sets its arm's keys; an arm's keys go back to A at the
    next change after it (written first, so a later arm's value of the same key wins)."""
    out: list[tuple[float, str, str]] = []
    segs = timeline(ml, active)
    for i, seg in enumerate(segs):
        arm = ml.arm(seg.arm)
        out += [(seg.start, k, v[0] if seg.label == "A" else v[1]) for k, v in arm.keys.items()]
        if i + 1 == len(segs) or segs[i + 1].arm != seg.arm:
            out += [(seg.end, k, a) for k, (a, _) in arm.keys.items()]
    return sorted(out, key=lambda c: c[0])


def schedule_text(ml: MultiLive, active: list[str]) -> str:
    segs = timeline(ml, active)
    head = [f"# dev.py train multilive {ml.name}: base {ml.base}, {len(active)} arms from {ml.start_s:g} s"]
    head += [f"# {s.start:g}-{s.end:g} s {s.label} {s.arm}" for s in segs]
    return "\n".join(head + [f"at {t:g}s set {k}={v}" for t, k, v in _changes(ml, active)]) + "\n"


def segment_numbers(ml: MultiLive, active: list[str]) -> dict[int, Segment | None]:
    """The registry's segment number of every distinct schedule time: its segment, or None (a restore alone)."""
    starts = {s.start: s for s in timeline(ml, active)}
    times = sorted({t for t, _, _ in _changes(ml, active)})
    return {n: starts.get(t) for n, t in enumerate(times, 1)}


@dataclass
class Extras:
    jitter: list[tuple[datetime, float, float, float]] = field(default_factory=list)
    snaps: list[tuple[datetime, str]] = field(default_factory=list)
    hits: dict[str, list[datetime]] = field(default_factory=dict)
    vals: dict[str, list[tuple[datetime, float]]] = field(default_factory=dict)


def scan_extras(text: str, counts: dict[str, re.Pattern[str]], nums: dict[str, re.Pattern[str]]) -> Extras:
    ex = Extras(hits={k: [] for k in counts}, vals={k: [] for k in nums})
    for raw in text.splitlines():
        line = dev_live.ANSI_RE.sub("", raw)
        m = dev_live.TS_RE.match(line)
        if not m:
            continue
        ts = None

        def when() -> datetime:
            return datetime.fromisoformat(m.group(1).replace("Z", "+00:00"))

        if j := JITTER_RE.search(line):
            ts = when()
            ex.jitter.append((ts, float(j.group(1)), float(j.group(2)), float(j.group(3))))
        elif s := SNAP_RE.search(line):
            ts = when()
            ex.snaps.append((ts, s.group(1)))
        for k, rx in counts.items():
            if rx.search(line):
                ts = ts or when()
                ex.hits[k].append(ts)
        for k, rx in nums.items():
            if n := rx.search(line):
                ts = ts or when()
                ex.vals[k].append((ts, sum(float(g) for g in n.groups() if g)))
    return ex


def _segment_figures(log: dev_live.LiveLog, ex: Extras, lo: datetime, hi: datetime) -> dict[str, float] | None:
    fig = dev_live._figures(log, lo, hi)
    if fig is None:
        return None
    jit = [j for ts, secs, j, draws in ex.jitter if draws >= dev_live.WORLD_DRAWS_MIN
           and ts.timestamp() - secs >= lo.timestamp() - dev_live.EDGE_S and ts.timestamp() <= hi.timestamp() + dev_live.EDGE_S]
    fig["jitter_ms"] = statistics.median(jit) if jit else 0.0
    return fig


def _side_greps(ml: MultiLive, segments: list[dict[str, object]]) -> tuple[dict, dict]:
    counts, nums = {}, {}
    for k in ml.counts:
        counts[k] = {}
        for label in ("A", "B"):
            rows = [s for s in segments if s["label"] == label and "counts" in s]
            kept = sum(float(s["kept_s"]) for s in rows)
            n = sum(int(s["counts"][k]) for s in rows)
            counts[k][label] = n
            counts[k][f"{label}_min"] = n / kept * 60.0 if kept else 0.0
    for k in ml.nums:
        nums[k] = {}
        for label in ("A", "B"):
            got = [s["nums"][k] for s in segments if s["label"] == label and s.get("nums", {}).get(k) is not None]
            nums[k][label] = statistics.median(got) if got else None
    return counts, nums


def analyse(ml: MultiLive, active: list[str], text: str, stopped: bool = False) -> dict[str, object]:
    """Per arm: its segments, side medians, B-A, spread, noisy figures, greps and status (complete, cut, not
    reached, suspect: cut by a stopped launch)."""
    gone = [a for a in active if a not in {arm.name for arm in ml.arms}]
    if gone:
        raise MultiLiveError(f"multilive.{ml.name}: arms no longer in the plan: {', '.join(gone)}")
    log = dev_live.parse_log(text)
    ex = scan_extras(text, ml.counts, ml.nums)
    groups = dev_live.cluster(log.changes)
    settle = dev_live.settled_at(log)
    numbers = segment_numbers(ml, active)
    problems = []
    if not groups:
        problems.append("no `settings: segment` lines: the schedule was not applied (refused? no settings registry?)")
    elif max(n for _, n, _ in groups) > len(numbers):
        problems.append(f"segment {max(n for _, n, _ in groups)} logged, {len(numbers)} scheduled: not this schedule")
    hits = {k: sorted(v) for k, v in ex.hits.items()}
    by_arm: dict[str, list[dict[str, object]]] = {a: [] for a in active}
    arm_problems: dict[str, list[str]] = {a: [] for a in active}
    for i, (ts, n, values) in enumerate(groups):
        seg = numbers.get(n)
        if seg is None:
            continue
        end = groups[i + 1][0] if i + 1 < len(groups) else (log.end or ts)
        arm = ml.arm(seg.arm)
        lo = ts + timedelta(seconds=ml.drop_s)
        if settle is not None and settle > lo:
            lo = settle
        row: dict[str, object] = {"index": seg.index + 1, "n": n, "label": seg.label, "t": seg.start,
                                  "values": values, "kept_s": max(0.0, (end - lo).total_seconds()),
                                  "span_s": (end - ts).total_seconds(),
                                  "snaps": [name for t, name in ex.snaps if lo < t <= end]}
        want = {k.removeprefix("VICTORY_"): (a if seg.label == "A" else b) for k, (a, b) in arm.keys.items()}
        if any(values.get(k, v) != v for k, v in want.items()):
            arm_problems[seg.arm].append(f"segment {seg.index + 1} logged {values}, scheduled {want}")
        if settle is None or row["kept_s"] < dev_live.MIN_KEPT_S:
            row["skipped"] = "before the world settled" if settle is None or settle >= end else "too short"
        else:
            fig = _segment_figures(log, ex, lo, end)
            if fig is None:
                row["skipped"] = "no frame lines"
            else:
                row["figures"] = fig
                row["counts"] = {k: bisect.bisect_right(h, end) - bisect.bisect_right(h, lo) for k, h in hits.items()}
                row["nums"] = {}
                for k, vals in ex.vals.items():
                    got = [v for t, v in vals if lo < t <= end]
                    row["nums"][k] = statistics.median(got) if got else None
        by_arm[seg.arm].append(row)
    results, status = {}, {}
    for name in active:
        arm, rows = ml.arm(name), by_arm[name]
        complete = len(rows) == len(arm.labels) and float(rows[-1]["span_s"]) >= arm.segment_s - 1.0
        status[name] = "complete" if complete else "not reached" if not rows else "suspect" if stopped else "cut"
        sets, delta, spread, noisy = dev_live.summarise(rows, METRICS)
        counts, nums = _side_greps(ml, rows)
        mine = [*problems, *arm_problems[name]]
        if not complete:
            mine.append(f"{status[name]}: {len(rows)} of {len(arm.labels)} segments logged")
        results[name] = {"a": ml.base, "b": name, "keys": {k: list(v) for k, v in arm.keys.items()},
                         "segments": rows, "sets": sets, "delta": delta, "spread": spread, "noisy": noisy,
                         "problems": mine, "counts": counts, "nums": nums}
    return {"arms": list(active), "results": results, "status": status, "problems": problems,
            "settled": settle.isoformat() if settle else None}


def verdict(result: dict[str, object]) -> str:
    return str(dev_verdict.live_verdict(result)["verdict"])


def clear(result: dict[str, object]) -> bool:
    """KEEP or DROP with some figure's B-A at CLEAR times its floor or more (live_verdict's floors)."""
    if verdict(result) not in DECIDED:
        return False
    spread, delta = result.get("spread") or {}, result["delta"]
    for k, *_ in dev_live.METRICS:
        floor = max(spread[k], dev_verdict.LIVE_MIN_FLOOR[k]) if k in spread else dev_verdict.LIVE_DEFAULT_FLOOR[k]
        if abs(delta.get(k, 0.0)) >= CLEAR * floor:
            return True
    return False


def owed(ml: MultiLive, done: dict[int, dict[str, object]], step: int) -> tuple[list[str], dict, dict[str, str]]:
    """The arms a launch of step `step` runs, the results it carries from that step's earlier attempt (arm -> result)
    and why each other arm is left out."""
    carried, left = {}, {}
    for s in sorted(done):
        info = done[s].get("multilive")
        if done[s].get("set") != ml.name or not isinstance(info, dict):
            continue
        rid = f"{s:02d}_{ml.name}"
        status = {**info.get("carried_status", {}), **info.get("status", {})}
        for name, res in {**info.get("carried", {}), **info.get("results", {})}.items():
            st = status.get(name)
            if st == "suspect":
                left[name] = f"suspect: the launch {rid} stopped in its segments"
            elif st == "complete" and s == step:
                carried[name] = res
            elif st == "complete" and clear(res):
                left.setdefault(name, f"{verdict(res)} in {rid}")
    active = [a.name for a in ml.arms if a.name not in left and a.name not in carried]
    return active, carried, left


def merged(info: dict[str, object]) -> tuple[dict[str, dict], dict[str, str]]:
    """A launch's results with those it carried (results, status)."""
    results = {**info.get("carried", {}), **info.get("results", {})}
    status = {**{k: f"{v} (carried)" for k, v in info.get("carried_status", {}).items()}, **info.get("status", {})}
    return results, status


def verdict_rows(info: dict[str, object]) -> list[dict[str, object]]:
    results, status = merged(info)
    rows = []
    for name, res in results.items():
        row = dev_verdict.live_verdict(res)
        row["why"] = [f"status {status.get(name, '?')}", *row["why"]]
        rows.append(row)
    return rows


def _fmt(v: float | None, spec: str) -> str:
    return "-" if v is None else f"{v:{spec}}"


def report_lines(rid: str, ml: MultiLive | None, info: dict[str, object]) -> list[str]:
    """A markdown section for one multilive launch: one table per arm (sides, B-A, spread, greps)."""
    results, status = merged(info)
    lines = ["", f"### {rid}: multilive ({len(info.get('arms', []))} arms run, {len(info.get('carried', {}))} carried)", ""]
    for p in info.get("problems", []):
        lines.append(f"- **problem**: {p}")
    for name, why in info.get("left_out", {}).items():
        lines.append(f"- left out: {name} ({why})")
    cols = [c for _, c, _, _ in METRICS]
    for name, res in results.items():
        verdict_row = dev_verdict.live_verdict(res)
        lines += ["", f"#### {name}: {verdict_row['verdict']} ({status.get(name, '?')})", ""]
        lines += [f"- problem: {p}" for p in res["problems"]]
        greps = [*res.get("counts", {}), *res.get("nums", {})]
        lines += ["| side | segments | " + " | ".join(cols + greps) + " |", "|" + "---|" * (len(cols) + len(greps) + 2)]
        sets, delta, spread = res["sets"], res["delta"], res["spread"]
        for label in ("A", "B"):
            if label not in sets:
                continue
            cells = [f"{sets[label][k]:{spec}}" for k, _, spec, _ in METRICS]
            cells += [f"{res['counts'][g][label]} ({res['counts'][g][label + '_min']:.1f}/min)" for g in res.get("counts", {})]
            cells += [_fmt(res["nums"][g][label], ".2f") for g in res.get("nums", {})]
            lines.append(f"| {label} | {int(sets[label]['segments'])} | " + " | ".join(cells) + " |")
        if delta:
            cells = [(f"**{delta[k]:+{spec}} noisy**" if k in res["noisy"] else f"{delta[k]:+{spec}}")
                     for k, _, spec, _ in METRICS]
            lines.append("| B-A | | " + " | ".join(cells + [""] * len(greps)) + " |")
            if spread:
                cells = [f"{spread[k]:{spec}}" for k, _, spec, _ in METRICS]
                lines.append("| same-side spread | | " + " | ".join(cells + [""] * len(greps)) + " |")
        segs = " ".join(f"{s['label']}{s['t']:g}s" + (f"({s['skipped']})" if "skipped" in s else "") for s in res["segments"])
        lines += ["", f"Segments: {segs or 'none logged'}. Why: {'; '.join(verdict_row['why'])}."]
    return lines


def list_line(ml: MultiLive, active: list[str], left: dict[str, str], carried: dict) -> str:
    arms = " | ".join(f"{a.name} {''.join(a.labels)}x{a.segment_s:g}s" for a in ml.arms if a.name in active)
    extra = f"; left out {len(left)}" if left else ""
    extra += f"; carried {len(carried)}" if carried else ""
    return f"multilive base {ml.base} from {ml.start_s:g} s, {len(active)} arms: {arms or 'none (skipped)'}{extra}"
