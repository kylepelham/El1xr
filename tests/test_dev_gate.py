"""tools/dev_gate.py: what the merge gate reads from cargo, libtest and pytest output."""

import json
import subprocess
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import dev_gate  # noqa: E402


def _artifact(name, kind, exe, test=True):
    return json.dumps({
        "reason": "compiler-artifact",
        "target": {"name": name, "kind": [kind]},
        "profile": {"test": test},
        "executable": exe,
    })


def _warning(manifest, message, spans=True, rendered=None):
    return json.dumps({
        "reason": "compiler-message",
        "manifest_path": str(manifest),
        "message": {
            "level": "warning",
            "message": message,
            "spans": [{"file_name": "src/lib.rs"}] if spans else [],
            "rendered": rendered if rendered is not None else f"warning: {message}\n",
        },
    })


def test_test_binaries_are_labelled_by_crate_and_integration_target(tmp_path):
    lines = [
        _artifact("victory-gpu", "lib", "gpu.exe"),
        _artifact("load_probe", "test", "probe.exe"),
        _artifact("victory-gpu", "lib", None, test=False),
        "Compiling something",
    ]
    binaries, warnings = dev_gate.parse_cargo_messages(lines, tmp_path)
    assert binaries == [("victory_gpu", "gpu.exe"), ("load_probe[test]", "probe.exe")]
    assert warnings == []


def test_only_workspace_warnings_count_once_without_summaries(tmp_path):
    inside = tmp_path / "crates" / "victory-gpu" / "Cargo.toml"
    outside = tmp_path.parent / "registry" / "Cargo.toml"
    lines = [
        _warning(inside, "method `set` is never used"),
        _warning(inside, "method `set` is never used"),  # lib and lib test builds
        _warning(inside, "3 warnings emitted", spans=False),
        _warning(outside, "unused import"),
    ]
    _, warnings = dev_gate.parse_cargo_messages(lines, tmp_path)
    assert warnings == ["warning: method `set` is never used\n"]


def test_results_are_summed_over_libtest_and_read_from_pytest():
    libtest = (
        "test result: ok. 310 passed; 0 failed; 8 ignored; 0 measured\n"
        "test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured\n"
    )
    assert dev_gate.count_results("cargo", libtest) == (312, 1)
    assert dev_gate.count_results("pytest", "1 failed, 1341 passed, 323 skipped in 15.21s") == (1341, 1)
    assert dev_gate.count_results("pytest", "1342 passed, 323 skipped in 8.32s") == (1342, 0)
    assert dev_gate.count_results("policy", "asset policy: 220 files ok") == (0, 0)


def test_a_crashed_binary_counts_as_a_failure(tmp_path):
    step = dev_gate.Step("crash", [sys.executable, "-c", "import sys; sys.exit(3)"], tmp_path, "cargo")
    dev_gate.run_step(step)
    assert not step.ok and step.failed == 1


def test_unsharded_binaries_run_on_the_default_and_pytest_on_xdist_workers(tmp_path, monkeypatch):
    monkeypatch.setattr(dev_gate, "pytest_command", lambda python: [python, "-m", "pytest", "-q", "-n", "4"])
    build = dev_gate.Build(True, 1.0, [("victory_formats", "f.exe")])
    steps = dev_gate.plan(build, tmp_path, tmp_path, "python")
    commands = {s.label: s.cmd for s in steps}
    assert commands["victory_formats"] == ["f.exe"]
    assert commands["policy"] == ["cargo", "xtask", "policy"]
    assert commands["pytest"] == ["python", "-m", "pytest", "-q", "-n", "4"]


def test_pytest_uses_xdist_workers_only_when_the_interpreter_has_the_plugin(monkeypatch):
    seen = []

    def probe(returncode):
        def fake_run(cmd, **_kw):
            seen.append(cmd)
            return subprocess.CompletedProcess(cmd, returncode, b"", b"")
        return fake_run

    monkeypatch.setattr(dev_gate.subprocess, "run", probe(0))
    assert dev_gate.pytest_command("py") == ["py", "-m", "pytest", "-q", "-n", str(dev_gate.PYTEST_WORKERS)]
    assert seen == [["py", "-c", "import xdist"]]
    monkeypatch.setattr(dev_gate.subprocess, "run", probe(1))
    assert dev_gate.pytest_command("py") == ["py", "-m", "pytest", "-q"]

    def missing(cmd, **_kw):
        raise OSError("no such interpreter")

    monkeypatch.setattr(dev_gate.subprocess, "run", missing)
    assert dev_gate.pytest_command("py") == ["py", "-m", "pytest", "-q"]


