#!/usr/bin/env python3
"""Developer CLI for El1xr / VIctory builds, tests, policy checks, benchmarks, and captures.

Full command output is saved under E:/Gta6/AnyXboxX-coord/dev/ and every command prints a single
concise final summary line: `DEV <cmd> PASS|FAIL key=value ...`.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import re
import shlex
import shutil
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterator

import dev_driver
import dev_gate
import dev_greps
import dev_live
import dev_png
import dev_prewarm
import dev_stats
import dev_throttle
import dev_train
import dev_verdict
import dev_visual
import dev_warm

REPO_ROOT = Path(__file__).resolve().parent.parent
VICTORY_DIR = REPO_ROOT / "third_party" / "victory"
VICTORY_MANIFEST = VICTORY_DIR / "Cargo.toml"
LAUNCHER_PS1 = REPO_ROOT / "tools" / "lancer_victory.ps1"
SCOUT_PY = REPO_ROOT / "tools" / "scout_ecosystem.py"

COORD_DIR = Path(os.environ.get("ANYXBOXX_COORD_DIR", "E:/Gta6/AnyXboxX-coord"))
# One log folder per worktree job: AnyXboxX-coord/<job>/dev (the main checkout logs to AnyXboxX-coord/dev).
JOB = os.environ.get("ANYXBOXX_JOB") or (REPO_ROOT.name if REPO_ROOT.parent.name == "AnyXboxX-wt" else "")
DEV_DIR = COORD_DIR / JOB / "dev" if JOB else COORD_DIR / "dev"
SAVES_DIR = COORD_DIR / "saves"
LOCKS_DIR = COORD_DIR / "locks"
GPU_LOCK = LOCKS_DIR / "gpu.lock"

SAVE_NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]*$")
TS_RE = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z)\s+")

ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
FRAME_RE = re.compile(
    r"([0-9.]+) fps, ([0-9.]+) ms/frame \(([0-9.]+) preparing: "
    r"copies ([0-9.]+) ms ([0-9.]+) MB, textures ([0-9.]+) ms ([0-9.]+) MB; "
    r"([0-9.]+) on the GPU; ([0-9.]+) copying back ([0-9.]+) MB\), "
    r"([0-9.]+) draws/frame, dropped ([0-9.]+) without a host pipeline "
    r"\((\d+) being made\) and ([0-9.]+) indirect"
)
CARD_CLOCK_RE = re.compile(r"the card's clock per frame: nothing of ours queued [0-9.]+ ms \(the card about ([0-9.]+)% busy")
PACING_RE = re.compile(
    r"frame pacing: (\d+) frames in [0-9.]+ s \([0-9.]+ fps\), frame time p50 ([0-9.]+) ms "
    r"p95 ([0-9.]+) ms p99 ([0-9.]+) ms max ([0-9.]+) ms, (\d+) over 50 ms, (\d+) over 100 ms, "
    r"present interval sd ([0-9.]+) ms jitter ([0-9.]+) ms; ([0-9.]+) draws/frame"
)
DSTORAGE_RE = re.compile(r"dstorage: last [0-9.]+ s.*?mb_per_s=([0-9.]+)")
POLICY_FILES_RE = re.compile(r"asset policy:\s*(\d+)\s+files ok")
ALERT_LEVEL_RE = re.compile(r"\b(WARN|ERROR)\b")
OVERRIDE_KEY_RE = re.compile(r"^VICTORY_[A-Z0-9_]+$")

WORLD_DRAWS_MIN = 100.0
LOADING_P50_MS = 100.0
BUILD_GRACE_SECS = 600.0


def ensure_coord_dirs() -> None:
    DEV_DIR.mkdir(parents=True, exist_ok=True)
    LOCKS_DIR.mkdir(parents=True, exist_ok=True)


def read_log_text(path: Path) -> str:
    if not path.exists():
        return ""
    raw = path.read_bytes()
    if raw[:2] in (b"\xff\xfe", b"\xfe\xff"):
        return raw.decode("utf-16", errors="replace")
    return raw.decode("utf-8", errors="replace")


def resolve_game_dir(explicit: str | None) -> Path:
    candidates = [explicit, os.environ.get("VICTORY_GAME_DIR"), "E:/Gta6/Game"]
    for item in candidates:
        if not item:
            continue
        path = Path(item)
        if (path / "game_scarlett_master.exe").is_file():
            return path
    raise FileNotFoundError(
        "Could not locate game_scarlett_master.exe; pass --game-dir or set VICTORY_GAME_DIR"
    )


def parse_overrides(items: list[str]) -> dict[str, str]:
    parsed: dict[str, str] = {}
    for item in items:
        if "=" not in item:
            raise ValueError(f"Invalid --override '{item}': expected KEY=VALUE")
        key, value = item.split("=", 1)
        key = key.strip()
        if not OVERRIDE_KEY_RE.match(key):
            raise ValueError(f"Invalid override key '{key}': must match VICTORY_*")
        parsed[key] = value
    return parsed


@contextlib.contextmanager
def acquire_gpu_lock(tag: str, timeout_s: float = 4 * 3600.0) -> Iterator[None]:
    ensure_coord_dirs()
    with dev_throttle.hold_first_free([GPU_LOCK], tag, timeout_s):
        yield


def throttled(name: str):
    """Build/test/gate/policy: refuse on a nearly full drive, run below normal priority, and wait
    for one of the machine-wide build slots so parallel worktree jobs cannot swamp the CPU."""

    def wrap(func):
        def run(args: argparse.Namespace) -> int:
            free_gb = dev_throttle.disk_free_gb(REPO_ROOT)
            if free_gb < dev_throttle.MIN_FREE_GB:
                print(
                    f"DEV {name} FAIL disk_free_gb={free_gb:.0f} "
                    f"need={dev_throttle.MIN_FREE_GB:.0f} (clear target/ of merged worktrees)"
                )
                return 1
            ensure_coord_dirs()
            dev_throttle.below_normal_priority()
            with dev_throttle.build_slot(LOCKS_DIR, f"{JOB or 'main'}:{name}"):
                return func(args)

        return run

    return wrap


def run_logged_steps(
    steps: list[tuple[str, list[str], Path]], log_path: Path
) -> tuple[bool, dict[str, str]]:
    ensure_coord_dirs()
    all_ok = True
    outputs: dict[str, str] = {}
    with log_path.open("w", encoding="utf-8", errors="replace") as logf:
        for label, cmd, cwd in steps:
            logf.write(f"=== {label}: {' '.join(cmd)} (cwd={cwd}) ===\n")
            logf.flush()
            proc = subprocess.run(
                cmd,
                cwd=str(cwd),
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                encoding="utf-8",
                errors="replace",
            )
            output = proc.stdout or ""
            outputs[label] = output
            logf.write(output)
            if output and not output.endswith("\n"):
                logf.write("\n")
            logf.write(f"--- {label} exit={proc.returncode} ---\n")
            logf.flush()
            if proc.returncode != 0:
                all_ok = False
    return all_ok, outputs


def _ps_quote(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def resolve_save_dir(name: str, saves_dir: Path = SAVES_DIR) -> Path:
    if not SAVE_NAME_RE.match(name):
        raise ValueError(f"Invalid --save name '{name}': use letters, digits, '_', '-', '.'")
    path = saves_dir / name
    if not path.is_dir():
        available = sorted(p.name for p in saves_dir.iterdir() if p.is_dir()) if saves_dir.is_dir() else []
        hint = f" (available: {', '.join(available)})" if available else ""
        raise FileNotFoundError(f"Checkpoint save '{name}' not found under {saves_dir}{hint}")
    return path


@contextlib.contextmanager
def isolated_save_data_dir(
    save_name: str | None, tag: str, saves_dir: Path = SAVES_DIR, work_root: Path | None = None
) -> Iterator[Path | None]:
    if not save_name:
        yield None
        return
    src = resolve_save_dir(save_name, saves_dir=saves_dir)
    base = (work_root or DEV_DIR) / "run_data"
    base.mkdir(parents=True, exist_ok=True)
    safe_tag = re.sub(r"[^A-Za-z0-9_.-]+", "_", tag)
    run_dir = base / f"{safe_tag}_{os.getpid()}_{int(time.time() * 1000)}"
    shutil.copytree(src, run_dir)
    try:
        yield run_dir
    finally:
        shutil.rmtree(run_dir, ignore_errors=True)


def parse_title_args(items: list[str]) -> list[str]:
    parsed: list[str] = []
    for item in items:
        parsed.extend(shlex.split(item, posix=True))
    return parsed


def normalize_argv(argv: list[str]) -> list[str]:
    """Allow `--title-args -skipToIIS` without argparse mistaking `-skipToIIS` for an option."""
    out: list[str] = []
    i = 0
    while i < len(argv):
        if argv[i] == "--title-args" and i + 1 < len(argv):
            out.append(f"--title-args={argv[i + 1]}")
            i += 2
        else:
            out.append(argv[i])
            i += 1
    return out


FAST_BOOT_SAVE = "default_checkpoint"


def apply_fast_boot(args: argparse.Namespace, overrides: dict[str, str]) -> dict[str, str]:
    """`--fast-boot` (the default; `--no-fast-boot` turns it off): skip the startup intro (-skipToIIS) and load the frozen `default_checkpoint`
    save unless `--save` names another. The Enter cadence stays at 4 s: 0.5 s taps
    (VICTORY_AUTO_A_PERIOD_MS=500) dropped a run from the world back to a 2-draw screen for 20 s."""
    if not getattr(args, "fast_boot", False):
        return overrides
    args.skip_intro = True
    if not args.save:
        args.save = FAST_BOOT_SAVE
    return overrides


def apply_mute_default(overrides: dict[str, str], unmute: bool) -> tuple[dict[str, str], bool]:
    merged = dict(overrides)
    mute = not unmute
    if mute:
        merged.setdefault("VICTORY_MUTE", "1")
    return merged, mute


def build_launcher_command(
    game_dir: Path,
    diag: bool,
    overrides: dict[str, str],
    mute: bool = False,
    skip_intro: bool = False,
    data_dir: Path | None = None,
    title_args: list[str] | None = None,
    extra: list[str] | None = None,
) -> list[str]:
    parts = [
        "&",
        _ps_quote(str(LAUNCHER_PS1)),
        "-GameDir",
        _ps_quote(str(game_dir)),
        "-AutoA",
    ]
    if diag:
        parts.append("-Diag")
    if mute:
        parts.append("-Mute")
    if skip_intro:
        parts.append("-SkipIntro")
    if data_dir is not None:
        parts.extend(["-DataDir", _ps_quote(str(data_dir))])
    if title_args:
        quoted = ", ".join(_ps_quote(a) for a in title_args)
        parts.extend(["-TitleArgs", f"@({quoted})"])
    if overrides:
        entries = "; ".join(f"{_ps_quote(k)} = {_ps_quote(v)}" for k, v in overrides.items())
        parts.extend(["-Override", f"@{{ {entries} }}"])
    if extra:
        parts.extend(extra)
    return ["powershell", "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", " ".join(parts)]


def kill_process_tree(pid: int) -> None:
    if os.name == "nt":
        subprocess.run(
            ["taskkill", "/F", "/T", "/PID", str(pid)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
    else:
        try:
            os.kill(pid, 15)
        except OSError:
            pass


def run_timed_victory(cmd: list[str], log_path: Path, seconds: int) -> tuple[bool, int]:
    ensure_coord_dirs()
    with log_path.open("wb") as logf:
        proc = subprocess.Popen(
            cmd,
            cwd=str(REPO_ROOT),
            stdout=logf,
            stderr=subprocess.STDOUT,
        )
        launched_at: float | None = None
        started_at = time.monotonic()
        timed_out = False
        try:
            while proc.poll() is None:
                now = time.monotonic()
                if launched_at is None:
                    text = read_log_text(log_path)
                    if "mapped at" in text or "fps," in text:
                        launched_at = now
                    elif now - started_at >= BUILD_GRACE_SECS:
                        timed_out = True
                        break
                elif now - launched_at >= seconds:
                    timed_out = True
                    break
                time.sleep(0.5)
        finally:
            if proc.poll() is None:
                kill_process_tree(proc.pid)
                try:
                    proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    proc.kill()
        return timed_out, proc.returncode if proc.returncode is not None else -1


def parse_pacing_line(line: str) -> dict[str, float] | None:
    """The figures of one `frame pacing:` line (victory-gpu `pacing.rs`), or None."""
    m = PACING_RE.search(line)
    if not m:
        return None
    keys = ("frames", "p50", "p95", "p99", "max", "over50", "over100", "sd", "jitter", "draws")
    return {k: float(v) for k, v in zip(keys, m.groups())}


def settled_world_ts(windows: list[tuple[datetime, float]]) -> datetime | None:
    """End of the first world pacing window after the last one still loading (median frame over
    LOADING_P50_MS), as `summarize_pacing` counts settled; None while the world never settles."""
    loading = [i for i, (_, p50) in enumerate(windows) if p50 > LOADING_P50_MS]
    start = loading[-1] + 1 if loading else 0
    return windows[start][0] if start < len(windows) else None


def summarize_pacing(windows: list[dict[str, float]]) -> dict[str, float]:
    """Settled world windows: median p50/p99/sd/jitter, the worst frame, and the frames over
    50 and 100 ms (hitches) summed. Settled is after the last world window whose median frame
    is over LOADING_P50_MS (the world still streaming in), or the later half of the world
    windows when that leaves fewer than two."""
    world = [w for w in windows if w["draws"] >= WORLD_DRAWS_MIN]
    loading = [i for i, w in enumerate(world) if w["p50"] > LOADING_P50_MS]
    settled = world[loading[-1] + 1 :] if loading else world
    if len(settled) < 2:
        settled = world[len(world) // 2 :] if len(world) >= 4 else world
    if not settled:
        return {
            "p50_ms": 0.0, "p99_ms": 0.0, "max_ms": 0.0, "sd_ms": 0.0, "jitter_ms": 0.0,
            "over50": 0, "hitches": 0,
        }
    return {
        "p50_ms": statistics.median(w["p50"] for w in settled),
        "p99_ms": statistics.median(w["p99"] for w in settled),
        "max_ms": max(w["max"] for w in settled),
        "sd_ms": statistics.median(w["sd"] for w in settled),
        "jitter_ms": statistics.median(w["jitter"] for w in settled),
        "over50": int(sum(w["over50"] for w in settled)),
        "hitches": int(sum(w["over100"] for w in settled)),
    }


def parse_victory_log(text: str) -> dict[str, object]:
    world_rows: list[tuple[float, float, float]] = []
    busy_rows: list[float] = []
    frame_ms: float | None = None
    pacing_windows: list[dict[str, float]] = []
    dstorage_rates: list[float] = []
    refused = 0
    refused_ahead = 0
    device_removed = False
    first_ts: datetime | None = None
    first_world_ts: datetime | None = None
    world_window_ts: list[tuple[datetime, float]] = []
    timed_windows: list[dev_stats.Window] = []
    hitches: list[tuple[datetime, int, float]] = []

    for raw_line in text.splitlines():
        line = ANSI_RE.sub("", raw_line)
        ts_match = TS_RE.match(line)
        line_ts: datetime | None = None
        if ts_match:
            line_ts = datetime.fromisoformat(ts_match.group(1).replace("Z", "+00:00"))
            if first_ts is None:
                first_ts = line_ts
        if ALERT_LEVEL_RE.search(line) and "device removed" in line.lower():
            device_removed = True
        kind = dev_stats.refusal(line)
        if kind == "refused":
            refused += 1
        elif kind == "ahead":
            refused_ahead += 1
        m_frame = FRAME_RE.search(line)
        if m_frame:
            fps = float(m_frame.group(1))
            prep_ms = float(m_frame.group(3))
            gpu_ms = float(m_frame.group(8))
            draws = float(m_frame.group(11))
            if draws >= WORLD_DRAWS_MIN:
                world_rows.append((fps, gpu_ms, prep_ms))
                frame_ms = float(m_frame.group(2))
                if first_world_ts is None and line_ts is not None:
                    first_world_ts = line_ts
            else:
                frame_ms = None
        m_card = CARD_CLOCK_RE.search(line)
        if m_card and frame_ms is not None:
            # GPU busy: the share of the frame the card had work of ours queued.
            busy_rows.append(frame_ms * float(m_card.group(1)) / 100.0)
        hitch = dev_stats.parse_hitch_line(line)
        if hitch and line_ts is not None:
            hitches.append((line_ts, *hitch))
        pacing = parse_pacing_line(line)
        if pacing:
            pacing_windows.append(pacing)
            if line_ts is not None:
                timed_windows.append(dev_stats.Window(
                    line_ts, pacing["p50"], pacing["p99"], pacing["max"], pacing["over100"], pacing["draws"]
                ))
            if pacing["draws"] >= WORLD_DRAWS_MIN and line_ts is not None:
                if first_world_ts is None:
                    first_world_ts = line_ts
                world_window_ts.append((line_ts, pacing["p50"]))
        m_ds = DSTORAGE_RE.search(line)
        if m_ds:
            dstorage_rates.append(float(m_ds.group(1)))

    settled = world_rows[len(world_rows) // 2 :] if len(world_rows) >= 4 else world_rows
    fps_med = statistics.median(r[0] for r in settled) if settled else 0.0
    gpu_med = statistics.median(r[1] for r in settled) if settled else 0.0
    prep_med = statistics.median(r[2] for r in settled) if settled else 0.0
    ds_med = statistics.median(dstorage_rates) if dstorage_rates else 0.0
    busy_settled = busy_rows[len(busy_rows) // 2 :]
    busy_med = statistics.median(busy_settled) if busy_settled else 0.0
    boot_s = (first_world_ts - first_ts).total_seconds() if (first_ts and first_world_ts) else 0.0
    settled_ts = settled_world_ts(world_window_ts)
    settled_s = (settled_ts - first_ts).total_seconds() if (first_ts and settled_ts) else 0.0
    return {
        "world_samples": len(world_rows),
        "boot_s": boot_s,
        "settled_s": settled_s,
        "fps": fps_med,
        "gpu_ms": gpu_med,
        "prep_ms": prep_med,
        "dstorage_mbs": ds_med,
        "gpu_busy_ms": busy_med,
        "refused": refused,
        "refused_ahead": refused_ahead,
        "device_removed": device_removed,
        **summarize_pacing(pacing_windows),
        **dev_stats.world_phase(timed_windows, hitches),
    }


def release_build_cmd(env: dict[str, str] | None = None) -> list[str]:
    """The release build; `--features dlss` when VICTORY_DLSS_SDK names the NVIDIA DLSS SDK
    (kept outside the repository), so VICTORY_UPSCALE=dlss runs DLSS instead of a stretch."""
    env = os.environ if env is None else env
    cmd = ["cargo", "build", "--release", "-p", "victory", "--manifest-path", str(VICTORY_MANIFEST)]
    if env.get("VICTORY_DLSS_SDK", "").strip():
        cmd += ["--features", "dlss"]
    return cmd


@throttled("build")
def cmd_build(args: argparse.Namespace) -> int:
    rel_step = ("cargo build release", release_build_cmd(), REPO_ROOT)
    diag_step = (
        "cargo build diag",
        [
            "cargo", "build", "--profile", "diag", "-p", "victory",
            "--features", "diagnostics", "--manifest-path", str(VICTORY_MANIFEST),
        ],
        REPO_ROOT,
    )
    if args.both:
        profile, steps = "both", [rel_step, diag_step]
    elif args.diag:
        profile, steps = "diag", [diag_step]
    else:
        profile, steps = "release", [rel_step]

    t0 = time.monotonic()
    ok, _ = run_logged_steps(steps, DEV_DIR / "build.log")
    secs = time.monotonic() - t0
    print(f"DEV build {'PASS' if ok else 'FAIL'} profile={profile} secs={secs:.1f}")
    return 0 if ok else 1


def _dev_line(cmd: str, ok: bool, fields: dict[str, str]) -> str:
    return f"DEV {cmd} {'PASS' if ok else 'FAIL'} " + " ".join(f"{k}={v}" for k, v in fields.items())


@throttled("test")
def cmd_test(args: argparse.Namespace) -> int:
    ensure_coord_dirs()
    ok, _build, fields = dev_gate.gate(VICTORY_DIR, REPO_ROOT, COORD_DIR, DEV_DIR / "test.log", args.crate)
    print(_dev_line("test", ok, fields))
    return 0 if ok else 1


@throttled("gate")
def cmd_gate(args: argparse.Namespace) -> int:
    """The merge gate: every workspace test, policy and pytest, and no compiler warning."""
    ensure_coord_dirs()
    runs = max(1, args.runs)
    walls: list[str] = []
    fields: dict[str, str] = {}
    ok = True
    for _ in range(runs):
        ok, build, fields = dev_gate.gate(VICTORY_DIR, REPO_ROOT, COORD_DIR, DEV_DIR / "gate.log", None)
        ok = ok and not build.warnings
        walls.append(fields["wall_s"])
        if not ok:
            break
    fields["runs"] = f"{len(walls)}/{runs}"
    if runs > 1:
        fields["walls_s"] = ",".join(walls)
    print(_dev_line("gate", ok, fields))
    return 0 if ok else 1


@throttled("policy")
def cmd_policy(_args: argparse.Namespace) -> int:
    ok, outputs = run_logged_steps(
        [("cargo xtask policy", ["cargo", "xtask", "policy"], VICTORY_DIR)],
        DEV_DIR / "policy.log",
    )
    match = POLICY_FILES_RE.search(outputs.get("cargo xtask policy", ""))
    files = int(match.group(1)) if match else 0
    print(f"DEV policy {'PASS' if ok else 'FAIL'} files={files}")
    return 0 if ok else 1


BENCH_FIELDS = (
    ("boot_s", ".1f"), ("settled_s", ".1f"), ("fps", ".1f"), ("gpu_ms", ".1f"), ("prep_ms", ".1f"),
    ("gpu_busy_ms", ".1f"), ("dstorage_mbs", ".1f"), ("refused", "d"), ("refused_ahead", "d"),
    ("p50_ms", ".1f"), ("p99_ms", ".1f"),
    ("max_ms", ".1f"), ("sd_ms", ".1f"), ("jitter_ms", ".1f"), ("over50", "d"), ("hitches", "d"),
    ("steady_p99_ms", ".1f"), ("stream_s", ".1f"), ("world_over100", "d"), ("world_over250", "d"),
    ("worst_ms", ".1f"), ("worst_frame", "d"),
)


def bench_fields(stats: dict[str, object]) -> str:
    """The figures of a `DEV bench` line, in their fixed order."""
    return " ".join(
        f"{key}={int(stats[key]) if spec == 'd' else float(stats[key]):{spec}}" for key, spec in BENCH_FIELDS
    )


def apply_snaps(overrides: dict[str, str], every_s: float, snap_dir: Path) -> dict[str, str]:
    """`--snap-every N`: the game writes a PNG every N seconds (victory-gpu `snap.rs`) into
    `snap_dir`, emptied of an earlier run's snaps first."""
    if every_s <= 0:
        return overrides
    snap_dir.mkdir(parents=True, exist_ok=True)
    for old in snap_dir.glob("snap-*.png"):
        old.unlink()
    return {"VICTORY_SNAP_EVERY_S": f"{every_s:g}", "VICTORY_SNAP_DIR": str(snap_dir), **overrides}


