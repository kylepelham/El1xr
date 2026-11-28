"""The merge gate behind `dev.py test` and `dev.py gate`: one build, then every check at once.

The build compiles every VIctory workspace test binary and `xtask` in one cargo call (cargo
builds in parallel and holds its directory lock). Then the test binaries run directly, without
cargo, side by side with `cargo xtask policy` and pytest (with `PYTEST_WORKERS` pytest-xdist
workers when the test interpreter has the plugin: 33-41 s in one process, 15-21 s with 4).

Binaries in `SHARDED_BINARIES` run as several processes of one test thread each, every test in
exactly one of them; the other binaries use libtest's default. `victory_shader`'s GPU tests take
one process-wide lock and create a D3D12 device each; with libtest's default 32 threads in one
process the binary took 497-850 s here (and timed out at 900 s in one gate), against 60 s on one
thread and 24-28 s as 6-8 processes of one thread (measured 2026-11-27 with other jobs building;
on a busier machine later that day 6 shards took 47-51 s and 8 took 30-35 s).
`victory_gpu`'s tests share process-wide state (the write-watch tables, guarded pages, the lazy
copy-back switch) and need one test thread in their process; each shard is its own process with
its own state, so sharding keeps that (18 s on one thread, every test also passed alone in a
process of its own).
"""

from __future__ import annotations

import contextlib
import json
import os
import re
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, ContextManager

SHARDED_BINARIES = {"victory_shader": 8, "victory_gpu": 4}
PYTEST_WORKERS = 4
MAX_SHARD_ARGS_CHARS = 24_000  # Windows command lines end at 32,767 characters
STEP_TIMEOUT_S = 900.0
CARGO_RESULT_RE = re.compile(r"test result: (?:ok|FAILED)\.\s*(\d+) passed;\s*(\d+) failed;")
PYTEST_COUNT_RE = re.compile(r"\b(\d+) (passed|failed|error|errors)\b")
SUMMARY_WARNING_RE = re.compile(r"^\d+ warnings? emitted")


@dataclass
class Step:
    """One check of the run phase and, once run, what it gave."""

    label: str
    cmd: list[str]
    cwd: Path
    kind: str  # "cargo", "pytest" or "policy"
    ok: bool = False
    secs: float = 0.0
    output: str = ""
    passed: int = 0
    failed: int = 0


@dataclass
class Build:
    ok: bool
    secs: float
    binaries: list[tuple[str, str]] = field(default_factory=list)  # (label, executable)
    warnings: list[str] = field(default_factory=list)
    log: str = ""


def parse_cargo_messages(lines: list[str], workspace: Path) -> tuple[list[tuple[str, str]], list[str]]:
    """Test executables (label, path) and rendered warnings of workspace crates from
    `cargo --message-format=json` output. Summary lines ("3 warnings emitted") are not warnings."""
    binaries: list[tuple[str, str]] = []
    warnings: list[str] = []
    root = workspace.resolve().as_posix().lower()
    seen: set[str] = set()
    for line in lines:
        if not line.startswith("{"):
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        reason = msg.get("reason")
        if reason == "compiler-artifact" and msg.get("profile", {}).get("test") and msg.get("executable"):
            target = msg["target"]
            label = target["name"].replace("-", "_")
            if "lib" not in target["kind"]:
                label = f"{label}[{target['kind'][0]}]"
            binaries.append((label, msg["executable"]))
        elif reason == "compiler-message":
            diag = msg.get("message", {})
            manifest = Path(msg.get("manifest_path", "")).resolve().as_posix().lower()
            if diag.get("level") != "warning" or not manifest.startswith(root):
                continue
            if not diag.get("spans") and SUMMARY_WARNING_RE.match(diag.get("message", "")):
                continue
            rendered = diag.get("rendered") or diag.get("message", "")
            if rendered not in seen:
                seen.add(rendered)
                warnings.append(rendered)
    return binaries, warnings


def count_results(kind: str, output: str) -> tuple[int, int]:
    """(passed, failed) from a libtest binary's or pytest's output."""
    if kind == "cargo":
        found = CARGO_RESULT_RE.findall(output)
        return sum(int(p) for p, _ in found), sum(int(f) for _, f in found)
    if kind == "pytest":
        passed = failed = 0
        summary_line = next(
            (line for line in reversed(output.splitlines()) if PYTEST_COUNT_RE.search(line) and " in " in line), ""
        )
        for n, what in PYTEST_COUNT_RE.findall(summary_line):
            if what == "passed":
                passed = int(n)
            else:
                failed += int(n)
        return passed, failed
    return 0, 0


