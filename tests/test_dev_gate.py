"""tools/dev_gate.py: what the merge gate reads from cargo, libtest and pytest output."""

import json
import sys
from pathlib import Path

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


def test_victory_gpu_runs_on_one_thread_and_others_on_the_default(tmp_path):
    build = dev_gate.Build(True, 1.0, [("victory_gpu", "g.exe"), ("victory_shader", "s.exe")])
    steps = dev_gate.plan(build, tmp_path, tmp_path, "python")
    commands = {s.label: s.cmd for s in steps}
    assert commands["victory_gpu"] == ["g.exe", "--test-threads=1"]
    assert commands["victory_shader"] == ["s.exe"]
    assert commands["policy"] == ["cargo", "xtask", "policy"]
    assert commands["pytest"][1:] == ["-m", "pytest", "-q"]
