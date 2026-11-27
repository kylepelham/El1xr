"""tools/dev_train.py (batch plans, one run per call, the report) and the world-phase figures of
tools/dev_stats.py that `dev.py bench` and `dev.py train` print."""

import importlib.util
import json
import struct
import sys
import zlib
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest

TOOLS = Path(__file__).resolve().parents[1] / "tools"
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))

import dev_png
import dev_stats
import dev_train

PLAN_TOML = """
seconds = 60
quiet = true
snap_every = 10
baseline = "off"

[common]
VICTORY_THREAD_TICKET = 0

[sets.on]
VICTORY_FOO = 1
VICTORY_BAR = "fast"
VICTORY_BAZ = true

[sets.off]
VICTORY_FOO = 0

[sets.on_minus_foo]
from = "on"
VICTORY_FOO = 0
"""


def load_dev():
    spec = importlib.util.spec_from_file_location("el1xr_dev_train", TOOLS / "dev.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def plan_file(tmp_path, text=PLAN_TOML, name="batch7.toml"):
    path = tmp_path / name
    path.write_text(text, encoding="utf-8")
    return path


def test_a_toml_plan_resolves_sets_through_common_and_from(tmp_path):
    plan = dev_train.load_plan(plan_file(tmp_path))
    assert plan.name == "batch7"
    assert plan.order == ["on", "off", "off", "on"], "ABBA of on and off by default"
    assert (plan.seconds, plan.quiet, plan.snap_every, plan.baseline) == (60, True, 10.0, "off")
    assert plan.sets["on"] == {
        "VICTORY_THREAD_TICKET": "0", "VICTORY_FOO": "1", "VICTORY_BAR": "fast", "VICTORY_BAZ": "1",
    }
    assert plan.sets["off"] == {"VICTORY_THREAD_TICKET": "0", "VICTORY_FOO": "0"}
    assert plan.sets["on_minus_foo"] == {**plan.sets["on"], "VICTORY_FOO": "0"}


def test_a_json_plan_with_its_own_order_and_default_bench_options(tmp_path):
    data = {"order": ["a", "b"], "sets": {"a": {"VICTORY_X": 2}, "b": {}}}
    plan = dev_train.load_plan(plan_file(tmp_path, json.dumps(data), "p.json"))
    assert plan.order == ["a", "b"]
    assert plan.baseline == "a", "no set named off: the first of the order"
    assert (plan.seconds, plan.snap_every, plan.fast_boot, plan.quiet) == (200, 15.0, True, False)
    assert plan.sets == {"a": {"VICTORY_X": "2"}, "b": {}}


@pytest.mark.parametrize(
    ("data", "message"),
    [
        ({"sets": {}}, "no sets"),
        ({"sets": {"on": {}, "off": {}}, "rounds": 3}, "unknown plan keys: rounds"),
        ({"sets": {"on": {"FOO": 1}, "off": {}}}, "not a VICTORY_"),
        ({"sets": {"on": {"from": "x"}, "off": {}}}, "names no set 'x'"),
        ({"sets": {"on": {"from": "off"}, "off": {"from": "on"}}}, "loops"),
        ({"sets": {"on": {}}}, "lacks: off"),
        ({"sets": {"on": {}, "off": {}}, "baseline": "x"}, "baseline"),
        ({"sets": {"on": {"VICTORY_X": [1]}, "off": {}}}, "string, number or boolean"),
        ({"sets": {"../on": {}, "off": {}}, "order": ["off"]}, "usable name"),
    ],
)
def test_plan_mistakes_are_refused_by_name(data, message):
    with pytest.raises(dev_train.PlanError, match=message):
        dev_train.parse_plan("p", data)


def test_pick_runs_the_order_then_extra_runs_of_a_set():
    plan = dev_train.parse_plan("p", {"sets": {"on": {}, "off": {}, "on_minus_x": {}}})
    assert dev_train.pick(plan, {}, "next") == (1, "on")
    done = {1: {"set": "on"}, 2: {"set": "off"}}
    assert dev_train.pick(plan, done, "next") == (3, "off")
    assert dev_train.pick(plan, done, "on") == (4, "on"), "a set's first pending step of the order"
    assert dev_train.pick(plan, done, "2") == (2, "off"), "a step again"
    assert dev_train.pick(plan, done, "on_minus_x") == (5, "on_minus_x"), "an extra run after the order"
    done[5] = {"set": "on_minus_x"}
    assert dev_train.pick(plan, done, "on_minus_x") == (6, "on_minus_x")
    assert dev_train.pick(plan, done, "5") == (5, "on_minus_x")
    full = {i: {"set": s} for i, s in enumerate(plan.order, 1)}
    with pytest.raises(dev_train.PlanError, match="every step"):
        dev_train.pick(plan, full, "next")
    with pytest.raises(dev_train.PlanError, match="no set"):
        dev_train.pick(plan, {}, "nope")
    with pytest.raises(dev_train.PlanError, match="not in the order"):
        dev_train.pick(plan, {}, "9")


def stats(fps, steady_p99, over250, boot_s=40.0, worst=300.0, worst_frame=900):
    return {
        "fps": fps, "prep_ms": 25.0, "p50_ms": 40.0, "p99_ms": 70.0, "steady_p99_ms": steady_p99,
        "world_over100": 10, "world_over250": over250, "worst_ms": worst, "worst_frame": worst_frame,
        "boot_s": boot_s, "settled_s": 90.0, "stream_s": 50.0, "refused": 0,
    }


def record(step, name, fps, steady_p99, over250, ok=True, before=1, after=1):
    return {
        "step": step, "set": name, "ok": ok, "stats": stats(fps, steady_p99, over250), "snaps": 4,
        "resets_before": before, "resets_after": after,
    }


def test_results_round_trip_and_the_report_has_runs_pending_medians_and_deltas(tmp_path):
    plan = dev_train.parse_plan("p", {"sets": {"on": {}, "off": {}}})
    dev_train.save_result(tmp_path, 1, record(1, "on", 26.0, 60.0, 2))
    dev_train.save_result(tmp_path, 2, record(2, "off", 24.0, 70.0, 5, after=2))
    dev_train.save_result(tmp_path, 3, record(3, "off", 25.0, 72.0, 7))
    dev_train.save_result(tmp_path, 3, record(3, "off", 22.0, 74.0, 9))  # step 3 run again
    (tmp_path / "report.json").write_text("{}", encoding="utf-8")
    done = dev_train.load_results(tmp_path)
    assert sorted(done) == [1, 2, 3]
    assert done[3]["stats"]["fps"] == 22.0
    assert dev_train.next_pending(plan, done) == "04_on"
    text, figures = dev_train.report(plan, done)
    assert "| 01_on | on | 26.0 | 25.0 | 40.0 | 70.0 | 60.0 | 10 | 2 | 300 @900 |" in text
    assert "| 02_off | off |" in text and "2 (+1!)" in text, "a reset during the run is flagged"
    assert "| 04_on | on | pending |" in text
    assert figures["medians"]["off"]["fps"] == 23.0
    assert figures["deltas"]["on"]["fps"] == pytest.approx(3.0)
    assert figures["deltas"]["on"]["world_over250"] == pytest.approx(-5.0)
    assert "| on | 1 | 26.0 (+3.0) |" in text


def test_failed_runs_stay_out_of_the_medians():
    plan = dev_train.parse_plan("p", {"sets": {"on": {}, "off": {}}})
    done = {1: record(1, "on", 26.0, 60.0, 2), 2: record(2, "on", 2.0, 900.0, 50, ok=False)}
    _, figures = dev_train.report(plan, done)
    assert figures["medians"]["on"]["fps"] == 26.0
    assert figures["deltas"] == {}, "no baseline run yet"


T0 = datetime(2026, 11, 27, 12, 0, tzinfo=UTC)


def window(s, p50, p99, mx, over100, draws=1500):
    return dev_stats.Window(T0 + timedelta(seconds=s), p50, p99, mx, over100, draws)


def test_the_streaming_window_ends_after_the_last_world_window_with_a_burst_of_hitches():
    windows = [
        window(5, 300.0, 2000.0, 2000.0, 3, draws=7),  # menu / loading screen
        window(10, 150.0, 8000.0, 8700.0, 2),  # world entry
        window(15, 45.0, 120.0, 300.0, 1),
        window(20, 79.0, 450.0, 450.0, 14),  # streaming burst at a playable median
        window(25, 42.0, 80.0, 110.0, 1),
        window(30, 41.0, 70.0, 90.0, 0),
        window(35, 40.0, 60.0, 95.0, 0),
    ]
    hitches = [
        (T0 + timedelta(seconds=3), 100, 2000.0),  # before the world: not counted
        (T0 + timedelta(seconds=9), 840, 8723.6),
        (T0 + timedelta(seconds=14), 900, 260.0),
        (T0 + timedelta(seconds=19), 950, 450.0),
        (T0 + timedelta(seconds=24), 990, 110.0),
    ]
    out = dev_stats.world_phase(windows, hitches)
    assert out["stream_s"] == 15.0, "from the end of the menu window to the end of the burst"
    assert out["steady_windows"] == 3
    assert out["steady_p99_ms"] == 70.0
    assert out["steady_max_ms"] == 110.0
    assert out["steady_over100"] == 1
    assert out["world_over100"] == 18
    assert out["world_over250"] == 3
    assert (out["worst_ms"], out["worst_frame"]) == (8723.6, 840)


def test_a_world_that_never_streams_is_all_steady_and_no_world_is_all_zero():
    out = dev_stats.world_phase([window(5, 40.0, 60.0, 70.0, 0), window(10, 41.0, 62.0, 99.0, 0)], [])
    assert (out["stream_s"], out["steady_windows"], out["steady_p99_ms"]) == (0.0, 2, 61.0)
    assert (out["worst_ms"], out["worst_frame"]) == (99.0, 0), "no hitch line: the worst window's max"
    empty = dev_stats.world_phase([window(5, 40.0, 60.0, 70.0, 0, draws=5)], [])
    assert set(empty.values()) == {0}
    assert dev_stats.parse_hitch_line("hitch: frame 316 took 224.0 ms; presenting thread") == (316, 224.0)


def test_the_bench_log_parser_adds_the_world_phase_figures():
    dev = load_dev()
    pacing = (
        "2026-11-27T12:00:{s}.000000Z  INFO victory_gpu::pacing: frame pacing: 100 frames in 5.0 s (20.0 fps), "
        "frame time p50 {p50} ms p95 60.0 ms p99 {p99} ms max {mx} ms, 10 over 50 ms, {over} over 100 ms, "
        "present interval sd 9.0 ms jitter 4.0 ms; {draws} draws/frame"
    )
    log = "\n".join((
        pacing.format(s="05", p50=400.0, p99=500.0, mx=500.0, over=10, draws=6),
        "2026-11-27T12:00:08.000000Z  INFO victory_gpu::hitch: hitch: frame 50 took 600.0 ms; presenting thread",
        pacing.format(s="10", p50=45.0, p99=600.0, mx=600.0, over=4, draws=1500),
        pacing.format(s="15", p50=40.0, p99=65.0, mx=80.0, over=0, draws=1500),
    ))
    s = dev.parse_victory_log(log)
    assert (s["stream_s"], s["steady_p99_ms"], s["world_over250"], s["worst_frame"]) == (5.0, 65.0, 1, 50)
    line = dev.bench_fields(s)
    assert line.startswith("boot_s=")
    assert "steady_p99_ms=65.0 stream_s=5.0 world_over100=4 world_over250=1 worst_ms=600.0 worst_frame=50" in line
    assert dev.bench_fields(dev.parse_victory_log("")).endswith("worst_ms=0.0 worst_frame=0")


def test_snaps_set_the_switch_and_clear_an_earlier_runs_pictures(tmp_path):
    dev = load_dev()
    snap_dir = tmp_path / "snaps"
    assert dev.apply_snaps({"VICTORY_A": "1"}, 0.0, snap_dir) == {"VICTORY_A": "1"}
    assert not snap_dir.exists()
    snap_dir.mkdir()
    (snap_dir / "snap-0015s-f400.png").write_bytes(b"old")
    (snap_dir / "keep.txt").write_text("x", encoding="utf-8")
    out = dev.apply_snaps({"VICTORY_A": "1"}, 15.0, snap_dir)
    assert out == {"VICTORY_SNAP_EVERY_S": "15", "VICTORY_SNAP_DIR": str(snap_dir), "VICTORY_A": "1"}
    assert sorted(p.name for p in snap_dir.iterdir()) == ["keep.txt"]
    assert dev.apply_snaps({"VICTORY_SNAP_EVERY_S": "5"}, 2.5, snap_dir)["VICTORY_SNAP_EVERY_S"] == "5"
    args = dev.build_parser().parse_args(["bench", "--snap-every", "15"])
    assert args.snap_every == 15.0
    assert dev.build_parser().parse_args(["bench"]).snap_every == 0.0


def test_train_runs_one_step_per_call_and_reports(tmp_path, monkeypatch, capsys):
    dev = load_dev()
    monkeypatch.setattr(dev, "DEV_DIR", tmp_path / "dev")
    calls = []

    def fake_bench(args, overrides, log_path, snap_dir, what):
        assert what == "train"
        calls.append((overrides, log_path.name, snap_dir.name, args.seconds, args.snap_every, args.quiet))
        fps = 26.0 if overrides.get("VICTORY_FOO") == "1" else 24.0
        s = dev.parse_victory_log("")
        s.update(world_samples=10, fps=fps)
        return s, 3, 3, 4

    monkeypatch.setattr(dev, "run_bench", fake_bench)
    plan = plan_file(tmp_path)
    assert dev.main(["train", "--plan", str(plan)]) == 0
    assert dev.main(["train", "--plan", str(plan), "--run", "next"]) == 0
    assert dev.main(["train", "--plan", str(plan), "--run", "on_minus_foo"]) == 0
    assert [c[1:3] for c in calls] == [
        ("01_on.log", "01_on_snaps"), ("02_off.log", "02_off_snaps"), ("05_on_minus_foo.log", "05_on_minus_foo_snaps"),
    ]
    assert calls[0][0]["VICTORY_FOO"] == "1" and calls[1][0]["VICTORY_FOO"] == "0"
    assert calls[0][3:] == (60, 10.0, True)
    out = capsys.readouterr().out
    assert "DEV train PASS run=01_on boot_s=" in out and "snaps=4 next=02_off" in out
    assert "next=03_off" in out
    assert dev.main(["train", "--plan", str(plan), "--report"]) == 0
    out = capsys.readouterr().out
    assert "| 03_off | off | pending |" in out
    assert "| on | 1 | 26.0 (+2.0) |" in out
    assert "DEV train-report PASS runs=3 next=03_off" in out
    train_dir = tmp_path / "dev" / "train_batch7"
    assert json.loads((train_dir / "report.json").read_text(encoding="utf-8"))["deltas"]["on"]["fps"] == 2.0
    assert dev.main(["train", "--plan", str(plan), "--list"]) == 0
    assert "05_on_minus_foo done: VICTORY_THREAD_TICKET=0 VICTORY_FOO=0" in capsys.readouterr().out


def test_train_refuses_a_bad_plan_without_running(tmp_path, monkeypatch, capsys):
    dev = load_dev()
    monkeypatch.setattr(dev, "DEV_DIR", tmp_path / "dev")
    monkeypatch.setattr(dev, "run_bench", lambda *a: pytest.fail("ran"))
    bad = plan_file(tmp_path, '[sets.on]\nFOO = 1\n[sets.off]\n', "bad.toml")
    assert dev.main(["train", "--plan", str(bad)]) == 1
    assert "DEV train FAIL reason=plan" in capsys.readouterr().out
    assert dev.main(["train", "--plan", str(plan_file(tmp_path)), "--run", "nope"]) == 1
    assert "DEV train FAIL reason=run" in capsys.readouterr().out


def png_with_every_filter(path, width, height, rgb_rows):
    """A PNG whose rows cycle through all five filters (as screenshot.rs's adaptive encoder may)."""
    def paeth(a, b, c):
        p = a + b - c
        pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
        return a if pa <= pb and pa <= pc else b if pb <= pc else c

    raw, prev = b"", bytes(width * 3)
    for y, row in enumerate(rgb_rows):
        kind = y % 5
        out = bytearray()
        for i, x in enumerate(row):
            a = row[i - 3] if i >= 3 else 0
            c = prev[i - 3] if i >= 3 else 0
            pred = [0, a, prev[i], (a + prev[i]) >> 1, paeth(a, prev[i], c)][kind]
            out.append((x - pred) & 255)
        raw += bytes([kind]) + bytes(out)
        prev = row

    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))

    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    path.write_bytes(dev_png.SIGNATURE + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))


