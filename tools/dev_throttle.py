"""Machine-wide throttles for tools/dev.py, so many worktree jobs can share one PC.

- `hold_first_free`: holds the first free lock file of a set (one file = one slot), waiting until
  one frees. The GPU lock is a set of one; builds use `BUILD_SLOTS` files.
- `below_normal_priority`: lowers this process (and the cargo/rustc/test processes it starts,
  which inherit the class on Windows) so builds never starve the desktop or a game run.
- `disk_free_gb`: builds refuse to start under `MIN_FREE_GB`; a full drive fails every job.
- `free_build_slots`: for a game run that is not `--quiet`, holds the build slots free right now
  without waiting, always leaving writers at least one, so fewer builds compete with the game.
- `builds_idle`: lowers running cargo/rustc processes (and what they started) to the idle priority
  class for a game run, and puts them back after.
"""

from __future__ import annotations

import contextlib
import os
import shutil
import threading
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterator

BUILD_SLOTS = 3
MIN_FREE_GB = 40.0
BELOW_NORMAL_PRIORITY_CLASS = 0x4000
IDLE_PRIORITY_CLASS = 0x40
POLL_S = 0.25


def build_slots() -> int:
    return max(1, int(os.environ.get("ANYXBOXX_BUILD_SLOTS", BUILD_SLOTS)))


def _try_lock(fd: int) -> bool:
    try:
        os.lseek(fd, 0, os.SEEK_SET)
        if os.name == "nt":
            import msvcrt

            msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
        else:
            import fcntl

            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return True
    except OSError:
        return False


def _unlock(fd: int) -> None:
    try:
        os.lseek(fd, 0, os.SEEK_SET)
        if os.name == "nt":
            import msvcrt

            msvcrt.locking(fd, msvcrt.LK_UNLCK, 1)
        else:
            import fcntl

            fcntl.flock(fd, fcntl.LOCK_UN)
    except OSError:
        pass


@contextlib.contextmanager
def hold_first_free(paths: list[Path], tag: str, timeout_s: float) -> Iterator[Path]:
    """Holds the first lock file in `paths` nobody else holds, waiting up to `timeout_s`."""
    for path in paths:
        path.parent.mkdir(parents=True, exist_ok=True)
    fds = [os.open(str(p), os.O_CREAT | os.O_RDWR) for p in paths]
    held = -1
    try:
        deadline = time.monotonic() + timeout_s
        while held < 0:
            held = next((i for i, fd in enumerate(fds) if _try_lock(fd)), -1)
            if held < 0:
                if time.monotonic() >= deadline:
                    raise TimeoutError(f"Timed out waiting for a free lock among {[str(p) for p in paths]}")
                time.sleep(POLL_S)
        _stamp(fds[held], tag)
        yield paths[held]
    finally:
        if held >= 0:
            _unlock(fds[held])
        for fd in fds:
            os.close(fd)


def _stamp(fd: int, tag: str) -> None:
    stamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    os.lseek(fd, 0, os.SEEK_SET)
    os.ftruncate(fd, 0)
    os.write(fd, f"pid={os.getpid()} tag={tag} started={stamp}\n".encode("utf-8"))


def build_slot(locks_dir: Path, tag: str, timeout_s: float = 4 * 3600.0):
    paths = [locks_dir / f"build{i}.lock" for i in range(build_slots())]
    return hold_first_free(paths, tag, timeout_s)


@contextlib.contextmanager
def all_build_slots(locks_dir: Path, tag: str, timeout_s: float = 4 * 3600.0) -> Iterator[None]:
    """Holds every build slot: running builds finish first and no new one starts until release.
    For quiet benches, whose numbers swing several-fold when the machine is busy building."""
    with contextlib.ExitStack() as stack:
        for i in range(build_slots()):
            stack.enter_context(hold_first_free([locks_dir / f"build{i}.lock"], tag, timeout_s))
        yield


@contextlib.contextmanager
def free_build_slots(locks_dir: Path, tag: str, leave: int = 1) -> Iterator[list[Path]]:
    """Holds every build slot nobody holds right now, without waiting, but at most
    `build_slots() - leave` of them, so writers always keep `leave` slots. Builds already running
    keep theirs. Yields the slots held; all are released on exit."""
    paths = [locks_dir / f"build{i}.lock" for i in range(build_slots())]
    most = max(0, len(paths) - max(0, leave))
    locks_dir.mkdir(parents=True, exist_ok=True)
    held: list[tuple[int, Path]] = []
    try:
        for path in paths:
            if len(held) >= most:
                break
            fd = os.open(str(path), os.O_CREAT | os.O_RDWR)
            if _try_lock(fd):
                _stamp(fd, tag)
                held.append((fd, path))
            else:
                os.close(fd)
        yield [path for _, path in held]
    finally:
        for fd, _ in held:
            _unlock(fd)
            os.close(fd)


BUILD_PROCESSES = frozenset({"cargo.exe", "rustc.exe", "clippy-driver.exe", "rustdoc.exe"})


