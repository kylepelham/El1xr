"""`dev.py train --all`: every pending arm of a plan back to back, unattended, in one call.

Each arm runs as `--run` does (its prewarm, the gpu.lock file around the game, the prewarm ahead of
the next arm), then the report, verdict and visual diff are written and one line is printed. The
call stops at once, writing why to `<train dir>/STOPPED.md`, on:

- a driver reset during the arm (the reset count rose),
- `device removed` (WARN/ERROR) or `DEVICE_HUNG` in its log,
- a hang watch stall over dev_early.STALL_S,
- a run with 0 frames,
- a world that ran at under STOP_FPS settled fps.

A stopped arm is saved with `stopped` and runs again on the next `--all`. Any other failed arm
(no world, a failed prewarm) is tried once more, then saved with `failed` and the call goes on.
`--max-minutes N` starts no arm expected to end past N minutes from the call's start (the
expected time: the median wall time of this call's arms so far, else the arm's seconds plus
dev_train.ALL_ARM_EXTRA_S). Ctrl+C ends the call; finished arms are saved and `--all` resumes
with the arm it interrupted.
"""

from __future__ import annotations

import statistics
import time
from collections.abc import Callable
from datetime import UTC, datetime
from pathlib import Path

import dev_early
import dev_greps
import dev_train

STOP_FPS = 5.0
RETRIES = 1
HANG_LINES = 20
# (stats key, label, format) of the per-arm line.
ARM_FIELDS = (
    ("fps", "fps", ".1f"), ("prep_ms", "prep", ".1f"), ("steady_p99_ms", "p99", ".1f"),
    ("world_over250", ">250", ".0f"), ("boot_s", "boot_s", ".1f"), ("settled_s", "settled_s", ".1f"),
    ("stream_s", "stream_s", ".1f"), ("refused", "refused", ".0f"),
)


def stop_reason(record: dict[str, object]) -> str | None:
    """Why this arm stops the whole call, or None."""
    if record.get("prewarm_failed"):
        return None
    stats = record["stats"]
    before, after = record.get("resets_before"), record.get("resets_after")
    if before is not None and after is not None and after > before:
        return f"driver reset during the run (resets since boot {before} -> {after})"
    fatal = record.get("fatal") or {}
    if stats.get("device_removed") or fatal.get("DEVICE_HUNG"):
        return "device removed / DEVICE_HUNG in the log"
    stall = float(stats.get("hang_stall_s", 0.0))
    if stall > dev_early.STALL_S:
        return f"hang watch: the card's fence stood {stall:.1f} s (over {dev_early.STALL_S:g} s)"
    if int(stats.get("frames", 0)) == 0:
        return "0 frames"
    if int(stats.get("world_samples", 0)) > 0 and float(stats.get("fps", 0.0)) < STOP_FPS:
        return f"settled fps {float(stats['fps']):.1f} under {STOP_FPS:g}"
    return None


def arm_line(plan: dev_train.Plan, done: dict[int, dict[str, object]], step: int, extra: str = "") -> str:
    """One line per arm: its figures with deltas against the baseline set's medians, how it ended, its wall time."""
    rec = done[step]
    rid = dev_train.run_id(step, str(rec["set"]))
    base = dev_train.set_medians({s: r for s, r in done.items() if r["set"] == plan.baseline}).get(plan.baseline)
    stats = rec["stats"]
    parts = []
    for key, label, spec in ARM_FIELDS:
        value = float(stats.get(key, 0))
        cell = f"{label}={value:{spec}}"
        if base is not None and rec["set"] != plan.baseline and key in base:
            cell += f"({value - base[key]:+.1f})"
        parts.append(cell)
    status = "STOP" if rec.get("stopped") else "FAIL" if not rec.get("ok") else "PASS"
    ended = rec.get("ended") or {}
    how = f"{ended.get('how', 'time')}@{ended['at_s']:.0f}s" if "at_s" in ended else ended.get("how", "-")
    resets = rec.get("resets_after")
    tail = f" {extra}" if extra else ""
    return (f"DEV train-arm {status} run={rid} {' '.join(parts)} ended={how} resets={'?' if resets is None else resets}"
            f" wall_s={float(rec.get('wall_s', 0)):.0f}{tail}")


