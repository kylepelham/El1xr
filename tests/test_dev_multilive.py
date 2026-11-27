"""tools/dev_multilive.py (multi-switch live launches of `dev.py train`), tools/dev_mlrun.py (their glue) and
tools/dev_multivis.py (per arm visuals), on synthetic logs and PNGs (no game, no build slot, no gpu lock)."""

import sys
import tomllib
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest

TOOLS = Path(__file__).resolve().parents[1] / "tools"
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))

import dev_mlrun
import dev_multilive
import dev_train

T0 = datetime(2026, 11, 27, 18, 0, 0, tzinfo=UTC)

PLAN = """
seconds = 60
baseline = "base"
order = ["ml", "base"]

[sets.base]
VICTORY_FOO = 0
VICTORY_KEEP = "1"

[multilive.ml]
arms = ["FOO=1", "VICTORY_BAR=2 BAZ=1", { set = "KEEP=2", segment_s = 30, at_s = 300 }]
segment_s = 20
start_s = 100
count = { flush = 'batch flush per frame' }
num = { guards = 'guards ([0-9.]+) ms' }
"""

LIVE = {"VICTORY_FOO": "Live", "VICTORY_BAR": "Live", "VICTORY_BAZ": "Live", "VICTORY_KEEP": "Live"}


def plan_of(text: str = PLAN, kinds=None) -> dev_train.Plan:
    return dev_train.parse_plan("p", tomllib.loads(text), kinds)


def stamp(t: float) -> str:
    return (T0 + timedelta(seconds=t)).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def frame_line(t, fps, prep, draws=5000):
    return (f"\x1b[2m{stamp(t)}\x1b[0m \x1b[32m INFO\x1b[0m victory_gpu::host: {fps:.1f} fps, {1000 / fps:.1f} ms/frame "
            f"({prep:.1f} preparing: copies 0.1 ms 0.1 MB, textures 0.0 ms 0.0 MB; 0.0 on the GPU; 0.0 copying back 0.0 MB), "
            f"{draws} draws/frame, dropped 0 without a host pipeline (0 being made) and 0 indirect")


def pacing_line(t, p50, p99, jitter=1.0, over100=0, draws=5000):
    return (f"{stamp(t)}  INFO victory_gpu::pacing: frame pacing: 150 frames in 5.0 s (30.0 fps), frame time p50 "
            f"{p50:.1f} ms p95 {p99 - 1:.1f} ms p99 {p99:.1f} ms max {p99:.1f} ms, 3 over 50 ms, {over100} over 100 ms, "
            f"present interval sd 2.0 ms jitter {jitter:.1f} ms; {draws} draws/frame")


def launch_log(ml, active, fps_of, end=None, extra_of=None):
    """A launch of `active`: loading until 60 s, then world; the schedule's changes logged at their times (the
    registry's numbering), a frame line, a pacing window and a snap every 5 s. `fps_of(arm, label)` gives a
    segment's fps; `extra_of(t, arm, label)` more lines."""
    numbers = dev_multilive.segment_numbers(ml, active)
    changes = dev_multilive._changes(ml, active)
    times = sorted({t for t, _, _ in changes})
    stop = end if end is not None else dev_multilive.seconds(ml, active)
    lines, current = [], (None, "A")
    t = 5.0
    while t <= stop:
        for n, at in enumerate(times, 1):
            if t - 5.0 <= at < t:
                for ct, key, value in changes:
                    if ct == at:
                        lines.append(f"{stamp(at)}  INFO victory_core::settings::schedule: settings: segment {n} "
                                     f"{key}={value} at {at:.1f}")
                seg = numbers[n]
                current = (seg.arm, seg.label) if seg else (None, "A")
        if t <= 60.0:
            lines += [frame_line(t, 3.0, 50.0), pacing_line(t, 300.0, 900.0, over100=10)]
        else:
            fps = fps_of(*current)
            lines += [frame_line(t, fps, 1000.0 / fps / 2), pacing_line(t, 1000.0 / fps, 2000.0 / fps, jitter=fps / 10)]
            if extra_of:
                lines += [f"{stamp(t)}  INFO victory_gpu::host::late: {x}" for x in extra_of(t, *current)]
        lines.append(f"{stamp(t + 0.5)}  INFO victory_gpu::snap: snap saved path=E:\\x\\01_ml_snaps\\snap-{int(t):04d}s-f{int(t) * 30}.png frame={int(t) * 30}")
        t += 5.0
    return "\n".join(lines) + "\n"


