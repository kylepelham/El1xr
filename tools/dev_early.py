"""A live watch of a train arm's log while the game runs (`dev.py run_timed_victory`'s monitor).

Early stop (plan `early_stop`, on by default for loud arms): the run ends once the world has
settled and its last `steady_seconds` of pacing windows (victory-gpu `pacing.rs`, one line every
5 s, the windows `dev_stats` reads) give fps and prep medians whose 95% interval is within
FPS_TOL fps and PREP_TOL ms. A window's fps is its pacing line's; its prep is the median of the
`... ms/frame (N preparing` lines logged since the window before. A streaming or loading world
window (`dev_stats.streaming`) or a window off the world empties the steady run, so boot_s,
settled_s and stream_s are all behind it when a run ends. The interval of a median is taken as
1.96 x 1.2533 x the windows' standard deviation over the square root of their count (the normal
approximation of the median's standard error). Never before `min_seconds` from launch; the
caller's timer still ends the run at `seconds`.

Fatal lines end the run too, FATAL_GRACE_S after the first (the device-removed report and the
hang watch's lines follow it): `device removed` at WARN/ERROR, `DEVICE_HUNG`, or a hang watch
stall over STALL_S.
"""

from __future__ import annotations

import math
import re
import statistics
from dataclasses import dataclass, field
from pathlib import Path

import dev_stats

ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
PACING_RE = re.compile(
    r"frame pacing: (\d+) frames in ([0-9.]+) s \(([0-9.]+) fps\), frame time p50 ([0-9.]+) ms .*?"
    r", (\d+) over 100 ms, .*?; ([0-9.]+) draws/frame"
)
PREP_RE = re.compile(r"[0-9.]+ fps, [0-9.]+ ms/frame \(([0-9.]+) preparing")
STALL_RE = re.compile(r"hang watch: the card's fence has stood at \d+ for ([0-9.]+) s|hang watch: the card moved on.*?stood_s=([0-9.]+)")
LEVEL_RE = re.compile(r"\b(WARN|ERROR)\b")
FPS_TOL = 0.5
PREP_TOL = 1.0
STALL_S = 9.0
FATAL_GRACE_S = 10.0
Z95 = 1.96
MEDIAN_SE = 1.2533
MIN_WINDOWS = 4


def stall_seconds(line: str) -> float | None:
    """How long a hang watch line says the card's fence stood, or None."""
    m = STALL_RE.search(line)
    if not m:
        return None
    return float(m.group(1) or m.group(2))


def fatal_line(line: str, stall_s: float = STALL_S) -> str | None:
    """Why this log line (without colour codes) ends a run, or None."""
    if "DEVICE_HUNG" in line:
        return "DEVICE_HUNG"
    if LEVEL_RE.search(line) and "device removed" in line.lower():
        return "device removed"
    stood = stall_seconds(line)
    if stood is not None and stood > stall_s:
        return f"hang watch stall {stood:.1f} s"
    return None


def half_width(values: list[float]) -> float:
    """The 95% interval's half width of the median of `values` (normal approximation)."""
    if len(values) < 2:
        return math.inf
    return Z95 * MEDIAN_SE * statistics.stdev(values) / math.sqrt(len(values))


@dataclass
class Window:
    secs: float
    fps: float
    prep: float | None


