"""`refused` counts only refusals a draw, dispatch or pipeline needed; shaders VICTORY_TRANSLATE_AHEAD translated
ahead that do not translate are `refused_ahead` (victory-gpu `pipeline/translate_ahead.rs`)."""

import importlib.util
import sys
from pathlib import Path

TOOLS = Path(__file__).resolve().parents[1] / "tools"
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))

import dev_stats  # noqa: E402

TS = "2026-11-27T15:01:02.123456Z"
# The lines as victory-gpu `pipeline.rs` / `translate_ahead.rs` write them (without colour codes).
AHEAD = (
    f"{TS}  INFO victory_gpu::pipeline::translate_ahead: shader translated ahead is not translatable; counted as "
    'refused only once a draw or dispatch needs it (VICTORY_TRANSLATE_AHEAD) stage="compute" hash=0e64f14fae060d04 '
    'name="CS_TraceShadowRays_Quarter_2d98e5c6_Wrapped" error=converter shader.dxil v1: GFX10 code refused_ahead=1'
)
NEEDED = (
    f"{TS}  WARN victory_gpu::pipeline: shader does not translate; its pipelines' draws are dropped "
    'stage="compute" hash=0e64f14fae060d04 name="CS_TraceShadowRays_Quarter_2d98e5c6_Wrapped" '
    "error=does not translate (refused ahead)"
)
PIPELINE = f"{TS}  WARN victory_gpu::pipeline::host: host pipeline refused shaders=[vertex 01, pixel 02]"
INFO_OTHER = f"{TS}  INFO victory_gpu::pipeline: shader translated stage=\"pixel\" hash=1 user_data=0 samplers=0"


def load_dev():
    spec = importlib.util.spec_from_file_location("el1xr_dev_refused", TOOLS / "dev.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_refusal_kinds():
    assert dev_stats.refusal(AHEAD) == "ahead"
    assert dev_stats.refusal(NEEDED) == "refused"
    assert dev_stats.refusal(PIPELINE) == "refused"
    assert dev_stats.refusal(INFO_OTHER) is None
    # Only WARN/ERROR lines are refusals.
    assert dev_stats.refusal(f"{TS}  INFO victory_gpu::pipeline: does not translate") is None


def test_bench_counts_refused_ahead_apart():
    dev = load_dev()
    ahead_only = dev.parse_victory_log("\n".join([AHEAD] * 7 + [INFO_OTHER]))
    assert (ahead_only["refused"], ahead_only["refused_ahead"]) == (0, 7)
    needed = dev.parse_victory_log("\n".join([AHEAD, NEEDED, PIPELINE]))
    assert (needed["refused"], needed["refused_ahead"]) == (2, 1)
    assert dev.parse_victory_log("")["refused_ahead"] == 0


def test_bench_line_shows_refused_ahead_after_refused():
    dev = load_dev()
    line = dev.bench_fields(dev.parse_victory_log("\n".join([AHEAD] * 7)))
    assert " refused=0 refused_ahead=7 " in f" {line} "
