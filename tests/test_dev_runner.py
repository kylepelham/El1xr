"""tools/dev_runner.py (`train --all`: unattended arms, stops, retries, time budget, Ctrl+C and resume),
tools/dev_look.py (`look PLAN ARM`) and `train --quick`. The game never starts: run_bench and the
prewarms are stubbed as in tests/test_dev_train.py."""

import json
import sys
from pathlib import Path

import pytest

TOOLS = Path(__file__).resolve().parents[1] / "tools"
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))

import dev_runner
import dev_train
from test_dev_train import load_dev, no_game

PLAN = """
seconds = 200
snap_every = 0

[sets.on]
VICTORY_FOO = 1

[sets.off]
VICTORY_FOO = 0

[greps]
foo_skipped = 'foo copies skipped: ([0-9]+)'
"""


def plan_file(tmp_path, text=PLAN):
    path = tmp_path / "b3.toml"
    path.write_text(text, encoding="utf-8")
    return path


class Bench:
    """A stand-in run_bench: per call, the outcome the test lists (default a passing run), and a log."""

    def __init__(self, dev, outcomes=None):
        self.dev, self.outcomes, self.calls = dev, list(outcomes or []), []

    def __call__(self, args, overrides, log_path, snap_dir, what):
        self.calls.append((log_path.stem, args.seconds, args.monitor.early, args.quiet))
        kind = self.outcomes.pop(0) if self.outcomes else "pass"
        if kind == "interrupt":
            raise KeyboardInterrupt
        fps = 26.0 if overrides.get("VICTORY_FOO") == "1" else 24.0
        s = self.dev.parse_victory_log("")
        s.update(world_samples=10, fps=fps, prep_ms=24.0, frames=4000, world_s=120.0, world_over100=6, boot_s=40.0)
        before, after = 1, 1
        lines = ["INFO victory_gpu: foo copies skipped: 12" if fps > 25 else "INFO victory_gpu: start"]
        if kind == "noworld":
            s.update(world_samples=0, fps=0.0)
        elif kind == "zero":
            s.update(world_samples=0, fps=0.0, frames=0)
        elif kind == "reset":
            after = 2
        elif kind == "slow":
            s.update(fps=3.0)
        elif kind == "stall":
            s.update(hang_stall_s=9.5)
            lines.append("ERROR victory_gpu::hang_watch: hang watch: the card's fence has stood at 4 for 9.5 s")
        elif kind == "hung":
            lines.append("ERROR victory_gpu: present failed: DXGI_ERROR_DEVICE_HUNG")
        log_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
        return s, before, after, 0


def setup(tmp_path, monkeypatch, outcomes=None):
    dev = load_dev()
    monkeypatch.setattr(dev, "DEV_DIR", tmp_path / "dev")
    bench = Bench(dev, outcomes)
    monkeypatch.setattr(dev, "run_bench", bench)
    no_game(dev, monkeypatch)
    return dev, bench, tmp_path / "dev" / "train_b3"


def test_all_runs_every_pending_arm_and_writes_report_and_verdict(tmp_path, monkeypatch, capsys):
    dev, bench, train_dir = setup(tmp_path, monkeypatch)
    plan = plan_file(tmp_path)
    assert dev.main(["train", "--plan", str(plan), "--list"]) == 0
    assert "--all: 4 arms pending; hold the Drift gpu lock with ttl_minutes>=" in capsys.readouterr().out
    assert dev.main(["train", "--plan", str(plan), "--all"]) == 0
    out = capsys.readouterr().out
    assert [c[0] for c in bench.calls] == ["01_on", "02_off", "03_off", "04_on"]
    assert all(c[2] for c in bench.calls), "loud arms stop early by default"
    assert "DEV train-all: 4 arms pending (01_on 02_off 03_off 04_on); gpu lock ttl_minutes>=" in out
    assert "DEV train-arm PASS run=04_on fps=26.0(+2.0) prep=24.0(+0.0)" in out
    assert "DEV train-all PASS ran=4 failed=0" in out
    assert (train_dir / "report.md").exists() and (train_dir / "verdict.json").exists()
    assert dev.main(["train", "--plan", str(plan), "--all"]) == 0
    assert "0 arms pending (none)" in capsys.readouterr().out


