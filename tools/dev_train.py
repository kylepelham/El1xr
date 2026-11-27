"""`dev.py train`: a batch plan, run one bench per call, and a report across its runs.

The operator holds the Drift `gpu` lock around each call (one lock per run); `train` takes no lock
of its own beyond the gpu.lock file every bench takes.

Plan (TOML or JSON; every key optional but `sets`):

    seconds = 200                       # bench options for every run (fast boot settles at ~100 s)
    quiet = true                        # hold every build slot during each run
    snap_every = 15                     # a PNG every 15 s (0: none)
    save = "default_checkpoint"
    fast_boot = true
    unmute = false
    order = ["on", "off", "off", "on"]  # default: ABBA of on and off
    baseline = "off"                    # deltas against it (default: off, else the first set run)

    [common]                            # in every set, under its own values
    VICTORY_THREAD_TICKET = 0

    [sets.on]
    VICTORY_FOO = 1
    VICTORY_BAR = "fast"

    [sets.off]
    VICTORY_FOO = 0

    [sets.on_minus_foo]
    from = "on"                         # another set's values, then these
    VICTORY_FOO = 0

Values are strings, numbers or booleans (true is "1", false "0"). Results go to
`<dev dir>/train_<plan file stem>/`: `NN_<set>.log`, `NN_<set>.json` and `NN_<set>_snaps/`.
`--compare A B` writes `compare_<A>_vs_<B>/cmp-<A s>s-<B s>s.png`: each snap of run A (left)
next to the snap of run B (right) nearest in time.
"""

from __future__ import annotations

import json
import re
import statistics
import tomllib
from dataclasses import dataclass
from pathlib import Path

OVERRIDE_KEY_RE = re.compile(r"^VICTORY_[A-Z0-9_]+$")
SET_NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]*$")
RESULT_RE = re.compile(r"^(\d+)_(.+)\.json$")
SNAP_RE = re.compile(r"^snap-(\d+)s-f(\d+)\.png$")
DEFAULT_ORDER = ("on", "off", "off", "on")
DEFAULT_SECONDS = 200
DEFAULT_SNAP_EVERY = 15.0
PLAN_KEYS = {"seconds", "quiet", "snap_every", "save", "fast_boot", "unmute", "order", "baseline", "common", "sets"}

# (stats key, column header, format): the report's columns, in order.
COLUMNS = (
    ("fps", "fps", ".1f"),
    ("prep_ms", "prep", ".1f"),
    ("p50_ms", "p50", ".1f"),
    ("p99_ms", "p99", ".1f"),
    ("steady_p99_ms", "steady p99", ".1f"),
    ("world_over100", ">100", "d"),
    ("world_over250", ">250", "d"),
    ("worst_ms", "worst ms", ".0f"),
    ("boot_s", "boot_s", ".1f"),
    ("settled_s", "settled_s", ".1f"),
    ("stream_s", "stream_s", ".1f"),
    ("refused", "refused", "d"),
)


class PlanError(ValueError):
    pass


@dataclass
class Plan:
    name: str
    sets: dict[str, dict[str, str]]
    order: list[str]
    baseline: str
    seconds: int = DEFAULT_SECONDS
    quiet: bool = False
    snap_every: float = DEFAULT_SNAP_EVERY
    save: str | None = None
    fast_boot: bool = True
    unmute: bool = False


def _value(key: str, value: object) -> str:
    if isinstance(value, bool):
        return "1" if value else "0"
    if isinstance(value, (int, float, str)):
        return str(value)
    raise PlanError(f"{key}: a string, number or boolean, not {type(value).__name__}")


def _overrides(where: str, table: object) -> dict[str, str]:
    if not isinstance(table, dict):
        raise PlanError(f"{where}: a table of VICTORY_* values")
    out: dict[str, str] = {}
    for key, value in table.items():
        if key == "from":
            continue
        if not OVERRIDE_KEY_RE.match(key):
            raise PlanError(f"{where}: '{key}' is not a VICTORY_* name")
        out[key] = _value(f"{where}.{key}", value)
    return out


def resolve_sets(raw: dict[str, object], common: dict[str, str]) -> dict[str, dict[str, str]]:
    """Each set's overrides: `common`, then the set it comes `from` (recursively), then its own."""
    resolved: dict[str, dict[str, str]] = {}

    def resolve(name: str, seen: tuple[str, ...]) -> dict[str, str]:
        if name in resolved:
            return resolved[name]
        if name in seen:
            raise PlanError(f"sets: 'from' loops through {' -> '.join((*seen, name))}")
        table = raw.get(name)
        if table is None:
            raise PlanError(f"sets.{seen[-1]}: 'from' names no set '{name}'")
        base = dict(common)
        parent = table.get("from") if isinstance(table, dict) else None
        if parent is not None:
            base.update(resolve(str(parent), (*seen, name)))
        base.update(_overrides(f"sets.{name}", table))
        resolved[name] = base
        return base

    for name in raw:
        if not SET_NAME_RE.match(name):
            raise PlanError(f"sets: '{name}' is not a usable name (letters, digits, '_', '-', '.')")
        resolve(name, ())
    return resolved


