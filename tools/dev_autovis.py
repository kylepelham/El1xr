"""Automatic visual diff for `dev.py train` (after every `--all` arm, or `train --autovis`).

Every run with snaps is set against a baseline run (the baseline set's run nearest in the order,
the earlier on a tie). Each snap is matched to the baseline snap nearest in time since the first
present (fast-boot runs line up near the start); when another baseline snap within MATCH_WINDOW_S
has a colour histogram closer by more than HIST_MARGIN (the times drifted), that one is used.

A pair is measured after a small blur (Gaussian, BLUR_RADIUS pixels): the mean absolute RGB
difference of every BLOCK x BLOCK block (0-255), the image's score (the mean of its blocks) and
its flagged blocks (over BLOCK_LIMIT). A pair is flagged when its score is over SCORE_LIMIT or its
flagged blocks are over FLAGGED_SHARE of the frame; flagged pairs get a sheet
`autovis/<arm>_vs_<baseline>-<t>s.png`: baseline | arm | heatmap, the worst block groups outlined
(thin, square: sunset on the arm, light on the heatmap).
Later baseline runs are also set against the first one (`noise`: what a scene moving between runs
alone gives). Every snap of every run is checked for a fully black frame (every pixel 0) and for a
black region (connected blocks of all-zero pixels) of BLACK_REGION_SHARE of the frame or more.

Results: `autovis/<run>.json` per run (kept while its snaps and baseline do not change) and
`autovis/visual.json` over all. Needs numpy and Pillow (the coord venv has both); without them the
caller skips it.
"""

from __future__ import annotations

import json
from collections import deque
from pathlib import Path

import dev_train

try:
    import numpy as np
    from PIL import Image, ImageDraw, ImageFilter
except ImportError:  # pragma: no cover - the coord venv has both
    np = None

BLOCK = 16
BLUR_RADIUS = 1.0
BLOCK_LIMIT = 40.0
SCORE_LIMIT = 8.0
FLAGGED_SHARE = 0.02
MATCH_WINDOW_S = 30
HIST_MARGIN = 0.05
HIST_STEP = 4
BLACK_REGION_SHARE = 0.10
PANEL_W = 960
HEADER_H = 22
GAP = 4
BOXES = 5
OUTLINE = (255, 140, 60)
BACKGROUND = (24, 18, 40)
TEXT = (235, 232, 245)
SCHEMA = 1


def missing() -> str | None:
    """What is missing to run the visual diff, or None."""
    return None if np is not None else "numpy and Pillow are not importable"


def load(path: Path):
    with Image.open(path) as im:
        return np.asarray(im.convert("RGB"))


def histogram(arr) -> object:
    small = arr[::HIST_STEP, ::HIST_STEP] >> 5
    idx = (small[..., 0].astype(np.int32) << 6) | (small[..., 1].astype(np.int32) << 3) | small[..., 2]
    counts = np.bincount(idx.ravel(), minlength=512).astype(np.float64)
    return counts / max(counts.sum(), 1.0)


def hist_distance(a, b) -> float:
    return float(0.5 * np.abs(a - b).sum())


def blurred(arr):
    return np.asarray(Image.fromarray(arr).filter(ImageFilter.GaussianBlur(BLUR_RADIUS)), dtype=np.int16)


def block_diff(a, b):
    """The mean absolute RGB difference of every BLOCK x BLOCK block of `a` against `b` (resized to `a`'s size when
    they differ), after the blur: an array of (rows, columns)."""
    if a.shape != b.shape:
        b = np.asarray(Image.fromarray(b).resize((a.shape[1], a.shape[0]), Image.BILINEAR))
    diff = np.abs(blurred(a) - blurred(b)).mean(axis=2)
    rows, cols = diff.shape[0] // BLOCK, diff.shape[1] // BLOCK
    if rows == 0 or cols == 0:
        return np.array([[float(diff.mean())]])
    return diff[: rows * BLOCK, : cols * BLOCK].reshape(rows, BLOCK, cols, BLOCK).mean(axis=(1, 3))


def groups(mask) -> list[tuple[int, int, int, int, int]]:
    """4-connected groups of True cells: (cells, row0, col0, row1, col1), largest first."""
    seen = np.zeros_like(mask, dtype=bool)
    out = []
    rows, cols = mask.shape
    for r0, c0 in zip(*np.nonzero(mask)):
        if seen[r0, c0]:
            continue
        seen[r0, c0] = True
        queue, cells = deque([(r0, c0)]), 0
        top, left, bottom, right = r0, c0, r0, c0
        while queue:
            r, c = queue.popleft()
            cells += 1
            top, left, bottom, right = min(top, r), min(left, c), max(bottom, r), max(right, c)
            for rr, cc in ((r - 1, c), (r + 1, c), (r, c - 1), (r, c + 1)):
                if 0 <= rr < rows and 0 <= cc < cols and mask[rr, cc] and not seen[rr, cc]:
                    seen[rr, cc] = True
                    queue.append((rr, cc))
        out.append((cells, int(top), int(left), int(bottom), int(right)))
    return sorted(out, reverse=True)


