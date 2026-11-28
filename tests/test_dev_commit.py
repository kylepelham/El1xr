"""tools/dev.py and tools/dev_throttle.py around a game run's memory: a game that dies before the
planned end fails its run (never PASS), a launch first makes room in the machine's commit (waits for
builds, then preempts them), and writers' builds, tests and gates wait for a short game run and
retry what it preempted. The game never starts."""

import contextlib
import json
import os
import sys
import time
from pathlib import Path

import pytest

TOOLS = Path(__file__).resolve().parents[1] / "tools"
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))

import dev_throttle  # noqa: E402
from test_dev_early import boot, window  # noqa: E402
from test_dev_train import load_dev, no_game, plan_file  # noqa: E402

OOM_DEATH = (
    "2026-11-28T10:05:19.738183Z ERROR victory_gpu::host: a host state prelude did not record (Not enough memory "
    "resources are available to complete this operation. (0x8007000E)); a diagnostics build with "
    "VICTORY_D3D12_DEBUG=1 names the command\n"
    "VIctory: host GPU failure: a host state prelude did not record (Not enough memory resources are available to "
    "complete this operation. (0x8007000E)); a diagnostics build with VICTORY_D3D12_DEBUG=1 names the command\n"
)
OOM_SURVIVED = (
    "2026-11-28T10:08:16.830200Z  WARN victory_gpu::tiles: no heap for texture levels given on demand: those levels "
    "stay empty error=Not enough memory resources are available to complete this operation. (0x8007000E)\n"
)
CRASH = "VIctory: unhandled exception 0xc0000005 at victory.exe+0x3b37df (read 0x0: nothing mapped there)\n"


def test_a_game_that_exits_before_the_planned_end_died_and_its_log_says_why():
    dev = load_dev()
    assert dev.game_death(dev.RunEnd(False, 1, 79.4), OOM_SURVIVED + OOM_DEATH) == (79.4, "0x8007000E_out_of_memory")
    assert dev.game_death(dev.RunEnd(False, -1073741819, 12.0), CRASH) == (12.0, "unhandled_exception_0xc0000005")
    assert dev.game_death(dev.RunEnd(False, 1, 5.0), "") == (5.0, "exit_1")
    assert dev.game_death(dev.RunEnd(False, -1073741819, 5.0), "") == (5.0, "exit_0xc0000005")
    removed = "2026-11-28T10:00:00Z ERROR victory_gpu::host: device removed: hung (0x887A0006)\n"
    assert dev.game_death(dev.RunEnd(False, 1, 30.0), removed) == (30.0, "device_removed")


def test_a_run_the_timer_ended_died_only_on_a_fatal_report():
    dev = load_dev()
    assert dev.game_death(dev.RunEnd(True, 1, 110.0), OOM_SURVIVED) is None, "a WARN 0x8007000E is survived"
    assert dev.game_death(dev.RunEnd(True, 1, 110.0), "") is None
    assert dev.game_death(dev.RunEnd(True, 1, 110.0), CRASH) == (110.0, "unhandled_exception_0xc0000005")
    assert dev.game_death(dev.RunEnd(True, 1, 110.0), OOM_DEATH)[1] == "0x8007000E_out_of_memory"


def test_died_runs_fail_bench_ok_and_say_so():
    dev = load_dev()
    stats = dev.parse_victory_log("mapped at 0x0\n" + boot() + "".join(window() for _ in range(6)))
    assert dev.bench_ok(stats)
    died = dev.apply_death(dict(stats), dev.RunEnd(False, 1, 79.0, 0.8), OOM_DEATH)
    assert not dev.bench_ok(died)
    assert dev.died_field(died) == " ended=died:79s reason=0x8007000E_out_of_memory"
    assert died["commit_min_gb"] == 0.8
    assert dev.died_field(dev.apply_death(dict(stats), dev.RunEnd(True, 0, 110.0), "")) == ""


def test_run_timed_victory_tells_a_launcher_that_exits_by_itself(tmp_path):
    """A stand-in for the launcher (not the game) that maps and exits after a second."""
    dev = load_dev()
    script = tmp_path / "fake.py"
    script.write_text("import sys, time\nprint('mapped at 0x0', flush=True)\ntime.sleep(1)\nsys.exit(3)\n", encoding="utf-8")
    t0 = time.monotonic()
    end = dev.run_timed_victory([sys.executable, str(script)], tmp_path / "run.log", 60)
    assert not end.timed_out and end.returncode == 3
    assert end.ran_s < 30 and time.monotonic() - t0 < 30


