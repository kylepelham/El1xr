"""tools/dev_visual.py: visual hints (ON snaps against OFF snaps above the OFF-vs-OFF noise floor)
and the mean colour of a reference rectangle in every run's snap."""

import importlib.util
import json
import random
import sys
from pathlib import Path

import pytest

TOOLS = Path(__file__).resolve().parents[1] / "tools"
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))

import dev_png
import dev_train
import dev_visual

W, H = 256, 192  # thumbs of 64x48, blocks of 8x6 (32x32 pixels each)


def load_dev():
    spec = importlib.util.spec_from_file_location("el1xr_dev_visual", TOOLS / "dev.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def scene(t, seed, paint=None):
    """A smooth scene that moves a little with `t`, with per-run noise; `paint` = (box, rgb)."""
    rng = random.Random(seed * 1000 + t)
    rows = []
    for y in range(H):
        row = bytearray()
        for x in range(W):
            base = (x + t) % 256 // 2 + y // 4
            n = rng.randrange(-6, 7)
            row += bytes((min(max(base + n, 0), 255), min(max(base // 2 + 40 + n, 0), 255), 90))
        rows.append(row)
    if paint:
        (x0, y0, x1, y1), rgb = paint
        for y in range(y0, y1):
            rows[y][x0 * 3 : x1 * 3] = bytes(rgb) * (x1 - x0)
    return dev_png.Image(W, H, [bytes(r) for r in rows])


def solid(rgb, w=W, h=H):
    return dev_png.Image(w, h, [bytes(rgb) * w] * h)


def test_features_count_black_blocks_blocky_tiles_and_colours():
    black = dev_visual.features(solid((0, 0, 0)))
    assert (black.bw, black.bh, black.black, black.blocky) == (8, 6, 1.0, 0)
    assert black.hist[0] == 1.0
    tiled = solid((200, 200, 200))
    rows = [bytearray(r) for r in tiled.rows]
    for y in range(64, 96):  # one flat black tile at block (3, 2), inside the grid
        rows[y][96 * 3 : 128 * 3] = bytes(32 * 3)
    f = dev_visual.features(dev_png.Image(W, H, [bytes(r) for r in rows]))
    assert f.blocky == 1
    assert f.black == pytest.approx(1 / 48)
    assert f.mean[2 * 8 + 3] == (0.0, 0.0, 0.0) and f.var[2 * 8 + 3] == 0.0
    assert dev_visual.compare(f, dev_visual.features(solid((0, 0, 0), w=128))) is None, "other grids"


def test_without_off_off_pairs_only_the_absolute_minimums_apply():
    floor = dev_visual.noise_floor([])
    assert floor.pairs == 0 and floor.limits == dev_visual.ABS_MIN
    assert floor.block_limit == dev_visual.BLOCK_ABS_MIN and floor.block_map(10) is None
    assert dev_visual.largest_region([True, True, False, False, False, True, False, True, True], 3) == [5, 7, 8]


def write_run(train_dir, step, name, paint_at=None):
    folder = train_dir / f"{step:02d}_{name}_snaps"
    folder.mkdir(parents=True)
    for t in (15, 30, 45, 60):
        paint = ((96, 64, 192, 160), (255, 0, 255)) if t == paint_at else None
        dev_png.write_png(folder / f"snap-{t:04d}s-f{t * 30}.png", scene(t + step % 2, step, paint))
    dev_train.save_result(train_dir, step, {"step": step, "set": name, "ok": True, "stats": {}, "snaps": 4})


def test_visual_flags_only_the_on_snap_that_differs_beyond_the_off_off_floor(tmp_path, monkeypatch, capsys):
    dev = load_dev()
    monkeypatch.setattr(dev, "DEV_DIR", tmp_path / "dev")
    plan = tmp_path / "b.toml"
    plan.write_text("[sets.on]\n[sets.off]\n", encoding="utf-8")
    train_dir = tmp_path / "dev" / "train_b"
    write_run(train_dir, 1, "on", paint_at=30)
    write_run(train_dir, 2, "off")
    write_run(train_dir, 3, "off")
    write_run(train_dir, 4, "on")
    assert dev.main(["train", "--plan", str(plan), "--visual"]) == 0
    out = capsys.readouterr().out
    assert "HINT, not a verdict" in out
    assert "DEV train-visual PASS pairs=8 flagged=1 off_off_pairs=4" in out
    summary = json.loads((train_dir / "visual" / "visual.json").read_text(encoding="utf-8"))
    (finding,) = summary["findings"]
    assert (finding["on"], finding["off"], finding["t_on"]) == ("01_on", "02_off", 30), "on 1 against off 2"
    assert finding["region_blocks"] == 9
    assert finding["box"] == [64, 32, 224, 192], "the painted 3x3 blocks and one block around them"
    crop = dev_png.read_png(Path(finding["crop"]))
    assert (crop.width, crop.height) == (160 * 2 + 8, 160), "ON | OFF crops of the box"
    assert crop.rows[60][60 * 3 : 61 * 3] == b"\xff\x00\xff", "the ON side shows the painted region"
    assert "| 01_on 30s | 02_off 30s |" in (train_dir / "visual" / "visual.md").read_text(encoding="utf-8")
    assert dev.main(["train", "--plan", str(plan), "--report"]) == 0
    assert "- 01_on 30s vs 02_off 30s: score" in capsys.readouterr().out
    assert dev.main(["train", "--plan", str(plan), "--visual", "04_on", "3"]) == 0
    assert "pairs=4 flagged=0" in capsys.readouterr().out
    assert dev.main(["train", "--plan", str(plan), "--visual", "04_on"]) == 1
    assert dev.main(["train", "--plan", str(plan), "--visual", "04_on", "9"]) == 1
    assert "DEV train-visual FAIL reason=run" in capsys.readouterr().out


def test_a_noisy_floor_is_called_weak():
    floor = dev_visual.Floor(3, {**dev_visual.ABS_MIN, "diff": 120.0}, 30.0)
    assert any(line.startswith("**Weak hints**") for line in dev_visual.report_lines([], floor))


def test_region_reports_the_mean_colour_of_a_reference_rectangle_in_each_runs_nearest_snap(tmp_path, monkeypatch, capsys):
    dev = load_dev()
    monkeypatch.setattr(dev, "DEV_DIR", tmp_path / "dev")
    train_dir = tmp_path / "dev" / "train_b"
    reference = solid((10, 10, 10), w=2 * W, h=2 * H)  # twice the snaps' size
    rows = [bytearray(r) for r in reference.rows]
    for y in range(100, 200):
        rows[y][200 * 3 : 300 * 3] = bytes((128, 128, 128)) * 100  # the grey hoodie
    ref = tmp_path / "ref.png"
    dev_png.write_png(ref, dev_png.Image(2 * W, 2 * H, [bytes(r) for r in rows]))
    for step, name, rgb in ((1, "on", (20, 20, 20)), (2, "off", (126, 128, 130))):
        folder = train_dir / f"{step:02d}_{name}_snaps"
        folder.mkdir(parents=True)
        image = solid((0, 0, 0))
        hoodie = [bytearray(r) for r in image.rows]
        for y in range(50, 100):
            hoodie[y][100 * 3 : 150 * 3] = bytes(rgb) * 50
        dev_png.write_png(folder / "snap-0120s-f3600.png", dev_png.Image(W, H, [bytes(r) for r in hoodie]))
        dev_png.write_png(folder / "snap-0015s-f450.png", solid((255, 255, 255)))
        dev_train.save_result(train_dir, step, {"step": step, "set": name, "ok": True, "stats": {}})
    plan = tmp_path / "b.toml"
    plan.write_text("[sets.on]\n[sets.off]\n", encoding="utf-8")
    args = ["train", "--plan", str(plan), "--region", str(ref), "200", "100", "300", "200"]
    assert dev.main([*args, "--at", "110"]) == 0
    out = capsys.readouterr().out
    assert "#808080 (128, 128, 128) luma 128" in out
    assert "| 01_on | 110s | 120s | #141414 (20, 20, 20) luma 20 | 187 |" in out, "black, far from grey"
    assert "| 02_off | 110s | 120s | #7e8082 (126, 128, 130) luma 128 | 3 |" in out
    strip = dev_png.read_png(train_dir / "visual" / "region-200_100_300_200-110s.png")
    assert (strip.width, strip.height) == (100 + 8 + 50 + 8 + 50, 100)
    assert dev.main(args) == 1, "--at is needed"
    assert dev.main([*args[:-1], "999", "--at", "1"]) == 1, "outside the reference"
    assert "DEV train-region FAIL reason=args" in capsys.readouterr().out
    assert dev_visual.scale_box((0, 0, 1, 1), (100, 100), (10, 10)) == (0, 0, 1, 1), "at least a pixel"