def run_bench(
    args: argparse.Namespace, overrides: dict[str, str], log_path: Path, snap_dir: Path, what: str = "bench"
) -> tuple[dict[str, object], int | None, int | None, int]:
    """One timed release run under gpu.lock and `_quiet(args, what)`: (stats, driver resets before
    and after, snaps written).
    Raises FileNotFoundError or ValueError (before anything runs) for a bad game folder or save."""
    game_dir = resolve_game_dir(args.game_dir)
    overrides = apply_fast_boot(args, overrides)
    title_args = parse_title_args(args.title_args)
    if args.save:
        resolve_save_dir(args.save)
    overrides = apply_snaps(overrides, args.snap_every, snap_dir)
    overrides, mute = apply_mute_default(overrides, args.unmute)
    with isolated_save_data_dir(args.save, "bench") as data_dir:
        cmd = build_launcher_command(
            game_dir=game_dir,
            diag=args.diag,
            overrides=overrides,
            mute=mute,
            skip_intro=args.skip_intro,
            data_dir=data_dir,
            title_args=title_args,
        )
        with acquire_gpu_lock(what), _quiet(args, what):
            resets_before = dev_driver.resets_since_boot()
            run_timed_victory(cmd, log_path, args.seconds)
            resets_after = dev_driver.resets_since_boot()
    stats = parse_victory_log(read_log_text(log_path))
    snaps = len(list(snap_dir.glob("snap-*.png"))) if args.snap_every > 0 else 0
    return stats, resets_before, resets_after, snaps