def _build_tree(psutil) -> list:
    """Running cargo/rustc processes and everything they started (linkers, build scripts, tests)."""
    found: dict[int, object] = {}
    for proc in psutil.process_iter(["name"]):
        if (proc.info.get("name") or "").lower() not in BUILD_PROCESSES:
            continue
        found[proc.pid] = proc
        try:
            for child in proc.children(recursive=True):
                found.setdefault(child.pid, child)
        except (psutil.NoSuchProcess, psutil.AccessDenied):
            pass
    found.pop(os.getpid(), None)
    return list(found.values())


@contextlib.contextmanager
def builds_idle(psutil_module=None) -> Iterator[int]:
    """Windows: lowers running builds (`_build_tree`) to the idle priority class for the duration,
    then restores each to the class it had (if it is still the same process and still idle).
    Processes they start meanwhile inherit the idle class; those still running at the end are put
    at below normal, the class `dev.py` builds run at. Yields how many were lowered. A no-op
    elsewhere or without psutil (`psutil_module`: for tests)."""
    psutil = psutil_module
    if psutil is None and os.name == "nt":
        try:
            import psutil
        except ImportError:
            psutil = None
    if psutil is None:
        yield 0
        return
    started = time.time()
    lowered: list[tuple[object, float, int]] = []
    for proc in _build_tree(psutil):
        try:
            before = proc.nice()
            if before == IDLE_PRIORITY_CLASS:
                continue
            created = proc.create_time()
            proc.nice(IDLE_PRIORITY_CLASS)
            lowered.append((proc, created, before))
        except (psutil.NoSuchProcess, psutil.AccessDenied):
            pass
    try:
        yield len(lowered)
    finally:
        restored = set()
        for proc, created, before in lowered:
            try:
                if proc.create_time() == created and proc.nice() == IDLE_PRIORITY_CLASS:
                    proc.nice(before)
                restored.add(proc.pid)
            except (psutil.NoSuchProcess, psutil.AccessDenied):
                pass
        for proc in _build_tree(psutil):
            try:
                if (
                    proc.pid not in restored
                    and proc.create_time() >= started
                    and proc.nice() == IDLE_PRIORITY_CLASS
                ):
                    proc.nice(BELOW_NORMAL_PRIORITY_CLASS)
            except (psutil.NoSuchProcess, psutil.AccessDenied):
                pass


def _worktree_tools(psutil) -> list:
    """Windowless programs run from a worktree's cargo target (offline `victory translate`, test
    executables started by hand), which `_build_tree` does not see."""
    found = []
    for proc in psutil.process_iter(["exe"]):
        exe = (proc.info.get("exe") or "").lower().replace("/", "\\")
        if "\\anyxboxx-wt\\" in exe and "\\target\\" in exe:
            found.append(proc)
    return found


@contextlib.contextmanager
def builds_suspended(psutil_module=None, rescan_s: float = 1.0) -> Iterator[int]:
    """`--quiet` without the wait: suspends every build (`_build_tree`) and worktree tool or test
    executable (`_worktree_tools`, which includes writers' GPU tests) for the duration and resumes
    them after, so a quiet run starts at once and nothing competes with the game for the CPU or the
    card. Rescans every `rescan_s` seconds so programs started during the run are caught too. This
    dev.py, its parents and everything it starts (the game, its prewarm) are spared. Yields how many
    were suspended at the start. A no-op without psutil."""
    psutil = psutil_module
    if psutil is None:
        try:
            import psutil
        except ImportError:
            psutil = None
    if psutil is None:
        yield 0
        return
    me = psutil.Process()
    parents: set[int] = {me.pid}
    try:
        parents.update(p.pid for p in me.parents())
    except Exception:
        pass
    paused: dict[int, object] = {}
    lock = threading.Lock()

    spared = set(parents)

    def sweep() -> None:
        # Once spared, always spared: the game stays spared if the launcher between it and dev.py exits.
        try:
            spared.update(c.pid for c in me.children(recursive=True))
        except Exception:
            pass
        for proc in _build_tree(psutil) + _worktree_tools(psutil):
            if proc.pid in spared or proc.pid in paused:
                continue
            try:
                proc.suspend()
            except Exception:
                continue
            with lock:
                paused[proc.pid] = proc

    sweep()
    first = len(paused)
    stop = threading.Event()

    def loop() -> None:
        while not stop.wait(rescan_s):
            sweep()

    watcher = threading.Thread(target=loop, name="quiet-suspend", daemon=True)
    if rescan_s > 0:
        watcher.start()
    try:
        yield first
    finally:
        stop.set()
        if watcher.is_alive():
            watcher.join(timeout=10)
        with lock:
            procs = list(paused.values())
        for proc in procs:
            try:
                proc.resume()
            except Exception:
                pass


def below_normal_priority() -> None:
    if os.name == "nt":
        import ctypes

        kernel32 = ctypes.windll.kernel32
        kernel32.SetPriorityClass(kernel32.GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS)


def disk_free_gb(path: Path) -> float:
    return shutil.disk_usage(path).free / 1e9