def black_info(arr) -> dict[str, object] | None:
    """A fully black frame or a large black region, or None."""
    if not arr.any():
        return {"kind": "black", "share": 1.0}
    bh, bw = arr.shape[0] // BLOCK, arr.shape[1] // BLOCK
    if bh == 0 or bw == 0:
        return None
    zero = arr[: bh * BLOCK, : bw * BLOCK].reshape(bh, BLOCK, bw, BLOCK, 3).max(axis=(1, 3, 4)) == 0
    found = groups(zero)
    share = found[0][0] / zero.size if found else 0.0
    if share >= BLACK_REGION_SHARE:
        _, top, left, bottom, right = found[0]
        box = [left * BLOCK, top * BLOCK, (right + 1) * BLOCK, (bottom + 1) * BLOCK]
        return {"kind": "black_region", "share": round(share, 3), "box": box}
    return None


def match(arm: list[tuple[int, Path]], base: list[tuple[int, Path]], hists: dict[Path, object]) -> list[tuple[int, Path, int, Path, str]]:
    """Each arm snap with its baseline snap: nearest in time, or the closest histogram within MATCH_WINDOW_S."""
    out = []
    if not base:
        return out
    for t, path in arm:
        u, near = min(base, key=lambda b: (abs(b[0] - t), b[0]))
        near_d = hist_distance(hists[path], hists[near])
        window = [b for b in base if abs(b[0] - t) <= MATCH_WINDOW_S]
        best = min(window, key=lambda b: (hist_distance(hists[path], hists[b[1]]), abs(b[0] - t)))
        if best[1] != near and hist_distance(hists[path], hists[best[1]]) < near_d - HIST_MARGIN:
            out.append((t, path, best[0], best[1], "histogram"))
        else:
            out.append((t, path, u, near, "time"))
    return out


def _panel(arr, width: int):
    im = Image.fromarray(arr)
    scale = width / im.width
    return im.resize((width, max(1, round(im.height * scale))), Image.BILINEAR), scale


def heat_image(diff, size: tuple[int, int]):
    level = np.clip(diff / (2 * BLOCK_LIMIT), 0.0, 1.0)[..., None]
    rgb = (np.array(BACKGROUND) * (1 - level) + np.array(OUTLINE) * level).astype(np.uint8)
    return Image.fromarray(rgb).resize(size, Image.NEAREST)


def sheet(base_arr, arm_arr, diff, title: str, out: Path) -> None:
    """baseline | arm | heatmap, the BOXES largest groups of flagged blocks outlined (thin, square) on the arm and
    the heatmap, a one-line header."""
    left, scale = _panel(base_arr, PANEL_W)
    mid, _ = _panel(arm_arr, PANEL_W)
    heat = heat_image(diff, mid.size)
    w, h = PANEL_W * 3 + GAP * 2, mid.height + HEADER_H
    canvas = Image.new("RGB", (w, h), BACKGROUND)
    for i, panel in enumerate((left, mid, heat)):
        canvas.paste(panel, (i * (PANEL_W + GAP), HEADER_H))
    draw = ImageDraw.Draw(canvas)
    draw.text((6, 5), title, fill=TEXT)
    px = BLOCK * scale
    for _, top, lft, bottom, right in groups(diff > BLOCK_LIMIT)[:BOXES]:
        for x0, colour in ((PANEL_W + GAP, OUTLINE), (2 * (PANEL_W + GAP), TEXT)):
            draw.rectangle(
                (x0 + lft * px, HEADER_H + top * px, x0 + (right + 1) * px - 1, HEADER_H + (bottom + 1) * px - 1),
                outline=colour, width=1,
            )
    out.parent.mkdir(parents=True, exist_ok=True)
    canvas.save(out)