def bench_ok(stats: dict[str, object]) -> bool:
    return int(stats["world_samples"]) > 0 and not bool(stats["device_removed"])


def cmd_bench(args: argparse.Namespace) -> int:
    log_path = DEV_DIR / "bench.log"
    snap_dir = DEV_DIR / "bench_snaps"
    try:
        stats, resets_before, resets_after, snaps = run_bench(args, parse_overrides(args.override), log_path, snap_dir)
    except (FileNotFoundError, ValueError) as exc:
        ensure_coord_dirs()
        log_path.write_text(f"error: {exc}\n", encoding="utf-8")
        print(f"DEV bench FAIL {bench_fields(parse_victory_log(''))}")
        return 1
    ok = bench_ok(stats)
    snapped = f" snaps={snaps} snap_dir={snap_dir}" if args.snap_every > 0 else ""
    print(f"{_dev_line('bench', ok, {})}{bench_fields(stats)} driver_resets={_count(resets_after)}{snapped}")
    _print_driver_warning(resets_before, resets_after)
    _print_hang_lines(log_path)
    return 0 if ok else 1


def _count(value: int | None) -> str:
    return "?" if value is None else str(value)


@contextlib.contextmanager
def _quiet(args: argparse.Namespace, what: str) -> Iterator[None]:
    """`--quiet`: hold every build slot for the run so no build competes with the game.
    Otherwise: hold the slots free right now without waiting (writers keep at least one), and run
    the builds already going at idle priority, until the run ends."""
    quiet = getattr(args, "quiet", False)
    tag = f"{JOB or 'main'}:{'quiet' if quiet else 'run'}-{what}"
    if quiet:
        # Take the free slots at once (none left to writers) and suspend running builds and worktree
        # tools for the run: no wait for the machine's builds to finish, none competing with the game.
        with dev_throttle.free_build_slots(LOCKS_DIR, tag, leave=0) as held, \
                dev_throttle.builds_suspended() as paused:
            print(f"DEV {what}: quiet: holding {len(held)} of {dev_throttle.build_slots()} build slots, "
                  f"{paused} build/tool processes suspended for the run", flush=True)
            yield
        return
    with dev_throttle.free_build_slots(LOCKS_DIR, tag) as held, dev_throttle.builds_idle() as idled, \
            dev_throttle.builds_suspended(builds=False) as tools:
        print(
            f"DEV {what}: holding {len(held)} of {dev_throttle.build_slots()} build slots for the run; "
            f"{idled} build processes at idle priority, {tools} worktree tests/tools suspended",
            flush=True,
        )
        yield