def train_run(dev, tmp_path, monkeypatch, end, log_text):
    game = tmp_path / "game"
    game.mkdir(parents=True, exist_ok=True)

    def fake_run(cmd, log_path, seconds, monitor=None):
        log_path.write_text(log_text, encoding="utf-8")
        return end

    monkeypatch.setattr(dev, "DEV_DIR", tmp_path / "dev")
    no_game(dev, monkeypatch)
    monkeypatch.setattr(dev, "_quiet", lambda args, what: contextlib.nullcontext())
    monkeypatch.setattr(dev, "_room", lambda what: contextlib.nullcontext())
    monkeypatch.setattr(dev, "acquire_gpu_lock", lambda tag, timeout_s=0.0: contextlib.nullcontext())
    monkeypatch.setattr(dev, "run_timed_victory", fake_run)
    monkeypatch.setattr(dev, "resolve_game_dir", lambda explicit: game)
    monkeypatch.setattr(dev, "build_launcher_command", lambda **kw: ["victory"])
    monkeypatch.setattr(dev, "resolve_save_dir", lambda *a, **kw: tmp_path)
    monkeypatch.setattr(dev, "isolated_save_data_dir", lambda save, what: contextlib.nullcontext(tmp_path))
    monkeypatch.setattr(dev.dev_driver, "resets_since_boot", lambda: 0)
    return dev.main(["train", "--plan", str(plan_file(tmp_path)), "--run", "1"])


def test_a_train_arm_whose_game_died_fails_with_ended_died(tmp_path, monkeypatch, capsys):
    """b5q 04_face: the game died of 0x8007000E at 79 s of 110 and the arm still printed PASS ended=time:110s."""
    log = "mapped at 0x0\n" + boot() + "".join(window() for _ in range(8))
    dev = load_dev()
    assert train_run(dev, tmp_path / "a", monkeypatch, dev.RunEnd(True, 0, 110.0), log) == 0
    assert "DEV train PASS run=01_on" in capsys.readouterr().out
    dev = load_dev()
    assert train_run(dev, tmp_path / "b", monkeypatch, dev.RunEnd(False, 1, 79.0), log + OOM_DEATH) == 1
    out = capsys.readouterr().out
    assert "DEV train FAIL run=01_on" in out
    assert "ended=died:79.0s reason=0x8007000E_out_of_memory" in out


def test_writers_wait_while_a_game_run_holds_the_gpu_lock_and_commit_is_short(tmp_path):
    lock = tmp_path / "gpu.lock"
    lines, sleeps = [], []
    frees = iter([30.0, 40.0, 44.0, 50.0])
    with dev_throttle.hold_first_free([lock], "operator:train", 1.0):
        dev_throttle.wait_for_game_room(lock, "gate", free_fn=lambda: next(frees), sleep=sleeps.append, out=lines.append)
        assert len(sleeps) == 3
        assert lines[0].startswith("DEV gate: waiting: a game run holds the gpu lock and commit free is 30.0 GB")
        assert lines[-1].startswith("DEV gate: going on after waiting")
        # Enough commit: no wait even with the lock held.
        assert dev_throttle.wait_for_game_room(lock, "gate", free_fn=lambda: 46.0, sleep=pytest.fail, out=pytest.fail) < 1
    # No game run: no wait however short commit is.
    dev_throttle.wait_for_game_room(lock, "gate", free_fn=lambda: 1.0, sleep=pytest.fail, out=pytest.fail)


class Mem:
    def __init__(self, private):
        self.private = private


class Proc:
    def __init__(self, world, pid, name, cmdline, parent=None, gb=0.0, exe=""):
        self.world, self.pid, self.name, self._cmdline = world, pid, name, cmdline
        self.parent, self.gb, self.exe, self.killed = parent, gb, exe, False
        world.procs[pid] = self

    @property
    def info(self):
        return {"name": self.name, "exe": self.exe}

    def parents(self):
        out, p = [], self.parent
        while p is not None:
            out.append(self.world.procs[p])
            p = self.world.procs[p].parent
        return out

    def children(self, recursive=False):
        kids = [p for p in self.world.procs.values() if p.parent == self.pid and not p.killed]
        if recursive:
            for kid in list(kids):
                kids.extend(kid.children(recursive=True))
        return kids

    def cmdline(self):
        return self._cmdline

    def memory_info(self):
        return Mem(self.gb * (1 << 30))

    def kill(self):
        self.killed = True