def python_for_tests(repo_root: Path, coord_dir: Path) -> str:
    """The interpreter with the test requirements: `ANYXBOXX_PYTHON`, the checkout's `.venv`,
    the shared `AnyXboxX-coord/venv`, else this one."""
    explicit = os.environ.get("ANYXBOXX_PYTHON")
    if explicit:
        return explicit
    for venv in (repo_root / ".venv", coord_dir / "venv"):
        exe = venv / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
        if exe.is_file():
            return str(exe)
    return sys.executable


def build(manifest: Path, crate: str | None) -> Build:
    """Compiles the test binaries (every workspace crate, or `crate`) and xtask."""
    t0 = time.monotonic()
    scope = ["-p", crate] if crate else ["--workspace"]
    test_cmd = ["cargo", "test", "--no-run", "--message-format=json", "--manifest-path", str(manifest), *scope]
    proc = subprocess.run(test_cmd, capture_output=True, text=True, encoding="utf-8", errors="replace")
    binaries, warnings = parse_cargo_messages(proc.stdout.splitlines(), manifest.parent)
    log = f"=== build: {' '.join(test_cmd)} exit={proc.returncode} ===\n{proc.stderr}"
    ok = proc.returncode == 0
    if ok:
        xtask_cmd = ["cargo", "build", "--quiet", "-p", "xtask", "--manifest-path", str(manifest)]
        xproc = subprocess.run(xtask_cmd, capture_output=True, text=True, encoding="utf-8", errors="replace")
        log += f"=== build: {' '.join(xtask_cmd)} exit={xproc.returncode} ===\n{xproc.stdout}{xproc.stderr}"
        ok = xproc.returncode == 0
    if warnings:
        log += "=== warnings ===\n" + "\n".join(warnings) + "\n"
    return Build(ok, time.monotonic() - t0, binaries, warnings, log)


