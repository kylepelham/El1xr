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
- `make_room`: before a game launch, makes sure the machine has `game_commit_gb()` of commit free
  (the game peaks at 33-35 GB of private commit; a suspended build keeps all of its own): waits for
  running builds while holding the build slots that free up, then preempts (kills) the build trees
  holding the most and leaves a marker for the writer's dev.py, which waits for the run and retries.
- `wait_for_game_room`: builds, tests and gates wait while a game run holds the gpu lock and free
  commit is under `WRITER_COMMIT_GB`.
"""

from __future__ import annotations

import contextlib
import json
import os
import shutil
import threading
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Iterator

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


def _started_by(proc, pid: int) -> bool:
    """Whether `pid` is among `proc`'s ancestors, read from the candidate itself so a process this
    dev.py starts between two scans (the launcher's cargo, the game) is never taken for a writer's.
    Unknown counts as ours: better to leave a writer's test running than to pause the game."""
    try:
        return any(parent.pid == pid for parent in proc.parents())
    except Exception:
        return True


@contextlib.contextmanager
def builds_suspended(psutil_module=None, rescan_s: float = 1.0, builds: bool = True) -> Iterator[int]:
    """`--quiet` without the wait: suspends every build (`_build_tree`) and worktree tool or test
    executable (`_worktree_tools`, which includes writers' GPU tests) for the duration and resumes
    them after, so a quiet run starts at once and nothing competes with the game for the CPU or the
    card. Rescans every `rescan_s` seconds so programs started during the run are caught too. This
    dev.py, its parents and everything it starts (the game, its prewarm) are spared. `builds=False`
    leaves builds running and suspends only the worktree tools (loud runs: builds go on at idle
    priority, but no test may share the card with the game). Yields how many were suspended at the
    start. A no-op without psutil."""
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
        for proc in (_build_tree(psutil) if builds else []) + _worktree_tools(psutil):
            if proc.pid in spared or proc.pid in paused:
                continue
            if _started_by(proc, me.pid):
                spared.add(proc.pid)
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


# The 9950X3D's second CCD (logical CPUs 16-31, 32 MB L3): builds and worktree tests go there while
# the game runs, so the V-cache CCD (0-15, 96 MB L3) and its cache stay the game's.
ASIDE_CPUS = tuple(range(16, 32))


@contextlib.contextmanager
def builds_aside(psutil_module=None, rescan_s: float = 2.0, cpus: tuple[int, ...] = ASIDE_CPUS) -> Iterator[int]:
    """Loud runs: pins every build (`_build_tree`) and worktree tool or test (`_worktree_tools`) to
    `cpus` at the idle priority class for the duration, rescanning every `rescan_s` seconds for new
    ones, and gives each back all CPUs after. Nothing is paused, so writers' tests and gates keep
    going. This dev.py, its parents and everything it starts (the game) are spared. Yields how many
    were moved at the start. A no-op without psutil or with fewer CPUs than `cpus` names."""
    psutil = psutil_module
    if psutil is None:
        try:
            import psutil
        except ImportError:
            psutil = None
    if psutil is None or (psutil.cpu_count() or 0) <= max(cpus, default=0):
        yield 0
        return
    every = list(range(psutil.cpu_count()))
    idle = getattr(psutil, "IDLE_PRIORITY_CLASS", None)
    me = psutil.Process()
    spared: set[int] = {me.pid}
    try:
        spared.update(p.pid for p in me.parents())
    except Exception:
        pass
    moved: dict[int, object] = {}
    lock = threading.Lock()

    def sweep() -> None:
        try:
            spared.update(c.pid for c in me.children(recursive=True))
        except Exception:
            pass
        for proc in _build_tree(psutil) + _worktree_tools(psutil):
            if proc.pid in spared or proc.pid in moved:
                continue
            if _started_by(proc, me.pid):
                spared.add(proc.pid)
                continue
            try:
                proc.cpu_affinity(list(cpus))
                if idle is not None:
                    proc.nice(idle)
            except Exception:
                continue
            with lock:
                moved[proc.pid] = proc

    sweep()
    first = len(moved)
    stop = threading.Event()

    def loop() -> None:
        while not stop.wait(rescan_s):
            sweep()

    watcher = threading.Thread(target=loop, name="builds-aside", daemon=True)
    if rescan_s > 0:
        watcher.start()
    try:
        yield first
    finally:
        stop.set()
        if watcher.is_alive():
            watcher.join(timeout=10)
        with lock:
            procs = list(moved.values())
        for proc in procs:
            try:
                proc.cpu_affinity(every)
            except Exception:
                pass


def below_normal_priority() -> None:
    if os.name == "nt":
        import ctypes

        kernel32 = ctypes.windll.kernel32
        kernel32.SetPriorityClass(kernel32.GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS)


def disk_free_gb(path: Path) -> float:
    return shutil.disk_usage(path).free / 1e9


GAME_COMMIT_GB = 40.0
WRITER_COMMIT_GB = 45.0
COMMIT_WAIT_S = 360.0
COMMIT_POLL_S = 5.0
COMMIT_SAY_S = 30.0
PREEMPTED_DIR = "preempted"
PREEMPTED_LOG = "preempted.log"
GIB = float(1 << 30)


def game_commit_gb() -> float:
    """The free commit a game launch needs (`ANYXBOXX_GAME_COMMIT_GB`, default GAME_COMMIT_GB)."""
    try:
        return float(os.environ.get("ANYXBOXX_GAME_COMMIT_GB", GAME_COMMIT_GB))
    except ValueError:
        return GAME_COMMIT_GB


def commit_free_gb() -> float | None:
    """The commit the machine can still hand out (commit limit minus commit charge), in GiB; None
    off Windows. Suspending a process gives none back: only its exit does."""
    if os.name != "nt":
        return None
    import ctypes

    class MemoryStatusEx(ctypes.Structure):
        _fields_ = [
            ("dwLength", ctypes.c_ulong), ("dwMemoryLoad", ctypes.c_ulong),
            ("ullTotalPhys", ctypes.c_ulonglong), ("ullAvailPhys", ctypes.c_ulonglong),
            ("ullTotalPageFile", ctypes.c_ulonglong), ("ullAvailPageFile", ctypes.c_ulonglong),
            ("ullTotalVirtual", ctypes.c_ulonglong), ("ullAvailVirtual", ctypes.c_ulonglong),
            ("ullAvailExtendedVirtual", ctypes.c_ulonglong),
        ]

    status = MemoryStatusEx()
    status.dwLength = ctypes.sizeof(status)
    if not ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(status)):
        return None
    return status.ullAvailPageFile / GIB