def _print_driver_warning(before: int | None, after: int | None) -> None:
    line = dev_driver.warning(before, after)
    if line:
        print(line)


def _print_hang_lines(log_path: Path) -> None:
    """The run's device-removed report and hang watch lines (`dev_greps.hang_lines`), if any."""
    for line in dev_greps.hang_lines(read_log_text(log_path)):
        print(f"DEV hang: {line}")


def cmd_capture(args: argparse.Namespace) -> int:
    tag = args.tag or datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    safe_tag = re.sub(r"[^A-Za-z0-9_.-]+", "_", tag)
    cap_dir = DEV_DIR / f"cap_{safe_tag}"
    log_path = DEV_DIR / f"capture_{safe_tag}.log"
    cap_dir.mkdir(parents=True, exist_ok=True)
    try:
        game_dir = resolve_game_dir(args.game_dir)
        user_overrides = apply_fast_boot(args, parse_overrides(args.override))
        title_args = parse_title_args(args.title_args)
        if args.save:
            resolve_save_dir(args.save)
    except (FileNotFoundError, ValueError) as exc:
        log_path.write_text(f"error: {exc}\n", encoding="utf-8")
        print(f"DEV capture FAIL boot_s=0.0 settled_s=0.0 frames=0 refused=0 dir={cap_dir}")
        return 1

    overrides = {
        "VICTORY_CAPTURE": str(cap_dir),
        "VICTORY_CAPTURE_WORLD": "1",
    }
    overrides.update(user_overrides)
    overrides, mute = apply_mute_default(overrides, args.unmute)
    with isolated_save_data_dir(args.save, f"capture_{safe_tag}") as data_dir:
        cmd = build_launcher_command(
            game_dir=game_dir,
            diag=True,
            overrides=overrides,
            mute=mute,
            skip_intro=args.skip_intro,
            data_dir=data_dir,
            title_args=title_args,
        )
        with acquire_gpu_lock(f"capture:{safe_tag}"), _quiet(args, "capture"):
            resets_before = dev_driver.resets_since_boot()
            run_timed_victory(cmd, log_path, args.seconds)
            resets_after = dev_driver.resets_since_boot()

    stats = parse_victory_log(read_log_text(log_path))
    frames = len(list(cap_dir.glob("*.png")))
    ok = frames > 0 and not bool(stats["device_removed"])
    status = "PASS" if ok else "FAIL"
    print(
        f"DEV capture {status} boot_s={float(stats['boot_s']):.1f} "
        f"settled_s={float(stats['settled_s']):.1f} frames={frames} refused={int(stats['refused'])} refused_ahead={int(stats['refused_ahead'])} "
        f"driver_resets={_count(resets_after)} dir={cap_dir}"
    )
    _print_driver_warning(resets_before, resets_after)
    _print_hang_lines(log_path)
    return 0 if ok else 1