def list_tests(exe: str, cwd: Path) -> list[str] | None:
    """The test names of a libtest binary (`--list`, ignored ones included), or None if it cannot
    list them."""
    try:
        proc = subprocess.run(
            [exe, "--list"], cwd=str(cwd), capture_output=True, text=True, encoding="utf-8",
            errors="replace", timeout=120,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if proc.returncode != 0:
        return None
    return sorted(line[: -len(": test")] for line in proc.stdout.splitlines() if line.endswith(": test"))


def shards(names: list[str], count: int) -> list[list[str]]:
    """`names` dealt round robin into `count` non-empty groups (more when a group's command line
    would grow too long), so the neighbouring tests of one slow module spread out."""
    total = sum(len(n) + 3 for n in names)
    count = max(count, -(-total // MAX_SHARD_ARGS_CHARS))
    count = max(1, min(count, len(names)))
    return [names[i::count] for i in range(count)]


def cargo_steps(label: str, exe: str, victory_dir: Path) -> list[Step]:
    """A sharded binary as its shards, or as one process of one thread when it cannot list its tests."""
    if label not in SHARDED_BINARIES:
        return [Step(label, [exe], victory_dir, "cargo")]
    names = list_tests(exe, victory_dir)
    if not names:
        return [Step(label, [exe, "--test-threads=1"], victory_dir, "cargo")]
    return [
        Step(f"{label}#{i + 1}", [exe, "--test-threads=1", "--exact", *group], victory_dir, "cargo")
        for i, group in enumerate(shards(names, SHARDED_BINARIES[label]))
    ]


def pytest_command(python: str) -> list[str]:
    """pytest with `PYTEST_WORKERS` pytest-xdist workers, or in one process when `python` lacks xdist."""
    cmd = [python, "-m", "pytest", "-q"]
    try:
        probe = subprocess.run([python, "-c", "import xdist"], capture_output=True, timeout=60)
    except (OSError, subprocess.TimeoutExpired):
        return cmd
    return cmd + ["-n", str(PYTEST_WORKERS)] if probe.returncode == 0 else cmd


def plan(b: Build, victory_dir: Path, repo_root: Path, python: str) -> list[Step]:
    steps = [step for label, exe in b.binaries for step in cargo_steps(label, exe, victory_dir)]
    steps.append(Step("policy", ["cargo", "xtask", "policy"], victory_dir, "policy"))
    steps.append(Step("pytest", pytest_command(python), repo_root, "pytest"))
    return steps


def run_step(step: Step) -> Step:
    t0 = time.monotonic()
    try:
        proc = subprocess.run(
            step.cmd, cwd=str(step.cwd), stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, encoding="utf-8", errors="replace", timeout=STEP_TIMEOUT_S,
        )
        step.output, step.ok = proc.stdout or "", proc.returncode == 0
        if not step.ok:
            step.output += f"\n(exit code {proc.returncode})\n"
    except subprocess.TimeoutExpired as exc:
        out = exc.stdout or b""
        step.output = (out.decode("utf-8", "replace") if isinstance(out, bytes) else out) + (
            f"\n(timed out after {STEP_TIMEOUT_S:.0f} s)\n"
        )
        step.ok = False
    step.secs = time.monotonic() - t0
    step.passed, step.failed = count_results(step.kind, step.output)
    if step.kind == "cargo" and not step.ok and step.failed == 0:
        step.failed = 1  # crashed or hung: no result line
    return step


def run_all(steps: list[Step]) -> list[Step]:
    """All at once: the slowest one bounds the wall time."""
    with ThreadPoolExecutor(max_workers=max(1, len(steps))) as pool:
        return list(pool.map(run_step, steps))


def _shown(cmd: list[str]) -> str:
    """A command for the log, a shard's test names counted rather than listed."""
    if "--exact" in cmd:
        at = cmd.index("--exact") + 1
        return " ".join(cmd[:at]) + f" <{len(cmd) - at} tests>"
    return " ".join(cmd)


def write_log(path: Path, b: Build, steps: list[Step]) -> None:
    with path.open("w", encoding="utf-8", errors="replace") as logf:
        logf.write(b.log)
        for s in steps:
            logf.write(f"=== {s.label}: {_shown(s.cmd)} (cwd={s.cwd}) ===\n{s.output}")
            if s.output and not s.output.endswith("\n"):
                logf.write("\n")
            logf.write(f"--- {s.label} {'ok' if s.ok else 'FAIL'} secs={s.secs:.1f} ---\n")


def summary(b: Build, steps: list[Step], wall: float) -> dict[str, str]:
    """`key=value` fields of the DEV line."""
    slowest = max(steps, key=lambda s: s.secs, default=None)
    failing = ([] if b.ok else ["build"]) + [s.label for s in steps if not s.ok]
    fields = {
        "passed": str(sum(s.passed for s in steps)),
        "failed": str(sum(s.failed for s in steps)),
        "warnings": str(len(b.warnings)),
        "build_s": f"{b.secs:.1f}",
        "run_s": f"{max((s.secs for s in steps), default=0.0):.1f}",
        "wall_s": f"{wall:.1f}",
    }
    if slowest is not None:
        fields["slowest"] = f"{slowest.label}:{slowest.secs:.1f}"
    if failing:
        fields["failing"] = ",".join(failing)
    return fields


def gate(
    victory_dir: Path, repo_root: Path, coord_dir: Path, log_path: Path, crate: str | None,
    build_slot: Callable[[], ContextManager[object]] = contextlib.nullcontext,
    wait_room: Callable[[], object] = lambda: None,
) -> tuple[bool, Build, dict[str, str]]:
    """Builds inside `build_slot()`, then outside it runs every check at once and logs them;
    whether all passed, the build, and the DEV line's fields. `wait_room()` runs before the checks
    (they hold tens of GB of commit: `dev.py` waits there while a game run is short of it)."""
    t0 = time.monotonic()
    with build_slot():
        b = build(victory_dir / "Cargo.toml", crate)
    steps: list[Step] = []
    if b.ok:
        wait_room()
        steps = run_all(plan(b, victory_dir, repo_root, python_for_tests(repo_root, coord_dir)))
    write_log(log_path, b, steps)
    ok = b.ok and all(s.ok for s in steps)
    return ok, b, summary(b, steps, time.monotonic() - t0)