def held_by_other(path: Path) -> bool:
    """Whether someone holds the lock file `path` (a `hold_first_free` set of one) right now."""
    try:
        fd = os.open(str(path), os.O_RDWR)
    except OSError:
        return False
    try:
        if _try_lock(fd):
            _unlock(fd)
            return False
        return True
    finally:
        os.close(fd)


def wait_for_game_room(
    gpu_lock: Path, name: str, need_gb: float = WRITER_COMMIT_GB,
    free_fn: Callable[[], float | None] | None = None, poll_s: float | None = None,
    sleep: Callable[[float], None] = time.sleep, out: Callable[[str], None] = print,
) -> float:
    """Builds, tests and gates: waits while a game run holds `gpu_lock` and free commit is under
    `need_gb` (two programs over the commit limit kill the game with 0x8007000E). Returns the
    seconds waited."""
    free_fn = free_fn or commit_free_gb
    poll_s = COMMIT_POLL_S if poll_s is None else poll_s
    t0 = time.monotonic()
    said = None
    while True:
        free = free_fn()
        if free is None or free >= need_gb or not held_by_other(gpu_lock):
            break
        now = time.monotonic()
        if said is None or now - said >= 60:
            out(f"DEV {name}: waiting: a game run holds the gpu lock and commit free is {free:.1f} GB "
                f"(need {need_gb:g} GB to start)")
            said = now
        sleep(poll_s)
    waited = time.monotonic() - t0
    if said is not None:
        out(f"DEV {name}: going on after waiting {waited:.0f}s for the game run")
    return waited


def preempt_marker(locks_dir: Path, pid: int) -> Path:
    return locks_dir / PREEMPTED_DIR / str(pid)


def take_preempted(locks_dir: Path, pid: int | None = None) -> str | None:
    """The marker a game run left when it killed this dev.py's builds or tests (its text), removed;
    None when there is none."""
    marker = preempt_marker(locks_dir, os.getpid() if pid is None else pid)
    try:
        text = marker.read_text(encoding="utf-8")
    except OSError:
        return None
    with contextlib.suppress(OSError):
        marker.unlink()
    return text.strip() or "preempted"


@dataclass
class Tree:
    """Builds and tests one writer started: `owner` is its dev.py (which retries), or None for a
    cargo started by hand (then `procs` is that cargo and what it started)."""

    owner: int | None
    root: int
    procs: list = field(default_factory=list)
    commit_gb: float = 0.0
    label: str = ""


def _private_gb(proc) -> float:
    try:
        mem = proc.memory_info()
    except Exception:
        return 0.0
    return float(getattr(mem, "private", getattr(mem, "vms", 0))) / GIB


def _is_dev_py(proc) -> bool:
    try:
        return any(os.path.basename(arg).lower() == "dev.py" for arg in proc.cmdline()[1:])
    except Exception:
        return False