def gradient(width, height, seed):
    return [bytes((seed * 31 + x * 7 + y * 13 + (x * y) % 251) & 255 for x in range(width * 3)) for y in range(height)]


def test_pngs_of_every_row_filter_read_back_and_side_by_side_pads_the_shorter(tmp_path):
    rows = gradient(6, 10, 1)
    png_with_every_filter(tmp_path / "a.png", 6, 10, rows)
    a = dev_png.read_png(tmp_path / "a.png")
    assert (a.width, a.height, a.rows) == (6, 10, rows)
    b = dev_png.Image(2, 3, [b"\x01" * 6] * 3)
    both = dev_png.side_by_side(a, b, gap=1)
    assert (both.width, both.height) == (9, 10)
    assert both.rows[0] == rows[0] + b"\xff\x00\xff" + b"\x01" * 6
    assert both.rows[9] == rows[9] + b"\xff\x00\xff" + bytes(6), "the shorter one padded with black"
    dev_png.write_png(tmp_path / "both.png", both)
    again = dev_png.read_png(tmp_path / "both.png")
    assert (again.width, again.height, again.rows) == (9, 10, both.rows)


def test_snaps_pair_with_the_nearest_in_time_and_runs_are_found_by_step_id_or_set(tmp_path):
    for name in ["snap-0015s-f400.png", "snap-0030s-f800.png", "snap-0002s-f9.png", "other.png"]:
        (tmp_path / name).write_bytes(b"")
    found = dev_train.snaps(tmp_path)
    assert [t for t, _ in found] == [2, 15, 30]
    right = [(14, "r14"), (16, "r16"), (45, "r45")]
    assert [(t, u) for t, _, u, _ in dev_train.pair_snaps(found, right)] == [(2, 14), (15, 14), (30, 16)]
    assert dev_train.pair_snaps(found, []) == []
    plan = dev_train.parse_plan("p", {"sets": {"on": {}, "off": {}}})
    done = {1: record(1, "on", 26.0, 60.0, 2), 2: record(2, "off", 24.0, 70.0, 5),
            4: record(4, "on", 20.0, 60.0, 2, ok=False)}
    assert dev_train.find_run(plan, done, "2") == 2
    assert dev_train.find_run(plan, done, "04_on") == 4
    assert dev_train.find_run(plan, done, "on") == 1, "the latest run of the set that passed"
    with pytest.raises(dev_train.PlanError, match="no finished run '3'"):
        dev_train.find_run(plan, done, "3")


