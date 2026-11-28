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

    [greps]                             # log lines counted per run (see tools/dev_greps.py)
    foo_skipped = 'foo copies skipped: ([0-9]+)'

    prewarm = "pipelines"               # before each arm, in the same call: "pipelines" (translate + the recipes'
                                        # host pipelines, under the gpu lock), "shaders", or "none"; a no-op
                                        # when a stamp says that variant is warm (tools/dev_warm.py)
    prewarm_ahead = true                # while an arm runs (not a quiet one), translate the next arm's
                                        # variant at idle priority
    ahead_jobs = 8                      # its worker threads
    early_stop = true                   # end a run once the settled world is steady (tools/dev_early.py);
                                        # default: on for loud arms, off for quiet ones
    min_seconds = 90                    # early stop never before this (from launch), never after `seconds`
    steady_seconds = 60                 # steady windows needed after the world settled
    profile = "release"                 # the cargo profile whose victory.exe runs (dev_warm.RELEASE_PROFILES;
                                        # release-lto: build it first with `dev.py build --profile release-lto`)

A set may say `quiet = true|false`, `early_stop = true|false` and `profile = "..."` (its runs only; not inherited
through `from`): two exes A/B'd in one plan. `[live.<name>]` tables are
live arms (one launch, Live settings switched A/B/A/B; see tools/dev_live.py) and `[multilive.<name>]` tables
multi-switch live launches (one launch, several Live switches A/B'd one after another; see tools/dev_multilive.py);
`order` names both like sets.

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
from dataclasses import dataclass, field
from pathlib import Path

import dev_greps
import dev_live
import dev_multilive
import dev_warm

OVERRIDE_KEY_RE = re.compile(r"^VICTORY_[A-Z0-9_]+$")
SET_NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]*$")
RESULT_RE = re.compile(r"^(\d+)_(.+)\.json$")
SNAP_RE = re.compile(r"^snap-(\d+)s-f(\d+)\.png$")
DEFAULT_ORDER = ("on", "off", "off", "on")
DEFAULT_SECONDS = 200
DEFAULT_SNAP_EVERY = 15.0
PLAN_KEYS = {
    "seconds", "quiet", "snap_every", "save", "fast_boot", "unmute", "order", "baseline", "common", "sets", "greps",
    "live", "multilive", "prewarm", "prewarm_ahead", "ahead_jobs", "early_stop", "min_seconds", "steady_seconds",
    "profile",
}
SET_FLAGS = ("from", "quiet", "early_stop", "profile")
DEFAULT_MIN_SECONDS = 90
DEFAULT_STEADY_SECONDS = 60
# `--all`: per arm beyond its seconds (boot, prewarm made ahead, report), plus once for the first prewarm and a retry.
ALL_ARM_EXTRA_S = 150
ALL_EXTRA_S = 300
PREWARM_MODES = ("pipelines", "shaders", "none")
DEFAULT_AHEAD_JOBS = 8
# A run's Drift gpu lock: its seconds, plus boot grace, prewarm and margin, never under 20 minutes (AGENTS.md).
TTL_EXTRA_S = 300
TTL_MIN_MINUTES = 20

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
    greps: dict[str, re.Pattern[str]] = field(default_factory=dict)
    live: dict[str, dev_live.LiveArm] = field(default_factory=dict)
    multilive: dict[str, dev_multilive.MultiLive] = field(default_factory=dict)
    set_quiet: dict[str, bool] = field(default_factory=dict)
    prewarm: str = "pipelines"
    prewarm_ahead: bool = True
    ahead_jobs: int = DEFAULT_AHEAD_JOBS
    early_stop: bool | None = None
    set_early: dict[str, bool] = field(default_factory=dict)
    min_seconds: int = DEFAULT_MIN_SECONDS
    steady_seconds: int = DEFAULT_STEADY_SECONDS
    profile: str = "release"
    set_profile: dict[str, str] = field(default_factory=dict)


def _profile(where: str, value: object) -> str:
    if value not in dev_warm.RELEASE_PROFILES:
        raise PlanError(f"{where}: one of {', '.join(dev_warm.RELEASE_PROFILES)}, not '{value}'")
    return str(value)


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
        if key in SET_FLAGS:
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