@dataclass
class Monitor:
    early: bool
    min_s: float = 90.0
    steady_s: float = 60.0
    fps_tol: float = FPS_TOL
    prep_tol: float = PREP_TOL
    stall_s: float = STALL_S
    steady: list[Window] = field(default_factory=list)
    preps: list[float] = field(default_factory=list)
    world: bool = False
    fatal: str | None = None
    fatal_at: float | None = None
    ended: dict[str, object] | None = None
    _partial: str = ""

    def feed(self, text: str, elapsed: float) -> str | None:
        """New log text, `elapsed` seconds after launch: "fatal" or "early" when the run should end now, else None."""
        lines = (self._partial + text).split("\n")
        self._partial = lines.pop()
        for raw in lines:
            self._line(ANSI_RE.sub("", raw))
        if self.fatal is not None:
            if self.fatal_at is None:
                self.fatal_at = elapsed
            if elapsed - self.fatal_at >= FATAL_GRACE_S:
                self.ended = {"how": "fatal", "why": self.fatal, "at_s": round(elapsed, 1)}
                return "fatal"
            return None
        if self.early and elapsed >= self.min_s:
            figures = self.steady_figures()
            if figures and figures["fps_hw"] <= self.fps_tol and figures["prep_hw"] <= self.prep_tol:
                self.ended = {"how": "early", "at_s": round(elapsed, 1), **figures}
                return "early"
        return None

    def _line(self, line: str) -> None:
        if self.fatal is None:
            self.fatal = fatal_line(line, self.stall_s)
        m = PREP_RE.search(line)
        if m:
            self.preps.append(float(m.group(1)))
            return
        m = PACING_RE.search(line)
        if not m:
            return
        secs, fps, p50, over100, draws = (float(m.group(i)) for i in (2, 3, 4, 5, 6))
        prep = statistics.median(self.preps) if self.preps else None
        self.preps = []
        if draws < dev_stats.WORLD_DRAWS_MIN:
            self.steady = []
            return
        self.world = True
        if p50 > dev_stats.LOADING_P50_MS or over100 >= dev_stats.STREAM_OVER100:
            self.steady = []
            return
        self.steady.append(Window(secs, fps, prep))

    def steady_figures(self) -> dict[str, float] | None:
        """The trailing steady windows covering `steady_s`: their count, fps and prep medians and interval half
        widths; None while the steady run is shorter."""
        tail, covered = [], 0.0
        for w in reversed(self.steady):
            tail.append(w)
            covered += w.secs
            if covered >= self.steady_s:
                break
        if covered < self.steady_s or len(tail) < MIN_WINDOWS:
            return None
        fps = [w.fps for w in tail]
        prep = [w.prep for w in tail if w.prep is not None]
        if len(prep) < MIN_WINDOWS:
            return None
        return {
            "windows": len(tail), "fps": statistics.median(fps), "fps_hw": half_width(fps),
            "prep_ms": statistics.median(prep), "prep_hw": half_width(prep),
        }

    def summary(self) -> str:
        e = self.ended or {}
        if e.get("how") == "early":
            return (f"early stop at {e['at_s']:.0f}s (fps {e['fps']:.1f} +-{e['fps_hw']:.2f}, prep {e['prep_ms']:.1f} "
                    f"+-{e['prep_hw']:.2f} ms over {e['windows']} steady windows)")
        if e.get("how") == "fatal":
            return f"fatal stop at {e['at_s']:.0f}s: {e['why']}"
        return ""


class LogTail:
    """The text appended to a log since the last read (UTF-8, or UTF-16 after a byte order mark)."""

    def __init__(self, path: Path) -> None:
        self.path = path
        self.pos = 0
        self.encoding: str | None = None
        self.pending = b""

    def read(self) -> str:
        try:
            with self.path.open("rb") as fh:
                fh.seek(self.pos)
                data = fh.read()
        except OSError:
            return ""
        self.pos += len(data)
        data = self.pending + data
        if self.encoding is None:
            if len(data) < 2:
                self.pending = data
                return ""
            if data[:2] in (b"\xff\xfe", b"\xfe\xff"):
                self.encoding = "utf-16-le" if data[:2] == b"\xff\xfe" else "utf-16-be"
                data = data[2:]
            else:
                self.encoding = "utf-8"
        if self.encoding == "utf-8":
            cut = data.rfind(b"\n") + 1
        else:
            cut = len(data) - len(data) % 2
        self.pending = data[cut:]
        return data[:cut].decode(self.encoding, errors="replace")
