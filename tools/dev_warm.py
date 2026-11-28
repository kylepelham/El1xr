"""Warm stamps for the shader cache: `dev.py prewarm` and `dev.py train` skip a prewarm that already ran.

The shader cache (`%LOCALAPPDATA%\\victory\\cache\\shaders`, or `VICTORY_CACHE_DIR`) is content-addressed: every
variant of the translator's key lives in the same folder. A prewarm that passed writes a stamp named after a
signature of what decides that key from outside: the victory.exe it ran and the launcher script (size and
modification time, so a rebuild or a new launcher default invalidates it) and the launch overrides (every `VICTORY_*` value, since which ones join the key is the
translator's business). A later prewarm with the same signature is a no-op; `--force` runs it anyway.

A stamp says `pipelines: true` when the prewarm also made the recipes' host pipelines (`--pipelines`); a
translation-only stamp (the prewarm ahead of the next arm, which runs at idle priority during a game run and
must not touch the card) satisfies only a translation-only request. A stamp counts only while the shader cache
folder exists (a cleared cache is cold whatever the stamps say). Recipes runs record after a stamp are not
made ahead (the game makes them as it meets them, as without a prewarm); `--force` remakes them.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
from datetime import datetime, timezone
from pathlib import Path

LIVE_RE = re.compile(r'name: "(VICTORY_[A-Z0-9_]+)".*?apply: Apply::(\w+)')
SETTINGS_TABLE = Path("third_party/victory/crates/victory-core/src/settings/table.rs")
EXPERT_RE = re.compile(r'\("(VICTORY_[A-Z0-9_]+)", Apply::(\w+)\)')
SETTINGS_EXPERT = SETTINGS_TABLE.with_name("expert.rs")


# The release-family cargo profiles (third_party/victory/Cargo.toml) `bench`, `train` and the launcher can run;
# `diag` stays its own flag (it also needs the diagnostics feature).
RELEASE_PROFILES = ("release", "release-lto")


def victory_exe(repo_root: Path, diag: bool = False, profile: str = "release") -> Path:
    """The victory.exe the launcher runs for this profile (it honours CARGO_TARGET_DIR)."""
    target = Path(os.environ["CARGO_TARGET_DIR"]) if os.environ.get("CARGO_TARGET_DIR") else (
        repo_root / "third_party" / "victory" / "target"
    )
    return target / ("diag" if diag else profile) / "victory.exe"


def exe_identity(exe: Path, launcher: Path | None = None) -> str:
    def one(path: Path) -> str:
        try:
            st = path.stat()
        except OSError:
            return "missing"
        return f"{st.st_size}:{st.st_mtime_ns}"

    return one(exe) if launcher is None else f"{one(exe)}|{one(launcher)}"


def shader_cache_dir(overrides: dict[str, str]) -> Path | None:
    """The shader cache folder a run with these overrides uses (config.rs: VICTORY_CACHE_DIR, else
    %LOCALAPPDATA%\\victory\\cache), or None when it cannot be told."""
    base = overrides.get("VICTORY_CACHE_DIR") or os.environ.get("VICTORY_CACHE_DIR")
    if base:
        return Path(base) / "shaders"
    local = os.environ.get("LOCALAPPDATA")
    return Path(local) / "victory" / "cache" / "shaders" if local else None


def signature(overrides: dict[str, str], exe_id: str, diag: bool = False) -> str:
    """16 hex digits naming a (victory.exe, overrides, profile) combination."""
    body = json.dumps({"exe": exe_id, "diag": diag, "overrides": sorted(overrides.items())})
    return hashlib.sha256(body.encode("utf-8")).hexdigest()[:16]


def stamp_path(warm_dir: Path, sig: str) -> Path:
    return warm_dir / f"warm-{sig}.json"


def read_stamp(warm_dir: Path, sig: str) -> dict[str, object] | None:
    path = stamp_path(warm_dir, sig)
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None


def is_warm(warm_dir: Path, sig: str, pipelines: bool, cache_dir: Path | None = None) -> bool:
    """Whether a prewarm with this signature passed, with pipelines made if they are asked for, and the shader
    cache (`cache_dir`, when given) is still there."""
    if cache_dir is not None and not cache_dir.is_dir():
        return False
    stamp = read_stamp(warm_dir, sig)
    return stamp is not None and (bool(stamp.get("pipelines")) or not pipelines)


def write_stamp(
    warm_dir: Path, sig: str, pipelines: bool, counts: dict[str, int], overrides: dict[str, str], how: str
) -> Path:
    """Records a prewarm that passed. A translation-only stamp never replaces one with pipelines."""
    old = read_stamp(warm_dir, sig)
    record = {
        "pipelines": pipelines or bool(old and old.get("pipelines")),
        "translated": int(counts.get("translated", 0)),
        "hits": int(counts.get("hits", 0)),
        "shaders": int(counts.get("shaders", 0)),
        "how": how,
        "overrides": overrides,
        "when": datetime.now(timezone.utc).isoformat(timespec="seconds"),
    }
    warm_dir.mkdir(parents=True, exist_ok=True)
    path = stamp_path(warm_dir, sig)
    path.write_text(json.dumps(record, indent=2, sort_keys=True), encoding="utf-8")
    return path


def apply_kinds(repo_root: Path) -> dict[str, str] | None:
    """Each registry setting's apply kind (`Live`, `Restart`, `Retranslate`) as the settings table (described
    entries) and its expert list state it, or None when the tree has no settings registry yet."""
    try:
        text = (repo_root / SETTINGS_TABLE).read_text(encoding="utf-8")
    except OSError:
        return None
    kinds = {m.group(1): m.group(2) for m in LIVE_RE.finditer(text)}
    try:
        expert = (repo_root / SETTINGS_EXPERT).read_text(encoding="utf-8")
    except OSError:
        expert = ""
    for m in EXPERT_RE.finditer(expert):
        kinds.setdefault(m.group(1), m.group(2))
    return kinds
