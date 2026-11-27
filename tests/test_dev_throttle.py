"""tools/dev_throttle.py caps parallel builds machine-wide and refuses builds on a nearly full drive."""

import importlib.util
import sys
from pathlib import Path

import pytest

TOOLS = Path(__file__).resolve().parents[1] / "tools"
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))

import dev_throttle  # noqa: E402


def load_dev():
    spec = importlib.util.spec_from_file_location("dev", TOOLS / "dev.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_build_slots_admit_only_their_count(tmp_path, monkeypatch):
    monkeypatch.setenv("ANYXBOXX_BUILD_SLOTS", "2")
    with dev_throttle.build_slot(tmp_path, "a") as first, dev_throttle.build_slot(tmp_path, "b") as second:
        assert first != second
        with pytest.raises(TimeoutError):
            with dev_throttle.build_slot(tmp_path, "c", timeout_s=0.3):
                pass
    with dev_throttle.build_slot(tmp_path, "d", timeout_s=0.3) as again:
        assert again.name in ("build0.lock", "build1.lock")


def test_released_slot_is_reused(tmp_path, monkeypatch):
    monkeypatch.setenv("ANYXBOXX_BUILD_SLOTS", "1")
    for tag in ("a", "b"):
        with dev_throttle.build_slot(tmp_path, tag, timeout_s=0.3) as slot:
            assert slot.name == "build0.lock"


def test_throttled_command_refuses_a_nearly_full_drive(monkeypatch, capsys):
    dev = load_dev()
    monkeypatch.setattr(dev.dev_throttle, "disk_free_gb", lambda _path: 12.0)
    ran = []
    command = dev.throttled("build")(lambda args: ran.append(args) or 0)
    assert command(None) == 1
    assert not ran
    assert capsys.readouterr().out.startswith("DEV build FAIL disk_free_gb=12 need=40")


def test_throttled_command_runs_inside_a_slot(tmp_path, monkeypatch):
    dev = load_dev()
    monkeypatch.setattr(dev, "LOCKS_DIR", tmp_path)
    monkeypatch.setattr(dev, "ensure_coord_dirs", lambda: None)
    monkeypatch.setattr(dev.dev_throttle, "disk_free_gb", lambda _path: 500.0)
    monkeypatch.setattr(dev.dev_throttle, "below_normal_priority", lambda: None)
    monkeypatch.setenv("ANYXBOXX_BUILD_SLOTS", "1")

    def body(_args):
        with pytest.raises(TimeoutError):
            with dev_throttle.build_slot(tmp_path, "other", timeout_s=0.3):
                pass
        return 0

    assert dev.throttled("gate")(body)(None) == 0


def test_all_build_slots_block_every_build(tmp_path, monkeypatch):
    monkeypatch.setenv("ANYXBOXX_BUILD_SLOTS", "3")
    with dev_throttle.all_build_slots(tmp_path, "quiet"):
        with pytest.raises(TimeoutError):
            with dev_throttle.build_slot(tmp_path, "build", timeout_s=0.3):
                pass
    with dev_throttle.build_slot(tmp_path, "build", timeout_s=0.3):
        pass


def test_quiet_bench_flag_holds_all_slots(tmp_path, monkeypatch):
    dev = load_dev()
    monkeypatch.setattr(dev, "LOCKS_DIR", tmp_path)
    monkeypatch.setenv("ANYXBOXX_BUILD_SLOTS", "2")
    args = dev.build_parser().parse_args(["bench", "--quiet"])
    with dev._quiet(args, "bench"):
        with pytest.raises(TimeoutError):
            with dev_throttle.build_slot(tmp_path, "build", timeout_s=0.3):
                pass
    loud = dev.build_parser().parse_args(["bench"])
    with dev._quiet(loud, "bench"):
        with dev_throttle.build_slot(tmp_path, "build", timeout_s=0.3):
            pass
