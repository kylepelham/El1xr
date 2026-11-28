"""tools/dev_early.py: early stop of a settled, steady train arm, the fatal-line watch, and the log tail
`dev.py run_timed_victory` feeds it."""

import sys
import time
from pathlib import Path

TOOLS = Path(__file__).resolve().parents[1] / "tools"
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))

import dev_early
import dev_train

PACING = (
    "2026-11-27T12:00:00.000000Z  INFO victory_gpu::pacing: frame pacing: {frames} frames in 5.0 s ({fps} fps), "
    "frame time p50 {p50} ms p95 60.0 ms p99 70.0 ms max 90.0 ms, 0 over 50 ms, {over} over 100 ms, "
    "present interval sd 9.0 ms jitter 4.0 ms; {draws} draws/frame"
)
FRAME = (
    "2026-11-27T12:00:00.000000Z  INFO victory_gpu::host: {fps} fps, 40.0 ms/frame ({prep} preparing: copies 1.0 ms "
    "2.0 MB, textures 1.0 ms 1.0 MB; 10.0 on the GPU; 1.0 copying back 1.0 MB), 1500.0 draws/frame, dropped 0.0 "
    "without a host pipeline (0 being made) and 0.0 indirect"
)


def window(fps=25.0, prep=24.0, p50=40.0, over=0, draws=1500):
    return FRAME.format(fps=fps, prep=prep) + "\n" + PACING.format(
        frames=int(fps * 5), fps=fps, p50=p50, over=over, draws=draws
    ) + "\n"


def boot():
    """Menu windows, world entry (loading), then a streaming burst."""
    return window(draws=6, p50=300.0) + window(p50=150.0) + window(p50=45.0, over=9)


def test_a_steady_settled_world_stops_early_but_never_before_min_seconds():
    m = dev_early.Monitor(early=True, min_s=90, steady_s=60)
    text = boot() + "".join(window(fps=25.0 + 0.1 * (i % 3), prep=24.0 + 0.2 * (i % 2)) for i in range(12))
    assert m.feed(text, 60.0) is None, "steady for 60 s, but before min_seconds"
    assert m.feed("", 95.0) == "early"
    assert m.ended["how"] == "early" and m.ended["windows"] == 12 and m.ended["at_s"] == 95.0
    assert m.ended["fps_hw"] <= dev_early.FPS_TOL and m.ended["prep_hw"] <= dev_early.PREP_TOL
    assert m.summary().startswith("early stop at 95s (fps 25.1 +-")


def test_streaming_resets_the_steady_run_and_noisy_or_short_runs_do_not_stop():
    m = dev_early.Monitor(early=True, min_s=0, steady_s=60)
    assert m.feed(boot() + "".join(window() for _ in range(11)), 100.0) is None, "55 s of steady windows"
    assert m.feed(window(over=4), 105.0) is None, "a burst of hitches: streaming again"
    assert m.feed("".join(window() for _ in range(11)), 160.0) is None
    assert m.feed(window(), 165.0) == "early"
    noisy = dev_early.Monitor(early=True, min_s=0, steady_s=60)
    assert noisy.feed(boot() + "".join(window(fps=20.0 + 8 * (i % 2)) for i in range(20)), 200.0) is None
    off = dev_early.Monitor(early=False, min_s=0, steady_s=60)
    assert off.feed(boot() + "".join(window() for _ in range(20)), 200.0) is None
    assert off.ended is None


def test_fatal_lines_end_the_run_after_a_grace_for_the_reports_that_follow():
    m = dev_early.Monitor(early=False)
    assert m.feed("2026-11-27T12:00:01Z ERROR victory_gpu: DXGI_ERROR_DEVICE_HUNG on present\n", 50.0) is None
    assert m.feed("2026-11-27T12:00:02Z ERROR victory_gpu::dred: dred: last op\n", 55.0) is None
    assert m.feed("", 50.0 + dev_early.FATAL_GRACE_S) == "fatal"
    assert m.ended == {"how": "fatal", "why": "DEVICE_HUNG", "at_s": 60.0}
    stall = "ERROR victory_gpu::hang_watch: hang watch: the card's fence has stood at 41 for {s} s with work queued up to 44"
    assert dev_early.fatal_line(stall.format(s="3.0")) is None
    assert dev_early.fatal_line(stall.format(s="9.4")) == "hang watch stall 9.4 s"
    moved = "WARN victory_gpu::hang_watch: hang watch: the card moved on fence=44 stood_s=12.5"
    assert dev_early.stall_seconds(moved) == 12.5
    assert dev_early.fatal_line("WARN victory_gpu: device removed: reason 887a0006") == "device removed"
    assert dev_early.fatal_line("INFO victory_gpu: device-removed data on (VICTORY_DRED=1)") is None


def test_the_log_tail_returns_whole_new_text_in_utf8_and_utf16(tmp_path):
    log = tmp_path / "a.log"
    log.write_bytes("fps é".encode()[:-1])
    tail = dev_early.LogTail(log)
    assert tail.read() == "", "no whole line yet (and half an é)"
    with log.open("ab") as fh:
        fh.write("é\nnext".encode()[1:])
    assert tail.read() == "fps é\n"
    wide = tmp_path / "w.log"
    wide.write_bytes(b"\xff\xfe" + "ab\n".encode("utf-16-le") + b"c")
    tail = dev_early.LogTail(wide)
    assert tail.read() == "ab\n"
    with wide.open("ab") as fh:
        fh.write(b"\x00")
    assert tail.read() == "c"
    assert dev_early.LogTail(tmp_path / "none.log").read() == ""


def test_early_stop_defaults_on_for_loud_arms_and_off_for_quiet_and_live_ones():
    plan = dev_train.parse_plan("p", {"sets": {"on": {"early_stop": False}, "off": {}, "x": {}}, "order": ["on", "off"]})
    assert dev_train.step_early_stop(plan, "off", quiet=False) is True
    assert dev_train.step_early_stop(plan, "off", quiet=True) is False
    assert dev_train.step_early_stop(plan, "on", quiet=False) is False, "the set's own key"
    forced = dev_train.parse_plan("p", {"sets": {"on": {}, "off": {}}, "early_stop": True, "min_seconds": 120})
    assert dev_train.step_early_stop(forced, "on", quiet=True) is True
    assert (forced.min_seconds, forced.steady_seconds) == (120, 60)
    assert plan.sets["on"] == {}, "early_stop is not a VICTORY_* value"


def test_the_timer_ends_a_run_when_its_monitor_says_so(tmp_path, monkeypatch):
    """`run_timed_victory` with a monitor, on a stand-in process (not the game) that logs a steady world and sleeps."""
    from test_dev_train import load_dev

    dev = load_dev()
    monkeypatch.setattr(dev, "MONITOR_POLL_SECS", 0.1)
    script = tmp_path / "fake.py"
    lines = "mapped at 0x0\n" + boot() + "".join(window() for _ in range(6))
    (tmp_path / "lines.txt").write_text(lines, encoding="utf-8")
    script.write_text(
        "import sys, time\n"
        f"sys.stdout.write(open(r'{tmp_path / 'lines.txt'}', encoding='utf-8').read())\n"
        "sys.stdout.flush()\ntime.sleep(60)\n",
        encoding="utf-8",
    )
    monitor = dev_early.Monitor(early=True, min_s=0, steady_s=20)
    t0 = time.monotonic()
    end = dev.run_timed_victory([sys.executable, str(script)], tmp_path / "run.log", 60, monitor)
    assert end.timed_out and monitor.ended["how"] == "early"
    assert time.monotonic() - t0 < 30
