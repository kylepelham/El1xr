"""Log greps and health flags for `dev.py train --report`, and `train --greps-from`.

A plan's optional `[greps]` table names regexes to count in every run's log:

    [greps]
    alias_skipped = 'alias copies skipped'
    merged = 'submissions merged per frame: ([0-9.]+)'      # group 1 (or one named `value`) is a number

The report gives, per grep and run, the count of matching lines and, for a regex with a group,
the median and last number it captured. Every run is also checked for lines naming `device
removed`, `DEVICE_HUNG` or `panicked`, and for a sharp rise in WARN lines against the baseline
arm (more than WARN_RISE_FACTOR times its median plus WARN_RISE_MIN), with the WARN messages
that rose most. The lines of the device-removed report (`dred:`, `VICTORY_DRED=1`) and of the
hang watch (`hang watch:`, `VICTORY_HANG_WATCH`) are kept per run (`hang_lines`): bench,
capture and train print them under the run's summary line, and the train report lists them.

`--greps-from <ready.md ...>` reads the `## Batch items` tables of writers' ready.md files and
adds every backticked log-line phrase it finds there (best effort: placeholders such as `N` or
`<site>` and anything after `(` are cut away, the longest literal part is kept) to the plan's
`[greps]`, skipping names and regexes it already has.
"""

from __future__ import annotations

import json
import re
import statistics
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path

ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
LEVEL_RE = re.compile(r"^\S+\s+(WARN|ERROR)\s+([\w:]+): (.*)$")
FATAL = (
    ("device removed", re.compile(r"device removed", re.IGNORECASE)),
    ("DEVICE_HUNG", re.compile(r"DEVICE_HUNG")),
    ("panicked", re.compile(r"\bpanicked\b")),
)
WARN_RISE_FACTOR = 2.0
WARN_RISE_MIN = 20
TOP_WARN_KINDS = 3
GREP_NAME_RE = re.compile(r"^[A-Za-z][A-Za-z0-9_-]*$")
FIELD_RE = re.compile(r"\s\w+=")
BACKTICK_RE = re.compile(r"`([^`\n]+)`")
PLACEHOLDER_RE = re.compile(r"^(?:[A-Z]|\.\.\.|…)$")
CUT_CHARS = "(<{[…="
REGEX_SPECIAL = set(".^$*+?{}[]\\|()")
SKIP_PHRASE_RE = re.compile(
    r"^(?:VICTORY_|bench\b|train\b|capture\b)|--|\s=\s|->|[/\\]|\bdev\.py\b|\bcargo\b|\.(?:rs|png|md|toml|py|log)\b"
)
TABLE_HEADER_RE = re.compile(r"^\s*\[greps\]\s*(?:#.*)?$")
HANG_RE = re.compile(r"\b(?:dred|hang watch): ")
HANG_SKIP_RE = re.compile(r"device-removed data on\b|hang watch: on\b")
HANG_LINES_MOST = 80


class GrepError(ValueError):
    pass


def parse_greps(table: object) -> dict[str, re.Pattern[str]]:
    """The plan's `[greps]`: name -> compiled regex. Raises GrepError for a bad one."""
    if not isinstance(table, dict):
        raise GrepError("greps: a table of name = regex")
    out = {}
    for name, regex in table.items():
        if not GREP_NAME_RE.match(name):
            raise GrepError(f"greps: '{name}' is not a usable name (letters, digits, '_', '-')")
        if not isinstance(regex, str):
            raise GrepError(f"greps.{name}: a regex string")
        try:
            out[name] = re.compile(regex)
        except re.error as exc:
            raise GrepError(f"greps.{name}: {exc}") from exc
    return out


def _number(match: re.Match[str]) -> float | None:
    if not match.re.groups:
        return None
    text = match.group("value") if "value" in match.re.groupindex else match.group(1)
    try:
        return float(text.replace(",", ""))
    except (AttributeError, ValueError):
        return None