def write_stopped(
    train_dir: Path, plan: dev_train.Plan, rid: str, reason: str, record: dict[str, object] | None, hang: list[str]
) -> Path:
    path = train_dir / "STOPPED.md"
    lines = [
        f"# train {plan.name} stopped", "",
        f"- arm: {rid}",
        f"- when: {datetime.now(UTC).isoformat(timespec='seconds')}",
        f"- why: {reason}",
    ]
    if record and record.get("log"):
        lines.append(f"- log: {record['log']}")
    if hang:
        lines += ["", "Hang lines:", "", "```", *hang[:HANG_LINES], "```"]
    lines += [
        "", "Resume: fix the cause first (after a driver reset: restart the adapter and make one OFF health run), then",
        "`dev.py train --plan <plan> --all` runs this arm again and goes on.",
    ]
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


def run_all(
    plan: dev_train.Plan,
    done: dict[int, dict[str, object]],
    train_dir: Path,
    pending: list[tuple[int, str]],
    run_arm: Callable[[int, str], dict[str, object] | None],
    after_arm: Callable[[int], str],
    max_minutes: float,
    read_text: Callable[[Path], str],
    clock: Callable[[], float] = time.monotonic,
) -> int:
    """0 when every pending arm ran (or the time budget ended the call), 1 when an arm could not start, 2 on a stop,
    130 on Ctrl+C. `run_arm` runs and saves one arm (None: it could not start); `after_arm` writes the report and
    returns extra fields for the arm's line."""
    stopped = train_dir / "STOPPED.md"
    if stopped.exists():
        first = next((ln for ln in stopped.read_text(encoding="utf-8").splitlines() if ln.startswith("- why:")), "")
        print(f"DEV train-all: resuming after an earlier stop ({first[2:]})", flush=True)
        stopped.unlink()
    ids = [dev_train.run_id(s, n) for s, n in pending]
    print(f"DEV train-all: {len(pending)} arms pending ({' '.join(ids) or 'none'}); gpu lock ttl_minutes>="
          f"{dev_train.all_ttl_minutes(plan, pending)}", flush=True)
    t0 = clock()
    walls: list[float] = []
    counts = [0, 0]
    rid = ids[0] if ids else "done"
    try:
        return _arms(plan, done, train_dir, pending, run_arm, after_arm, max_minutes, read_text, clock, t0, walls, counts)
    except KeyboardInterrupt:
        nxt = dev_train.next_pending(plan, dev_train.load_results(train_dir)) if train_dir.is_dir() else rid
        print(f"DEV train-all STOP reason=interrupted ran={counts[0]} failed={counts[1]} next={nxt} "
              f"(resume with --all)", flush=True)
        return 130


def _arms(plan, done, train_dir, pending, run_arm, after_arm, max_minutes, read_text, clock, t0, walls, counts) -> int:
    stopped = train_dir / "STOPPED.md"
    for step, name in pending:
        ran, failed = counts
        rid = dev_train.run_id(step, name)
        if max_minutes > 0:
            expect = statistics.median(walls) if walls else dev_train.step_seconds(plan, name) + dev_train.ALL_ARM_EXTRA_S
            if clock() - t0 + expect > max_minutes * 60:
                print(f"DEV train-all STOP reason=max-minutes ran={ran} failed={failed} next={rid} "
                      f"(resume with --all)", flush=True)
                return 0
        for attempt in range(RETRIES + 1):
            start = clock()
            record = run_arm(step, name)
            walls.append(clock() - start)
            if record is None:
                write_stopped(train_dir, plan, rid, "the arm could not start (setup: game folder, save or plan)", None, [])
                print(f"DEV train-all FAIL reason=setup run={rid} stopped={stopped}", flush=True)
                return 1
            done[step] = record
            if attempt:
                record["attempt"] = attempt + 1
                dev_train.save_result(train_dir, step, record)
            reason = stop_reason(record)
            if reason:
                record["stopped"] = reason
                dev_train.save_result(train_dir, step, record)
                log = Path(str(record["log"])) if record.get("log") else None
                hang = dev_greps.hang_lines(read_text(log)) if log is not None else []
                write_stopped(train_dir, plan, rid, reason, record, hang)
                print(arm_line(plan, done, step, after_arm(step)), flush=True)
                print(f"DEV train-all STOP reason=\"{reason}\" run={rid} stopped={stopped}", flush=True)
                return 2
            if record.get("ok") or attempt == RETRIES:
                break
            print(f"DEV train-all: {rid} failed (attempt {attempt + 1}); trying it once more", flush=True)
        if not record.get("ok"):
            record["failed"] = True
            dev_train.save_result(train_dir, step, record)
            counts[1] += 1
        counts[0] += 1
        print(arm_line(plan, done, step, after_arm(step)), flush=True)
    print(f"DEV train-all PASS ran={counts[0]} failed={counts[1]} minutes={(clock() - t0) / 60:.1f} "
          f"report={train_dir / 'report.md'} verdict={train_dir / 'verdict.md'}", flush=True)
    return 0