def parse_plan(name: str, data: dict[str, object]) -> Plan:
    unknown = set(data) - PLAN_KEYS
    if unknown:
        raise PlanError(f"unknown plan keys: {', '.join(sorted(unknown))}")
    raw_sets = data.get("sets")
    if not isinstance(raw_sets, dict) or not raw_sets:
        raise PlanError("the plan names no sets ([sets.<name>])")
    sets = resolve_sets(raw_sets, _overrides("common", data.get("common", {})))
    order = [str(s) for s in data.get("order", DEFAULT_ORDER)]
    missing = [s for s in order if s not in sets]
    if missing:
        raise PlanError(f"order names sets the plan lacks: {', '.join(missing)}")
    baseline = str(data.get("baseline", "off" if "off" in sets else order[0] if order else next(iter(sets))))
    if baseline not in sets:
        raise PlanError(f"baseline '{baseline}' is not a set")
    return Plan(
        name=name,
        sets=sets,
        order=order,
        baseline=baseline,
        seconds=int(data.get("seconds", DEFAULT_SECONDS)),
        quiet=bool(data.get("quiet", False)),
        snap_every=float(data.get("snap_every", DEFAULT_SNAP_EVERY)),
        save=str(data["save"]) if data.get("save") else None,
        fast_boot=bool(data.get("fast_boot", True)),
        unmute=bool(data.get("unmute", False)),
    )


def load_plan(path: Path) -> Plan:
    text = path.read_text(encoding="utf-8")
    try:
        data = json.loads(text) if path.suffix.lower() == ".json" else tomllib.loads(text)
    except (json.JSONDecodeError, tomllib.TOMLDecodeError) as exc:
        raise PlanError(f"{path}: {exc}") from exc
    return parse_plan(re.sub(r"[^A-Za-z0-9_.-]+", "_", path.stem), data)


def run_id(step: int, set_name: str) -> str:
    return f"{step:02d}_{set_name}"


def load_results(train_dir: Path) -> dict[int, dict[str, object]]:
    """Every finished run's record, by step."""
    results: dict[int, dict[str, object]] = {}
    if not train_dir.is_dir():
        return results
    for path in train_dir.glob("*.json"):
        m = RESULT_RE.match(path.name)
        if m:
            results[int(m.group(1))] = json.loads(path.read_text(encoding="utf-8"))
    return results


def save_result(train_dir: Path, step: int, record: dict[str, object]) -> Path:
    path = train_dir / f"{run_id(step, str(record['set']))}.json"
    for old in train_dir.glob(f"{step:02d}_*.json"):
        old.unlink()
    path.write_text(json.dumps(record, indent=2, sort_keys=True), encoding="utf-8")
    return path


def pick(plan: Plan, done: dict[int, dict[str, object]], run: str) -> tuple[int, str]:
    """The step and set `--run` names: `next` (the first step of the order not run yet), a step
    number (run again if done), or a set (its first step not run yet, else an extra step after
    the order and every run so far)."""
    if run == "next":
        for step, name in enumerate(plan.order, 1):
            if step not in done:
                return step, name
        raise PlanError("every step of the order has run (use --run <set> for an extra run)")
    if run.isdigit():
        step = int(run)
        if 1 <= step <= len(plan.order):
            return step, plan.order[step - 1]
        if step in done:
            return step, str(done[step]["set"])
        raise PlanError(f"step {step} is not in the order (1-{len(plan.order)})")
    if run not in plan.sets:
        raise PlanError(f"no set '{run}' (sets: {', '.join(plan.sets)})")
    for step, name in enumerate(plan.order, 1):
        if name == run and step not in done:
            return step, name
    return max([len(plan.order), *done], default=0) + 1, run


def find_run(plan: Plan, done: dict[int, dict[str, object]], which: str) -> int:
    """The step a `--compare` argument names: a step number, a run id (`03_off`), or a set (its
    latest run that passed, else its latest run)."""
    m = re.match(r"^(\d+)(?:_.*)?$", which)
    if m and int(m.group(1)) in done:
        return int(m.group(1))
    runs = [s for s in sorted(done) if done[s]["set"] == which]
    passed = [s for s in runs if done[s].get("ok")]
    if passed or runs:
        return (passed or runs)[-1]
    raise PlanError(f"no finished run '{which}' (runs: {', '.join(run_id(s, str(done[s]['set'])) for s in sorted(done))})")