class World:
    """Enough of psutil for `build_trees`: processes with parents, command lines and private bytes."""

    def __init__(self, me):
        self.procs: dict[int, Proc] = {}
        self.me = me

    def Process(self, pid=None):
        return self.procs[self.me if pid is None else pid]

    def process_iter(self, attrs):
        return [p for p in list(self.procs.values()) if not p.killed]

    def committed(self):
        return sum(p.gb for p in self.procs.values() if not p.killed)


def machine():
    """The operator's dev.py (spared, with its launcher's cargo), a writer's gate (dev.py, cargo, a
    10 GB test), a writer's build (dev.py, cargo, two rustc of 3 GB) and a cargo started by hand."""
    w = World(me=10)
    Proc(w, 1, "explorer.exe", ["explorer"])
    Proc(w, 10, "python.exe", ["python", "tools/dev.py", "train"], parent=1)
    Proc(w, 11, "cargo.exe", ["cargo", "build"], parent=10, gb=1.0)
    Proc(w, 20, "python.exe", ["python", "tools/dev.py", "gate"], parent=1, gb=0.1)
    Proc(w, 21, "cargo.exe", ["cargo", "test"], parent=20, gb=0.5)
    Proc(w, 22, "victory_gpu-1.exe", ["victory_gpu-1.exe"], parent=21, gb=10.0,
         exe="E:\\Gta6\\AnyXboxX-wt\\a\\third_party\\victory\\target\\debug\\deps\\victory_gpu-1.exe")
    Proc(w, 30, "python.exe", ["python", "E:/Gta6/AnyXboxX-wt/b/tools/dev.py", "build"], parent=1, gb=0.1)
    Proc(w, 31, "cargo.exe", ["cargo", "build", "--release"], parent=30, gb=0.5)
    Proc(w, 32, "rustc.exe", ["rustc"], parent=31, gb=3.0)
    Proc(w, 33, "rustc.exe", ["rustc"], parent=31, gb=3.0)
    Proc(w, 40, "bash.exe", ["bash"], parent=1)
    Proc(w, 41, "cargo.exe", ["cargo", "test", "-p", "victory-gpu"], parent=40, gb=0.5)
    Proc(w, 42, "rustc.exe", ["rustc"], parent=41, gb=2.0)
    return w


def test_build_trees_group_by_writer_and_spare_the_operator():
    w = machine()
    trees = dev_throttle.build_trees(w, me=10)
    assert [(t.owner, t.root) for t in trees] == [(20, 20), (30, 30), (None, 41)]
    assert [round(t.commit_gb, 1) for t in trees] == [10.5, 6.5, 2.5]
    assert all(11 not in [p.pid for p in t.procs] for t in trees)


def test_make_room_waits_then_preempts_the_biggest_and_marks_its_dev_py(tmp_path, monkeypatch):
    monkeypatch.setenv("ANYXBOXX_BUILD_SLOTS", "2")
    w = machine()
    limit = 50.0
    free = lambda: limit - 20.0 - w.committed()  # noqa: E731 (the game's 0 and Windows' 20 GB)
    lines, sleeps = [], []
    room = dev_throttle.make_room(tmp_path, "train", need_gb=12.0, wait_s=0.0, psutil_module=w, free_fn=free,
                                  sleep=sleeps.append, out=lines.append)
    assert room.preempted == 1 and room.free_gb >= 12.0
    assert all(w.procs[p].killed for p in (21, 22)) and not w.procs[20].killed, "the dev.py lives on to retry"
    assert not any(w.procs[p].killed for p in (11, 31, 32, 33, 41, 42))
    marker = json.loads(dev_throttle.preempt_marker(tmp_path, 20).read_text(encoding="utf-8"))
    assert marker["by"] == "train" and marker["owner"] == 20 and marker["commit_gb"] == 10.5
    log = (tmp_path / "preempted.log").read_text(encoding="utf-8").splitlines()
    assert len(log) == 1 and json.loads(log[0])["root"] == 20
    assert any("preempting" in line for line in lines)
    # The slots it held while it decided are given back.
    with dev_throttle.all_build_slots(tmp_path, "after", timeout_s=0.3):
        pass