def test_train_compare_writes_one_side_by_side_png_per_snap_of_run_a(tmp_path, monkeypatch, capsys):
    dev = load_dev()
    monkeypatch.setattr(dev, "DEV_DIR", tmp_path / "dev")
    plan = plan_file(tmp_path)
    train_dir = tmp_path / "dev" / "train_batch7"
    train_dir.mkdir(parents=True)
    dev_train.save_result(train_dir, 1, record(1, "on", 26.0, 60.0, 2))
    dev_train.save_result(train_dir, 2, record(2, "off", 24.0, 70.0, 5))
    for rid, times, seed in [("01_on", [15, 30], 1), ("02_off", [16, 31, 46], 2)]:
        folder = train_dir / f"{rid}_snaps"
        folder.mkdir()
        for t in times:
            png_with_every_filter(folder / f"snap-{t:04d}s-f{t * 30}.png", 4, 3, gradient(4, 3, seed + t))
    assert dev.main(["train", "--plan", str(plan), "--compare", "on", "off"]) == 0
    out_dir = train_dir / "compare_01_on_vs_02_off"
    assert "DEV train-compare PASS left=01_on right=02_off pairs=2" in capsys.readouterr().out
    assert sorted(p.name for p in out_dir.iterdir()) == ["cmp-0015s-0016s.png", "cmp-0030s-0031s.png"]
    image = dev_png.read_png(out_dir / "cmp-0030s-0031s.png")
    assert (image.width, image.height) == (16, 3)
    assert image.rows[1] == gradient(4, 3, 31)[1] + b"\xff\x00\xff" * 8 + gradient(4, 3, 33)[1]
    assert dev.main(["train", "--plan", str(plan), "--compare", "1", "9"]) == 1
    assert "DEV train-compare FAIL reason=run" in capsys.readouterr().out


