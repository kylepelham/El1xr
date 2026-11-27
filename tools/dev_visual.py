"""Visual HINTS for `dev.py train` (not verdicts): numbers that point the operator at snaps worth a
look, from stdlib-only image statistics.

`--visual [A B]`: every snap of an ON run is paired (`dev_train.pair_snaps`) with the snap of an
OFF (baseline) run nearest in time, and the pair is measured:

- `diff`: mean absolute difference of block mean colours (0-255),
- `black`: change in the share of near-black blocks,
- `hist`: colour histogram distance (64 bins, 0-1),
- `blocky`: change in the count of flat blocks that differ sharply from all four neighbours
  (blocky tiles),
- `region`: the largest connected group of blocks that differ more than the OFF-vs-OFF pair at
  that time differs there.

The noise floor is the OFF-vs-OFF spread: the same numbers for every snap pair between two
baseline runs (scenes move between runs, so ON-vs-OFF always differs somewhat). A metric is
flagged only above max(FLOOR_FACTOR x the OFF-OFF 90th percentile, ABS_MIN); without two
baseline runs with snaps only ABS_MIN applies. Pairs are ranked by their largest ratio to the
floor; a flagged region gets an ON | OFF crop.

`--region REF X0 Y0 X1 Y1 --at T...`: the mean colour of a rectangle (in REF's pixels, scaled to
each snap) in REF and in every run's snap nearest each time T, with its distance to REF's.

Images are sampled every STEP pixels each way (a 1920x1080 snap gives 480x270) and cut into
BLOCK x BLOCK blocks of samples (32x32 snap pixels).
"""

from __future__ import annotations

import json
import math
import statistics
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path

import dev_png
import dev_train

STEP = 4
BLOCK = 8
BLACK_LUMA = 16.0
FLAT_VAR = 60.0
BLOCKY_EDGE = 48.0
FLOOR_FACTOR = 1.5
ABS_MIN = {"diff": 4.0, "black": 0.02, "hist": 0.05, "blocky": 3.0}
BLOCK_ABS_MIN = 24.0
BLOCK_FACTOR = 2.0
MIN_REGION_BLOCKS = 3
CROPS = 20
# An OFF-vs-OFF diff limit above this means the baseline runs' scenes do not line up in time.
NOISY_DIFF_LIMIT = 40.0
METRICS = ("diff", "black", "hist", "blocky")
SQUARES = [i * i for i in range(256)]
HIST_R = bytes((v >> 6) * 16 for v in range(256))
HIST_G = bytes((v >> 6) * 4 for v in range(256))
HIST_B = bytes(v >> 6 for v in range(256))
HINT = "HINT, not a verdict: look at the PNGs before calling anything."


@dataclass
class Features:
    width: int
    height: int
    bw: int
    bh: int
    mean: list[tuple[float, float, float]]
    luma: list[float]
    var: list[float]
    black: float
    hist: list[float]
    blocky: int


def luma(rgb: tuple[float, float, float]) -> float:
    return 0.299 * rgb[0] + 0.587 * rgb[1] + 0.114 * rgb[2]


