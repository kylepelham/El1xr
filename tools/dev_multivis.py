"""Visual check of a multilive launch (tools/dev_multilive.py), per arm and segment.

For every arm run in the launch: a strip `autovis/<run>/<arm>-strip.png` with one row per segment (side, time,
up to STRIP_SNAPS of its snaps evenly picked), each B snap set against the A snap of the same arm nearest in time
(dev_autovis.block_diff; a sheet `autovis/<run>/<arm>-B<t>s_vs_A<t>s.png` when dev_autovis flags it), and black
frames per side (dev_autovis.black_info). The noise reference is the median score of consecutive snaps of one
segment (same values, the scene moving alone): a B-vs-A median well above it points at the switch. Results in
`autovis/<run>/multilive.json`, kept while the run's snaps do not change.
"""

from __future__ import annotations

import json
import re
import statistics
from pathlib import Path

import dev_autovis

THUMB_W = 320
STRIP_SNAPS = 4
LABEL_W = 150
NAME_RE = re.compile(r"^snap-(\d+)s-f\d+\.png$")
SCHEMA = 1


def _t(name: str) -> int:
    m = NAME_RE.match(name)
    return int(m.group(1)) if m else 0


def _picked(names: list[str], count: int) -> list[str]:
    if len(names) <= count:
        return names
    return [names[round(i * (len(names) - 1) / (count - 1))] for i in range(count)]


def _safe(name: str) -> str:
    return re.sub(r"[^A-Za-z0-9_.=-]+", "_", name)[:80]


def strip(rows: list[tuple[str, list[Path]]], title: str, out: Path) -> None:
    Image, ImageDraw = dev_autovis.Image, dev_autovis.ImageDraw
    thumbs = [[dev_autovis._panel(dev_autovis.load(p), THUMB_W)[0] for p in paths] for _, paths in rows]
    th = max((t.height for row in thumbs for t in row), default=THUMB_W * 9 // 16)
    w = LABEL_W + STRIP_SNAPS * (THUMB_W + dev_autovis.GAP)
    h = dev_autovis.HEADER_H + len(rows) * (th + dev_autovis.GAP)
    canvas = Image.new("RGB", (w, h), dev_autovis.BACKGROUND)
    draw = ImageDraw.Draw(canvas)
    draw.text((6, 5), title, fill=dev_autovis.TEXT)
    for r, ((label, _), row) in enumerate(zip(rows, thumbs)):
        y = dev_autovis.HEADER_H + r * (th + dev_autovis.GAP)
        draw.text((6, y + 6), label, fill=dev_autovis.TEXT)
        for c, im in enumerate(row):
            canvas.paste(im, (LABEL_W + c * (THUMB_W + dev_autovis.GAP), y))
    out.parent.mkdir(parents=True, exist_ok=True)
    canvas.save(out)


def arm_visual(rid: str, arm: str, result: dict[str, object], snap_dir: Path, out_dir: Path) -> dict[str, object]:
    segments = [s for s in result["segments"] if s.get("snaps")]
    images: dict[str, object] = {}

    def image(name: str):
        if name not in images:
            images[name] = dev_autovis.load(snap_dir / name)
        return images[name]

    side = {"A": [], "B": []}
    noise, black = [], {"A": 0, "B": 0}
    for seg in segments:
        names = [n for n in seg["snaps"] if (snap_dir / n).exists()]
        side[seg["label"]] += [(_t(n), n) for n in names]
        for n in names:
            if dev_autovis.black_info(image(n)):
                black[seg["label"]] += 1
        for prev, cur in zip(names, names[1:]):
            noise.append(float(dev_autovis.block_diff(image(cur), image(prev)).mean()))
    pairs = []
    for t, name in side["B"]:
        if not side["A"]:
            break
        u, other = min(side["A"], key=lambda a: (abs(a[0] - t), a[0]))
        diff = dev_autovis.block_diff(image(name), image(other))
        score, blocks = float(diff.mean()), int((diff > dev_autovis.BLOCK_LIMIT).sum())
        flagged = score > dev_autovis.SCORE_LIMIT or blocks > dev_autovis.FLAGGED_SHARE * diff.size
        entry = {"t_b": t, "t_a": u, "score": round(score, 2), "flagged_blocks": blocks, "flagged": flagged, "sheet": None}
        if flagged:
            path = out_dir / f"{_safe(arm)}-B{t:04d}s_vs_A{u:04d}s.png"
            dev_autovis.sheet(image(other), image(name), diff, f"{rid} {arm}: A {u}s | B {t}s | score {score:.1f}", path)
            entry["sheet"] = str(path)
        pairs.append(entry)
    rows = [(f"{s['label']} {s['t']:g}s", [snap_dir / n for n in _picked(s["snaps"], STRIP_SNAPS) if (snap_dir / n).exists()])
            for s in segments]
    strip_path = None
    if rows:
        strip_path = out_dir / f"{_safe(arm)}-strip.png"
        strip(rows, f"{rid} {arm}: one row per segment (A = base, B = arm)", strip_path)
    scores = [p["score"] for p in pairs]
    return {
        "arm": arm, "snaps": {k: len(v) for k, v in side.items()}, "black": black, "pairs": pairs,
        "flagged": sum(1 for p in pairs if p["flagged"]), "b_vs_a_median": statistics.median(scores) if scores else None,
        "noise_median": statistics.median(noise) if noise else None, "strip": str(strip_path) if strip_path else None,
    }


def update(rid: str, info: dict[str, object], snap_dir: Path, autovis_dir: Path) -> dict[str, object]:
    """Every arm of one launch (cached in `autovis/<run>/multilive.json` while its snaps do not change)."""
    out_dir = autovis_dir / rid
    sig = {"schema": SCHEMA, "snaps": sorted(p.name for p in snap_dir.glob("snap-*.png")) if snap_dir.is_dir() else [],
           "arms": list(info.get("results", {}))}
    cache = out_dir / "multilive.json"
    if cache.exists():
        old = json.loads(cache.read_text(encoding="utf-8"))
        if old.get("sig") == sig:
            return old
    for stale in out_dir.glob("*.png") if out_dir.is_dir() else []:
        stale.unlink()
    out_dir.mkdir(parents=True, exist_ok=True)
    arms = {arm: arm_visual(rid, arm, res, snap_dir, out_dir) for arm, res in info.get("results", {}).items()}
    summary = {"sig": sig, "run": rid, "arms": arms}
    cache.write_text(json.dumps(summary, indent=2), encoding="utf-8")
    return summary


def lines(summary: dict[str, object]) -> list[str]:
    out = []
    for arm, v in summary["arms"].items():
        b_a = "-" if v["b_vs_a_median"] is None else f"{v['b_vs_a_median']:.1f}"
        noise = "-" if v["noise_median"] is None else f"{v['noise_median']:.1f}"
        out.append(f"{summary['run']} {arm}: snaps A {v['snaps']['A']} B {v['snaps']['B']}, black A {v['black']['A']} "
                   f"B {v['black']['B']}, B-vs-A score median {b_a} (same-segment noise {noise}), flagged "
                   f"{v['flagged']}, strip {v['strip']}")
    return out
