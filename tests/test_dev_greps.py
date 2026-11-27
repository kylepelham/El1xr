"""tools/dev_greps.py: a plan's [greps] counted in every run's log, the report's flags (fatal
lines, a WARN rise against the baseline arm), and `train --greps-from` reading writers' ready.md."""

import importlib.util
import json
import sys
import tomllib
from pathlib import Path

import pytest

TOOLS = Path(__file__).resolve().parents[1] / "tools"
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))

import dev_greps
import dev_train


def load_dev():
    spec = importlib.util.spec_from_file_location("el1xr_dev_greps", TOOLS / "dev.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def info(message, target="victory_gpu::host"):
    return f"\x1b[2m2026-11-27T12:00:00.000000Z\x1b[0m \x1b[32m INFO\x1b[0m \x1b[2m{target}\x1b[0m: {message}"


def warn(message, target="victory_gpu::host"):
    return f"2026-11-27T12:00:00.000000Z  WARN {target}: {message}"


GREPS = {
    "merged": r"submissions merged per frame: ([0-9.]+)",
    "skipped": r"alias copies skipped",
    "named": r"drop reasons per frame \(over (?P<frames>\d+) frames\): draw (?P<value>[0-9,.]+)",
}


def test_greps_count_lines_and_capture_numbers_through_ansi():
    greps = dev_greps.parse_greps(GREPS)
    log = "\n".join([
        info("submissions merged per frame: 3.5"),
        info("submissions merged per frame: 4"),
        info("submissions merged per frame: 6.25 lists=3"),
        info("alias copies skipped share=0.8"),
        info("drop reasons per frame (over 300 frames): draw 1,200.5"),
        info("unrelated"),
    ])
    scan = dev_greps.scan_log(log, greps)
    assert scan.greps["merged"] == (3, [3.5, 4.0, 6.25])
    assert scan.greps["skipped"] == (1, [])
    assert scan.greps["named"] == (1, [1200.5]), "a group named value is the number"
    assert dev_greps.grep_cell(*scan.greps["merged"]) == "3 / med 4 / last 6.25"
    assert dev_greps.grep_cell(*scan.greps["skipped"]) == "1"
    assert dev_greps.grep_figures(0, []) == {"count": 0, "median": None, "last": None}


@pytest.mark.parametrize(
    ("table", "message"),
    [({"1bad": "x"}, "usable name"), ({"ok": "("}, "greps.ok"), ({"ok": 3}, "regex string"), (["x"], "table")],
)
def test_bad_greps_are_refused(table, message):
    with pytest.raises(ValueError, match=message):
        dev_greps.parse_greps(table)
    with pytest.raises(dev_train.PlanError, match=message):
        dev_train.parse_plan("p", {"sets": {"on": {}, "off": {}}, "greps": table})


def test_fatal_lines_and_warn_kinds_are_counted():
    log = "\n".join([
        "2026-11-27T12:00:00Z ERROR victory_gpu::host: relay: fence 1 did not reach 3363; device removed: The GPU",
        "VIctory: host GPU failure: Device Removed",
        "hresult DXGI_ERROR_DEVICE_HUNG",
        "thread 'victory-submit' panicked at crates/victory-gpu/src/host.rs:12:5",
        warn("a pipeline creation method that is not implemented: slot 12 slot=4", "victory_gpu::device"),
        warn("a pipeline creation method that is not implemented: slot 13 slot=5", "victory_gpu::device"),
        warn("late by 13 ms"),
    ])
    scan = dev_greps.scan_log(log, {})
    assert scan.fatal == {"device removed": 2, "DEVICE_HUNG": 1, "panicked": 1}
    assert scan.warns == 3
    assert scan.warn_kinds == {
        "victory_gpu::device: a pipeline creation method that is not implemented: slot N": 2,
        "victory_gpu::host: late by N ms": 1,
    }


def scan(warns, fatal=None, kinds=None, greps=None):
    s = dev_greps.LogScan(greps=greps or {}, fatal=fatal or {}, warns=warns)
    s.warn_kinds.update(kinds or {"x: a": warns})
    return s


def test_a_sharp_warn_rise_against_the_baseline_is_flagged_with_the_kinds_that_rose():
    base = dev_greps.baseline_warns([scan(30, kinds={"x: a": 30}), scan(50, kinds={"x: a": 40, "x: b": 10})])
    assert base[0] == 40
    assert dev_greps.flags(scan(100), base, False) == [], "100 is not over 2 x 40 + 20"
    risen = scan(150, kinds={"x: a": 40, "x: b": 60, "x: c": 50})
    assert dev_greps.flags(risen, base, False) == ["WARN 150 (baseline 40): +55 x: b; +50 x: c; +5 x: a"]
    assert dev_greps.flags(risen, base, True) == [], "the baseline arm is not compared with itself"
    assert dev_greps.flags(scan(5, fatal={"panicked": 1}), None, False) == ["panicked x1"]


def record(step, name, ok=True):
    stats = {k: 1.0 for k, _, _ in dev_train.COLUMNS}
    return {"step": step, "set": name, "ok": ok, "stats": stats, "snaps": 0, "resets_before": 0, "resets_after": 0}


def test_the_report_has_a_flags_column_a_flags_section_and_greps_per_run():
    plan = dev_train.parse_plan("p", {"sets": {"on": {}, "off": {}}, "greps": {"merged": "merged: ([0-9]+)"}})
    done = {1: record(1, "on"), 2: record(2, "off"), 3: record(3, "off")}
    scans = {
        1: scan(200, fatal={"DEVICE_HUNG": 1}, kinds={"h: new warning": 190, "x: a": 10}, greps={"merged": (2, [3.0, 5.0])}),
        2: scan(10, greps={"merged": (0, [])}),
        3: scan(12, greps={"merged": (0, [])}),
    }
    text, figures = dev_train.report(plan, done, scans)
    assert "| ok | flags |" in text
    assert "| yes | **DEVICE_HUNG x1; WARN 200 (baseline 11)** |" in text
    assert "- **01_on**: DEVICE_HUNG x1 / WARN 200 (baseline 11): +190 h: new warning" in text
    assert "| merged | `merged: ([0-9]+)` | 2 / med 4 / last 5 | 0 | 0 |" in text
    assert figures["runs"]["01_on"]["greps"]["merged"] == {"count": 2, "median": 4.0, "last": 5.0}
    assert figures["runs"]["02_off"]["flags"] == []
    clean, _ = dev_train.report(plan, done)
    assert "## Flags" not in clean and "## Greps" not in clean, "nothing without the logs"


READY = """# popin ready

## Batch items

| switch=value | what it should change | what could go wrong / where to look |
|---|---|---|
| `VICTORY_DROP_REASONS=1` (diag) | Adds `drop reasons per frame (VICTORY_DROP_REASONS=1, over N frames): <site>: <reason>` lines. | Grep `alias copies skipped`; nothing in `crates/victory-gpu/src/draw.rs`. |
| `VICTORY_BVH_HOOK=1` | Log: `wrapped N imports of XboxBvhBuilder.dll`, `bvh hook: inputs` and `native rt bind: a trace names address=0x1`. | Run `bench --quiet --seconds 200`; see `VICTORY_X`, `--override`, `x = 1`, `N`, `hit`. |

## Files touched

| `not a batch item line` | x |
"""


def test_greps_from_takes_backticked_log_lines_of_batch_item_rows_only():
    found = dev_greps.extract(READY, "popin")
    assert [(f.name, f.regex, f.note) for f in found] == [
        ("drop_reasons_per_frame", "drop reasons per frame", "popin: VICTORY_DROP_REASONS=1"),
        ("alias_copies_skipped", "alias copies skipped", "popin: VICTORY_DROP_REASONS=1"),
        ("imports_of_xboxbvhbuilder_dll", r"imports of XboxBvhBuilder\.dll", "popin: VICTORY_BVH_HOOK=1"),
        ("bvh_hook_inputs", "bvh hook: inputs", "popin: VICTORY_BVH_HOOK=1"),
        ("native_rt_bind_a_trace_names_address", "native rt bind: a trace names address", "popin: VICTORY_BVH_HOOK=1"),
    ]
    assert dev_greps.slug("a very long phrase that keeps going well past forty characters") == (
        "a_very_long_phrase_that_keeps_going_well"
    )
    assert dev_greps.slug("5 s late") == "g_5_s_late"


def test_merging_skips_known_regexes_and_makes_names_unique():
    found = [dev_greps.Found("a", "x", "n"), dev_greps.Found("a", "y", "n"), dev_greps.Found("b", "x", "n")]
    added, skipped = dev_greps.merge_found({"a": "z"}, found)
    assert [(f.name, f.regex) for f in added] == [("a_2", "x"), ("a_3", "y")]
    assert skipped == 1


PLAN = "seconds = 200\r\n\r\n[greps]\r\nold = 'bvh hook: inputs'\r\n\r\n[sets.on]\r\nVICTORY_A = 1\r\n\r\n[sets.off]\r\n"


def test_train_greps_from_writes_into_the_plans_greps_table(tmp_path, capsys):
    dev = load_dev()
    plan = tmp_path / "b.toml"
    plan.write_bytes(PLAN.encode())
    ready = tmp_path / "popin" / "ready.md"
    ready.parent.mkdir()
    ready.write_bytes(READY.encode("utf-8") + "\n| `VICTORY_Y=1` | \u2014 `it's quoted` |\n".encode("cp1252"))
    assert dev.main(["train", "--plan", str(plan), "--greps-from", str(ready)]) == 0
    assert "DEV train-greps PASS added=4 skipped=1" in capsys.readouterr().out
    text = plan.read_bytes().decode()
    assert "\r\n[greps]\r\nold = 'bvh hook: inputs'\r\ndrop_reasons_per_frame = 'drop reasons per frame'  # popin: " in text
    greps = tomllib.loads(text)["greps"]
    assert greps["imports_of_xboxbvhbuilder_dll"] == r"imports of XboxBvhBuilder\.dll"
    assert tomllib.loads(text)["sets"]["on"] == {"VICTORY_A": 1}, "the tables after [greps] are kept"
    assert dev.main(["train", "--plan", str(plan), "--greps-from", str(ready)]) == 0
    assert "added=0 skipped=5" in capsys.readouterr().out
    assert dev.main(["train", "--plan", str(plan), "--greps-from", str(tmp_path / "none.md")]) == 1


def test_greps_from_adds_a_greps_table_to_a_plan_without_one_and_to_json(tmp_path):
    toml_plan = tmp_path / "a.toml"
    toml_plan.write_text("[sets.on]\n[sets.off]\n", encoding="utf-8")
    dev_greps.add_to_plan(toml_plan, [dev_greps.Found("q", "it's", "n")])
    assert toml_plan.read_text(encoding="utf-8") == '[sets.on]\n[sets.off]\n\n[greps]\nq = "it\'s"  # n\n'
    json_plan = tmp_path / "a.json"
    json_plan.write_text(json.dumps({"sets": {"on": {}, "off": {}}}), encoding="utf-8")
    dev_greps.add_to_plan(json_plan, [dev_greps.Found("q", "x", "n")])
    assert dev_train.load_plan(json_plan).greps["q"].pattern == "x"


def test_train_report_reads_each_runs_log(tmp_path, monkeypatch, capsys):
    dev = load_dev()
    monkeypatch.setattr(dev, "DEV_DIR", tmp_path / "dev")
    plan = tmp_path / "b.toml"
    plan.write_text("[greps]\nmerged = 'merged: ([0-9]+)'\n[sets.on]\n[sets.off]\n", encoding="utf-8")
    train_dir = tmp_path / "dev" / "train_b"
    train_dir.mkdir(parents=True)
    dev_train.save_result(train_dir, 1, record(1, "on"))
    dev_train.save_result(train_dir, 2, record(2, "off"))
    (train_dir / "01_on.log").write_text(info("merged: 7") + "\nthread 'main' panicked at x\n", encoding="utf-8")
    assert dev.main(["train", "--plan", str(plan), "--report"]) == 0
    out = capsys.readouterr().out
    assert "| merged | `merged: ([0-9]+)` | 1 / med 7 / last 7 |" in out, "02_off has no log: no column"
    assert "- **01_on**: panicked x1" in out


def test_hang_lines_keep_the_dred_report_and_the_hang_watch_once_each():
    text = "\n".join([
        info("dred: device-removed data on (breadcrumbs, page faults; a marker before each title command)"),
        info("hang watch: on (VICTORY_HANG_WATCH)"),
        info("hang watch: the card's fence has stood at 1702 for 3.0 s with work queued up to 1703", target="victory_gpu::hang_watch"),
        info("relay: fence 18446744073709551615 did not reach 1703; device removed: hung (0x887A0006)"),
        "dred: removal reason 0x887a0006: hung",
        "dred: victory list #7 (submission 412) stopped at command 1 of 3 (Dispatch)",
        "VIctory: host GPU failure: relay: fence 18446744073709551615 did not reach 1703; device removed",
        "dred: removal reason 0x887a0006: hung",
    ])
    assert dev_greps.hang_lines(text) == [
        "hang watch: the card's fence has stood at 1702 for 3.0 s with work queued up to 1703",
        "dred: removal reason 0x887a0006: hung",
        "dred: victory list #7 (submission 412) stopped at command 1 of 3 (Dispatch)",
    ]
    assert dev_greps.hang_lines(text, most=1) == [
        "hang watch: the card's fence has stood at 1702 for 3.0 s with work queued up to 1703",
    ]
    assert dev_greps.scan_log(text, {}).hang[1] == "dred: removal reason 0x887a0006: hung"