@pytest.mark.parametrize(
    ("kind", "why"),
    [
        ("reset", "driver reset during the run (resets since boot 1 -> 2)"),
        ("zero", "0 frames"),
        ("slow", "settled fps 3.0 under 5"),
        ("stall", "hang watch: the card's fence stood 9.5 s"),
        ("hung", "device removed / DEVICE_HUNG in the log"),
    ],
)
def test_all_stops_at_once_writes_stopped_md_and_resume_runs_that_arm_again(tmp_path, monkeypatch, capsys, kind, why):
    dev, bench, train_dir = setup(tmp_path, monkeypatch, ["pass", kind])
    plan = plan_file(tmp_path)
    assert dev.main(["train", "--plan", str(plan), "--all"]) == 2
    out = capsys.readouterr().out
    assert [c[0] for c in bench.calls] == ["01_on", "02_off"], "no retry, no later arm"
    assert "DEV train-arm STOP run=02_off" in out and "DEV train-all STOP reason=" in out
    stopped = (train_dir / "STOPPED.md").read_text(encoding="utf-8")
    assert f"- why: {why}" in stopped and "- arm: 02_off" in stopped
    if kind == "stall":
        assert "hang watch: the card's fence has stood at 4 for 9.5 s" in stopped
    assert dev_train.load_results(train_dir)[2]["stopped"].startswith(why[:10])
    assert dev.main(["train", "--plan", str(plan), "--all"]) == 0
    out = capsys.readouterr().out
    assert "resuming after an earlier stop" in out
    assert [c[0] for c in bench.calls[2:]] == ["02_off", "03_off", "04_on"]
    assert not (train_dir / "STOPPED.md").exists()
    assert "stopped" not in dev_train.load_results(train_dir)[2]


def test_a_failed_arm_is_tried_once_more_then_marked_failed_and_the_call_goes_on(tmp_path, monkeypatch, capsys):
    dev, bench, train_dir = setup(tmp_path, monkeypatch, ["noworld", "pass", "noworld", "noworld"])
    assert dev.main(["train", "--plan", str(plan_file(tmp_path)), "--all"]) == 0
    out = capsys.readouterr().out
    assert [c[0] for c in bench.calls] == ["01_on", "01_on", "02_off", "02_off", "03_off", "04_on"]
    assert "01_on failed (attempt 1); trying it once more" in out
    done = dev_train.load_results(train_dir)
    assert done[1]["ok"] and done[1]["attempt"] == 2
    assert done[2]["failed"] and not done[2]["ok"]
    assert "DEV train-arm FAIL run=02_off" in out and "DEV train-all PASS ran=4 failed=1" in out
    verdict = json.loads((train_dir / "verdict.json").read_text(encoding="utf-8"))
    assert verdict["rows"][0]["set"] == "on"


def test_ctrl_c_ends_the_call_and_all_resumes_with_the_interrupted_arm(tmp_path, monkeypatch, capsys):
    dev, bench, train_dir = setup(tmp_path, monkeypatch, ["pass", "interrupt"])
    plan = plan_file(tmp_path)
    assert dev.main(["train", "--plan", str(plan), "--all"]) == 130
    assert "DEV train-all STOP reason=interrupted ran=1 failed=0 next=02_off" in capsys.readouterr().out
    assert sorted(dev_train.load_results(train_dir)) == [1]
    assert dev.main(["train", "--plan", str(plan), "--all"]) == 0
    assert [c[0] for c in bench.calls[2:]] == ["02_off", "03_off", "04_on"]