def warn_kind(target: str, message: str) -> str:
    """A WARN line's kind: its target and message without fields, digits as N."""
    cut = FIELD_RE.search(message)
    text = message[: cut.start()] if cut else message
    return re.sub(r"\d+", "N", f"{target}: {text.strip()}")[:100]


@dataclass
class LogScan:
    greps: dict[str, tuple[int, list[float]]] = field(default_factory=dict)
    fatal: dict[str, int] = field(default_factory=dict)
    warns: int = 0
    warn_kinds: Counter[str] = field(default_factory=Counter)
    hang: list[str] = field(default_factory=list)


def hang_lines(text: str, most: int = HANG_LINES_MOST) -> list[str]:
    """The `dred:` and `hang watch:` lines of a run's log, from those words on, each once (the
    crash report repeats the failure's text), at most `most`; not the lines saying they are on."""
    out: list[str] = []
    seen: set[str] = set()
    for raw in text.splitlines():
        line = ANSI_RE.sub("", raw).rstrip()
        m = HANG_RE.search(line)
        if not m or HANG_SKIP_RE.search(line):
            continue
        line = line[m.start():]
        if line in seen:
            continue
        seen.add(line)
        out.append(line)
        if len(out) >= most:
            break
    return out


def scan_log(text: str, greps: dict[str, re.Pattern[str]]) -> LogScan:
    counts = {name: 0 for name in greps}
    values: dict[str, list[float]] = {name: [] for name in greps}
    scan = LogScan()
    for raw in text.splitlines():
        line = ANSI_RE.sub("", raw)
        for name, pattern in greps.items():
            m = pattern.search(line)
            if m:
                counts[name] += 1
                number = _number(m)
                if number is not None:
                    values[name].append(number)
        for name, pattern in FATAL:
            if pattern.search(line):
                scan.fatal[name] = scan.fatal.get(name, 0) + 1
        level = LEVEL_RE.match(line)
        if level and level.group(1) == "WARN":
            scan.warns += 1
            scan.warn_kinds[warn_kind(level.group(2), level.group(3))] += 1
    scan.greps = {name: (counts[name], values[name]) for name in greps}
    scan.hang = hang_lines(text)
    return scan


def grep_figures(count: int, values: list[float]) -> dict[str, float | int | None]:
    return {
        "count": count,
        "median": statistics.median(values) if values else None,
        "last": values[-1] if values else None,
    }


def grep_cell(count: int, values: list[float]) -> str:
    if not values:
        return str(count)
    return f"{count} / med {statistics.median(values):g} / last {values[-1]:g}"


def baseline_warns(scans: list[LogScan]) -> tuple[float, Counter[str]] | None:
    """The baseline arm's median WARN count and median count of each WARN kind."""
    if not scans:
        return None
    kinds = set().union(*(s.warn_kinds for s in scans))
    per_kind = Counter({k: statistics.median(s.warn_kinds.get(k, 0) for s in scans) for k in kinds})
    return statistics.median(s.warns for s in scans), per_kind


def flags(scan: LogScan, base: tuple[float, Counter[str]] | None, is_baseline: bool) -> list[str]:
    """What a run's log says went wrong: fatal lines, and a WARN rise against the baseline arm
    (with the kinds that rose most)."""
    out = [f"{name} x{n}" for name, n in scan.fatal.items()]
    if base is not None and not is_baseline:
        base_warns, base_kinds = base
        if scan.warns > WARN_RISE_FACTOR * base_warns + WARN_RISE_MIN:
            rises = sorted(
                ((n - base_kinds.get(kind, 0), kind) for kind, n in scan.warn_kinds.items()),
                key=lambda r: (-r[0], r[1]),
            )
            top = "; ".join(f"+{rise:g} {kind}" for rise, kind in rises[:TOP_WARN_KINDS] if rise > 0)
            out.append(f"WARN {scan.warns} (baseline {base_warns:g}): {top}")
    return out


# --- --greps-from ---------------------------------------------------------------------------


@dataclass
class Found:
    name: str
    regex: str
    note: str