def train_compare(plan: dev_train.Plan, done: dict[int, dict[str, object]], train_dir: Path, a: str, b: str) -> int:
    """Side-by-side PNGs of two runs' snaps: each of run A's (left) with run B's nearest in time."""
    try:
        sa, sb = dev_train.find_run(plan, done, a), dev_train.find_run(plan, done, b)
    except dev_train.PlanError as exc:
        print(f"error: {exc}")
        print("DEV train-compare FAIL reason=run")
        return 1
    ida, idb = (dev_train.run_id(s, str(done[s]["set"])) for s in (sa, sb))
    pairs = dev_train.pair_snaps(
        dev_train.snaps(train_dir / f"{ida}_snaps"), dev_train.snaps(train_dir / f"{idb}_snaps")
    )
    out_dir = train_dir / f"compare_{ida}_vs_{idb}"
    out_dir.mkdir(parents=True, exist_ok=True)
    for old in out_dir.glob("cmp-*.png"):
        old.unlink()
    for t, left, u, right in pairs:
        image = dev_png.side_by_side(dev_png.read_png(left), dev_png.read_png(right))
        dev_png.write_png(out_dir / f"cmp-{t:04d}s-{u:04d}s.png", image)
    ok = bool(pairs)
    print(f"DEV train-compare {'PASS' if ok else 'FAIL'} left={ida} right={idb} pairs={len(pairs)} dir={out_dir}")
    return 0 if ok else 1


def train_greps_from(plan_path: Path, plan: dev_train.Plan, sources: list[str]) -> int:
    """Adds the log-line phrases of writers' ready.md batch items to the plan's [greps]."""
    found = []
    if sources == ["all"]:
        sources = [str(p) for p in sorted(COORD_DIR.glob("*/ready.md"))]
    for source in sources:
        path = Path(source)
        if not path.is_file():
            print(f"error: no file {path}")
            print("DEV train-greps FAIL reason=source")
            return 1
        found += dev_greps.extract(read_log_text(path), path.parent.name or path.stem)
    added, skipped = dev_greps.merge_found({n: p.pattern for n, p in plan.greps.items()}, found)
    dev_greps.add_to_plan(plan_path, added)
    for f in added:
        print(f"{f.name} = {f.regex}  # {f.note}")
    dev_train.load_plan(plan_path)
    print(f"DEV train-greps PASS added={len(added)} skipped={skipped} plan={plan_path}")
    return 0


def train_visual(plan: dev_train.Plan, done: dict[int, dict[str, object]], train_dir: Path, which: list[str]) -> int:
    """Visual hints: ON snaps against OFF snaps, above the OFF-vs-OFF noise floor."""
    if len(which) not in (0, 2):
        print("error: --visual takes no runs (every ON run against the baseline) or two (A B)")
        print("DEV train-visual FAIL reason=args")
        return 1
    try:
        lines, summary = dev_visual.visual(plan, done, train_dir, which)
    except dev_train.PlanError as exc:
        print(f"error: {exc}")
        print("DEV train-visual FAIL reason=run")
        return 1
    print("\n".join(lines))
    ok = summary["pairs"] > 0
    print(
        f"DEV train-visual {'PASS' if ok else 'FAIL'} pairs={summary['pairs']} flagged={summary['flagged']} "
        f"off_off_pairs={summary['floor']['off_off_pairs']} dir={train_dir / 'visual'} (hint, not a verdict)"
    )
    return 0 if ok else 1


def train_region(done: dict[int, dict[str, object]], train_dir: Path, spec: list[str], times: list[float] | None) -> int:
    """Mean colour of a reference PNG's rectangle in every run's snap nearest each time."""
    try:
        if not times:
            raise ValueError("--region needs --at T [T ...] (seconds since the run's first present)")
        box = tuple(int(v) for v in spec[1:])
        lines, measured = dev_visual.region(done, train_dir, Path(spec[0]), box, times)
    except (OSError, ValueError) as exc:
        print(f"error: {exc}")
        print("DEV train-region FAIL reason=args")
        return 1
    print("\n".join(lines))
    ok = measured > 0
    print(f"DEV train-region {'PASS' if ok else 'FAIL'} measured={measured} dir={train_dir / 'visual'} (hint, not a verdict)")
    return 0 if ok else 1


def train_scans(plan: dev_train.Plan, done: dict[int, dict[str, object]], train_dir: Path) -> dict[int, dev_greps.LogScan]:
    return {
        step: dev_greps.scan_log(read_log_text(log), plan.greps)
        for step, rec in done.items()
        if (log := train_dir / f"{dev_train.run_id(step, str(rec['set']))}.log").exists()
    }


def train_lives(plan: dev_train.Plan, done: dict[int, dict[str, object]], train_dir: Path) -> dict[int, dict[str, object]]:
    """Every live run's segments and per-set figures, read again from its log."""
    out = {}
    for step, rec in done.items():
        arm = plan.live.get(str(rec["set"]))
        log = train_dir / f"{dev_train.run_id(step, str(rec['set']))}.log"
        if arm is not None and log.exists():
            out[step] = dev_live.analyse(arm, read_log_text(log))
    return out


def train_verdict(plan: dev_train.Plan, done: dict[int, dict[str, object]], train_dir: Path) -> int:
    scans = train_scans(plan, done, train_dir)
    base = dev_greps.baseline_warns([scans[s] for s in sorted(scans) if done[s]["set"] == plan.baseline])
    flagged: dict[str, list[str]] = {}
    for s, scan in sorted(scans.items()):
        found = dev_greps.flags(scan, base, done[s]["set"] == plan.baseline)
        if found:
            rid = dev_train.run_id(s, str(done[s]["set"]))
            flagged.setdefault(str(done[s]["set"]), []).append(f"{rid}: " + "; ".join(f.split(':')[0] for f in found))
    rows, floor, sources = dev_verdict.set_verdicts(plan.sets, plan.baseline, done, flagged)
    rows += [dev_verdict.live_verdict(r) for _, r in sorted(train_lives(plan, done, train_dir).items())]
    text = "\n".join(dev_verdict.verdict_lines(rows, floor, sources, plan.name, plan.baseline)) + "\n"
    train_dir.mkdir(parents=True, exist_ok=True)
    (train_dir / "verdict.md").write_text(text, encoding="utf-8")
    (train_dir / "verdict.json").write_text(json.dumps({"floor": floor, "rows": rows}, indent=2), encoding="utf-8")
    print(text, end="")
    counts = {v: sum(1 for r in rows if r["verdict"] == v) for v in ("KEEP", "DROP", "INCONCLUSIVE")}
    print(f"DEV train-verdict PASS rows={len(rows)} keep={counts['KEEP']} drop={counts['DROP']} "
          f"inconclusive={counts['INCONCLUSIVE']} floors={'repeats' if sources else 'defaults'} "
          f"verdict={train_dir / 'verdict.md'} (suggestions)")
    return 0


def run_prewarm(
    game_dir: Path, overrides: dict[str, str], mute: bool, pipelines: bool, jobs: int, log_path: Path
) -> tuple[bool, dict[str, int]]:
    """One prewarm in this process (no build slot: it runs inside a train call's gpu lock window)."""
    cmd = prewarm_command(game_dir, overrides, mute, False, pipelines, jobs)
    with acquire_gpu_lock("prewarm") if pipelines else contextlib.nullcontext():
        ok, outputs = run_logged_steps([("prewarm", cmd, REPO_ROOT)], log_path)
    text = outputs.get("prewarm", "")
    counts = dev_prewarm.parse_summary(text)
    ok = ok and counts["shaders"] > 0 and (counts["recipes"] > 0 if pipelines else not dev_prewarm.device_made(text))
    return ok, counts


def train_warm(plan: dev_train.Plan, name: str, game_dir_arg: str | None, train_dir: Path) -> tuple[bool, str]:
    """Makes a step's shader variant warm as the plan's `prewarm` says, unless a stamp says it is.
    Raises FileNotFoundError for a missing game folder."""
    if plan.prewarm == "none":
        return True, "off"
    overrides, mute = apply_mute_default(dev_train.step_overrides(plan, name), plan.unmute)
    pipelines = plan.prewarm == "pipelines"
    if is_warm(overrides, pipelines):
        return True, "warm"
    game_dir = resolve_game_dir(game_dir_arg)
    print(f"DEV train: prewarm ({plan.prewarm}) of {name}", flush=True)
    ok, counts = run_prewarm(game_dir, overrides, mute, pipelines, 0, train_dir / f"prewarm_{name}.log")
    if not ok:
        return False, "failed"
    dev_warm.write_stamp(warm_dir(), warm_signature(overrides), pipelines, counts, overrides, "train")
    return True, f"made:{counts['translated']}"