def test_arms_parse_with_a_from_the_base_set_else_empty_and_registry_kinds_checked():
    plan = plan_of(kinds=LIVE)
    ml = plan.multilive["ml"]
    assert [a.name for a in ml.arms] == ["FOO=1", "BAR=2 BAZ=1", "KEEP=2"]
    assert ml.arms[0].keys == {"VICTORY_FOO": ("0", "1")}
    assert ml.arms[1].keys == {"VICTORY_BAR": ("", "2"), "VICTORY_BAZ": ("", "1")}
    assert ml.arms[2].keys == {"VICTORY_KEEP": ("1", "2")} and ml.arms[2].segment_s == 30 and ml.arms[2].at_s == 300
    assert ml.arms[0].labels == list("ABBA") and ml.launch == plan.sets["base"]
    assert dev_train.step_overrides(plan, "ml") == plan.sets["base"]
    assert not dev_train.step_early_stop(plan, "ml", False)
    assert set(ml.counts) == {"flush"} and set(ml.nums) == {"guards"}


@pytest.mark.parametrize(
    ("change", "kinds", "message"),
    [
        (('"FOO=1"', '"FOO=0"'), None, "what the launch has already"),
        (('"FOO=1"', '"FOO"'), None, "is not KEY=value"),
        (('"FOO=1"', '"FOO=1"'), {"VICTORY_FOO": "Retranslate"}, "Retranslate, not Live"),
        (("segment_s = 20", "segment_s = 20\npattern = \"AA\""), None, "both"),
        (("segment_s = 20", "segment_s = 10"), None, r"drop_s \+ 8"),
        (("at_s = 300", "at_s = 150"), None, "before"),
        (("segment_s = 20", "segment_s = 20\nwhat = 1"), None, "unknown keys what"),
        (("[multilive.ml]", "[multilive.base]"), None, "a set has that name"),
        (('order = ["ml", "base"]', 'order = ["mlx"]'), None, "lacks: mlx"),
        (('"FOO=1", ', '"FOO=1", "FOO=1", '), None, "given twice"),
    ],
)
def test_multilive_mistakes_are_refused_by_name(change, kinds, message):
    with pytest.raises(dev_train.PlanError, match=message):
        plan_of(PLAN.replace(*change), kinds)


def test_the_schedule_runs_arms_back_to_back_and_restores_each_arm_at_its_end():
    ml = plan_of().multilive["ml"]
    active = [a.name for a in ml.arms]
    text = dev_multilive.schedule_text(ml, active)
    lines = [ln for ln in text.splitlines() if ln.startswith("at ")]
    assert lines[:5] == ["at 100s set VICTORY_FOO=0", "at 120s set VICTORY_FOO=1", "at 140s set VICTORY_FOO=1",
                         "at 160s set VICTORY_FOO=0", "at 180s set VICTORY_FOO=0"]
    assert lines[5:7] == ["at 180s set VICTORY_BAR=", "at 180s set VICTORY_BAZ="]
    assert "at 260s set VICTORY_BAR=" in lines and "at 300s set VICTORY_KEEP=1" in lines
    assert lines[-1] == "at 420s set VICTORY_KEEP=1"
    numbers = dev_multilive.segment_numbers(ml, active)
    assert numbers[1].arm == "FOO=1" and numbers[5].arm == "BAR=2 BAZ=1" and numbers[5].label == "A"
    assert numbers[9] is None  # 260 s: the second arm's restore alone (the third waits for at_s 300)
    assert numbers[10].start == 300 and numbers[len(numbers)] is None
    assert dev_multilive.seconds(ml, active) == 425
    assert dev_multilive.seconds(ml, ["FOO=1"]) == 185


