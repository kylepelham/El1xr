"""`dev.py train` glue for multilive launches (tools/dev_multilive.py): what a launch runs, its record, and the
analyses, verdict rows and visuals read back from the runs' logs and snaps."""

from __future__ import annotations

import json
from collections.abc import Callable
from pathlib import Path

import dev_multilive
import dev_multivis
import dev_train

_CACHE: dict[tuple[str, int, float, tuple[str, ...], bool], dict[str, object]] = {}


def prepare(
    plan: dev_train.Plan, done: dict[int, dict[str, object]], train_dir: Path, step: int, name: str
) -> tuple[list[str], dict[str, object]]:
    """The arms this launch runs and the part of its record known before it (carried results, arms left out).
    A retry of a step keeps the earlier attempt's log and snaps as `<run>.try<k>.log` / `<run>_snaps.try<k>`."""
    ml = plan.multilive[name]
    active, carried, left = dev_multilive.owed(ml, done, step)
    rid = dev_train.run_id(step, name)
    log = train_dir / f"{rid}.log"
    if active and step in done and log.exists():
        k = 1
        while (train_dir / f"{rid}.try{k}.log").exists():
            k += 1
        log.rename(train_dir / f"{rid}.try{k}.log")
        snaps = train_dir / f"{rid}_snaps"
        if snaps.is_dir():
            snaps.rename(train_dir / f"{rid}_snaps.try{k}")
    if active:
        schedule = train_dir / f"{rid}.schedule.txt"
        schedule.write_text(dev_multilive.schedule_text(ml, active), encoding="utf-8")
    info = {"carried": carried, "carried_status": {n: "complete" for n in carried}, "left_out": left}
    return active, info


def skipped_record(step: int, name: str, info: dict[str, object], empty_stats: dict[str, object]) -> dict[str, object]:
    """The record of a launch with no arm owed (nothing runs)."""
    return {
        "step": step, "set": name, "kind": "multilive", "ok": True, "skipped": "no arm owed (every arm decided, "
        "carried or a suspect)", "stats": empty_stats, "snaps": 0, "resets_before": None, "resets_after": None,
        "log": "", "seconds": 0, "ended": {"how": "skipped"}, "fatal": {}, "wall_s": 0.0,
        "multilive": {"arms": [], "results": {}, "status": {}, "problems": [], **info},
    }


def stopped(record: dict[str, object], reason: str | None) -> bool:
    return bool(reason or record.get("stopped")) or (record.get("ended") or {}).get("how") == "fatal"


def analyses(
    plan: dev_train.Plan, done: dict[int, dict[str, object]], train_dir: Path, read_text: Callable[[Path], str]
) -> dict[int, dict[str, object]]:
    """Every multilive run's analysis, read again from its log (the saved one when the log or the plan's entry is
    gone), with what it carried."""
    out = {}
    for step, rec in done.items():
        ml, info = plan.multilive.get(str(rec["set"])), rec.get("multilive")
        if not isinstance(info, dict):
            continue
        log = train_dir / f"{dev_train.run_id(step, str(rec['set']))}.log"
        arms = tuple(info.get("arms", []))
        if ml is not None and arms and log.exists():
            st = log.stat()
            key = (str(log), st.st_size, st.st_mtime, arms, stopped(rec, None))
            try:
                if key not in _CACHE:
                    _CACHE[key] = dev_multilive.analyse(ml, list(arms), read_text(log), key[-1])
                info = {**info, **_CACHE[key]}
            except dev_multilive.MultiLiveError:
                pass
        out[step] = info
    return out


def verdict_rows(analysed: dict[int, dict[str, object]]) -> list[dict[str, object]]:
    return [row for _, info in sorted(analysed.items()) for row in dev_multilive.verdict_rows(info)]


def visuals(done: dict[int, dict[str, object]], train_dir: Path, analysed: dict[int, dict[str, object]]) -> list[str]:
    """Per arm sheets of every multilive run with snaps; one line per arm."""
    lines = []
    for step, info in sorted(analysed.items()):
        rid = dev_train.run_id(step, str(done[step]["set"]))
        snaps = train_dir / f"{rid}_snaps"
        if info.get("results") and snaps.is_dir():
            lines += dev_multivis.lines(dev_multivis.update(rid, info, snaps, train_dir / "autovis"))
    return lines


def cached_visual_lines(done: dict[int, dict[str, object]], train_dir: Path) -> list[str]:
    """The per arm visual lines already made (for report.md; nothing is computed)."""
    lines = []
    for step in sorted(done):
        path = train_dir / "autovis" / dev_train.run_id(step, str(done[step]["set"])) / "multilive.json"
        if done[step].get("kind") == "multilive" and path.exists():
            lines += dev_multivis.lines(json.loads(path.read_text(encoding="utf-8")))
    return lines


def run_lines(rid: str, info: dict[str, object]) -> list[str]:
    """One `DEV train-multilive` line per arm of a finished launch."""
    out = []
    results, status = dev_multilive.merged(info)
    for arm, res in results.items():
        d = res["delta"]
        figs = f"delta_fps={d['fps']:+.1f} delta_prep={d['prep_ms']:+.1f} delta_p99={d['p99_ms']:+.1f}" if d else "delta=-"
        out.append(f"DEV train-multilive run={rid} arm=\"{arm}\" status=\"{status.get(arm, '?')}\" "
                   f"verdict={dev_multilive.verdict(res)} {figs} noisy={','.join(res['noisy']) or '-'}")
    for arm, why in info.get("left_out", {}).items():
        out.append(f"DEV train-multilive run={rid} arm=\"{arm}\" left_out=\"{why}\"")
    return out