def phrase_literal(phrase: str) -> str | None:
    """The longest literal run of words of a backticked phrase that reads as a log line, or None."""
    phrase = phrase.strip()
    if " " not in phrase or SKIP_PHRASE_RE.search(phrase):
        return None
    runs: list[list[str]] = [[]]
    for word in phrase.split():
        cut = min((word.index(c) for c in CUT_CHARS if c in word), default=None)
        if cut is not None:
            if word[:cut]:
                runs[-1].append(word[:cut])
            runs.append([])
            continue
        if PLACEHOLDER_RE.match(word):
            runs.append([])
            continue
        runs[-1].append(word)
    best = max((" ".join(r).strip(" :,;.") for r in runs), key=len)
    if len(re.findall(r"[A-Za-z]{2,}", best)) < 2:
        return None
    return best


def escape(text: str) -> str:
    """`text` as a regex matching it literally, with only regex syntax escaped (readable in a plan)."""
    return "".join(f"\\{c}" if c in REGEX_SPECIAL else c for c in text)


def slug(text: str) -> str:
    words = re.findall(r"[a-z0-9]+", text.lower())
    out = ""
    for w in words:
        if out and len(out) + 1 + len(w) > 40:
            break
        out = f"{out}_{w}" if out else w
    if not out or not out[0].isalpha():
        out = f"g_{out}"
    return out


def batch_rows(text: str) -> list[str]:
    """The table rows under every `## Batch items...` heading."""
    rows, inside = [], False
    for line in text.splitlines():
        if line.startswith("## "):
            inside = line[3:].lower().startswith("batch items")
            continue
        if inside and line.lstrip().startswith("|") and not re.match(r"^\s*\|[\s:|-]*$", line):
            rows.append(line)
    return rows


def extract(text: str, source: str) -> list[Found]:
    """Log-line greps named in a ready.md's batch-item rows."""
    found = []
    for row in batch_rows(text):
        spans = BACKTICK_RE.findall(row)
        switch = next((s for s in spans if s.startswith("VICTORY_")), row.split("|")[1].strip()[:40])
        for span in spans:
            literal = phrase_literal(span)
            if literal:
                found.append(Found(slug(literal), escape(literal), f"{source}: {switch}"))
    return found


def merge_found(existing: dict[str, str], found: list[Found]) -> tuple[list[Found], int]:
    """The finds to add (new regexes, names made unique) and how many were skipped."""
    names, regexes = set(existing), set(existing.values())
    added, skipped = [], 0
    for f in found:
        if f.regex in regexes:
            skipped += 1
            continue
        name, n = f.name, 2
        while name in names:
            name, n = f"{f.name}_{n}", n + 1
        names.add(name)
        regexes.add(f.regex)
        added.append(Found(name, f.regex, f.note))
    return added, skipped


def toml_string(text: str) -> str:
    return f"'{text}'" if "'" not in text else json.dumps(text)


def add_to_plan(path: Path, added: list[Found]) -> None:
    """Writes `added` into the plan's `[greps]` (made at the end when it has none)."""
    if not added:
        return
    if path.suffix.lower() == ".json":
        data = json.loads(path.read_text(encoding="utf-8"))
        data.setdefault("greps", {}).update({f.name: f.regex for f in added})
        path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
        return
    with open(path, encoding="utf-8", newline="") as fh:
        text = fh.read()
    eol = "\r\n" if "\r\n" in text else "\n"
    new = [f"{f.name} = {toml_string(f.regex)}  # {f.note}" for f in added]
    lines = text.splitlines()
    header = next((i for i, line in enumerate(lines) if TABLE_HEADER_RE.match(line)), None)
    if header is None:
        lines += [*([""] if lines and lines[-1].strip() else []), "[greps]", *new]
    else:
        end = next((i for i in range(header + 1, len(lines)) if lines[i].lstrip().startswith("[")), len(lines))
        while end > header + 1 and not lines[end - 1].strip():
            end -= 1
        lines[end:end] = new
    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(eol.join(lines) + eol)