def snaps(folder: Path) -> list[tuple[int, Path]]:
    """A run's snaps as (seconds since its first present, path), in time order."""
    found = []
    if folder.is_dir():
        for path in folder.iterdir():
            m = SNAP_RE.match(path.name)
            if m:
                found.append((int(m.group(1)), path))
    return sorted(found)


def pair_snaps(left: list[tuple[int, Path]], right: list[tuple[int, Path]]) -> list[tuple[int, Path, int, Path]]:
    """Each snap of `left` with the snap of `right` nearest in time (the earlier on a tie)."""
    if not right:
        return []
    out = []
    for t, path in left:
        u, other = min(right, key=lambda r: (abs(r[0] - t), r[0]))
        out.append((t, path, u, other))
    return out


def next_pending(plan: Plan, done: dict[int, dict[str, object]]) -> str:
    for step, name in enumerate(plan.order, 1):
        if step not in done:
            return run_id(step, name)
    return "done"


def listing(plan: Plan, done: dict[int, dict[str, object]]) -> str:
    """The order, step by step: run id, whether it has run, and the set's overrides."""
    lines = []
    for step in sorted(set(range(1, len(plan.order) + 1)) | set(done)):
        name = str(done[step]["set"]) if step in done else plan.order[step - 1]
        state = "done" if step in done else "pending"
        values = " ".join(f"{k}={v}" for k, v in plan.sets[name].items()) if name in plan.sets else "(set no longer in the plan)"
        lines.append(f"{run_id(step, name)} {state}: {values or '(defaults)'}")
    return "\n".join(lines) + "\n"


def _cell(stats: dict[str, object], key: str, spec: str) -> str:
    value = stats.get(key, 0)
    return f"{int(value) if spec == 'd' else float(value):{spec}}"


def _resets(record: dict[str, object]) -> str:
    before, after = record.get("resets_before"), record.get("resets_after")
    if after is None:
        return "?"
    if before is not None and after > before:
        return f"{after} (+{after - before}!)"
    return str(after)


def report(plan: Plan, done: dict[int, dict[str, object]]) -> tuple[str, dict[str, object]]:
    """The markdown report and its figures: every run, pending steps, then each set's medians and
    their differences from the baseline set."""
    head = ["run", "set", *[c[1] for c in COLUMNS], "resets", "snaps", "ok"]
    lines = [f"# train {plan.name}", "", "| " + " | ".join(head) + " |", "|" + "---|" * len(head)]
    steps = sorted(set(range(1, len(plan.order) + 1)) | set(done))
    for step in steps:
        if step not in done:
            name = plan.order[step - 1]
            lines.append(f"| {run_id(step, name)} | {name} | " + " | ".join(["pending"] + [""] * (len(head) - 3)) + " |")
            continue
        rec = done[step]
        stats = rec["stats"]
        cells = [_cell(stats, k, s) for k, _, s in COLUMNS]
        cells[COLUMNS.index(("worst_ms", "worst ms", ".0f"))] += f" @{int(stats.get('worst_frame', 0))}"
        ok = "yes" if rec.get("ok") else "**no**"
        row = [run_id(step, str(rec["set"])), str(rec["set"]), *cells, _resets(rec), str(rec.get("snaps", 0)), ok]
        lines.append("| " + " | ".join(row) + " |")
    medians = set_medians(done)
    figures: dict[str, object] = {"plan": plan.name, "baseline": plan.baseline, "medians": medians, "deltas": {}}
    if medians:
        lines += ["", f"## Medians by set (deltas against {plan.baseline})", ""]
        lines.append("| set | runs | " + " | ".join(c[1] for c in COLUMNS) + " |")
        lines.append("|" + "---|" * (len(COLUMNS) + 2))
        base = medians.get(plan.baseline)
        for name, m in medians.items():
            cells = []
            delta = {}
            for key, _, spec in COLUMNS:
                cell = f"{m[key]:{'.1f' if spec == 'd' else spec}}"
                if base is not None and name != plan.baseline:
                    delta[key] = m[key] - base[key]
                    cell += f" ({delta[key]:+.1f})"
                cells.append(cell)
            if delta:
                figures["deltas"][name] = delta
            lines.append(f"| {name} | {int(m['runs'])} | " + " | ".join(cells) + " |")
    return "\n".join(lines) + "\n", figures


def set_medians(done: dict[int, dict[str, object]]) -> dict[str, dict[str, float]]:
    """Each set's median of every column over its runs that passed, in first-run order."""
    by_set: dict[str, list[dict[str, object]]] = {}
    for step in sorted(done):
        rec = done[step]
        if rec.get("ok"):
            by_set.setdefault(str(rec["set"]), []).append(rec["stats"])
    return {
        name: {"runs": float(len(runs)), **{k: statistics.median(float(r.get(k, 0)) for r in runs) for k, _, _ in COLUMNS}}
        for name, runs in by_set.items()
    }