def test_per_arm_figures_counts_nums_jitter_and_snaps():
    ml = plan_of().multilive["ml"]
    active = [a.name for a in ml.arms]

    def fps(arm, label):
        return 30.0 if (arm == "FOO=1" and label == "B") else 20.0

    def extra(t, arm, label):
        out = [f"guards {3.0 if label == 'B' and arm == 'FOO=1' else 1.0:.1f} ms"]
        return out + (["batch flush per frame: 2"] if label == "B" else [])

    info = dev_multilive.analyse(ml, active, launch_log(ml, active, fps, extra_of=extra))
    assert info["problems"] == [] and set(info["status"].values()) == {"complete"}
    foo = info["results"]["FOO=1"]
    assert [s["label"] for s in foo["segments"]] == list("ABBA")
    assert foo["sets"]["B"]["fps"] == pytest.approx(30.0) and foo["delta"]["fps"] == pytest.approx(10.0)
    assert foo["delta"]["jitter_ms"] == pytest.approx(1.0) and foo["noisy"] == []
    assert foo["counts"]["flush"]["A"] == 0 and foo["counts"]["flush"]["B"] > 0
    assert foo["nums"]["guards"] == {"A": pytest.approx(1.0), "B": pytest.approx(3.0)}
    assert all(s["snaps"] for s in foo["segments"]) and foo["segments"][1]["snaps"][0].startswith("snap-012")
    assert dev_multilive.verdict(foo) == "KEEP" and dev_multilive.clear(foo)
    bar = info["results"]["BAR=2 BAZ=1"]
    assert bar["delta"]["fps"] == pytest.approx(0.0) and dev_multilive.verdict(bar) == "INCONCLUSIVE"
    text = "\n".join(dev_multilive.report_lines("01_ml", ml, info))
    assert "#### FOO=1: KEEP (complete)" in text and "| B-A | | +10.0 |" in text and "flush" in text
    rows = dev_multilive.verdict_rows(info)
    assert [r["verdict"] for r in rows] == ["KEEP", "INCONCLUSIVE", "INCONCLUSIVE"]


def test_a_stopped_launch_names_the_suspect_and_the_next_launch_runs_only_what_is_owed():
    ml = plan_of().multilive["ml"]
    active = [a.name for a in ml.arms]
    text = launch_log(ml, active, lambda arm, label: 30.0 if (arm == "FOO=1" and label == "B") else 20.0, end=215)
    info = dev_multilive.analyse(ml, active, text, stopped=True)
    assert info["status"] == {"FOO=1": "complete", "BAR=2 BAZ=1": "suspect", "KEEP=2": "not reached"}
    assert any("suspect: 2 of 4" in p for p in info["results"]["BAR=2 BAZ=1"]["problems"])
    done = {1: {"set": "ml", "kind": "multilive", "multilive": {**info, "carried": {}, "carried_status": {}}}}
    again, carried, left = dev_multilive.owed(ml, done, 1)
    assert again == ["KEEP=2"] and list(carried) == ["FOO=1"] and "suspect" in left["BAR=2 BAZ=1"]
    later, carried2, left2 = dev_multilive.owed(ml, done, 3)
    assert later == ["KEEP=2"] and carried2 == {} and left2["FOO=1"].startswith("KEEP in 01_ml")
    cut = dev_multilive.analyse(ml, active, text, stopped=False)
    assert cut["status"]["BAR=2 BAZ=1"] == "cut"
    done_cut = {1: {"set": "ml", "multilive": {**cut, "carried": {}, "carried_status": {}}}}
    assert dev_multilive.owed(ml, done_cut, 1)[0] == ["BAR=2 BAZ=1", "KEEP=2"]


