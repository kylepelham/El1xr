"""`dev.py prewarm`: fills the translator's shader cache offline, for a given set of switch values.

Every change to the shader translator (`victory-shader`'s source) or to a switch it reads changes the cache key,
so the first run after it, and the first run of each batch arm, translates about 1,700 shaders at their first use.
`prewarm` translates every shader the title ships (`shaders.rpf`, the game executable and its two DLLs, as the
launcher's -Precompile does) on every core, into the same cache (`%LOCALAPPDATA%\\victory\\cache\\shaders`), with
the launcher's environment plus the given overrides, so the key is the one the run will use.

Translation only (the default) makes no device on the card: the host's two facts that join the key (the most lanes
of a wave, the view heap's null slot) are given (`VICTORY_HOST_WAVE`, `VICTORY_VIEW_LIMIT`), by default this
machine's RTX 5090 as every run's log states them ("host wave lanes min=32 max=32", "host view heaps hold 1000000
views"). `--pipelines` also makes the compute pipelines (on the card, under the gpu lock).
"""

from __future__ import annotations

import re
from pathlib import Path

# RTX 5090: "host wave lanes min=32 max=32", "host view heaps hold 1000000 views" (null slot: the last).
DEFAULT_HOST_WAVE = 32
DEFAULT_VIEW_LIMIT = 1_000_000 - 1

WAVE_RE = re.compile(r"host wave lanes min=\d+ max=(\d+)")
VIEWS_RE = re.compile(r"host view heaps hold (\d+) views")
WARM_RE = re.compile(
    r"warm\s+(\d+) translated, (\d+) refused; (\d+) compute pipelines, (\d+) refused; (\d+) s"
)
CACHE_RE = re.compile(r"warm cache\s+(\d+) from the cache, (\d+) translated now")


def host_facts_from_log(text: str) -> tuple[int, int] | None:
    """(most wave lanes, view heap null slot) as a run's log states them, if it does."""
    wave = WAVE_RE.search(text)
    views = VIEWS_RE.search(text)
    if not wave or not views:
        return None
    return int(wave.group(1)), int(views.group(1)) - 1


def prewarm_overrides(
    overrides: dict[str, str], pipelines: bool, host_wave: int, view_limit: int
) -> dict[str, str]:
    """The launcher overrides for a prewarm: the arm's own, then the host's facts for translation only."""
    merged = dict(overrides)
    if not pipelines:
        merged["VICTORY_HOST_WAVE"] = str(host_wave)
        merged["VICTORY_VIEW_LIMIT"] = str(view_limit)
    return merged


def launcher_extra(pipelines: bool, jobs: int) -> list[str]:
    """The launcher switches that make it warm and stop before the title."""
    extra = ["-Precompile", "-NoGame"]
    if jobs > 0:
        extra += ["-WarmJobs", str(jobs)]
    if not pipelines:
        extra.append("-WarmNoPipelines")
    return extra


def parse_summary(text: str) -> dict[str, int]:
    """The warm-up's counts from its output (zeros when it did not finish)."""
    out = {
        "shaders": 0,
        "refused": 0,
        "pipelines": 0,
        "pipelines_refused": 0,
        "warm_s": 0,
        "hits": 0,
        "translated": 0,
    }
    m = WARM_RE.search(text)
    if m:
        for key, value in zip(("shaders", "refused", "pipelines", "pipelines_refused", "warm_s"), m.groups()):
            out[key] = int(value)
    c = CACHE_RE.search(text)
    if c:
        out["hits"], out["translated"] = int(c.group(1)), int(c.group(2))
    return out


def device_made(text: str) -> bool:
    """Whether the warm-up made a device on the card (it logs the host's wave lanes when it does)."""
    return WAVE_RE.search(text) is not None


def needed_files(game_dir: Path) -> list[Path]:
    """What -Precompile reads; all must be there."""
    return [
        game_dir / "shaders.rpf",
        game_dir / "game_scarlett_master.exe",
        game_dir / "XboxBvhBuilder.dll",
        game_dir / "amd_fidelityfx_upscaler_xbox.dll",
    ]