def compare_runs(arm_rid: str, arm_dir: Path, base_rid: str, base_dir: Path, out_dir: Path, sheets: bool = True) -> dict[str, object]:
    """The pairs of one run's snaps against a baseline run's (and their sheets when flagged), and the arm's black
    frames."""
    arm, base = dev_train.snaps(arm_dir), dev_train.snaps(base_dir)
    base_images: dict[Path, object] = {}

    def base_image(path: Path):
        if path not in base_images:
            base_images[path] = load(path)
        return base_images[path]

    hists = {p: histogram(load(p)) for _, p in arm}
    hists.update({p: histogram(base_image(p)) for _, p in base})
    pairs, black = [], []
    for old in out_dir.glob(f"{arm_rid}_vs_*.png") if out_dir.is_dir() else []:
        old.unlink()
    matched = match(arm, base, hists) if base else [(t, p, 0, None, "") for t, p in arm]
    for t, pa, u, pb, how in matched:
        arm_image = load(pa)
        info = black_info(arm_image)
        if info:
            black.append({"run": arm_rid, "t": t, **info})
        if pb is None:
            continue
        diff = block_diff(arm_image, base_image(pb))
        score = float(diff.mean())
        flagged_blocks = int((diff > BLOCK_LIMIT).sum())
        flagged = score > SCORE_LIMIT or flagged_blocks > FLAGGED_SHARE * diff.size
        entry = {
            "arm": arm_rid, "baseline": base_rid, "t_arm": t, "t_base": u, "matched_by": how,
            "score": round(score, 2), "flagged_blocks": flagged_blocks, "blocks": int(diff.size), "flagged": flagged,
            "sheet": None,
        }
        if flagged and sheets:
            path = out_dir / f"{arm_rid}_vs_{base_rid}-{t:04d}s.png"
            title = (f"baseline {base_rid} {u}s | arm {arm_rid} {t}s ({how}) | score {score:.1f}, "
                     f"{flagged_blocks} blocks over {BLOCK_LIMIT:g}")
            sheet(base_image(pb), arm_image, diff, title, path)
            entry["sheet"] = str(path)
        pairs.append(entry)
    return {"arm": arm_rid, "baseline": base_rid, "pairs": pairs, "black": black}


def _signature(*folders: Path) -> list[str]:
    return [f"{f.name}/{p.name}:{p.stat().st_size}" for f in folders for _, p in dev_train.snaps(f)]


def update(plan: dev_train.Plan, done: dict[int, dict[str, object]], train_dir: Path) -> dict[str, object]:
    """Every run with snaps against its baseline run (cached per run), then `autovis/visual.json`."""
    out_dir = train_dir / "autovis"
    out_dir.mkdir(parents=True, exist_ok=True)
    rid = {s: dev_train.run_id(s, str(done[s]["set"])) for s in done}
    folder = {s: train_dir / f"{rid[s]}_snaps" for s in done}
    with_snaps = [s for s in sorted(done) if dev_train.snaps(folder[s])]
    base = [s for s in with_snaps if done[s]["set"] == plan.baseline]
    pairs, noise, black = [], [], []
    for s in with_snaps:
        if done[s]["set"] == plan.baseline:
            other = base[0] if base[0] != s else None
        else:
            other = min(base, key=lambda b: (abs(b - s), b)) if base else None
        sig = {"schema": SCHEMA, "snaps": _signature(folder[s], *([folder[other]] if other else []))}
        cache = out_dir / f"{rid[s]}.json"
        result = json.loads(cache.read_text(encoding="utf-8")) if cache.exists() else None
        if not result or result.get("sig") != sig:
            is_noise = done[s]["set"] == plan.baseline
            result = compare_runs(
                rid[s], folder[s], rid[other] if other else "", folder[other] if other else out_dir / "none", out_dir,
                sheets=not is_noise,
            )
            result["sig"] = sig
            cache.write_text(json.dumps(result, indent=2), encoding="utf-8")
        (noise if done[s]["set"] == plan.baseline else pairs).extend(result["pairs"])
        black.extend(result["black"])
    summary = {
        "thresholds": {"block": BLOCK, "blur_radius": BLUR_RADIUS, "block_limit": BLOCK_LIMIT, "score_limit": SCORE_LIMIT,
                       "flagged_share": FLAGGED_SHARE, "black_region_share": BLACK_REGION_SHARE},
        "pairs": pairs, "flagged": sum(1 for p in pairs if p["flagged"]), "noise": noise, "black": black,
    }
    (out_dir / "visual.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
    return summary


def arm_entries(summary: dict[str, object], rid: str) -> tuple[list[dict[str, object]], list[dict[str, object]]]:
    """An arm's flagged pairs (worst first) and black frames."""
    flagged = sorted((p for p in summary.get("pairs", []) if p["arm"] == rid and p["flagged"]), key=lambda p: -p["score"])
    return flagged, [b for b in summary.get("black", []) if b["run"] == rid]


def summary_lines(summary: dict[str, object]) -> list[str]:
    noise = [p["score"] for p in summary["noise"]]
    lines = [f"noise (baseline vs baseline): {len(noise)} pairs, score max {max(noise):.1f}" if noise else
             "noise: no second baseline run with snaps"]
    for p in sorted((p for p in summary["pairs"] if p["flagged"]), key=lambda p: -p["score"]):
        lines.append(f"flagged {p['arm']} {p['t_arm']}s vs {p['baseline']} {p['t_base']}s ({p['matched_by']}): "
                     f"score {p['score']:.1f}, {p['flagged_blocks']}/{p['blocks']} blocks  {p['sheet']}")
    for b in summary["black"]:
        lines.append(f"black {b['run']} {b['t']}s: {b['kind']} {b['share']:.0%}")
    return lines