AHEAD_TIMEOUT_S = 1800.0


def start_ahead(plan: dev_train.Plan, name: str, game_dir: Path, train_dir: Path) -> tuple | None:
    """Translates `name`'s shader variant (translation only: no device, no gpu lock, no build slot) at idle priority
    while this arm runs, into the cache that arm will use. None when it is already stamped warm."""
    overrides, mute = apply_mute_default(dev_train.step_overrides(plan, name), plan.unmute)
    if is_warm(overrides, False):
        return None
    cmd = prewarm_command(game_dir, overrides, mute, False, False, plan.ahead_jobs)
    log = train_dir / f"ahead_{name}.log"
    flags = subprocess.IDLE_PRIORITY_CLASS | subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0
    with log.open("wb") as out:
        proc = subprocess.Popen(cmd, cwd=str(REPO_ROOT), stdout=out, stderr=subprocess.STDOUT, creationflags=flags)
    print(f"DEV train: translating {name}'s shaders ahead at idle priority ({plan.ahead_jobs} jobs) log={log}", flush=True)
    return name, proc, log, overrides, time.monotonic()


def finish_ahead(ahead: tuple, kill: bool = False) -> dict[str, object]:
    """Waits for the prewarm ahead (it must end inside this call: Drift ends what a command started) and stamps it."""
    name, proc, log, overrides, t0 = ahead
    if kill:
        kill_process_tree(proc.pid)
    elif proc.poll() is None:
        print(f"DEV train: waiting for the prewarm ahead of {name}", flush=True)
    try:
        proc.wait(timeout=AHEAD_TIMEOUT_S)
    except subprocess.TimeoutExpired:
        kill_process_tree(proc.pid)
        proc.wait(timeout=10)
    text = read_log_text(log)
    counts = dev_prewarm.parse_summary(text)
    ok = proc.returncode == 0 and counts["shaders"] > 0 and not dev_prewarm.device_made(text)
    if ok:
        dev_warm.write_stamp(warm_dir(), warm_signature(overrides), False, counts, overrides, "ahead")
    return {"set": name, "ok": ok, "translated": counts["translated"], "hits": counts["hits"],
            "secs": round(time.monotonic() - t0, 1), "log": str(log)}


def train_warm_only(plan: dev_train.Plan, done: dict[int, dict[str, object]], train_dir: Path, args: argparse.Namespace) -> int:
    """`--warm [next|N|SET]`: the prewarm check `--run` makes, alone."""
    try:
        step, name = dev_train.pick(plan, done, args.warm)
        train_dir.mkdir(parents=True, exist_ok=True)
        ok, status = train_warm(plan, name, args.game_dir, train_dir)
    except (dev_train.PlanError, FileNotFoundError) as exc:
        print(f"error: {exc}")
        print("DEV train-warm FAIL reason=run")
        return 1
    print(f"DEV train-warm {'PASS' if ok else 'FAIL'} run={dev_train.run_id(step, name)} prewarm={status}")
    return 0 if ok else 1


def _live_line(rid: str, result: dict[str, object]) -> str:
    sets, delta = result["sets"], result["delta"]
    parts = [f"{label}={result['a' if label == 'A' else 'b']} fps={sets[label]['fps']:.1f} prep={sets[label]['prep_ms']:.1f}"
             for label in ("A", "B") if label in sets]
    if delta:
        parts.append(f"delta_fps={delta['fps']:+.1f} delta_p99={delta['p99_ms']:+.1f}")
    parts.append(f"noisy={','.join(result['noisy']) or '-'} problems={len(result['problems'])}")
    return f"DEV train-live run={rid} " + " ".join(parts)


def cmd_train(args: argparse.Namespace) -> int:
    """One run of a batch plan (`--run`, with its prewarm), or the report across its runs (`--report`, `--list`,
    `--verdict`)."""
    try:
        plan = dev_train.load_plan(Path(args.plan), dev_warm.apply_kinds(REPO_ROOT))
    except (OSError, dev_train.PlanError) as exc:
        print(f"error: {exc}")
        print("DEV train FAIL reason=plan")
        return 1
    train_dir = DEV_DIR / f"train_{plan.name}"
    done = dev_train.load_results(train_dir)
    if args.list:
        print(dev_train.listing(plan, done), end="")
        print(f"DEV train-list PASS runs={len(done)} next={dev_train.next_pending(plan, done)} dir={train_dir}")
        return 0
    if args.compare:
        return train_compare(plan, done, train_dir, *args.compare)
    if args.greps_from:
        return train_greps_from(Path(args.plan), plan, args.greps_from)
    if args.visual is not None:
        return train_visual(plan, done, train_dir, args.visual)
    if args.region:
        return train_region(done, train_dir, args.region, args.at)
    if args.verdict:
        return train_verdict(plan, done, train_dir)
    if args.warm:
        return train_warm_only(plan, done, train_dir, args)
    if args.report:
        text, figures = dev_train.report(plan, done, train_scans(plan, done, train_dir), train_lives(plan, done, train_dir))
        visual = dev_visual.report_section(train_dir)
        if visual:
            text += "\n".join(visual) + "\n"
        train_dir.mkdir(parents=True, exist_ok=True)
        (train_dir / "report.md").write_text(text, encoding="utf-8")
        (train_dir / "report.json").write_text(json.dumps(figures, indent=2), encoding="utf-8")
        print(text, end="")
        print(f"DEV train-report PASS runs={len(done)} next={dev_train.next_pending(plan, done)} "
              f"report={train_dir / 'report.md'}")
        return 0
    try:
        step, name = dev_train.pick(plan, done, args.run)
    except dev_train.PlanError as exc:
        print(f"error: {exc}")
        print("DEV train FAIL reason=run")
        return 1
    rid = dev_train.run_id(step, name)
    train_dir.mkdir(parents=True, exist_ok=True)
    log_path = train_dir / f"{rid}.log"
    overrides = dev_train.step_overrides(plan, name)
    arm = plan.live.get(name)
    if arm is not None:
        schedule = train_dir / f"{rid}.schedule.txt"
        schedule.write_text(dev_live.schedule_text(arm), encoding="utf-8")
        overrides["VICTORY_SETTINGS_SCHEDULE"] = str(schedule)
    quiet = args.quiet if args.quiet is not None else dev_train.step_quiet(plan, name)
    bench_args = argparse.Namespace(
        game_dir=args.game_dir, fast_boot=plan.fast_boot, save=plan.save, skip_intro=False, title_args=[],
        unmute=plan.unmute, diag=False, seconds=dev_train.step_seconds(plan, name), quiet=quiet,
        snap_every=plan.snap_every,
    )
    ahead = None
    try:
        warm_ok, warm = train_warm(plan, name, args.game_dir, train_dir)
        if not warm_ok:
            print(f"DEV train FAIL run={rid} reason=prewarm log={train_dir / f'prewarm_{name}.log'}")
            return 1
        nxt = dev_train.next_after(plan, done, step)
        # Not during a quiet run: those are the final before/after numbers and must have the machine to themselves.
        if plan.prewarm_ahead and plan.prewarm != "none" and not quiet and nxt is not None:
            ahead = start_ahead(plan, nxt[1], resolve_game_dir(args.game_dir), train_dir)
        stats, before, after, snaps = run_bench(bench_args, overrides, log_path, train_dir / f"{rid}_snaps", what="train")
    except (FileNotFoundError, ValueError) as exc:
        if ahead is not None:
            finish_ahead(ahead, kill=True)
        print(f"error: {exc}")
        print(f"DEV train FAIL run={rid} reason=setup")
        return 1
    ahead_info = finish_ahead(ahead) if ahead is not None else None
    ok = bench_ok(stats)
    done[step] = {
        "step": step, "set": name, "kind": "live" if arm is not None else "set", "overrides": overrides, "ok": ok,
        "stats": stats, "resets_before": before, "resets_after": after, "snaps": snaps, "log": str(log_path),
        "quiet": quiet, "prewarm": warm, "ahead": ahead_info,
        "finished": datetime.now(timezone.utc).isoformat(timespec="seconds"),
    }
    if arm is not None:
        done[step]["live"] = dev_live.analyse(arm, read_log_text(log_path))
    dev_train.save_result(train_dir, step, done[step])
    nxt_id = dev_train.next_pending(plan, done)
    nxt_after = dev_train.next_after(plan, done, 0)
    ttl = f" next_ttl_min={dev_train.ttl_minutes(plan, nxt_after[1])}" if nxt_after else ""
    ahead_field = f"{ahead_info['set']}:{ahead_info['translated'] if ahead_info['ok'] else 'failed'}" if ahead_info else "none"
    print(
        f"{_dev_line('train', ok, {'run': rid})} {bench_fields(stats)} driver_resets={_count(after)} "
        f"snaps={snaps} next={nxt_id}{ttl} prewarm={warm} ahead={ahead_field} log={log_path}"
    )
    if arm is not None:
        print(_live_line(rid, done[step]["live"]))
    _print_driver_warning(before, after)
    _print_hang_lines(log_path)
    return 0 if ok else 1


