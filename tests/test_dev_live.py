"""tools/dev_live.py (live A/B arms), tools/dev_verdict.py (train --verdict), tools/dev_warm.py (warm stamps) and
their wiring in `dev.py train` / `dev.py prewarm`, on synthetic logs (no game, no build slot, no gpu lock)."""

import importlib.util
import json
import sys
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest

TOOLS = Path(__file__).resolve().parents[1] / "tools"
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))

import dev_live
import dev_train
import dev_verdict
import dev_warm

T0 = datetime(2026, 11, 27, 18, 0, 0, tzinfo=UTC)

LIVE_PLAN = """
seconds = 60
baseline = "off"
order = ["awake", "off", "on", "off"]

[sets.off]
VICTORY_FOO = 0
VICTORY_AWAKE = 0

[sets.on]
from = "off"
VICTORY_FOO = 1

[sets.awake]
from = "off"
VICTORY_AWAKE = 1

[live.awake_live]
a = "off"
b = "awake"
segment_s = 40
segments = 4
start_s = 100
quiet = true
"""


def load_dev():
    spec = importlib.util.spec_from_file_location("dev_under_test", TOOLS / "dev.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def stamp(t: float) -> str:
    return (T0 + timedelta(seconds=t)).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def frame_line(t, fps, prep, draws=5000):
    return (f"\x1b[2m{stamp(t)}\x1b[0m \x1b[32m INFO\x1b[0m victory_gpu::host: {fps:.1f} fps, {1000 / fps:.1f} ms/frame "
            f"({prep:.1f} preparing: copies 0.1 ms 0.1 MB, textures 0.0 ms 0.0 MB; 0.0 on the GPU; 0.0 copying back 0.0 MB), "
            f"{draws} draws/frame, dropped 0 without a host pipeline (0 being made) and 0 indirect")


def pacing_line(t, p50, p99, over100=0, draws=5000):
    return (f"{stamp(t)}  INFO victory_gpu::pacing: frame pacing: 150 frames in 5.0 s (30.0 fps), frame time p50 "
            f"{p50:.1f} ms p95 {p99 - 1:.1f} ms p99 {p99:.1f} ms max {p99:.1f} ms, 3 over 50 ms, {over100} over 100 ms, "
            f"present interval sd 2.0 ms jitter 1.0 ms; {draws} draws/frame")


def segment_line(t, n, name, value):
    return f"{stamp(t)}  INFO victory_core::settings: settings: segment {n} {name}={value} at {t:.1f}"


def hitch_line(t, ms):
    return f"{stamp(t)}  INFO victory_gpu::hitch: hitch: frame 9 took {ms:.1f} ms; presenting thread: submit 1.0 ms (1x)"


def live_log(fps_of, prep_of=lambda label, i: 10.0, p99_of=lambda label, i: 40.0, labels="ABAB", start=100.0,
             seg=40.0, settle=60.0, hitches=()):
    """A run: loading until `settle` (p50 over 100 ms), then world; a segment per label from `start`; a frame line
    and a pacing window every 5 s. The first 5 s after each change run at 5 fps / p99 900 (the change's own
    hitch), which the analysis must leave out."""
    lines = []
    changes = {start + i * seg: (i + 1, label) for i, label in enumerate(labels)}
    end = start + len(labels) * seg + 5.0
    label, idx, since = "A", -1, 0.0
    t = 5.0
    while t <= end:
        for at, (n, lab) in changes.items():
            if t - 5.0 <= at < t:
                lines.append(segment_line(at, n, "VICTORY_AWAKE", "0" if lab == "A" else "1"))
                label, idx, since = lab, n - 1, at
        if t <= settle:
            lines.append(frame_line(t, 3.0, 50.0, draws=5000))
            lines.append(pacing_line(t, 300.0, 900.0, 10))
        elif idx >= 0 and t - since <= 5.0:
            lines.append(frame_line(t, 5.0, 80.0))
            lines.append(pacing_line(t, 33.0, 900.0, 4))
        else:
            lines.append(frame_line(t, fps_of(label, idx), prep_of(label, idx)))
            lines.append(pacing_line(t, 1000.0 / fps_of(label, idx), p99_of(label, idx)))
        t += 5.0
    for at, ms in hitches:
        lines.append(hitch_line(at, ms))
    return "\n".join(lines) + "\n"


def test_a_live_arm_launches_with_a_and_schedules_every_differing_key():
    plan = dev_train.parse_plan("p", __import__("tomllib").loads(LIVE_PLAN.replace('"awake", "off"', '"awake_live", "off"')))
    arm = plan.live["awake_live"]
    assert arm.keys == {"VICTORY_AWAKE": ("0", "1")}
    assert arm.launch["VICTORY_AWAKE"] == "0" and arm.labels() == ["A", "B", "A", "B"]
    assert arm.seconds == 100 + 4 * 40 + 5
    text = dev_live.schedule_text(arm)
    assert [ln for ln in text.splitlines() if ln.startswith("at ")] == [
        "at 100s set VICTORY_AWAKE=0", "at 140s set VICTORY_AWAKE=1",
        "at 180s set VICTORY_AWAKE=0", "at 220s set VICTORY_AWAKE=1",
    ]
    assert dev_train.step_overrides(plan, "awake_live")["VICTORY_FOO"] == "0"
    assert dev_train.step_quiet(plan, "awake_live") and not dev_train.step_quiet(plan, "off")
    assert dev_train.ttl_minutes(plan, "off") == 20
    assert "01_awake_live pending [265 s, quiet, gpu lock ttl>=20 min]: live off vs awake ABAB" in dev_train.listing(plan, {})


@pytest.mark.parametrize(
    ("live", "kinds", "message"),
    [
        ('[live.x]\na = "off"\nb = "nope"\n', None, "'nope' is not a set"),
        ('[live.x]\na = "off"\nb = "off"\n', None, "do not differ"),
        ('[live.x]\na = "off"\nb = "on"\n', {"VICTORY_FOO": "Retranslate"}, "Retranslate, not Live"),
        ('[live.x]\na = "off"\nb = "on"\nsegment_s = 12\n', None, "longer than drop_s"),
        ('[live.x]\na = "off"\nb = "on"\npattern = "AA"\n', None, "both"),
        ('[live.x]\na = "off"\nb = "on"\nwhat = 1\n', None, "unknown keys what"),
        ('[live.on]\na = "off"\nb = "awake"\n', None, "a set has that name"),
    ],
)
def test_live_arm_mistakes_are_refused_by_name(live, kinds, message):
    base = LIVE_PLAN.split("[live.awake_live]")[0]
    with pytest.raises(dev_train.PlanError, match=message):
        dev_train.parse_plan("p", __import__("tomllib").loads(base + live), kinds)


def test_a_key_only_one_set_gives_cannot_be_scheduled():
    text = LIVE_PLAN.split("[live.awake_live]")[0] + '[sets.bare]\nVICTORY_FOO = 0\n[live.x]\na = "bare"\nb = "awake"\n'
    with pytest.raises(dev_train.PlanError, match="only one set gives it"):
        dev_train.parse_plan("p", __import__("tomllib").loads(text))


def test_registry_live_settings_are_accepted():
    plan = dev_train.parse_plan("p", __import__("tomllib").loads(LIVE_PLAN), {"VICTORY_AWAKE": "Live"})
    assert "awake_live" in plan.live


def arm_of(**kw):
    plan = dev_train.parse_plan("p", __import__("tomllib").loads(LIVE_PLAN))
    arm = plan.live["awake_live"]
    for k, v in kw.items():
        setattr(arm, k, v)
    return arm


def test_frames_go_to_their_segment_less_the_drop_and_medians_are_per_set():
    fps = {"A": [30.0, 0, 31.0, 0], "B": [0, 35.0, 0, 36.0]}
    text = live_log(lambda label, i: fps[label][i] if i >= 0 else 30.0,
                    prep_of=lambda label, i: 8.0 if label == "A" else 6.0,
                    hitches=[(150.0, 300.0), (230.0, 400.0), (260.0, 120.0)])
    result = dev_live.analyse(arm_of(), text)
    assert result["problems"] == []
    assert [s["label"] for s in result["segments"]] == ["A", "B", "A", "B"]
    a, b = result["sets"]["A"], result["sets"]["B"]
    assert a["segments"] == 2 and b["segments"] == 2
    assert a["fps"] == 30.5 and b["fps"] == 35.5, "the 5 fps frames right after each change are left out"
    assert a["prep_ms"] == 8.0 and b["prep_ms"] == 6.0
    assert a["p99_ms"] == 40.0 and b["p99_ms"] == 40.0, "p99 900 windows after the change are left out"
    assert result["delta"]["fps"] == 5.0
    assert b["over250_min"] > 0 and a["over250_min"] == 0
    assert "fps" not in result["noisy"]
    lines = "\n".join(dev_live.report_lines("01_awake_live", result))
    assert "| A off | 2 | 30.5 |" in lines and "| B awake | 2 | 35.5 |" in lines and "| B-A | | +5.0 |" in lines


def test_a_vs_a_spread_above_a_vs_b_is_flagged_noisy():
    fps = {"A": [28.0, 0, 34.0, 0], "B": [0, 31.0, 0, 32.0]}
    result = dev_live.analyse(arm_of(), live_log(lambda label, i: fps[label][i] if i >= 0 else 30.0))
    assert result["spread"]["fps"] == 6.0 and abs(result["delta"]["fps"]) < 6.0
    assert "fps" in result["noisy"]
    assert "A-vs-A spread exceeds A-vs-B for: fps" in "\n".join(dev_live.report_lines("x", result))


def test_segments_before_the_world_settles_are_left_out():
    text = live_log(lambda label, i: 30.0 if label == "A" else 33.0, settle=150.0)
    result = dev_live.analyse(arm_of(), text)
    first = result["segments"][0]
    assert first.get("skipped") == "before the world settled"
    assert result["sets"]["A"]["segments"] == 1 and result["sets"]["B"]["segments"] == 2


def test_no_segment_lines_is_a_problem_not_a_crash():
    result = dev_live.analyse(arm_of(), "\n".join([frame_line(10, 30, 5), pacing_line(10, 33, 40)]))
    assert result["sets"] == {} and "schedule was not applied" in result["problems"][0]
    row = dev_verdict.live_verdict(result)
    assert row["verdict"] == "INCONCLUSIVE"


def test_change_lines_of_one_segment_cluster_and_a_wrong_order_is_reported():
    lines = [segment_line(100.0, 1, "VICTORY_AWAKE", "1"), segment_line(100.2, 1, "VICTORY_X", "2"),
             segment_line(140.0, 2, "VICTORY_AWAKE", "0")]
    groups = dev_live.cluster(dev_live.parse_log("\n".join(lines)).changes)
    assert [(g[1], g[2]) for g in groups] == [(1, {"AWAKE": "1", "X": "2"}), (2, {"AWAKE": "0"})]
    result = dev_live.analyse(arm_of(), "\n".join(lines))
    assert any("segment order BA" in p for p in result["problems"])


def rec(step, name, ok=True, **stats):
    base = {"fps": 30.0, "prep_ms": 10.0, "steady_p99_ms": 60.0, "world_over250": 5, "boot_s": 50.0, "stream_s": 100.0}
    base.update(stats)
    return {"step": step, "set": name, "ok": ok, "stats": base}


def test_verdicts_use_the_spread_of_repeats_as_the_noise_floor():
    sets = {"off": {"VICTORY_FOO": "0"}, "good": {"VICTORY_FOO": "1"}, "bad": {"VICTORY_FOO": "2"},
            "meh": {"VICTORY_FOO": "3"}, "mixed": {"VICTORY_FOO": "4"}, "broke": {"VICTORY_FOO": "5"}}
    done = {
        1: rec(1, "off", fps=29.0), 2: rec(2, "off", fps=31.0), 3: rec(3, "good", fps=34.0),
        4: rec(4, "bad", prep_ms=25.0), 5: rec(5, "meh", fps=31.5), 6: rec(6, "mixed", fps=35.0, boot_s=90.0),
        7: rec(7, "broke", ok=False),
    }
    rows, floor, sources = dev_verdict.set_verdicts(sets, "off", done, {"meh": ["05_meh: DXGI_ERROR_DEVICE_HUNG"]})
    assert sources == ["off"] and floor["fps"] == 2.0 and floor["prep_ms"] == dev_verdict.MIN_FLOOR["prep_ms"]
    got = {r["set"]: r["verdict"] for r in rows}
    assert got == {"good": "KEEP", "bad": "DROP", "meh": "DROP", "mixed": "INCONCLUSIVE", "broke": "DROP"}
    assert next(r for r in rows if r["set"] == "good")["switches"] == "VICTORY_FOO=1"
    text = "\n".join(dev_verdict.verdict_lines(rows, floor, sources, "b4", "off"))
    assert "same-set repeats of off" in text and "| good | `VICTORY_FOO=1` | 1 | **KEEP** |" in text


def test_without_repeats_the_default_floors_apply_and_small_deltas_are_noise():
    sets = {"off": {}, "on": {"VICTORY_FOO": "1"}}
    rows, floor, sources = dev_verdict.set_verdicts(sets, "off", {1: rec(1, "off"), 2: rec(2, "on", fps=32.0)}, {})
    assert sources == [] and floor == dev_verdict.DEFAULT_FLOOR
    assert rows[0]["verdict"] == "INCONCLUSIVE" and rows[0]["why"][0] == "within noise"


def test_a_live_verdict_takes_its_floor_from_its_own_segments():
    fps = {"A": [30.0, 0, 31.0, 0], "B": [0, 35.0, 0, 36.0]}
    result = dev_live.analyse(arm_of(), live_log(lambda label, i: fps[label][i] if i >= 0 else 30.0))
    row = dev_verdict.live_verdict(result)
    assert row["verdict"] == "KEEP" and "fps +5.0 (floor 1.0)" in row["why"][0]


def test_warm_stamps_follow_the_exe_and_the_overrides(tmp_path):
    exe = tmp_path / "victory.exe"
    exe.write_bytes(b"x")
    sig = dev_warm.signature({"VICTORY_A": "1"}, dev_warm.exe_identity(exe))
    assert sig == dev_warm.signature({"VICTORY_A": "1"}, dev_warm.exe_identity(exe))
    assert sig != dev_warm.signature({"VICTORY_A": "2"}, dev_warm.exe_identity(exe))
    assert not dev_warm.is_warm(tmp_path, sig, False)
    dev_warm.write_stamp(tmp_path, sig, False, {"translated": 3, "shaders": 3}, {}, "ahead")
    assert dev_warm.is_warm(tmp_path, sig, False) and not dev_warm.is_warm(tmp_path, sig, True)
    dev_warm.write_stamp(tmp_path, sig, True, {}, {}, "prewarm")
    dev_warm.write_stamp(tmp_path, sig, False, {}, {}, "ahead")
    assert dev_warm.is_warm(tmp_path, sig, True), "a translation-only stamp keeps an earlier pipelines one"
    exe.write_bytes(b"xy")
    assert dev_warm.signature({"VICTORY_A": "1"}, dev_warm.exe_identity(exe)) != sig, "a rebuild invalidates"


def test_apply_kinds_read_the_settings_table(tmp_path):
    table = tmp_path / dev_warm.SETTINGS_TABLE
    table.parent.mkdir(parents=True)
    table.write_text('Setting { name: "VICTORY_AWAKE", default: "0", apply: Apply::Live, },\n'
                     'Setting { name: "VICTORY_FOO", default: "0", apply: Apply::Retranslate, },\n', encoding="utf-8")
    assert dev_warm.apply_kinds(tmp_path) == {"VICTORY_AWAKE": "Live", "VICTORY_FOO": "Retranslate"}
    assert dev_warm.apply_kinds(tmp_path / "none") is None
    (tmp_path / dev_warm.SETTINGS_EXPERT).write_text('    ("VICTORY_BAR", Apply::Restart),\n'
                                                     '    ("VICTORY_AWAKE", Apply::Restart),\n', encoding="utf-8")
    assert dev_warm.apply_kinds(tmp_path) == {"VICTORY_AWAKE": "Live", "VICTORY_FOO": "Retranslate",
                                              "VICTORY_BAR": "Restart"}


def test_the_registry_segment_line_parses():
    """The line victory-core's `settings::schedule::segment_line` writes (its test holds the same text)."""
    line = ("2026-11-27T10:00:00.000000Z  INFO victory_core::settings: "
            "settings: segment 2 VICTORY_PRESENT_SMOOTH=0.5 at 190.0")
    log = dev_live.parse_log(line + "\n")
    assert [(n, name, value) for _, n, name, value in log.changes] == [(2, "VICTORY_PRESENT_SMOOTH", "0.5")]
    m = dev_live.SEGMENT_RE.search(line)
    assert m is not None and m.group(4) == "190.0"


def fake_world_stats(dev, fps):
    s = dev.parse_victory_log("")
    s.update(world_samples=10, fps=fps)
    return s


def fake_cache(tmp_path, monkeypatch):
    (tmp_path / "cache" / "shaders").mkdir(parents=True, exist_ok=True)
    monkeypatch.setenv("VICTORY_CACHE_DIR", str(tmp_path / "cache"))


def setup_train(dev, tmp_path, monkeypatch, bench_log=""):
    monkeypatch.setattr(dev, "DEV_DIR", tmp_path / "dev")
    fake_cache(tmp_path, monkeypatch)
    monkeypatch.setattr(dev, "resolve_game_dir", lambda arg: tmp_path)
    calls, warm = [], []

    def fake_bench(args, overrides, log_path, snap_dir, what):
        calls.append((dict(overrides), args.seconds, args.quiet))
        log_path.write_text(bench_log, encoding="utf-8")
        return fake_world_stats(dev, 30.0), 0, 0, 0

    def fake_prewarm(game_dir, overrides, mute, pipelines, jobs, log_path, profile="release"):
        warm.append((dict(overrides), pipelines))
        return True, {"translated": 7, "hits": 0, "shaders": 7, "recipes": 2}

    class Proc:
        pid, returncode = 1, 0

        def poll(self):
            return 0

        def wait(self, timeout=None):
            return 0

    def fake_popen(cmd, cwd, stdout, stderr, creationflags):
        warm.append(("ahead", creationflags))
        stdout.write(b"warm         7 translated, 0 refused; 0 compute pipelines, 0 refused; 1 s\n"
                     b"warm cache   0 from the cache, 7 translated now\n")
        return Proc()

    monkeypatch.setattr(dev, "run_bench", fake_bench)
    monkeypatch.setattr(dev, "run_prewarm", fake_prewarm)
    monkeypatch.setattr(dev, "build_launcher_command", lambda **kw: ["victory"])
    monkeypatch.setattr(dev.subprocess, "Popen", fake_popen)
    return calls, warm


def test_a_live_run_gets_its_schedule_and_its_report_section(tmp_path, monkeypatch, capsys):
    dev = load_dev()
    fps = {"A": [30.0, 0, 31.0, 0], "B": [0, 35.0, 0, 36.0]}
    log = live_log(lambda label, i: fps[label][i] if i >= 0 else 30.0)
    plan_text = LIVE_PLAN.replace('order = ["awake", "off", "on", "off"]', 'order = ["awake_live", "off"]')
    plan = tmp_path / "b9.toml"
    plan.write_text(plan_text, encoding="utf-8")
    calls, _ = setup_train(dev, tmp_path, monkeypatch, log)
    # The plan's made-up switch, Live, whatever the real registry says.
    monkeypatch.setattr(dev.dev_warm, "apply_kinds", lambda root: {"VICTORY_AWAKE": "Live"})
    assert dev.main(["train", "--plan", str(plan), "--run", "next"]) == 0
    overrides, seconds, quiet = calls[0]
    schedule = Path(overrides["VICTORY_SETTINGS_SCHEDULE"])
    assert schedule.name == "01_awake_live.schedule.txt" and "at 140s set VICTORY_AWAKE=1" in schedule.read_text()
    assert overrides["VICTORY_AWAKE"] == "0" and seconds == 265 and quiet is True
    out = capsys.readouterr().out
    assert "DEV train-live run=01_awake_live A=off fps=30.5" in out and "delta_fps=+5.0" in out
    saved = json.loads((tmp_path / "dev" / "train_b9" / "01_awake_live.json").read_text(encoding="utf-8"))
    assert saved["kind"] == "live" and saved["live"]["sets"]["B"]["fps"] == 35.5
    assert dev.main(["train", "--plan", str(plan), "--report"]) == 0
    assert "### 01_awake_live: live A=off vs B=awake" in capsys.readouterr().out
    assert dev.main(["train", "--plan", str(plan), "--verdict"]) == 0
    out = capsys.readouterr().out
    assert "| awake (live vs off) | `VICTORY_AWAKE=1` |" in out and "**KEEP**" in out
    assert (tmp_path / "dev" / "train_b9" / "verdict.md").exists()


def test_run_prewarms_its_arm_translates_the_next_ahead_and_skips_warm_variants(tmp_path, monkeypatch, capsys):
    dev = load_dev()
    plan = tmp_path / "b8.toml"
    plan.write_text('seconds = 60\nbaseline = "off"\norder = ["on", "off", "mid", "on"]\n'
                    '[sets.on]\nVICTORY_FOO = 1\nquiet = true\n[sets.off]\nVICTORY_FOO = 0\n[sets.mid]\nVICTORY_FOO = 2\n', encoding="utf-8")
    calls, warm = setup_train(dev, tmp_path, monkeypatch)
    assert dev.main(["train", "--plan", str(plan), "--run", "next"]) == 0
    out = capsys.readouterr().out
    assert warm == [({**warm[0][0], "VICTORY_FOO": "1"}, True)], "pipelines prewarm of the arm itself, no ahead"
    assert "prewarm=made:7 ahead=none" in out and calls[0][2] is True, "the set's own quiet: nothing runs beside it"
    assert "next_ttl_min=20" in out
    warm.clear()
    assert dev.main(["train", "--plan", str(plan), "--run", "2"]) == 0
    out = capsys.readouterr().out
    assert warm[0][1] is True and warm[0][0]["VICTORY_FOO"] == "0"
    assert warm[1][0] == "ahead", "the next arm translated during this (not quiet) run"
    if sys.platform == "win32":
        assert warm[1][1] & dev.subprocess.IDLE_PRIORITY_CLASS
    assert calls[1][2] is False and "ahead=mid:7" in out
    warm.clear()
    assert dev.main(["train", "--plan", str(plan), "--run", "next"]) == 0
    out = capsys.readouterr().out
    assert warm == [({**warm[0][0], "VICTORY_FOO": "2"}, True)], (
        "a translation-only stamp does not satisfy a pipelines prewarm; the next (on) is warm: no ahead")
    assert "ahead=none" in out
    warm.clear()
    assert dev.main(["train", "--plan", str(plan), "--run", "next", "--no-quiet"]) == 0
    assert warm == [] and calls[3][2] is False and "prewarm=warm" in capsys.readouterr().out
    assert dev.main(["train", "--plan", str(plan), "--warm", "off"]) == 0
    assert "DEV train-warm PASS run=05_off prewarm=warm" in capsys.readouterr().out


def test_prewarm_is_a_no_op_when_stamped(tmp_path, monkeypatch, capsys):
    dev = load_dev()
    monkeypatch.setattr(dev, "DEV_DIR", tmp_path / "dev")
    monkeypatch.setattr(dev, "prewarm_work", lambda args: pytest.fail("prewarm ran"))
    fake_cache(tmp_path, monkeypatch)
    overrides, _ = dev.apply_mute_default({"VICTORY_X": "1"}, False)
    dev_warm.write_stamp(dev.warm_dir(), dev.warm_signature(overrides), False, {}, overrides, "ahead")
    assert dev.main(["prewarm", "--override", "VICTORY_X=1"]) == 0
    assert "DEV prewarm PASS profile=release skipped=warm" in capsys.readouterr().out
    ran = []
    monkeypatch.setattr(dev, "prewarm_work", lambda args: ran.append(args.pipelines) or 0)
    assert dev.main(["prewarm", "--override", "VICTORY_X=1", "--pipelines"]) == 0
    assert dev.main(["prewarm", "--override", "VICTORY_X=1", "--force"]) == 0
    assert ran == [True, False]
    (tmp_path / "cache" / "shaders").rmdir()
    assert dev.main(["prewarm", "--override", "VICTORY_X=1"]) == 0
    assert ran == [True, False, False], "a cleared cache is cold whatever the stamps say"


def test_prewarm_of_both_ray_tracing_variants_needs_both_stamps(tmp_path, monkeypatch, capsys):
    dev = load_dev()
    monkeypatch.setattr(dev, "DEV_DIR", tmp_path / "dev")
    ran = []
    monkeypatch.setattr(dev, "prewarm_work", lambda args: ran.append(args.ray_tracing_both) or 0)
    fake_cache(tmp_path, monkeypatch)
    overrides, _ = dev.apply_mute_default({"VICTORY_X": "1"}, False)
    dev_warm.write_stamp(dev.warm_dir(), dev.warm_signature(overrides), False, {}, overrides, "prewarm")
    assert dev.main(["prewarm", "--override", "VICTORY_X=1", "--ray-tracing-both"]) == 0
    assert ran == [True], "the ray tracing on variant is not stamped"
    other = {**overrides, "VICTORY_RAY_TRACING": "on"}
    dev_warm.write_stamp(dev.warm_dir(), dev.warm_signature(other), False, {}, other, "prewarm")
    assert dev.main(["prewarm", "--override", "VICTORY_X=1", "--ray-tracing-both"]) == 0
    assert ran == [True] and "skipped=warm" in capsys.readouterr().out


def test_greps_from_all_reads_every_ready_md(tmp_path, monkeypatch, capsys):
    dev = load_dev()
    coord = tmp_path / "coord"
    for job, line in (("alpha", "alpha joins per frame"), ("beta", "beta skipped")):
        (coord / job).mkdir(parents=True)
        (coord / job / "ready.md").write_text(
            "## Batch items\n\n| switch | grep |\n|---|---|\n"
            f"| VICTORY_{job.upper()}=1 | `{line}: ([0-9]+)` |\n", encoding="utf-8")
    monkeypatch.setattr(dev, "COORD_DIR", coord)
    plan = tmp_path / "b7.toml"
    plan.write_text('[sets.on]\nVICTORY_FOO = 1\n[sets.off]\nVICTORY_FOO = 0\n', encoding="utf-8")
    dev.main(["train", "--plan", str(plan), "--greps-from", "all"])
    text = plan.read_text(encoding="utf-8")
    assert "alpha joins per frame" in text and "beta skipped" in text


def test_apply_kinds_reads_the_settings_registry():
    kinds = dev_warm.apply_kinds(Path(__file__).resolve().parents[1])
    assert kinds is not None and len(kinds) > 100
    assert kinds["VICTORY_PRESENT_CAP"] == "Live" and kinds["VICTORY_CARD_AWAKE"] == "Live"
    assert kinds["VICTORY_CARD_WINDOWS_MB"] == "Restart"
    assert kinds["VICTORY_PORT_CBV"] == "Retranslate"
    assert kinds["VICTORY_CARD_DECL_JOIN"] == "Live" and kinds["VICTORY_LATE_WAIT"] == "Restart"
