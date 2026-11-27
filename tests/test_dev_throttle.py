"""tools/dev_throttle.py caps parallel builds machine-wide and refuses builds on a nearly full drive."""

import contextlib
import importlib.util
import sys
import time
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
    monkeypatch.setattr(dev.dev_throttle, "builds_suspended", lambda: contextlib.nullcontext(0))
    args = dev.build_parser().parse_args(["bench", "--quiet"])
    with dev._quiet(args, "bench"):
        with pytest.raises(TimeoutError):
            with dev_throttle.build_slot(tmp_path, "build", timeout_s=0.3):
                pass
    monkeypatch.setattr(dev.dev_throttle, "builds_idle", lambda: contextlib.nullcontext(0))
    loud = dev.build_parser().parse_args(["bench"])
    with dev._quiet(loud, "bench"):
        with dev_throttle.build_slot(tmp_path, "build", timeout_s=0.3):
            pass


def test_free_build_slots_leave_writers_one_and_never_wait(tmp_path, monkeypatch):
    monkeypatch.setenv("ANYXBOXX_BUILD_SLOTS", "3")
    with dev_throttle.free_build_slots(tmp_path, "run") as held:
        assert len(held) == 2
        # Writers keep exactly one slot while the run lasts.
        with dev_throttle.build_slot(tmp_path, "writer", timeout_s=0.3) as slot:
            assert slot not in held
            with pytest.raises(TimeoutError):
                with dev_throttle.build_slot(tmp_path, "writer2", timeout_s=0.3):
                    pass
    # Released at the end: all three are free again.
    with dev_throttle.all_build_slots(tmp_path, "after", timeout_s=0.3):
        pass


def test_free_build_slots_take_only_what_is_free(tmp_path, monkeypatch):
    monkeypatch.setenv("ANYXBOXX_BUILD_SLOTS", "3")
    with dev_throttle.build_slot(tmp_path, "running", timeout_s=0.3) as running:
        with dev_throttle.free_build_slots(tmp_path, "run") as held:
            # One build runs: the two free slots are taken, none is waited for.
            assert len(held) == 2 and running not in held
    with dev_throttle.build_slot(tmp_path, "a", timeout_s=0.3), dev_throttle.build_slot(tmp_path, "b", timeout_s=0.3):
        with dev_throttle.free_build_slots(tmp_path, "run") as held:
            # Two builds run: the last slot is taken, writers keep their two.
            assert len(held) == 1


def test_free_build_slots_do_not_wait_when_all_are_busy(tmp_path, monkeypatch):
    monkeypatch.setenv("ANYXBOXX_BUILD_SLOTS", "2")
    with dev_throttle.all_build_slots(tmp_path, "builds", timeout_s=0.3):
        started = time.monotonic()
        with dev_throttle.free_build_slots(tmp_path, "run") as held:
            assert held == []
        assert time.monotonic() - started < 0.2


def test_a_single_slot_is_left_to_writers(tmp_path, monkeypatch):
    monkeypatch.setenv("ANYXBOXX_BUILD_SLOTS", "1")
    with dev_throttle.free_build_slots(tmp_path, "run") as held:
        assert held == []
        with dev_throttle.build_slot(tmp_path, "writer", timeout_s=0.3):
            pass


def test_loud_capture_holds_free_slots(tmp_path, monkeypatch):
    dev = load_dev()
    monkeypatch.setattr(dev, "LOCKS_DIR", tmp_path)
    monkeypatch.setenv("ANYXBOXX_BUILD_SLOTS", "3")
    monkeypatch.setattr(dev.dev_throttle, "builds_idle", lambda: contextlib.nullcontext(0))
    args = dev.build_parser().parse_args(["capture"])
    with dev._quiet(args, "capture"):
        with dev_throttle.build_slot(tmp_path, "writer", timeout_s=0.3):
            with pytest.raises(TimeoutError):
                with dev_throttle.build_slot(tmp_path, "writer2", timeout_s=0.3):
                    pass


class FakePsutil:
    """Just enough of psutil for `builds_idle`: named processes with children and a class."""

    class NoSuchProcess(Exception):
        pass

    class AccessDenied(Exception):
        pass

    def __init__(self):
        self.procs = []

    def add(self, name, nice, created=1.0, parent=None):
        proc = FakeProc(self, len(self.procs) + 100, name, nice, created, parent)
        self.procs.append(proc)
        return proc

    def process_iter(self, _attrs):
        return [p for p in self.procs if p.alive]