def warm_dir() -> Path:
    return DEV_DIR / "warm"


def warm_signature(overrides: dict[str, str], diag: bool = False) -> str:
    """The stamp signature of a prewarm with these launch overrides (mute applied), the current victory.exe and
    launcher."""
    exe = dev_warm.exe_identity(dev_warm.victory_exe(REPO_ROOT, diag), REPO_ROOT / "tools" / "lancer_victory.ps1")
    return dev_warm.signature(overrides, exe, diag)


def is_warm(overrides: dict[str, str], pipelines: bool, diag: bool = False) -> bool:
    return dev_warm.is_warm(warm_dir(), warm_signature(overrides, diag), pipelines, dev_warm.shader_cache_dir(overrides))


def prewarm_command(
    game_dir: Path, overrides: dict[str, str], mute: bool, diag: bool, pipelines: bool, jobs: int,
    host_wave: int = dev_prewarm.DEFAULT_HOST_WAVE, view_limit: int = dev_prewarm.DEFAULT_VIEW_LIMIT,
) -> list[str]:
    # As `bench` (release) or `capture` (--diag) launch it, so the key is theirs: the same launcher set, the arm's
    # overrides, mute and skip-intro (not in the key, but kept identical).
    return build_launcher_command(
        game_dir=game_dir,
        diag=diag,
        overrides=dev_prewarm.prewarm_overrides(overrides, pipelines, host_wave, view_limit),
        mute=mute,
        skip_intro=True,
        extra=dev_prewarm.launcher_extra(pipelines, jobs),
    )


def cmd_prewarm(args: argparse.Namespace) -> int:
    """A no-op when a stamp says this variant is warm (`dev_warm`; `--force` runs it anyway), else `prewarm_work`."""
    if not args.force:
        try:
            overrides, _ = apply_mute_default(parse_overrides(args.override), args.unmute)
        except ValueError:
            return prewarm_work(args)
        sig = warm_signature(overrides, args.diag)
        if is_warm(overrides, args.pipelines, args.diag):
            profile = "diag" if args.diag else "release"
            print(_dev_line("prewarm", True, {
                "profile": profile, "skipped": "warm", "stamp": str(dev_warm.stamp_path(warm_dir(), sig)),
            }))
            return 0
    return prewarm_work(args)


@throttled("prewarm")
def prewarm_work(args: argparse.Namespace) -> int:
    """Translate every shader the title ships into the shader cache for these switch values, without the title
    (`dev_prewarm`). Holds a build slot (it is a build's worth of CPU); `--pipelines` also takes the gpu lock."""
    profile = "diag" if args.diag else "release"
    log_path = DEV_DIR / f"prewarm_{profile}.log"
    try:
        game_dir = resolve_game_dir(args.game_dir)
        missing = [str(p) for p in dev_prewarm.needed_files(game_dir) if not p.is_file()]
        if missing:
            raise FileNotFoundError(f"prewarm needs {', '.join(missing)}")
        overrides = parse_overrides(args.override)
    except (FileNotFoundError, ValueError) as exc:
        log_path.write_text(f"error: {exc}\n", encoding="utf-8")
        print(f"DEV prewarm FAIL profile={profile} secs=0.0 shaders=0 hits=0 translated=0 refused=0 log={log_path}")
        return 1
    overrides, mute = apply_mute_default(overrides, args.unmute)
    cmd = prewarm_command(game_dir, overrides, mute, args.diag, args.pipelines, args.jobs, args.host_wave, args.view_limit)
    gpu = acquire_gpu_lock("prewarm") if args.pipelines else contextlib.nullcontext()
    t0 = time.monotonic()
    with gpu:
        ok, outputs = run_logged_steps([("prewarm", cmd, REPO_ROOT)], log_path)
    secs = time.monotonic() - t0
    text = outputs.get("prewarm", "")
    counts = dev_prewarm.parse_summary(text)
    device = dev_prewarm.device_made(text)
    ok = ok and counts["shaders"] > 0 and (counts["recipes"] > 0 if args.pipelines else not device)
    if ok:
        dev_warm.write_stamp(warm_dir(), warm_signature(overrides, args.diag), args.pipelines, counts, overrides, "prewarm")
    fields = {
        "profile": profile,
        "secs": f"{secs:.1f}",
        "warm_s": str(counts["warm_s"]),
        "shaders": str(counts["shaders"]),
        "hits": str(counts["hits"]),
        "translated": str(counts["translated"]),
        "refused": str(counts["refused"]),
        "device": "1" if device else "0",
        "log": str(log_path),
    }
    if args.pipelines:
        for key in ("recipes", "compiled", "from_library", "recipes_refused", "left_out", "unreadable", "other_key",
                    "recipes_s"):
            fields[key] = str(counts[key])
        fields["log"] = fields.pop("log")
    print(_dev_line("prewarm", ok, fields))
    return 0 if ok else 1