def test_make_room_waits_for_builds_that_finish_and_preempts_nothing(tmp_path, monkeypatch):
    monkeypatch.setenv("ANYXBOXX_BUILD_SLOTS", "2")
    w = machine()
    free = lambda: 50.0 - 20.0 - w.committed()  # noqa: E731
    held_during = []

    def sleep(_s):
        # The build slots are held while it waits: no new build starts.
        with pytest.raises(TimeoutError):
            with dev_throttle.build_slot(tmp_path, "writer", timeout_s=0.1):
                pass
        held_during.append(True)
        for pid in (21, 22):  # the gate's tests end by themselves
            w.procs[pid].killed = True

    room = dev_throttle.make_room(tmp_path, "bench", need_gb=12.0, wait_s=60.0, psutil_module=w, free_fn=free,
                                  sleep=sleep, out=lambda line: None)
    assert room.preempted == 0 and held_during == [True] and room.free_gb >= 12.0
    assert not (tmp_path / "preempted.log").exists()


def test_make_room_does_nothing_with_enough_commit(tmp_path):
    room = dev_throttle.make_room(tmp_path, "bench", need_gb=40.0, psutil_module=object(), free_fn=lambda: 41.0,
                                  sleep=pytest.fail, out=pytest.fail)
    assert (room.free_gb, room.waited_s, room.preempted) == (41.0, 0.0, 0)


def test_game_commit_threshold_comes_from_the_environment(monkeypatch):
    monkeypatch.setenv("ANYXBOXX_GAME_COMMIT_GB", "33")
    assert dev_throttle.game_commit_gb() == 33.0
    monkeypatch.delenv("ANYXBOXX_GAME_COMMIT_GB")
    assert dev_throttle.game_commit_gb() == dev_throttle.GAME_COMMIT_GB


def test_a_preempted_command_waits_and_retries(tmp_path, monkeypatch, capsys):
    dev = load_dev()
    monkeypatch.setattr(dev, "LOCKS_DIR", tmp_path)
    monkeypatch.setattr(dev, "ensure_coord_dirs", lambda: None)
    monkeypatch.setattr(dev.dev_throttle, "disk_free_gb", lambda _path: 500.0)
    monkeypatch.setattr(dev.dev_throttle, "below_normal_priority", lambda: None)
    waits = []
    monkeypatch.setattr(dev, "wait_for_game_room", lambda name: waits.append(name) or 0.0)
    calls = []

    def body(_args):
        calls.append(1)
        if len(calls) == 1:
            marker = dev_throttle.preempt_marker(tmp_path, os.getpid())
            marker.parent.mkdir(parents=True, exist_ok=True)
            marker.write_text('{"by": "train"}\n', encoding="utf-8")
            print("DEV gate FAIL killed")
            return 1
        return 0

    assert dev.throttled("gate", whole=False)(body)(None) == 0
    assert len(calls) == 2 and waits == ["gate", "gate"]
    assert "a game run preempted this gate" in capsys.readouterr().out
    assert dev_throttle.take_preempted(tmp_path) is None, "the marker is used up"


def test_a_command_preempted_every_time_gives_up(tmp_path, monkeypatch, capsys):
    dev = load_dev()
    monkeypatch.setattr(dev, "LOCKS_DIR", tmp_path)
    monkeypatch.setattr(dev, "ensure_coord_dirs", lambda: None)
    monkeypatch.setattr(dev.dev_throttle, "disk_free_gb", lambda _path: 500.0)
    monkeypatch.setattr(dev.dev_throttle, "below_normal_priority", lambda: None)
    monkeypatch.setattr(dev, "wait_for_game_room", lambda name: 0.0)
    monkeypatch.setattr(dev, "build_slot", lambda name: contextlib.nullcontext)

    def body(_args):
        marker = dev_throttle.preempt_marker(tmp_path, os.getpid())
        marker.parent.mkdir(parents=True, exist_ok=True)
        marker.write_text("x\n", encoding="utf-8")
        return 0

    assert dev.throttled("build")(body)(None) == 1
    assert f"DEV build FAIL reason=preempted {dev.PREEMPT_RETRIES + 1} times" in capsys.readouterr().out


def test_a_build_slot_waits_for_a_short_game_run(tmp_path, monkeypatch):
    dev = load_dev()
    monkeypatch.setattr(dev, "LOCKS_DIR", tmp_path)
    monkeypatch.setenv("ANYXBOXX_BUILD_SLOTS", "1")
    frees = iter([30.0, 30.0, 60.0, 60.0])
    monkeypatch.setattr(dev.dev_throttle, "commit_free_gb", lambda: next(frees))
    monkeypatch.setattr(dev.dev_throttle, "COMMIT_POLL_S", 0.01)
    with dev_throttle.hold_first_free([tmp_path / "gpu.lock"], "operator", 1.0):
        with dev.build_slot("gate")():
            pass
    assert next(frees, None) is None