def test_max_minutes_starts_no_arm_expected_to_end_past_the_budget(tmp_path):
    plan = dev_train.parse_plan("p", {"sets": {"on": {}, "off": {}}, "seconds": 200})
    now = [0.0]
    ran = []

    def run_arm(step, name):
        now[0] += 240.0
        ran.append(step)
        rec = {"step": step, "set": name, "ok": True, "stats": {"frames": 10, "world_samples": 5, "fps": 25.0},
               "resets_before": 0, "resets_after": 0}
        dev_train.save_result(tmp_path, step, rec)
        return rec

    rc = dev_runner.run_all(plan, {}, tmp_path, dev_train.pending_steps(plan, {}), run_arm, lambda s: "",
                            9.0, lambda p: "", clock=lambda: now[0])
    assert rc == 0 and ran == [1, 2], "4 min an arm: a third would end at 12 min"
    assert dev_runner.stop_reason({"stats": {"frames": 10}, "prewarm_failed": True}) is None


def test_look_shows_numbers_greps_visual_and_verdict_on_one_screen(tmp_path, monkeypatch, capsys):
    dev, _, train_dir = setup(tmp_path, monkeypatch)
    plan = plan_file(tmp_path)
    assert dev.main(["train", "--plan", str(plan), "--all"]) == 0
    (train_dir / "autovis").mkdir(exist_ok=True)
    (train_dir / "autovis" / "visual.json").write_text(json.dumps({
        "pairs": [{"arm": "01_on", "baseline": "02_off", "t_arm": 30, "t_base": 30, "matched_by": "time", "score": 14.0,
                   "flagged_blocks": 90, "blocks": 900, "flagged": True, "sheet": "S/01_on_vs_02_off-0030s.png"},
                  {"arm": "01_on", "baseline": "02_off", "t_arm": 45, "t_base": 45, "matched_by": "time", "score": 2.0,
                   "flagged_blocks": 0, "blocks": 900, "flagged": False, "sheet": None}],
        "black": [{"run": "01_on", "t": 15, "kind": "black", "share": 1.0}],
    }), encoding="utf-8")
    capsys.readouterr()
    assert dev.main(["look", str(plan), "01_on"]) == 0
    out = capsys.readouterr().out
    lines = out.splitlines()
    assert len(lines) < 60
    assert lines[0].startswith("look 01_on (set on: VICTORY_FOO=1): ok")
    assert "baseline off: median of 2 runs (02_off 03_off)" in out
    assert any(ln.startswith("fps") and ln.split()[1:] == ["26.0", "24.0", "+2.0"] for ln in lines)
    assert any(ln.startswith(">100 ms /min") and "3.0" in ln for ln in lines), "6 frames over 2 world minutes"
    assert "foo_skipped: 1 / 0" in out and "foo copies skipped: 12" in out
    assert "visual: 1 of 2 snap pairs flagged (worst score 14.0); 1 black frames" in out
    assert "S/01_on_vs_02_off-0030s.png" in out and "black 15s: black 100%" in out
    assert "verdict: " in out
    assert dev.main(["look", str(plan), "09_x"]) == 1


def test_quick_runs_one_set_outside_the_plan_and_looks_at_it(tmp_path, monkeypatch, capsys):
    dev, bench, train_dir = setup(tmp_path, monkeypatch)
    plan = plan_file(tmp_path)
    assert dev.main(["train", "--plan", str(plan), "--run", "2"]) == 0
    capsys.readouterr()
    assert dev.main(["train", "--plan", str(plan), "--quick", "on", "--seconds", "90"]) == 0
    out = capsys.readouterr().out
    assert bench.calls[-1][:2] == ("01_on", 90)
    assert (train_dir / "quick" / "01_on.json").exists()
    assert sorted(dev_train.load_results(train_dir)) == [2], "not a step of the plan"
    assert "look quick/01_on (set on: VICTORY_FOO=1): ok" in out
    assert "DEV train-quick PASS run=quick/01_on" in out
    assert dev.main(["train", "--plan", str(plan), "--quick", "nope"]) == 1
