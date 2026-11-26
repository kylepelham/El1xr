"""The merge gate behind `dev.py test` and `dev.py gate`: one build, then every check at once.

The build compiles every VIctory workspace test binary and `xtask` in one cargo call (cargo
builds in parallel and holds its directory lock). Then the test binaries run directly, without
cargo, side by side with `cargo xtask policy` and pytest. Binaries in `SERIAL_BINARIES` share
process-wide state between their tests (the write-watch tables, guarded pages, the lazy
copy-back switch) and run with one test thread; the other binaries use the default.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path

SERIAL_BINARIES = frozenset({"victory_gpu"})
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


def plan(b: Build, victory_dir: Path, repo_root: Path, python: str) -> list[Step]:
    steps = [
        Step(label, [exe, *(["--test-threads=1"] if label in SERIAL_BINARIES else [])], victory_dir, "cargo")
        for label, exe in b.binaries
    ]
    steps.append(Step("policy", ["cargo", "xtask", "policy"], victory_dir, "policy"))
    steps.append(Step("pytest", [python, "-m", "pytest", "-q"], repo_root, "pytest"))
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


def write_log(path: Path, b: Build, steps: list[Step]) -> None:
    with path.open("w", encoding="utf-8", errors="replace") as logf:
        logf.write(b.log)
        for s in steps:
            logf.write(f"=== {s.label}: {' '.join(s.cmd)} (cwd={s.cwd}) ===\n{s.output}")
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
) -> tuple[bool, Build, dict[str, str]]:
    """Builds, runs every check at once and logs them; whether all passed, the build, and the
    DEV line's fields."""
    t0 = time.monotonic()
    b = build(victory_dir / "Cargo.toml", crate)
    steps: list[Step] = []
    if b.ok:
        steps = run_all(plan(b, victory_dir, repo_root, python_for_tests(repo_root, coord_dir)))
    write_log(log_path, b, steps)
    ok = b.ok and all(s.ok for s in steps)
    return ok, b, summary(b, steps, time.monotonic() - t0)
