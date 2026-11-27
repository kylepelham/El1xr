"""Machine-wide throttles for tools/dev.py, so many worktree jobs can share one PC.

- `hold_first_free`: holds the first free lock file of a set (one file = one slot), waiting until
  one frees. The GPU lock is a set of one; builds use `BUILD_SLOTS` files.
- `below_normal_priority`: lowers this process (and the cargo/rustc/test processes it starts,
  which inherit the class on Windows) so builds never starve the desktop or a game run.
- `disk_free_gb`: builds refuse to start under `MIN_FREE_GB`; a full drive fails every job.
"""

from __future__ import annotations

import contextlib
import os
import shutil
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterator

BUILD_SLOTS = 3
MIN_FREE_GB = 40.0
BELOW_NORMAL_PRIORITY_CLASS = 0x4000
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
        fd = fds[held]
        stamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        os.lseek(fd, 0, os.SEEK_SET)
        os.ftruncate(fd, 0)
        os.write(fd, f"pid={os.getpid()} tag={tag} started={stamp}\n".encode("utf-8"))
        yield paths[held]
    finally:
        if held >= 0:
            _unlock(fds[held])
        for fd in fds:
            os.close(fd)


def build_slot(locks_dir: Path, tag: str, timeout_s: float = 4 * 3600.0):
    paths = [locks_dir / f"build{i}.lock" for i in range(build_slots())]
    return hold_first_free(paths, tag, timeout_s)


def below_normal_priority() -> None:
    if os.name == "nt":
        import ctypes

        kernel32 = ctypes.windll.kernel32
        kernel32.SetPriorityClass(kernel32.GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS)


def disk_free_gb(path: Path) -> float:
    return shutil.disk_usage(path).free / 1e9