def parse_plan(name: str, data: dict[str, object], kinds: dict[str, str] | None = None) -> Plan:
    """`kinds`: the settings registry's apply kind per setting (live arms schedule Live ones only), if known."""
    unknown = set(data) - PLAN_KEYS
    if unknown:
        raise PlanError(f"unknown plan keys: {', '.join(sorted(unknown))}")
    raw_sets = data.get("sets")
    if not isinstance(raw_sets, dict) or not raw_sets:
        raise PlanError("the plan names no sets ([sets.<name>])")
    sets = resolve_sets(raw_sets, _overrides("common", data.get("common", {})))
    try:
        live = dev_live.parse_live(data.get("live", {}), sets, kinds)
    except dev_live.LiveError as exc:
        raise PlanError(str(exc)) from exc
    order = [str(s) for s in data.get("order", DEFAULT_ORDER)]
    baseline = str(data.get("baseline", "off" if "off" in sets else order[0] if order else next(iter(sets))))
    try:
        multilive = dev_multilive.parse_multilive(data.get("multilive", {}), sets, baseline, kinds)
    except dev_multilive.MultiLiveError as exc:
        raise PlanError(str(exc)) from exc
    clash = set(multilive) & set(live)
    if clash:
        raise PlanError(f"multilive and live entries share names: {', '.join(sorted(clash))}")
    prewarm = str(data.get("prewarm", "pipelines"))
    if prewarm not in PREWARM_MODES:
        raise PlanError(f"prewarm: one of {', '.join(PREWARM_MODES)}, not '{prewarm}'")
    missing = [s for s in order if s not in sets and s not in live and s not in multilive]
    if missing:
        raise PlanError(f"order names sets the plan lacks: {', '.join(missing)}")
    if baseline not in sets:
        raise PlanError(f"baseline '{baseline}' is not a set")
    try:
        greps = dev_greps.parse_greps(data.get("greps", {}))
    except ValueError as exc:
        raise PlanError(str(exc)) from exc
    profile = _profile("profile", data.get("profile", "release"))
    set_profile = {
        n: _profile(f"sets.{n}.profile", t["profile"])
        for n, t in raw_sets.items() if isinstance(t, dict) and "profile" in t
    }
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
        greps=greps,
        live=live,
        multilive=multilive,
        set_quiet={n: bool(t["quiet"]) for n, t in raw_sets.items() if isinstance(t, dict) and "quiet" in t},
        prewarm=prewarm,
        prewarm_ahead=bool(data.get("prewarm_ahead", True)),
        ahead_jobs=int(data.get("ahead_jobs", DEFAULT_AHEAD_JOBS)),
        early_stop=bool(data["early_stop"]) if "early_stop" in data else None,
        set_early={n: bool(t["early_stop"]) for n, t in raw_sets.items() if isinstance(t, dict) and "early_stop" in t},
        min_seconds=int(data.get("min_seconds", DEFAULT_MIN_SECONDS)),
        steady_seconds=int(data.get("steady_seconds", DEFAULT_STEADY_SECONDS)),
        profile=profile,
        set_profile=set_profile,
    )


def load_plan(path: Path, kinds: dict[str, str] | None = None) -> Plan:
    text = path.read_text(encoding="utf-8")
    try:
        data = json.loads(text) if path.suffix.lower() == ".json" else tomllib.loads(text)
    except (json.JSONDecodeError, tomllib.TOMLDecodeError) as exc:
        raise PlanError(f"{path}: {exc}") from exc
    return parse_plan(re.sub(r"[^A-Za-z0-9_.-]+", "_", path.stem), data, kinds)


def step_overrides(plan: Plan, name: str) -> dict[str, str]:
    """The launch overrides of a set or live arm (a live arm launches with its set A's values, a multilive launch
    with its base set's)."""
    if name in plan.multilive:
        return dict(plan.multilive[name].launch)
    return dict(plan.live[name].launch) if name in plan.live else dict(plan.sets[name])


def step_seconds(plan: Plan, name: str, done: dict[int, dict[str, object]] | None = None, step: int = 0) -> int:
    """A step's run seconds; a multilive launch's from the arms it still owes (all of them without `done`)."""
    if name in plan.multilive:
        ml = plan.multilive[name]
        active = dev_multilive.owed(ml, done, step)[0] if done is not None else [a.name for a in ml.arms]
        return dev_multilive.seconds(ml, active) if active else 0
    return plan.live[name].seconds if name in plan.live else plan.seconds


