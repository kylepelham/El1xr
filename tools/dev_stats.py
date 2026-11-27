"""World-phase figures for tools/dev.py: the streaming window, the steady state after it, and the
hitches of the whole world phase.

Pacing windows (victory-gpu `pacing.rs`, one line every 5 s) are "world" when they ran at least
WORLD_DRAWS_MIN draws a frame. A world window is still streaming when its median frame is over
LOADING_P50_MS or STREAM_OVER100 or more of its frames took over 100 ms (texture and pipeline
streaming come in bursts of hitches at a playable median). The streaming window runs from the
start of the world phase (the end of the window before the first world one) to the end of the
last streaming world window (`stream_s`, 0 when none streams); everything after it is the
steady state.

Hitch lines (victory-gpu `hitch.rs`) name every frame of 250 ms or more, and shorter ones of
100 ms or more at most four a second: frames over 250 ms are counted from them, frames over
100 ms from the pacing windows (which count every frame).
"""

from __future__ import annotations

import re
import statistics
from dataclasses import dataclass
from datetime import datetime

HITCH_RE = re.compile(r"hitch: frame (\d+) took ([0-9.]+) ms")
ALERT_LEVEL_RE = re.compile(r"\b(WARN|ERROR)\b")
REFUSED_PATTERNS = (
    "does not translate",
    "host pipeline refused",
    "host mesh pipeline refused",
    "host root signature refused",
)
# victory-gpu `pipeline/translate_ahead.rs` (VICTORY_TRANSLATE_AHEAD): a shader translated ahead that does not
# translate, before any draw or dispatch needed it. An INFO line; the WARN refusal follows only if one does.
REFUSED_AHEAD_TEXT = "shader translated ahead is not translatable"


def refusal(line: str) -> str | None:
    """`"refused"` for a refusal a draw, dispatch or pipeline needed (a WARN or ERROR line), `"ahead"` for a shader
    only translated ahead that does not translate, else None. `line` is without colour codes."""
    if REFUSED_AHEAD_TEXT in line:
        return "ahead"
    if ALERT_LEVEL_RE.search(line) and any(p in line.lower() for p in REFUSED_PATTERNS):
        return "refused"
    return None
WORLD_DRAWS_MIN = 100.0
LOADING_P50_MS = 100.0
STREAM_OVER100 = 3
OVER250_MS = 250.0


@dataclass
class Window:
    ts: datetime
    p50: float
    p99: float
    max: float
    over100: float
    draws: float


def parse_hitch_line(line: str) -> tuple[int, float] | None:
    """(frame, ms) of a `hitch: frame N took X ms` line, or None."""
    m = HITCH_RE.search(line)
    return (int(m.group(1)), float(m.group(2))) if m else None


def streaming(w: Window) -> bool:
    return w.p50 > LOADING_P50_MS or w.over100 >= STREAM_OVER100


def world_phase(windows: list[Window], hitches: list[tuple[datetime, int, float]]) -> dict[str, float]:
    """Figures of the world phase from every pacing window (in log order) and every hitch line
    (timestamp, frame, ms). The world phase starts where the window before the first world one
    ended (hitch lines inside the first world window count)."""
    first = next((i for i, w in enumerate(windows) if w.draws >= WORLD_DRAWS_MIN), None)
    out = {
        "stream_s": 0.0, "steady_windows": 0, "steady_p99_ms": 0.0, "steady_max_ms": 0.0,
        "steady_over100": 0, "world_over100": 0, "world_over250": 0, "worst_ms": 0.0, "worst_frame": 0,
    }
    if first is None:
        return out
    world_start = windows[first - 1].ts if first > 0 else None
    world = [w for w in windows[first:] if w.draws >= WORLD_DRAWS_MIN]
    last_streaming = max((i for i, w in enumerate(world) if streaming(w)), default=-1)
    steady = world[last_streaming + 1 :]
    if last_streaming >= 0:
        out["stream_s"] = (world[last_streaming].ts - (world_start or world[0].ts)).total_seconds()
    world_hitches = [(f, ms) for ts, f, ms in hitches if world_start is None or ts > world_start]
    out["world_over100"] = int(sum(w.over100 for w in world))
    out["world_over250"] = sum(1 for _, ms in world_hitches if ms >= OVER250_MS)
    worst_frame, worst_ms = max(world_hitches, key=lambda h: h[1], default=(0, 0.0))
    window_max = max(w.max for w in world)
    if window_max > worst_ms:
        worst_frame, worst_ms = 0, window_max
    out["worst_ms"], out["worst_frame"] = worst_ms, worst_frame
    if steady:
        out["steady_windows"] = len(steady)
        out["steady_p99_ms"] = statistics.median(w.p99 for w in steady)
        out["steady_max_ms"] = max(w.max for w in steady)
        out["steady_over100"] = int(sum(w.over100 for w in steady))
    return out