def build_trees(psutil, me: int) -> list[Tree]:
    """Running builds and worktree tests grouped by the dev.py that started them (or by their top
    cargo), leaving out `me` and everything it started."""
    found: dict[int, object] = {}
    for proc in _build_tree(psutil) + _worktree_tools(psutil):
        found.setdefault(proc.pid, proc)
    trees: dict[tuple[str, int], Tree] = {}
    for proc in found.values():
        try:
            parents = proc.parents()
        except Exception:
            continue
        if proc.pid == me or any(p.pid == me for p in parents):
            continue
        owner = next((p for p in parents if _is_dev_py(p)), None)
        if owner is not None:
            key = ("dev", owner.pid)
            if key not in trees:
                try:
                    procs = owner.children(recursive=True)
                except Exception:
                    continue
                trees[key] = Tree(owner.pid, owner.pid, procs, label=" ".join(owner.cmdline()[1:])[:120])
            continue
        top = proc
        for parent in parents:
            if parent.pid not in found:
                break
            top = parent
        key = ("cargo", top.pid)
        if key not in trees:
            try:
                procs = [top, *top.children(recursive=True)]
                label = " ".join(top.cmdline())[:120]
            except Exception:
                continue
            trees[key] = Tree(None, top.pid, procs, label=label)
    for tree in trees.values():
        tree.commit_gb = sum(_private_gb(p) for p in tree.procs)
    return sorted(trees.values(), key=lambda t: t.commit_gb, reverse=True)


def preempt(tree: Tree, locks_dir: Path, by: str) -> None:
    """Kills `tree`'s processes (its dev.py lives on), marks its dev.py preempted, and logs it."""
    for proc in tree.procs:
        with contextlib.suppress(Exception):
            proc.kill()
    stamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    entry = {"at": stamp, "by": by, "owner": tree.owner, "root": tree.root,
             "commit_gb": round(tree.commit_gb, 1), "what": tree.label}
    (locks_dir / PREEMPTED_DIR).mkdir(parents=True, exist_ok=True)
    if tree.owner is not None:
        preempt_marker(locks_dir, tree.owner).write_text(json.dumps(entry) + "\n", encoding="utf-8")
    with (locks_dir / PREEMPTED_LOG).open("a", encoding="utf-8") as log:
        log.write(json.dumps(entry) + "\n")


class _SlotHog:
    """Holds every build slot that comes free (`grab`) until `release`: no new build starts."""

    def __init__(self, locks_dir: Path, tag: str) -> None:
        self.paths = [locks_dir / f"build{i}.lock" for i in range(build_slots())]
        self.tag = tag
        self.held: dict[Path, int] = {}
        locks_dir.mkdir(parents=True, exist_ok=True)

    def grab(self) -> None:
        for path in self.paths:
            if path in self.held:
                continue
            fd = os.open(str(path), os.O_CREAT | os.O_RDWR)
            if _try_lock(fd):
                _stamp(fd, self.tag)
                self.held[path] = fd
            else:
                os.close(fd)

    def release(self) -> None:
        for fd in self.held.values():
            _unlock(fd)
            os.close(fd)
        self.held.clear()


@dataclass
class Room:
    free_gb: float | None
    need_gb: float
    waited_s: float = 0.0
    preempted: int = 0


def make_room(
    locks_dir: Path, what: str, need_gb: float | None = None, wait_s: float = COMMIT_WAIT_S,
    psutil_module=None, free_fn: Callable[[], float | None] = commit_free_gb, poll_s: float = COMMIT_POLL_S,
    sleep: Callable[[float], None] = time.sleep, out: Callable[[str], None] = print,
) -> Room:
    """Before a game launch (the caller holds the gpu lock, so `wait_for_game_room` holds new
    writers back): with under `need_gb` of commit free, waits up to `wait_s` for running builds
    and worktree tests to finish while holding every build slot that frees up, then kills the
    trees holding the most (`preempt`) until enough is free. Off Windows or without psutil it only
    reads what is free."""
    need = game_commit_gb() if need_gb is None else need_gb
    free = free_fn()
    room = Room(free, need)
    if free is None or free >= need:
        return room
    psutil = psutil_module
    if psutil is None:
        try:
            import psutil
        except ImportError:
            return room
    me = psutil.Process().pid
    hog = _SlotHog(locks_dir, f"commit-{what}")
    t0 = time.monotonic()
    said = None
    try:
        trees = build_trees(psutil, me)
        while free is not None and free < need and trees and time.monotonic() - t0 < wait_s:
            hog.grab()
            now = time.monotonic()
            if said is None or now - said >= COMMIT_SAY_S:
                held = sum(t.commit_gb for t in trees)
                out(f"DEV {what}: commit free {free:.1f} GB, need {need:g}: waiting for {len(trees)} builds/tests "
                    f"({held:.1f} GB) to finish, new ones held back ({now - t0:.0f}s of {wait_s:.0f}s)")
                said = now
            sleep(poll_s)
            free = free_fn()
            trees = build_trees(psutil, me)
        room.waited_s = time.monotonic() - t0
        for tree in trees:
            if free is None or free >= need:
                break
            out(f"DEV {what}: preempting {tree.label or tree.root} (pid {tree.owner or tree.root}, "
                f"{tree.commit_gb:.1f} GB of commit); its dev.py retries after the run")
            preempt(tree, locks_dir, what)
            room.preempted += 1
            free = _settled_free(free, free_fn, sleep)
    finally:
        hog.release()
    room.free_gb = free
    return room


def _settled_free(before: float, free_fn: Callable[[], float | None], sleep: Callable[[float], None]) -> float | None:
    """Free commit once a kill has given it back (up to about 5 s)."""
    free = free_fn()
    for _ in range(10):
        if free is None or free > before + 0.5:
            break
        sleep(0.5)
        free = free_fn()
    return free