def step_quiet(plan: Plan, name: str) -> bool:
    entry = plan.live.get(name) or plan.multilive.get(name)
    if entry is not None and entry.quiet is not None:
        return bool(entry.quiet)
    return plan.set_quiet.get(name, plan.quiet)


def step_profile(plan: Plan, name: str) -> str:
    """The cargo profile whose victory.exe a step runs: the set's `profile`, else the plan's (live and multilive
    launches take the plan's)."""
    return plan.set_profile.get(name, plan.profile)


def step_early_stop(plan: Plan, name: str, quiet: bool) -> bool:
    """Whether a run of this set ends early once steady: the set's `early_stop`, else the plan's, else on for a
    loud run and off for a quiet one. Never for a live arm (its segments are scheduled)."""
    if name in plan.live or name in plan.multilive:
        return False
    if name in plan.set_early:
        return plan.set_early[name]
    return plan.early_stop if plan.early_stop is not None else not quiet


def pending_steps(plan: Plan, done: dict[int, dict[str, object]]) -> list[tuple[int, str]]:
    """What `--all` runs: every step of the order not run yet, and every step a stop ended (run again)."""
    return [
        (step, name) for step, name in enumerate(plan.order, 1)
        if step not in done or done[step].get("stopped")
    ]


def all_ttl_minutes(plan: Plan, pending: list[tuple[int, str]]) -> int:
    """The Drift gpu lock's ttl_minutes for one `--all` call over these steps (every arm at its full seconds)."""
    if not pending:
        return TTL_MIN_MINUTES
    secs = sum(step_seconds(plan, n) + ALL_ARM_EXTRA_S for _, n in pending)
    secs += ALL_EXTRA_S + max(step_seconds(plan, n) for _, n in pending)
    return max(TTL_MIN_MINUTES, -(-secs // 60))


def ttl_minutes(plan: Plan, name: str) -> int:
    """The Drift gpu lock's ttl_minutes for one `--run` of this set or arm."""
    return max(TTL_MIN_MINUTES, -(-(step_seconds(plan, name) + TTL_EXTRA_S) // 60))


def next_after(plan: Plan, done: dict[int, dict[str, object]], step: int) -> tuple[int, str] | None:
    """The first pending step of the order other than `step` (what runs after it)."""
    for s, name in enumerate(plan.order, 1):
        if s != step and s not in done:
            return s, name
    return None


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
    if run not in plan.sets and run not in plan.live and run not in plan.multilive:
        raise PlanError(f"no set '{run}' (sets: {', '.join([*plan.sets, *plan.live, *plan.multilive])})")
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
        if name in plan.live:
            arm = plan.live[name]
            values = (f"live {arm.a} vs {arm.b} {''.join(arm.labels())} x {arm.segment_s:g} s from {arm.start_s:g} s: "
                      + " ".join(f"{k}={a}/{b}" for k, (a, b) in arm.keys.items()))
        elif name in plan.multilive and state == "done":
            info = done[step].get("multilive") or {}
            values = f"multilive: {done[step].get('skipped') or ' | '.join(info.get('arms', []))}"
        elif name in plan.multilive:
            values = dev_multilive.list_line(plan.multilive[name], *dev_multilive.owed(plan.multilive[name], done, step))
        elif name in plan.sets:
            values = " ".join(f"{k}={v}" for k, v in plan.sets[name].items())
        else:
            values = "(set no longer in the plan)"
        if step_profile(plan, name) != "release":
            values = f"[{step_profile(plan, name)}] {values}".rstrip()
        extra = ""
        if state == "pending" and (name in plan.sets or name in plan.live or name in plan.multilive):
            extra = f" [{step_seconds(plan, name, done, step)} s{', quiet' if step_quiet(plan, name) else ''}, gpu lock ttl>={ttl_minutes(plan, name)} min]"
        lines.append(f"{run_id(step, name)} {state}{extra}: {values or '(defaults)'}")
    return "\n".join(lines) + "\n"


def totals(plan: Plan, done: dict[int, dict[str, object]]) -> str:
    """The pending launches and their minutes: game time, and wall time with each arm's boot, prewarm and report."""
    pending = [(s, n) for s, n in pending_steps(plan, done) if step_seconds(plan, n, done, s) > 0]
    game = sum(step_seconds(plan, n, done, s) for s, n in pending)
    wall = game + ALL_ARM_EXTRA_S * len(pending)
    ran = sum(1 for r in done.values() if not r.get("skipped"))
    return (f"launches: {len(pending)} pending ({ran} run), ~{game / 60:.0f} min of game time, "
            f"~{wall / 60:.0f} min with boot, prewarm and report")


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


def report(
    plan: Plan, done: dict[int, dict[str, object]], scans: dict[int, dev_greps.LogScan] | None = None,
    lives: dict[int, dict[str, object]] | None = None, multis: dict[int, dict[str, object]] | None = None,
) -> tuple[str, dict[str, object]]:
    """The markdown report and its figures: every run, pending steps, then each set's medians and
    their differences from the baseline set, then the plan's greps per run. `scans` are the runs'
    logs read with the plan's greps; a run's flags (fatal lines, a WARN rise against the baseline
    arm) come from them."""
    scans = scans or {}
    base = dev_greps.baseline_warns([scans[s] for s in sorted(scans) if done[s]["set"] == plan.baseline])
    run_flags = {s: dev_greps.flags(scan, base, done[s]["set"] == plan.baseline) for s, scan in scans.items()}
    head = ["run", "set", *[c[1] for c in COLUMNS], "resets", "snaps", "ok", "flags"]
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
        flagged = "**" + "; ".join(f.split(":")[0] for f in run_flags[step]) + "**" if run_flags.get(step) else ""
        row = [
            run_id(step, str(rec["set"])), str(rec["set"]), *cells, _resets(rec), str(rec.get("snaps", 0)), ok, flagged,
        ]
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
    if lives:
        lines += ["", "## Live arms (median over segments; first seconds after each change left out)"]
        for s in sorted(lives):
            lines += dev_live.report_lines(run_id(s, str(done[s]["set"])), lives[s])
        figures["live"] = {run_id(s, str(done[s]["set"])): lives[s] for s in sorted(lives)}
    if multis:
        lines += ["", "## Multilive launches (per arm: median over segments, A = base, B = the arm's values)"]
        for s in sorted(multis):
            ml = plan.multilive.get(str(done[s]["set"]))
            lines += dev_multilive.report_lines(run_id(s, str(done[s]["set"])), ml, multis[s])
        figures["multilive"] = {run_id(s, str(done[s]["set"])): multis[s] for s in sorted(multis)}
    ids = {s: run_id(s, str(done[s]["set"])) for s in sorted(scans)}
    figures["runs"] = {
        ids[s]: {
            "flags": run_flags[s], "fatal": scans[s].fatal, "warns": scans[s].warns,
            "greps": {n: dev_greps.grep_figures(*v) for n, v in scans[s].greps.items()},
        }
        for s in ids
    }
    if any(run_flags.values()):
        lines += ["", "## Flags", ""]
        lines += [f"- **{ids[s]}**: " + " / ".join(run_flags[s]) for s in ids if run_flags[s]]
    hung = [s for s in ids if scans[s].hang]
    if hung:
        lines += ["", "## Hang lines (`dred:`, `hang watch:`)"]
        for s in hung:
            lines += ["", f"**{ids[s]}**", "", "```", *scans[s].hang, "```"]
    if plan.greps and ids:
        lines += ["", "## Greps (lines matched / median / last captured number)", ""]
        lines.append("| grep | regex | " + " | ".join(ids.values()) + " |")
        lines.append("|" + "---|" * (len(ids) + 2))
        for name, pattern in plan.greps.items():
            regex = pattern.pattern.replace("|", "\\|")
            cells = [dev_greps.grep_cell(*scans[s].greps[name]) for s in ids]
            lines.append(f"| {name} | `{regex}` | " + " | ".join(cells) + " |")
    return "\n".join(lines) + "\n", figures


def set_medians(done: dict[int, dict[str, object]]) -> dict[str, dict[str, float]]:
    """Each set's median of every column over its runs that passed, in first-run order (live arms mix two sets in
    one run: their figures are in their own section)."""
    by_set: dict[str, list[dict[str, object]]] = {}
    for step in sorted(done):
        rec = done[step]
        if rec.get("ok") and rec.get("kind", "set") == "set":
            by_set.setdefault(str(rec["set"]), []).append(rec["stats"])
    return {
        name: {"runs": float(len(runs)), **{k: statistics.median(float(r.get(k, 0)) for r in runs) for k, _, _ in COLUMNS}}
        for name, runs in by_set.items()
    }