def cmd_scout(args: argparse.Namespace) -> int:
    cmd = [sys.executable, str(SCOUT_PY), *args.scout_args]
    ok, _ = run_logged_steps([("scout_ecosystem", cmd, REPO_ROOT)], DEV_DIR / "scout.log")
    status = "PASS" if ok else "FAIL"
    print(f"DEV scout {status}")
    return 0 if ok else 1


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="dev.py",
        description="El1xr / VIctory developer CLI with single-line DEV summaries.",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p_build = sub.add_parser("build", help="Build victory binary (release, diag, or both).")
    grp = p_build.add_mutually_exclusive_group()
    grp.add_argument("--release", action="store_true", help="Build release profile (default).")
    grp.add_argument("--diag", action="store_true", help="Build diag profile with diagnostics.")
    grp.add_argument("--both", action="store_true", help="Build both release and diag profiles.")
    p_build.set_defaults(func=cmd_build)

    p_test = sub.add_parser(
        "test", help="Build, then run every workspace test binary, cargo xtask policy and pytest at once."
    )
    p_test.add_argument("--crate", help="Limit cargo tests to a single workspace crate (-p).")
    p_test.set_defaults(func=cmd_test)

    p_gate = sub.add_parser("gate", help="Merge gate: `test` over the whole workspace, failing on any warning.")
    p_gate.add_argument("--runs", type=int, default=1, help="Run it this many times in a row (default: 1).")
    p_gate.set_defaults(func=cmd_gate)

    p_policy = sub.add_parser("policy", help="Run cargo xtask policy.")
    p_policy.set_defaults(func=cmd_policy)

    p_bench = sub.add_parser("bench", help="Run timed release benchmark under gpu.lock.")
    p_bench.add_argument("--seconds", type=int, default=75, help="Run duration in seconds (default: 75).")
    p_bench.add_argument("--override", action="append", default=[], metavar="K=V", help="VICTORY_* override.")
    p_bench.add_argument("--game-dir", help="Game folder containing game_scarlett_master.exe.")
    p_bench.add_argument("--diag", action="store_true", help="Run the diag build (no PNG capture).")
    p_bench.add_argument("--save", help="Named checkpoint under AnyXboxX-coord/saves/<name> (isolated copy per run).")
    p_bench.add_argument(
        "--title-args",
        action="append",
        default=[],
        metavar="ARGS",
        help="Title arguments passed after `--` (repeatable, e.g. --title-args=-skipToIIS).",
    )
    p_bench.add_argument("--skip-intro", action="store_true", help="Skip startup intro movie (VICTORY_SKIP_INTRO=1 / -SkipIntro).")
    p_bench.add_argument(
        "--fast-boot",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Fast boot (default on): --skip-intro and --save default_checkpoint unless --save is given. "
        "--no-fast-boot boots through the intro on the live save.",
    )
    p_bench.add_argument("--unmute", action="store_true", help="Do not mute game audio (default sets VICTORY_MUTE=1 and -Mute).")
    p_bench.add_argument(
        "--quiet", action="store_true",
        help="Hold every build slot during the run (waits for running builds) for numbers not skewed by builds.",
    )
    p_bench.add_argument(
        "--snap-every", type=float, default=0.0, metavar="N",
        help="Save a PNG of the presented frame every N seconds (VICTORY_SNAP_EVERY_S) into dev/bench_snaps/.",
    )
    p_bench.set_defaults(func=cmd_bench)

    p_cap = sub.add_parser("capture", help="Run timed diag PNG capture under gpu.lock.")
    p_cap.add_argument("--seconds", type=int, default=90, help="Run duration in seconds (default: 90).")
    p_cap.add_argument("--tag", help="Capture folder tag (writes to AnyXboxX-coord/dev/cap_<tag>/).")
    p_cap.add_argument("--override", action="append", default=[], metavar="K=V", help="VICTORY_* override.")
    p_cap.add_argument("--game-dir", help="Game folder containing game_scarlett_master.exe.")
    p_cap.add_argument("--save", help="Named checkpoint under AnyXboxX-coord/saves/<name> (isolated copy per run).")
    p_cap.add_argument(
        "--title-args",
        action="append",
        default=[],
        metavar="ARGS",
        help="Title arguments passed after `--` (repeatable, e.g. --title-args=-skipToIIS).",
    )
    p_cap.add_argument("--skip-intro", action="store_true", help="Skip startup intro movie (VICTORY_SKIP_INTRO=1 / -SkipIntro).")
    p_cap.add_argument(
        "--fast-boot",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Fast boot (default on): --skip-intro and --save default_checkpoint unless --save is given. "
        "--no-fast-boot boots through the intro on the live save.",
    )
    p_cap.add_argument("--unmute", action="store_true", help="Do not mute game audio (default sets VICTORY_MUTE=1 and -Mute).")
    p_cap.set_defaults(func=cmd_capture)

    p_train = sub.add_parser(
        "train", help="Batch plan: one bench run per call (--run), or the report across its runs (--report)."
    )
    p_train.add_argument("--plan", required=True, help="Plan file (.toml or .json); see tools/dev_train.py.")
    mode = p_train.add_mutually_exclusive_group()
    mode.add_argument(
        "--run", default="next", metavar="next|N|SET",
        help="Run one step: next (default) pending step, step N again, or SET (its next pending step, else an extra run).",
    )
    mode.add_argument("--report", action="store_true", help="Table of every run, medians by set and deltas, flags and [greps] per run.")
    mode.add_argument("--list", action="store_true", help="The steps, done or pending, with their overrides.")
    mode.add_argument(
        "--compare", nargs=2, metavar=("A", "B"),
        help="Side-by-side PNGs: each snap of run A (left) with run B's nearest in time (A, B: step, run id or set).",
    )
    mode.add_argument(
        "--greps-from", nargs="+", metavar="READY_MD",
        help="Add the backticked log lines of these ready.md files' batch items to the plan's [greps] "
        "(`all`: every AnyXboxX-coord/*/ready.md).",
    )
    mode.add_argument(
        "--visual", nargs="*", metavar="RUN",
        help="Visual hints: ON snaps vs OFF snaps above the OFF-vs-OFF noise floor (no runs: every ON run; or A B).",
    )
    mode.add_argument(
        "--region", nargs=5, metavar=("REF", "X0", "Y0", "X1", "Y1"),
        help="Mean colour of this rectangle of REF.png in every run's snap nearest each --at time.",
    )
    mode.add_argument(
        "--verdict", action="store_true",
        help="KEEP/DROP/INCONCLUSIVE per set (and live arm) against the baseline, with noise floors from same-set repeats.",
    )
    mode.add_argument(
        "--warm", nargs="?", const="next", metavar="next|N|SET",
        help="Prewarm check: make the step's shader variant warm as `--run` would before its run (no-op when stamped).",
    )
    p_train.add_argument(
        "--quiet", action=argparse.BooleanOptionalAction, default=None,
        help="This run only: hold every build slot (--quiet) or not (--no-quiet), whatever the plan says.",
    )
    p_train.add_argument("--at", nargs="+", type=float, metavar="T", help="Seconds for --region.")
    p_train.add_argument("--game-dir", help="Game folder containing game_scarlett_master.exe.")
    p_train.set_defaults(func=cmd_train)

    p_pre = sub.add_parser(
        "prewarm",
        help="Translate every shader the title ships into the shader cache for these switch values (no game run).",
    )
    p_pre.add_argument("--override", action="append", default=[], metavar="K=V", help="VICTORY_* override (the arm's).")
    p_pre.add_argument("--game-dir", help="Game folder containing game_scarlett_master.exe.")
    p_pre.add_argument("--diag", action="store_true", help="Warm the diag build's cache (what `capture` runs).")
    p_pre.add_argument("--unmute", action="store_true", help="As bench/capture --unmute (not in the key).")
    p_pre.add_argument("--jobs", type=int, default=0, help="Worker threads (default: every logical core).")
    p_pre.add_argument("--force", action="store_true", help="Run it even when a stamp says this variant is warm.")
    p_pre.add_argument(
        "--pipelines", action="store_true",
        help="Then make the host pipelines of the recipes runs wrote down (VICTORY_PIPELINE_RECIPES=1) into the "
        "pipeline library (makes a device on the card: hold the Drift gpu lock as for bench).",
    )
    p_pre.add_argument(
        "--host-wave", type=int, default=dev_prewarm.DEFAULT_HOST_WAVE,
        help="The host's most wave lanes, as a run's log states it (default: 32, this RTX 5090).",
    )
    p_pre.add_argument(
        "--view-limit", type=int, default=dev_prewarm.DEFAULT_VIEW_LIMIT,
        help="The host view heap's null slot: its views minus one (default: 999999, this RTX 5090).",
    )
    p_pre.set_defaults(func=cmd_prewarm)

    p_scout = sub.add_parser("scout", help="Run tools/scout_ecosystem.py.")
    p_scout.add_argument("scout_args", nargs=argparse.REMAINDER, help="Extra arguments forwarded to scout.")
    p_scout.set_defaults(func=cmd_scout)

    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    raw = sys.argv[1:] if argv is None else list(argv)
    args = parser.parse_args(normalize_argv(raw))
    return int(args.func(args))


if __name__ == "__main__":
    sys.exit(main())