def test_shards_hold_every_test_once_and_spread_neighbours():
    names = [f"m::t{i:02}" for i in range(20)]
    groups = dev_gate.shards(names, 6)
    assert len(groups) == 6 and all(groups)
    assert sorted(n for g in groups for n in g) == names
    assert groups[0][:2] == ["m::t00", "m::t06"]
    assert dev_gate.shards(names[:2], 6) == [["m::t00"], ["m::t01"]]


def test_shards_grow_in_number_before_a_command_line_grows_too_long():
    names = [f"module::a_long_test_name_{i:04}_" + "x" * 60 for i in range(2000)]
    groups = dev_gate.shards(names, 6)
    assert len(groups) > 6
    assert all(sum(len(n) + 3 for n in g) <= dev_gate.MAX_SHARD_ARGS_CHARS for g in groups)
    assert sorted(n for g in groups for n in g) == sorted(names)


@pytest.mark.parametrize("label", ["victory_shader", "victory_gpu"])
def test_sharded_binaries_run_as_one_thread_shards_covering_every_test(tmp_path, monkeypatch, label):
    names = [f"emit::valu_tests::t{i}" for i in range(13)]
    monkeypatch.setattr(dev_gate, "list_tests", lambda exe, cwd: names)
    build = dev_gate.Build(True, 1.0, [(label, "s.exe"), ("victory_formats", "f.exe")])
    steps = dev_gate.plan(build, tmp_path, tmp_path, "python")
    shard_steps = [s for s in steps if s.label.startswith(f"{label}#")]
    assert len(shard_steps) == dev_gate.SHARDED_BINARIES[label]
    for s in shard_steps:
        assert s.cmd[:3] == ["s.exe", "--test-threads=1", "--exact"]
    assert sorted(n for s in shard_steps for n in s.cmd[3:]) == sorted(names)
    assert {s.label: s.cmd for s in steps}["victory_formats"] == ["f.exe"]


@pytest.mark.parametrize("label", ["victory_shader", "victory_gpu"])
def test_a_sharded_binary_that_cannot_list_its_tests_runs_whole_on_one_thread(tmp_path, monkeypatch, label):
    # victory_gpu's tests share process-wide state: never libtest's default threads.
    monkeypatch.setattr(dev_gate, "list_tests", lambda exe, cwd: None)
    build = dev_gate.Build(True, 1.0, [(label, "s.exe")])
    steps = dev_gate.plan(build, tmp_path, tmp_path, "python")
    assert [s.cmd for s in steps if s.kind == "cargo"] == [["s.exe", "--test-threads=1"]]


def test_list_tests_reads_libtest_list_output(tmp_path, monkeypatch):
    out = "b::two: test\na::one: test\nc::bench: bench\n\n2 tests, 1 benchmark\n"
    seen = []

    def fake_run(cmd, **_kw):
        seen.append(cmd)
        return subprocess.CompletedProcess(cmd, 0, out, "")

    monkeypatch.setattr(dev_gate.subprocess, "run", fake_run)
    assert dev_gate.list_tests("s.exe", tmp_path) == ["a::one", "b::two"]
    assert seen == [["s.exe", "--list"]]
    monkeypatch.setattr(dev_gate.subprocess, "run", lambda cmd, **_kw: subprocess.CompletedProcess(cmd, 1, "", ""))
    assert dev_gate.list_tests("s.exe", tmp_path) is None


def test_the_log_counts_a_shards_tests_instead_of_listing_them(tmp_path):
    step = dev_gate.Step("victory_shader#1", ["s.exe", "--test-threads=1", "--exact", "a", "b"], tmp_path, "cargo")
    log = tmp_path / "gate.log"
    dev_gate.write_log(log, dev_gate.Build(True, 1.0), [step])
    assert "s.exe --test-threads=1 --exact <2 tests>" in log.read_text()
