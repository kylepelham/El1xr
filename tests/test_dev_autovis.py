"""tools/dev_autovis.py: the automatic visual diff of train arms' snaps (synthetic PNGs, numpy and Pillow from
the coord venv; skipped without them)."""

import json
import sys
from pathlib import Path

import pytest

np = pytest.importorskip("numpy")
Image = pytest.importorskip("PIL.Image")

TOOLS = Path(__file__).resolve().parents[1] / "tools"
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))

import dev_autovis
import dev_train

W, H = 256, 128


def scene(seed, red_box=False, black_rect=None, all_black=False):
    y, x = np.mgrid[0:H, 0:W]
    img = np.stack([(x + seed * 40) % 256, (y * 2 + seed * 17) % 256, ((x + y) // 2 + seed * 90) % 256], axis=2)
    img = img.astype(np.uint8)
    if red_box:
        img[32:96, 96:160] = (255, 0, 0)
    if black_rect:
        y0, y1, x0, x1 = black_rect
        img[y0:y1, x0:x1] = 0
    if all_black:
        img[:] = 0
    return img


def write(folder, t, img):
    folder.mkdir(parents=True, exist_ok=True)
    Image.fromarray(img).save(folder / f"snap-{t:04d}s-f{t * 25}.png")


def test_matching_takes_the_nearest_time_unless_another_snap_looks_much_closer(tmp_path):
    base = tmp_path / "base"
    for t, seed in [(15, 1), (30, 2), (45, 3)]:
        write(base, t, scene(seed))
    arm = tmp_path / "arm"
    write(arm, 15, scene(1))
    write(arm, 31, scene(3))  # the times drifted: this is the scene of 45 s
    a, b = dev_train.snaps(arm), dev_train.snaps(base)
    hists = {p: dev_autovis.histogram(dev_autovis.load(p)) for _, p in [*a, *b]}
    got = [(t, u, how) for t, _, u, _, how in dev_autovis.match(a, b, hists)]
    assert got == [(15, 15, "time"), (31, 45, "histogram")]


def test_a_changed_region_is_flagged_with_a_sheet_and_black_frames_are_found(tmp_path):
    base, arm, out = tmp_path / "02_off_snaps", tmp_path / "01_on_snaps", tmp_path / "autovis"
    for t in (15, 30, 45):
        write(base, t, scene(t))
    write(arm, 15, scene(15))
    write(arm, 30, scene(30, red_box=True))
    write(arm, 45, scene(45, all_black=True))
    result = dev_autovis.compare_runs("01_on", arm, "02_off", base, out)
    pairs = {p["t_arm"]: p for p in result["pairs"]}
    assert pairs[15]["score"] == 0.0 and not pairs[15]["flagged"] and pairs[15]["sheet"] is None
    assert pairs[30]["flagged"] and pairs[30]["flagged_blocks"] >= 9
    assert pairs[30]["blocks"] == (W // 16) * (H // 16)
    sheet = Path(pairs[30]["sheet"])
    assert sheet.name == "01_on_vs_02_off-0030s.png"
    with Image.open(sheet) as im:
        assert im.size == (3 * dev_autovis.PANEL_W + 2 * dev_autovis.GAP, H * dev_autovis.PANEL_W // W + dev_autovis.HEADER_H)
        px = np.asarray(im.convert("RGB"))
    assert (px == dev_autovis.OUTLINE).all(axis=2).any(), "the worst blocks are outlined"
    assert result["black"] == [{"run": "01_on", "t": 45, "kind": "black", "share": 1.0}]
    region = dev_autovis.black_info(scene(1, black_rect=(0, 64, 0, 128)))
    assert region["kind"] == "black_region" and region["share"] == 0.25 and region["box"] == [0, 0, 128, 64]
    assert dev_autovis.black_info(scene(1, black_rect=(0, 16, 0, 16))) is None, "one black block is not a region"


def test_update_sets_every_run_against_its_baseline_caches_and_writes_visual_json(tmp_path, monkeypatch):
    plan = dev_train.parse_plan("p", {"sets": {"on": {}, "off": {}}})
    done = {s: {"step": s, "set": n, "ok": True} for s, n in enumerate(plan.order, 1)}
    for s, name in enumerate(plan.order, 1):
        folder = tmp_path / f"{dev_train.run_id(s, name)}_snaps"
        write(folder, 15, scene(15))
        write(folder, 30, scene(30, red_box=(name == "on")))
    summary = dev_autovis.update(plan, done, tmp_path)
    assert {(p["arm"], p["baseline"]) for p in summary["pairs"]} == {("01_on", "02_off"), ("04_on", "03_off")}
    assert summary["flagged"] == 2 and len(summary["noise"]) == 2, "03_off against 02_off"
    assert all(not p["flagged"] for p in summary["noise"])
    data = json.loads((tmp_path / "autovis" / "visual.json").read_text(encoding="utf-8"))
    assert data["flagged"] == 2
    flagged, black = dev_autovis.arm_entries(data, "04_on")
    assert [p["t_arm"] for p in flagged] == [30] and black == []
    monkeypatch.setattr(dev_autovis, "compare_runs", lambda *a, **k: pytest.fail("cached runs are not measured again"))
    assert dev_autovis.update(plan, done, tmp_path)["flagged"] == 2
    lines = dev_autovis.summary_lines(summary)
    assert lines[0].startswith("noise (baseline vs baseline): 2 pairs")
    assert any(ln.startswith("flagged 01_on 30s vs 02_off 30s (time)") for ln in lines)