class FakeProc:
    def __init__(self, ps, pid, name, nice, created, parent):
        self.ps, self.pid, self.info, self._nice, self.created, self.parent = (
            ps, pid, {"name": name}, nice, created, parent
        )
        self.alive = True

    def _check(self):
        if not self.alive:
            raise FakePsutil.NoSuchProcess()

    def children(self, recursive=False):
        self._check()
        out = [p for p in self.ps.procs if p.parent is self and p.alive]
        if recursive:
            for child in list(out):
                out.extend(child.children(recursive=True))
        return out

    def nice(self, value=None):
        self._check()
        if value is None:
            return self._nice
        self._nice = value

    def create_time(self):
        self._check()
        return self.created


def test_builds_run_idle_for_the_run_and_get_their_class_back():
    ps = FakePsutil()
    below, normal, idle = 0x4000, 0x20, dev_throttle.IDLE_PRIORITY_CLASS
    cargo = ps.add("cargo.exe", below)
    rustc = ps.add("rustc.exe", below, parent=cargo)
    link = ps.add("link.exe", normal, parent=rustc)
    other = ps.add("Code.exe", normal)
    gone = ps.add("rustc.exe", below)
    changed = ps.add("rustc.exe", below)
    with dev_throttle.builds_idle(ps) as lowered:
        assert lowered == 5
        assert [p.nice() for p in (cargo, rustc, link, gone, changed)] == [idle] * 5
        assert other.nice() == normal
        # Meanwhile: one ends, someone else sets one's class, cargo starts a new rustc (idle, inherited).
        gone.alive = False
        changed.nice(normal)
        late = ps.add("rustc.exe", idle, created=time.time() + 1, parent=cargo)
    assert (cargo.nice(), rustc.nice(), link.nice()) == (below, below, normal)
    assert changed.nice() == normal
    assert late.nice() == below
    assert other.nice() == normal


def test_builds_idle_is_a_no_op_without_psutil(monkeypatch):
    monkeypatch.setattr(dev_throttle.os, "name", "posix")
    with dev_throttle.builds_idle() as lowered:
        assert lowered == 0


class _SuspendProc:
    def __init__(self, pid, name, exe=""):
        self.pid, self.info, self.state = pid, {"name": name, "exe": exe}, "running"

    def children(self, recursive=False):
        return []

    def suspend(self):
        self.state = "suspended"

    def resume(self):
        self.state = "running"


class _SuspendPsutil:
    NoSuchProcess = AccessDenied = Exception

    def __init__(self, procs, children_of_me=()):
        self.procs = procs
        self.children_of_me = list(children_of_me)

    def process_iter(self, _attrs):
        return list(self.procs)

    def Process(self):
        ps = self

        class _Me:
            pid = 1

            def parents(self):
                return []

            def children(self, recursive=False):
                return list(ps.children_of_me)
        return _Me()


def test_quiet_suspends_builds_and_worktree_tools_then_resumes():
    rustc = _SuspendProc(201, "rustc.exe")
    translate = _SuspendProc(202, "victory.exe", r"E:\Gta6\AnyXboxX-wt\shaderperf\third_party\victory\target\release\victory.exe")
    browser = _SuspendProc(203, "chrome.exe", r"C:\Program Files\Google\Chrome\chrome.exe")
    ps = _SuspendPsutil([rustc, translate, browser])
    with dev_throttle.builds_suspended(ps, rescan_s=0) as paused:
        assert paused == 2
        assert rustc.state == translate.state == "suspended"
        assert browser.state == "running"
    assert rustc.state == translate.state == "running"


def test_quiet_suspends_tests_started_during_the_run_but_spares_the_game():
    game = _SuspendProc(301, "victory.exe", r"E:\Gta6\AnyXboxX-wt\train\third_party\victory\target\release\victory.exe")
    ps = _SuspendPsutil([], children_of_me=[game])
    with dev_throttle.builds_suspended(ps, rescan_s=0.05) as paused:
        assert paused == 0
        late = _SuspendProc(302, "victory_gpu-1.exe", r"E:\Gta6\AnyXboxX-wt\racefix\third_party\victory\target\debug\deps\victory_gpu-1.exe")
        ps.procs.extend([game, late])
        deadline = time.time() + 5
        while late.state != "suspended" and time.time() < deadline:
            time.sleep(0.02)
        assert late.state == "suspended"
        assert game.state == "running"
    assert late.state == "running"