def test_a_retry_keeps_the_earlier_log_and_a_launch_with_nothing_owed_is_skipped(tmp_path):
    plan = plan_of()
    ml = plan.multilive["ml"]
    active = [a.name for a in ml.arms]
    info = dev_multilive.analyse(ml, active, launch_log(ml, active, lambda a, lab: 20.0), stopped=False)
    (tmp_path / "01_ml.log").write_text("old", encoding="utf-8")
    (tmp_path / "01_ml_snaps").mkdir()
    done = {1: {"set": "ml", "multilive": {**info, "results": {}, "status": {}, "carried": {}, "carried_status": {}}}}
    run, pre = dev_mlrun.prepare(plan, done, tmp_path, 1, "ml")
    assert run == active and (tmp_path / "01_ml.try1.log").read_text(encoding="utf-8") == "old"
    assert (tmp_path / "01_ml_snaps.try1").is_dir() and (tmp_path / "01_ml.schedule.txt").exists()
    assert pre["carried"] == {} and pre["left_out"] == {}
    done = {1: {"set": "ml", "multilive": {**info, "carried": {}, "carried_status": {}}}}
    run, pre = dev_mlrun.prepare(plan, done, tmp_path, 1, "ml")
    assert run == [] and set(pre["carried"]) == set(active)
    rec = dev_mlrun.skipped_record(1, "ml", pre, {"frames": 0})
    assert rec["skipped"] and rec["ok"] and dev_train.step_seconds(plan, "ml", {1: rec}, 1) == 0
    import dev_runner
    assert dev_runner.stop_reason(rec) is None
    lines = dev_mlrun.run_lines("01_ml", rec["multilive"])
    assert len(lines) == 3 and all("complete (carried)" in ln for ln in lines)


def test_list_prints_the_launch_count_and_minutes():
    plan = plan_of()
    listing = dev_train.listing(plan, {})
    assert "01_ml pending [425 s, gpu lock ttl>=20 min]: multilive base base from 100 s, 3 arms:" in listing
    assert "FOO=1 ABBAx20s | BAR=2 BAZ=1 ABBAx20s | KEEP=2 ABBAx30s" in listing
    total = dev_train.totals(plan, {})
    assert total.startswith("launches: 2 pending (0 run), ~8 min of game time")
    assert f"~{(425 + 60 + 2 * dev_train.ALL_ARM_EXTRA_S) / 60:.0f} min with boot" in total


def test_the_report_has_a_multilive_section_and_live_arms_still_parse():
    plan = plan_of(PLAN + '\n[sets.on]\nfrom = "base"\nVICTORY_FOO = 1\n[live.lv]\na = "base"\nb = "on"\n')
    assert set(plan.live) == {"lv"} and set(plan.multilive) == {"ml"}
    ml = plan.multilive["ml"]
    active = [a.name for a in ml.arms]
    info = dev_multilive.analyse(ml, active, launch_log(ml, active, lambda a, lab: 20.0))
    done = {1: {"set": "ml", "kind": "multilive", "ok": True, "stats": {}, "snaps": 0, "multilive": info}}
    text, figures = dev_train.report(plan, done, {}, {}, {1: info})
    assert "## Multilive launches" in text and "### 01_ml: multilive (3 arms run, 0 carried)" in text
    assert "01_ml" in figures["multilive"] and dev_train.set_medians(done) == {}


def test_per_arm_visuals_make_a_strip_and_flag_a_changed_b_snap(tmp_path):
    np = pytest.importorskip("numpy")
    Image = pytest.importorskip("PIL.Image")
    ml = plan_of().multilive["ml"]
    active = ["FOO=1"]
    info = dev_multilive.analyse(ml, active, launch_log(ml, active, lambda a, lab: 20.0))
    snaps = tmp_path / "01_ml_snaps"
    snaps.mkdir()
    for seg in info["results"]["FOO=1"]["segments"]:
        for name in seg["snaps"]:
            arr = np.full((90, 160, 3), 80, dtype=np.uint8)
            if seg["label"] == "B":
                arr[:, :80] = 250
            Image.fromarray(arr).save(snaps / name)
    import dev_multivis
    summary = dev_multivis.update("01_ml", info, snaps, tmp_path / "autovis")
    arm = summary["arms"]["FOO=1"]
    assert arm["snaps"]["A"] > 0 and arm["snaps"]["B"] > 0 and arm["noise_median"] == 0.0
    assert arm["flagged"] == arm["snaps"]["B"] and Path(arm["strip"]).exists()
    assert all(Path(p["sheet"]).exists() for p in arm["pairs"])
    again = dev_multivis.update("01_ml", info, snaps, tmp_path / "autovis")
    assert again == summary and "FOO=1: snaps A" in dev_multivis.lines(summary)[0]
