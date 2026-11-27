"""Tests for fastboot helpers in tools/dev.py (`--save`, `--title-args`, `--skip-intro`, `boot_s`)."""

import importlib.util
from pathlib import Path

import pytest

DEV_PATH = Path(__file__).resolve().parents[1] / "tools" / "dev.py"


def load_dev():
    spec = importlib.util.spec_from_file_location("el1xr_dev_fastboot", DEV_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def test_normalize_argv_and_parse_title_args_handle_leading_dashes():
    dev = load_dev()
    parser = dev.build_parser()

    raw = ["bench", "--title-args", "-skipToIIS", "--title-args", "-foo=1 -bar", "--skip-intro", "--save", "ckpt1"]
    args = parser.parse_args(dev.normalize_argv(raw))
    assert args.skip_intro is True
    assert args.save == "ckpt1"
    assert dev.parse_title_args(args.title_args) == ["-skipToIIS", "-foo=1", "-bar"]


def test_isolated_save_data_dir_protects_original_snapshot_and_cleans_up(tmp_path):
    dev = load_dev()
    saves_dir = tmp_path / "saves"
    snap = saves_dir / "default_checkpoint" / "7952987A" / "saves"
    snap.mkdir(parents=True)
    save_file = snap / "SARC0015"
    save_file.write_bytes(b"original-save-bytes")

    with dev.isolated_save_data_dir("default_checkpoint", "bench", saves_dir=saves_dir, work_root=tmp_path) as run_dir:
        assert run_dir is not None and run_dir.is_dir()
        copied = run_dir / "7952987A" / "saves" / "SARC0015"
        assert copied.read_bytes() == b"original-save-bytes"
        copied.write_bytes(b"mutated-during-run")

    assert not run_dir.exists()
    assert save_file.read_bytes() == b"original-save-bytes"


def test_resolve_save_dir_rejects_traversal_and_missing_names(tmp_path):
    dev = load_dev()
    with pytest.raises(ValueError):
        dev.resolve_save_dir("../escape", saves_dir=tmp_path)
    with pytest.raises(FileNotFoundError):
        dev.resolve_save_dir("nonexistent", saves_dir=tmp_path)


def test_build_launcher_command_emits_skip_intro_data_dir_and_title_args():
    dev = load_dev()
    cmd = dev.build_launcher_command(
        Path("E:/Gta6/Game"),
        diag=False,
        overrides={"VICTORY_MUTE": "1"},
        mute=True,
        skip_intro=True,
        data_dir=Path("E:/Gta6/run_data/test"),
        title_args=["-skipToIIS", "-flag=1"],
    )
    ps_cmd = cmd[-1]
    assert "-SkipIntro" in ps_cmd
    assert "-DataDir 'E:\\Gta6\\run_data\\test'" in ps_cmd or "-DataDir 'E:/Gta6/run_data/test'" in ps_cmd
    assert "-TitleArgs @('-skipToIIS', '-flag=1')" in ps_cmd


def test_parse_victory_log_reports_boot_seconds_to_first_world_frame():
    dev = load_dev()
    log = "\n".join(
        [
            "2026-11-26T23:00:00.000000Z  INFO victory_loader::windows: image mapped",
            "2026-11-26T23:00:12.500000Z  INFO victory_gpu::host: 30.0 fps, 33.3 ms/frame (1.0 preparing: copies 0.1 ms 0.1 MB, textures 0.2 ms 1.0 MB; 0.0 on the GPU; 0.0 copying back 0.0 MB), 12 draws/frame, dropped 0 without a host pipeline (0 being made) and 0 indirect",
            "2026-11-26T23:00:51.250000Z  INFO victory_gpu::host: 15.0 fps, 66.7 ms/frame (20.0 preparing: copies 5.0 ms 50.0 MB, textures 2.0 ms 4.0 MB; 5.0 on the GPU; 2.0 copying back 3.0 MB), 1200 draws/frame, dropped 0 without a host pipeline (0 being made) and 0 indirect",
        ]
    )
    stats = dev.parse_victory_log(log)
    assert stats["world_samples"] == 1
    assert stats["boot_s"] == pytest.approx(51.25)


def test_fast_boot_is_opt_in_and_sets_skip_intro_save_and_menu_cadence():
    dev = load_dev()
    parser = dev.build_parser()
    for sub in ("bench", "capture"):
        plain = parser.parse_args([sub])
        assert dev.apply_fast_boot(plain, {}) == {}
        assert plain.skip_intro is False and plain.save is None

        fast = parser.parse_args([sub, "--fast-boot"])
        overrides = dev.apply_fast_boot(fast, {})
        assert fast.skip_intro is True and fast.save == "default_checkpoint"
        assert overrides == {"VICTORY_AUTO_A_PERIOD_MS": "500"}

        mine = parser.parse_args([sub, "--fast-boot", "--save", "other"])
        kept = dev.apply_fast_boot(mine, {"VICTORY_AUTO_A_PERIOD_MS": "1000"})
        assert mine.save == "other" and kept == {"VICTORY_AUTO_A_PERIOD_MS": "1000"}


def pacing(ts: str, p50: float, draws: int) -> str:
    return (
        f"{ts}  INFO victory_gpu::pacing: frame pacing: 100 frames in 5.0 s (20.0 fps), frame time p50 {p50} ms "
        f"p95 60.0 ms p99 70.0 ms max 80.0 ms, 0 over 50 ms, 0 over 100 ms, "
        f"present interval sd 1.0 ms jitter 1.0 ms; {draws} draws/frame"
    )


def test_settled_s_is_the_first_world_window_after_the_last_loading_one():
    dev = load_dev()
    log = "\n".join(
        [
            "2026-11-26T23:00:00.000000Z  INFO victory_loader::windows: image mapped",
            pacing("2026-11-26T23:00:10.000000Z", 40.0, 12),
            pacing("2026-11-26T23:00:40.000000Z", 300.0, 1500),
            pacing("2026-11-26T23:00:45.000000Z", 40.0, 1500),
            pacing("2026-11-26T23:00:50.000000Z", 250.0, 1500),
            pacing("2026-11-26T23:00:55.000000Z", 35.0, 1500),
            pacing("2026-11-26T23:01:00.000000Z", 34.0, 1500),
        ]
    )
    stats = dev.parse_victory_log(log)
    assert stats["boot_s"] == pytest.approx(40.0)
    assert stats["settled_s"] == pytest.approx(55.0)
    assert dev.parse_victory_log(pacing("2026-11-26T23:00:00.000000Z", 300.0, 1500))["settled_s"] == 0.0