def features(image: dev_png.Image) -> Features:
    tw, th = image.width // STEP, image.height // STEP
    chans: list[list[bytes]] = [[], [], []]
    hist: Counter[int] = Counter()
    for y in range(th):
        row = image.rows[y * STEP]
        r, g, b = (row[c :: 3 * STEP][:tw] for c in range(3))
        for c, part in enumerate((r, g, b)):
            chans[c].append(part)
        mixed = (
            int.from_bytes(r.translate(HIST_R), "little")
            + int.from_bytes(g.translate(HIST_G), "little")
            + int.from_bytes(b.translate(HIST_B), "little")
        )
        hist.update(mixed.to_bytes(tw, "little"))
    bw, bh = max(tw // BLOCK, 1), max(th // BLOCK, 1)
    side_x, side_y = min(BLOCK, tw), min(BLOCK, th)
    n = side_x * side_y
    means, lumas, variances = [], [], []
    for by in range(bh):
        ys = range(by * side_y, (by + 1) * side_y)
        for bx in range(bw):
            x0, x1 = bx * side_x, (bx + 1) * side_x
            mean, var = [], 0.0
            for c in range(3):
                s = sum(sum(chans[c][y][x0:x1]) for y in ys)
                sq = sum(sum(map(SQUARES.__getitem__, chans[c][y][x0:x1])) for y in ys)
                m = s / n
                mean.append(m)
                var += sq / n - m * m
            means.append((mean[0], mean[1], mean[2]))
            lumas.append(luma(means[-1]))
            variances.append(var / 3)
    total = max(sum(hist.values()), 1)
    blocky = 0
    for by in range(1, bh - 1):
        for bx in range(1, bw - 1):
            i = by * bw + bx
            if variances[i] < FLAT_VAR and all(
                abs(lumas[i] - lumas[j]) > BLOCKY_EDGE for j in (i - 1, i + 1, i - bw, i + bw)
            ):
                blocky += 1
    return Features(
        width=image.width,
        height=image.height,
        bw=bw,
        bh=bh,
        mean=means,
        luma=lumas,
        var=variances,
        black=sum(1 for v in lumas if v < BLACK_LUMA) / len(lumas),
        hist=[hist.get(i, 0) / total for i in range(64)],
        blocky=blocky,
    )


def block_diffs(a: Features, b: Features) -> list[float]:
    return [(abs(p[0] - q[0]) + abs(p[1] - q[1]) + abs(p[2] - q[2])) / 3 for p, q in zip(a.mean, b.mean, strict=True)]


def compare(a: Features, b: Features) -> tuple[dict[str, float], list[float]] | None:
    """The pair's metrics (A against B) and its per-block differences; None when the block grids
    differ (snaps of another size)."""
    if (a.bw, a.bh) != (b.bw, b.bh):
        return None
    diffs = block_diffs(a, b)
    return {
        "diff": statistics.fmean(diffs),
        "black": a.black - b.black,
        "hist": 0.5 * sum(abs(p - q) for p, q in zip(a.hist, b.hist, strict=True)),
        "blocky": float(a.blocky - b.blocky),
    }, diffs


def q90(values: list[float]) -> float:
    ordered = sorted(values)
    return ordered[max(math.ceil(0.9 * len(ordered)) - 1, 0)] if ordered else 0.0


@dataclass
class Floor:
    """The noise floor from OFF-vs-OFF pairs: (seconds, block differences) per pair, and each
    metric's threshold."""

    pairs: int
    limits: dict[str, float]
    block_limit: float
    maps: list[tuple[int, list[float]]] = field(default_factory=list)

    def block_map(self, t: int) -> list[float] | None:
        return min(self.maps, key=lambda m: (abs(m[0] - t), m[0]))[1] if self.maps else None


def noise_floor(off_pairs: list[tuple[int, dict[str, float], list[float]]]) -> Floor:
    """Thresholds from OFF-vs-OFF pairs (seconds, metrics, block diffs): FLOOR_FACTOR times
    their 90th percentile of each metric's size, at least ABS_MIN."""
    limits = {
        k: max(FLOOR_FACTOR * q90([abs(m[k]) for _, m, _ in off_pairs]), ABS_MIN[k]) for k in METRICS
    }
    every_block = [d for _, _, diffs in off_pairs for d in diffs]
    block_limit = max(FLOOR_FACTOR * q90(every_block), BLOCK_ABS_MIN)
    return Floor(len(off_pairs), limits, block_limit, [(t, diffs) for t, _, diffs in off_pairs])


def largest_region(flags: list[bool], bw: int) -> list[int]:
    """The largest 4-connected group of flagged blocks."""
    seen, best = set(), []
    for start, on in enumerate(flags):
        if not on or start in seen:
            continue
        group, stack = [], [start]
        seen.add(start)
        while stack:
            i = stack.pop()
            group.append(i)
            x, y = i % bw, i // bw
            for j, ok in ((i - 1, x > 0), (i + 1, x < bw - 1), (i - bw, y > 0), (i + bw, i + bw < len(flags))):
                if ok and flags[j] and j not in seen:
                    seen.add(j)
                    stack.append(j)
        if len(group) > len(best):
            best = group
    return sorted(best)


@dataclass
class Finding:
    on_run: str
    off_run: str
    t_on: int
    t_off: int
    on_path: Path
    off_path: Path
    metrics: dict[str, float]
    ratios: dict[str, float]
    region: list[int]
    box: tuple[int, int, int, int] | None
    score: float
    crop: Path | None = None

    @property
    def flagged(self) -> bool:
        return self.score > 1.0


def judge(
    on_run: str, off_run: str, t_on: int, t_off: int, on_path: Path, off_path: Path,
    metrics: dict[str, float], diffs: list[float], on: Features, floor: Floor,
) -> Finding:
    """A pair's ratios to the floor, its suspicious region and its score (the largest ratio)."""
    ratios = {k: abs(metrics[k]) / floor.limits[k] for k in METRICS}
    ratios["blocky"] = max(metrics["blocky"], 0.0) / floor.limits["blocky"]
    off_map = floor.block_map(t_on)
    if off_map is not None and len(off_map) != len(diffs):
        off_map = None
    flags = [
        d > max(floor.block_limit, BLOCK_FACTOR * off_map[i] if off_map else 0.0) for i, d in enumerate(diffs)
    ]
    region = largest_region(flags, on.bw)
    box = None
    if len(region) >= MIN_REGION_BLOCKS:
        ratios["region"] = len(region) / MIN_REGION_BLOCKS
        px = STEP * BLOCK
        xs, ys = [i % on.bw for i in region], [i // on.bw for i in region]
        box = (
            max((min(xs) - 1) * px, 0), max((min(ys) - 1) * px, 0),
            min((max(xs) + 2) * px, on.width), min((max(ys) + 2) * px, on.height),
        )
    else:
        region = []
    return Finding(on_run, off_run, t_on, t_off, on_path, off_path, metrics, ratios, region, box, max(ratios.values()))


def crop(image: dev_png.Image, box: tuple[int, int, int, int]) -> dev_png.Image:
    x0, y0, x1, y1 = box
    x1, y1 = min(x1, image.width), min(y1, image.height)
    return dev_png.Image(x1 - x0, y1 - y0, [row[x0 * 3 : x1 * 3] for row in image.rows[y0:y1]])


def write_crop(finding: Finding, folder: Path) -> Path:
    """ON | OFF crops of the finding's region side by side."""
    box = finding.box or (0, 0, 0, 0)
    pair = dev_png.side_by_side(crop(dev_png.read_png(finding.on_path), box), crop(dev_png.read_png(finding.off_path), box))
    path = folder / f"crop-{finding.on_run}-{finding.t_on:04d}s.png"
    dev_png.write_png(path, pair)
    return path


def report_lines(findings: list[Finding], floor: Floor, top: int = 20) -> list[str]:
    lines = [
        "# Visual hints",
        "",
        HINT,
        "",
        f"Noise floor: {floor.pairs} OFF-vs-OFF snap pairs" if floor.pairs else
        "Noise floor: no OFF-vs-OFF pairs (fewer than two baseline runs with snaps): absolute minimums only",
        "",
        "| metric | limit |",
        "|---|---|",
        *[f"| {k} | {floor.limits[k]:.3g} |" for k in METRICS],
        f"| block (region) | {floor.block_limit:.3g} |",
        "",
    ]
    if floor.limits["diff"] > NOISY_DIFF_LIMIT:
        lines += [
            (
                f"**Weak hints**: OFF-vs-OFF snaps differ strongly (diff limit {floor.limits['diff']:.0f} > "
                f"{NOISY_DIFF_LIMIT:.0f}): the baseline runs' scenes do not line up in time, so little stands out."
            ),
            "",
        ]
    flagged = sorted((f for f in findings if f.flagged), key=lambda f: -f.score)
    lines.append(f"{len(flagged)} of {len(findings)} snap pairs above the floor (ranked; score = largest ratio to the limit):")
    lines.append("")
    if flagged:
        lines.append("| score | ON snap | OFF snap | diff | black | hist | blocky | region blocks | crop |")
        lines.append("|---|---|---|---|---|---|---|---|---|")
        for f in flagged[:top]:
            m = f.metrics
            lines.append(
                f"| {f.score:.2f} | {f.on_run} {f.t_on}s | {f.off_run} {f.t_off}s | {m['diff']:.1f} | {m['black']:+.3f} "
                f"| {m['hist']:.3f} | {m['blocky']:+.0f} | {len(f.region)} | {f.crop.name if f.crop else ''} |"
            )
    return lines


# --- region colour ----------------------------------------------------------------------------


def mean_colour(image: dev_png.Image, box: tuple[int, int, int, int]) -> tuple[float, float, float]:
    x0, y0, x1, y1 = box
    n = max((x1 - x0) * (y1 - y0), 1)
    sums = [0, 0, 0]
    for row in image.rows[y0:y1]:
        for c in range(3):
            sums[c] += sum(row[x0 * 3 + c : x1 * 3 : 3])
    return (sums[0] / n, sums[1] / n, sums[2] / n)


def scale_box(box: tuple[int, int, int, int], src: tuple[int, int], dst: tuple[int, int]) -> tuple[int, int, int, int]:
    """`box` in an image of size `src`, in an image of size `dst` (clipped, at least 1 pixel)."""
    sx, sy = dst[0] / src[0], dst[1] / src[1]
    x0, y0 = min(int(box[0] * sx), dst[0] - 1), min(int(box[1] * sy), dst[1] - 1)
    x1, y1 = max(min(round(box[2] * sx), dst[0]), x0 + 1), max(min(round(box[3] * sy), dst[1]), y0 + 1)
    return (x0, y0, x1, y1)


def colour_cell(rgb: tuple[float, float, float]) -> str:
    r, g, b = (round(v) for v in rgb)
    return f"#{r:02x}{g:02x}{b:02x} ({r}, {g}, {b}) luma {luma(rgb):.0f}"


def distance(a: tuple[float, float, float], b: tuple[float, float, float]) -> float:
    return math.dist(a, b)


# --- train --visual / --region ------------------------------------------------------------------


def _snaps_of(train_dir: Path, done: dict[int, dict[str, object]], step: int) -> list[tuple[int, Path]]:
    return dev_train.snaps(train_dir / f"{dev_train.run_id(step, str(done[step]['set']))}_snaps")


def visual(
    plan: dev_train.Plan, done: dict[int, dict[str, object]], train_dir: Path, which: list[str]
) -> tuple[list[str], dict[str, object]]:
    """`train --visual [A B]`: every ON run against the baseline run nearest in the order (or A
    against B), above the OFF-vs-OFF floor of the baseline runs. Writes crops, `visual.md` and
    `visual.json` into `<train dir>/visual/`. Raises PlanError for a run that is not there."""
    with_snaps = [s for s in sorted(done) if _snaps_of(train_dir, done, s)]
    base = [s for s in with_snaps if done[s]["set"] == plan.baseline]
    if which:
        runs = [(dev_train.find_run(plan, done, which[0]), dev_train.find_run(plan, done, which[1]))]
    else:
        ons = [s for s in with_snaps if done[s]["set"] != plan.baseline]
        runs = [(on, min(base, key=lambda b: (abs(b - on), b))) for on in ons] if base else []
    cache: dict[Path, Features] = {}

    def feat(path: Path) -> Features:
        if path not in cache:
            cache[path] = features(dev_png.read_png(path))
        return cache[path]

    rid = {s: dev_train.run_id(s, str(done[s]["set"])) for s in done}
    off_pairs = []
    for i, a in enumerate(base):
        for b in base[i + 1 :]:
            for t, pa, _, pb in dev_train.pair_snaps(_snaps_of(train_dir, done, a), _snaps_of(train_dir, done, b)):
                measured = compare(feat(pa), feat(pb))
                if measured:
                    off_pairs.append((t, *measured))
    floor = noise_floor(off_pairs)
    findings = []
    for on, off in runs:
        for t, pa, u, pb in dev_train.pair_snaps(_snaps_of(train_dir, done, on), _snaps_of(train_dir, done, off)):
            fa = feat(pa)
            measured = compare(fa, feat(pb))
            if measured:
                findings.append(judge(rid[on], rid[off], t, u, pa, pb, *measured, fa, floor))
    out = train_dir / "visual"
    out.mkdir(parents=True, exist_ok=True)
    for old in out.glob("crop-*.png"):
        old.unlink()
    ranked = sorted((f for f in findings if f.flagged), key=lambda f: -f.score)
    for f in ranked[:CROPS]:
        if f.box:
            f.crop = write_crop(f, out)
    lines = report_lines(findings, floor)
    summary = {
        "hint": HINT,
        "pairs": len(findings),
        "flagged": len(ranked),
        "floor": {"off_off_pairs": floor.pairs, "limits": floor.limits, "block_limit": floor.block_limit},
        "findings": [
            {
                "on": f.on_run, "off": f.off_run, "t_on": f.t_on, "t_off": f.t_off, "score": f.score,
                "metrics": f.metrics, "ratios": f.ratios, "region_blocks": len(f.region), "box": f.box,
                "crop": str(f.crop) if f.crop else None,
            }
            for f in ranked
        ],
    }
    (out / "visual.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    (out / "visual.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
    return lines, summary


def report_section(train_dir: Path, top: int = 5) -> list[str]:
    """The `--report` lines for an earlier `--visual` (none when it has not run)."""
    path = train_dir / "visual" / "visual.json"
    if not path.exists():
        return []
    data = json.loads(path.read_text(encoding="utf-8"))
    lines = [
        "", "## Visual hints (from `train --visual`; a hint, not a verdict)", "",
        (
            f"{data['flagged']} of {data['pairs']} snap pairs above the OFF-vs-OFF floor "
            f"({data['floor']['off_off_pairs']} OFF-OFF pairs); details in visual/visual.md."
        ),
    ]
    lines += [
        f"- {f['on']} {f['t_on']}s vs {f['off']} {f['t_off']}s: score {f['score']:.2f}"
        + (f", crop {Path(f['crop']).name}" if f["crop"] else "")
        for f in data["findings"][:top]
    ]
    return lines


def region(
    done: dict[int, dict[str, object]], train_dir: Path, ref: Path, box: tuple[int, int, int, int], times: list[float]
) -> tuple[list[str], int]:
    """`train --region REF X0 Y0 X1 Y1 --at T...`: the rectangle's mean colour in REF and in every
    run's snap nearest each time (lines, snaps measured); writes a strip of the crops per time into
    `<train dir>/visual/`."""
    reference = dev_png.read_png(ref)
    x0, y0, x1, y1 = box
    if not (0 <= x0 < x1 <= reference.width and 0 <= y0 < y1 <= reference.height):
        raise ValueError(f"the rectangle {box} is not inside {ref.name} ({reference.width}x{reference.height})")
    ref_rgb = mean_colour(reference, box)
    out = train_dir / "visual"
    out.mkdir(parents=True, exist_ok=True)
    lines = [
        "# Region colour", "", HINT, "",
        f"Reference {ref} {box}: {colour_cell(ref_rgb)}", "",
        "| run | at | snap | mean colour | distance to reference |", "|---|---|---|---|---|",
    ]
    images: dict[Path, dev_png.Image] = {}
    measured = 0
    for at in times:
        strip = crop(reference, box)
        for step in sorted(done):
            found = _snaps_of(train_dir, done, step)
            if not found:
                continue
            t, path = min(found, key=lambda s: (abs(s[0] - at), s[0]))
            image = images.setdefault(path, dev_png.read_png(path))
            scaled = scale_box(box, (reference.width, reference.height), (image.width, image.height))
            rgb = mean_colour(image, scaled)
            run = dev_train.run_id(step, str(done[step]["set"]))
            lines.append(f"| {run} | {at:g}s | {t}s | {colour_cell(rgb)} | {distance(rgb, ref_rgb):.0f} |")
            strip = dev_png.side_by_side(strip, crop(image, scaled))
            measured += 1
        name = f"region-{x0}_{y0}_{x1}_{y1}-{at:g}s.png"
        dev_png.write_png(out / name, strip)
        lines.append(f"| crops | {at:g}s | reference, then runs in order: visual/{name} | | |")
    (out / f"region-{x0}_{y0}_{x1}_{y1}.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    return lines, measured
