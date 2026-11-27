#!/usr/bin/env python3
"""Developer CLI for El1xr / VIctory builds, tests, policy checks, benchmarks, and captures.

Full command output is saved under E:/Gta6/AnyXboxX-coord/dev/ and every command prints a single
concise final summary line: `DEV <cmd> PASS|FAIL key=value ...`.
"""

from __future__ import annotations

import argparse
import contextlib
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
import dev_throttle

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
PACING_RE = re.compile(
    r"frame pacing: (\d+) frames in [0-9.]+ s \([0-9.]+ fps\), frame time p50 ([0-9.]+) ms "
    r"p95 ([0-9.]+) ms p99 ([0-9.]+) ms max ([0-9.]+) ms, (\d+) over 50 ms, (\d+) over 100 ms, "
    r"present interval sd ([0-9.]+) ms jitter ([0-9.]+) ms; ([0-9.]+) draws/frame"
)
DSTORAGE_RE = re.compile(r"dstorage: last [0-9.]+ s.*?mb_per_s=([0-9.]+)")
POLICY_FILES_RE = re.compile(r"asset policy:\s*(\d+)\s+files ok")
ALERT_LEVEL_RE = re.compile(r"\b(WARN|ERROR)\b")
OVERRIDE_KEY_RE = re.compile(r"^VICTORY_[A-Z0-9_]+$")

REFUSED_PATTERNS = (
    "does not translate",
    "host pipeline refused",
    "host mesh pipeline refused",
    "host root signature refused",
)
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
    pacing_windows: list[dict[str, float]] = []
    dstorage_rates: list[float] = []
    refused = 0
    device_removed = False
    first_ts: datetime | None = None
    first_world_ts: datetime | None = None
    world_window_ts: list[tuple[datetime, float]] = []

    for raw_line in text.splitlines():
        line = ANSI_RE.sub("", raw_line)
        ts_match = TS_RE.match(line)
        line_ts: datetime | None = None
        if ts_match:
            line_ts = datetime.fromisoformat(ts_match.group(1).replace("Z", "+00:00"))
            if first_ts is None:
                first_ts = line_ts
        if ALERT_LEVEL_RE.search(line):
            lower = line.lower()
            if "device removed" in lower:
                device_removed = True
            if any(pat in lower for pat in REFUSED_PATTERNS):
                refused += 1
        m_frame = FRAME_RE.search(line)
        if m_frame:
            fps = float(m_frame.group(1))
            prep_ms = float(m_frame.group(3))
            gpu_ms = float(m_frame.group(8))
            draws = float(m_frame.group(11))
            if draws >= WORLD_DRAWS_MIN:
                world_rows.append((fps, gpu_ms, prep_ms))
                if first_world_ts is None and line_ts is not None:
                    first_world_ts = line_ts
        pacing = parse_pacing_line(line)
        if pacing:
            pacing_windows.append(pacing)
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
        "refused": refused,
        "device_removed": device_removed,
        **summarize_pacing(pacing_windows),
    }


@throttled("build")
def cmd_build(args: argparse.Namespace) -> int:
    rel_step = (
        "cargo build release",
        ["cargo", "build", "--release", "-p", "victory", "--manifest-path", str(VICTORY_MANIFEST)],
        REPO_ROOT,
    )
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


def cmd_bench(args: argparse.Namespace) -> int:
    log_path = DEV_DIR / "bench.log"
    try:
        game_dir = resolve_game_dir(args.game_dir)
        overrides = apply_fast_boot(args, parse_overrides(args.override))
        title_args = parse_title_args(args.title_args)
        if args.save:
            resolve_save_dir(args.save)
    except (FileNotFoundError, ValueError) as exc:
        ensure_coord_dirs()
        log_path.write_text(f"error: {exc}\n", encoding="utf-8")
        print(
            "DEV bench FAIL boot_s=0.0 settled_s=0.0 fps=0.0 gpu_ms=0.0 prep_ms=0.0 dstorage_mbs=0.0 refused=0 "
            "p50_ms=0.0 p99_ms=0.0 max_ms=0.0 sd_ms=0.0 jitter_ms=0.0 over50=0 hitches=0"
        )
        return 1

    overrides, mute = apply_mute_default(overrides, args.unmute)
    with isolated_save_data_dir(args.save, "bench") as data_dir:
        cmd = build_launcher_command(
            game_dir=game_dir,
            diag=False,
            overrides=overrides,
            mute=mute,
            skip_intro=args.skip_intro,
            data_dir=data_dir,
            title_args=title_args,
        )
        with acquire_gpu_lock("bench"), _quiet(args, "bench"):
            resets_before = dev_driver.resets_since_boot()
            run_timed_victory(cmd, log_path, args.seconds)
            resets_after = dev_driver.resets_since_boot()

    stats = parse_victory_log(read_log_text(log_path))
    ok = int(stats["world_samples"]) > 0 and not bool(stats["device_removed"])
    status = "PASS" if ok else "FAIL"
    print(
        f"DEV bench {status} "
        f"boot_s={float(stats['boot_s']):.1f} "
        f"settled_s={float(stats['settled_s']):.1f} "
        f"fps={float(stats['fps']):.1f} "
        f"gpu_ms={float(stats['gpu_ms']):.1f} "
        f"prep_ms={float(stats['prep_ms']):.1f} "
        f"dstorage_mbs={float(stats['dstorage_mbs']):.1f} "
        f"refused={int(stats['refused'])} "
        f"p50_ms={float(stats['p50_ms']):.1f} "
        f"p99_ms={float(stats['p99_ms']):.1f} "
        f"max_ms={float(stats['max_ms']):.1f} "
        f"sd_ms={float(stats['sd_ms']):.1f} "
        f"jitter_ms={float(stats['jitter_ms']):.1f} "
        f"over50={int(stats['over50'])} "
        f"hitches={int(stats['hitches'])} "
        f"driver_resets={_count(resets_after)}"
    )
    _print_driver_warning(resets_before, resets_after)
    return 0 if ok else 1


def _count(value: int | None) -> str:
    return "?" if value is None else str(value)


def _quiet(args: argparse.Namespace, what: str):
    """`--quiet`: hold every build slot for the run so no build competes with the game."""
    if getattr(args, "quiet", False):
        return dev_throttle.all_build_slots(LOCKS_DIR, f"{JOB or 'main'}:quiet-{what}")
    return contextlib.nullcontext()


def _print_driver_warning(before: int | None, after: int | None) -> None:
    line = dev_driver.warning(before, after)
    if line:
        print(line)


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
        with acquire_gpu_lock(f"capture:{safe_tag}"):
            resets_before = dev_driver.resets_since_boot()
            run_timed_victory(cmd, log_path, args.seconds)
            resets_after = dev_driver.resets_since_boot()

    stats = parse_victory_log(read_log_text(log_path))
    frames = len(list(cap_dir.glob("*.png")))
    ok = frames > 0 and not bool(stats["device_removed"])
    status = "PASS" if ok else "FAIL"
    print(
        f"DEV capture {status} boot_s={float(stats['boot_s']):.1f} "
        f"settled_s={float(stats['settled_s']):.1f} frames={frames} refused={int(stats['refused'])} "
        f"driver_resets={_count(resets_after)} dir={cap_dir}"
    )
    _print_driver_warning(resets_before, resets_after)
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
