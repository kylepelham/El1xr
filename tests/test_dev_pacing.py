"""tools/dev.py reads the `frame pacing:` line that victory-gpu's pacing.rs logs every 5 s."""

import importlib.util
from pathlib import Path

TOOLS = Path(__file__).resolve().parents[1] / "tools"

# The exact line `pacing::tests::line_has_the_shape_dev_py_reads` checks on the Rust side.
LINE = (
    "frame pacing: 4 frames in 5.0 s (0.8 fps), frame time p50 45.0 ms p95 110.0 ms "
    "p99 110.0 ms max 110.0 ms, 2 over 50 ms, 1 over 100 ms, present interval sd 28.2 ms "
    "jitter 23.3 ms; 4830 draws/frame"
)


def load_dev():
    spec = importlib.util.spec_from_file_location("dev", TOOLS / "dev.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def pacing_line(p50, p99, mx, over50, over100, sd, draws, jitter=4.0):
    return (
        f"2026-11-26T19:20:38.290900Z  INFO victory_gpu::pacing: frame pacing: 110 frames in 5.0 s "
        f"(22.0 fps), frame time p50 {p50} ms p95 50.0 ms p99 {p99} ms max {mx} ms, "
        f"{over50} over 50 ms, {over100} over 100 ms, present interval sd {sd} ms jitter {jitter} ms; "
        f"{draws} draws/frame"
    )


def test_parses_the_line_rust_writes():
    figures = load_dev().parse_pacing_line(LINE)
    assert figures == {
        "frames": 4.0, "p50": 45.0, "p95": 110.0, "p99": 110.0, "max": 110.0,
        "over50": 2.0, "over100": 1.0, "sd": 28.2, "jitter": 23.3, "draws": 4830.0,
    }
    assert load_dev().parse_pacing_line("30.0 fps, 33.3 ms/frame") is None


def test_bench_summary_uses_world_windows_once_loaded():
    dev = load_dev()
    lines = [
        pacing_line(33.3, 34.0, 400.0, 0, 3, 1.0, 6),  # menu: not counted
        pacing_line(50.0, 90.0, 150.0, 40, 2, 9.0, 4800),
        pacing_line(48.0, 80.0, 120.0, 30, 1, 8.0, 4800),
        pacing_line(43.0, 60.0, 95.0, 10, 0, 5.0, 4800, jitter=6.0),
        pacing_line(42.0, 62.0, 130.0, 12, 1, 6.0, 4900),
    ]
    stats = dev.parse_victory_log("\n".join(lines))
    assert stats["p50_ms"] == 45.5
    assert stats["p99_ms"] == 71.0
    assert stats["max_ms"] == 150.0
    assert stats["sd_ms"] == 7.0
    assert stats["jitter_ms"] == 4.0
    assert stats["over50"] == 92
    assert stats["hitches"] == 4


def test_bench_summary_skips_world_loading():
    dev = load_dev()
    lines = [
        pacing_line(33.3, 34.0, 400.0, 0, 3, 1.0, 6),
        pacing_line(45.0, 60.0, 80.0, 5, 0, 4.0, 4800),
        pacing_line(300.0, 900.0, 1200.0, 12, 12, 300.0, 2500),  # world streaming in
        pacing_line(120.0, 500.0, 600.0, 30, 25, 90.0, 4200),
        pacing_line(46.0, 70.0, 104.0, 30, 1, 7.0, 5000),
        pacing_line(47.0, 72.0, 90.0, 28, 0, 6.0, 4900),
    ]
    stats = dev.parse_victory_log("\n".join(lines))
    assert stats["p50_ms"] == 46.5
    assert stats["max_ms"] == 104.0
    assert (stats["over50"], stats["hitches"]) == (58, 1)


def test_no_pacing_lines_give_zeros():
    stats = load_dev().parse_victory_log("nothing here\n")
    assert (stats["p99_ms"], stats["max_ms"], stats["hitches"]) == (0.0, 0.0, 0)