def test_a_train_run_holds_the_gpu_lock_and_quiets_builds_as_train(tmp_path, monkeypatch, capsys):
    """`train` goes through the same `_quiet` as `bench` (all slots with `quiet = true`, else the
    free slots and idle builds), tagged `train`, inside the gpu.lock."""
    import contextlib

    dev = load_dev()
    monkeypatch.setattr(dev, "DEV_DIR", tmp_path / "dev")
    game = tmp_path / "game"
    game.mkdir()
    events = []

    @contextlib.contextmanager
    def fake_quiet(args, what):
        events.append(("quiet", what, args.quiet))
        yield
        events.append(("quiet done", what))

    @contextlib.contextmanager
    def fake_gpu_lock(tag, timeout_s=0.0):
        events.append(("gpu", tag))
        yield

    def fake_run(cmd, log_path, seconds):
        events.append(("run", seconds))
        log_path.write_text("", encoding="utf-8")

    monkeypatch.setattr(dev, "_quiet", fake_quiet)
    monkeypatch.setattr(dev, "acquire_gpu_lock", fake_gpu_lock)
    monkeypatch.setattr(dev, "run_timed_victory", fake_run)
    monkeypatch.setattr(dev, "resolve_game_dir", lambda explicit: game)
    monkeypatch.setattr(dev, "build_launcher_command", lambda **kw: ["victory"])
    monkeypatch.setattr(dev, "resolve_save_dir", lambda *a, **kw: tmp_path)
    monkeypatch.setattr(dev, "isolated_save_data_dir", lambda save, what: contextlib.nullcontext(tmp_path))
    monkeypatch.setattr(dev.dev_driver, "resets_since_boot", lambda: 0)
    dev.main(["train", "--plan", str(plan_file(tmp_path))])
    assert events == [("gpu", "train"), ("quiet", "train", True), ("run", 60), ("quiet done", "train")]
    assert "DEV train FAIL run=01_on" in capsys.readouterr().out, "an empty log has no world"
