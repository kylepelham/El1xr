#!/usr/bin/env python3
"""El1xr ecosystem intelligence & watcher tool for GTA VI Xbox-on-PC (VIctory) development.

Zero-dependency Python 3 standard-library tool that discovers and monitors top
VIctory-based forks and related Xbox-on-PC ports from live trackers
(gta6-pc-race & PortWatch VI) plus a built-in priority seed list, deep-scans
recent commits, active branches, pull requests, and READMEs concurrently with
ETag caching, categorizes developments into actionable engineering buckets,
cross-checks against the local El1xr repository to separate [NEW] from
[LIKELY ALREADY HAVE], and persists seen state outside the git tree.

Usage:
    python tools/scout_ecosystem.py --once --top 15
    python tools/scout_ecosystem.py --once --top 10 --focus perf,visual_skin_face,animation_mouth
    python tools/scout_ecosystem.py --watch --interval 900 --top 15
    python tools/scout_ecosystem.py --once --reset-state
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import hashlib
import html
import json
import os
import re
import signal
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from collections import defaultdict
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

# ---------------------------------------------------------------------------
# Constants & Configuration
# ---------------------------------------------------------------------------

SCRIPT_VERSION = "1.2.0"
USER_AGENT = f"el1xr-victory-scout/{SCRIPT_VERSION}"

GTA6_PC_RACE_JSON_URL = "https://raw.githubusercontent.com/vuyp/gta6-pc-race/main/src/data.json"
GTA6_PC_RACE_HTML_URL = "https://vuyp.github.io/gta6-pc-race/"
PORTWATCH_JSON_URL = "https://raw.githubusercontent.com/4lwayz/portwatch-vi/main/data/tracker.json"
PORTWATCH_HTML_URL = "https://portwatch-vi.xyz/"

DEFAULT_LOCAL_REPO = Path(r"E:\Gta6\AnyXboxX")
DEFAULT_STATE_DIR = Path(os.environ.get("LOCALAPPDATA", str(Path.home()))) / "victory-scout"
DEFAULT_STATE_PATH = DEFAULT_STATE_DIR / "state.json"
DEFAULT_OUT_DIR = DEFAULT_STATE_DIR / "reports"

# Built-in priority seed list of top VIctory-based forks (always prioritized)
PRIORITY_SEEDS: tuple[str, ...] = (
    "stellarr-studio/kelvaroq",
    "uiliannn/VItoriaBr",
    "Belobug/VIctory-Belobug-AMD",
    "avatarmanz/VIctory-7900XTX",
    "fxceless/victory-fork",
    "DoubleGhosty/cool_fox",
    "TaynanGT/Vice-Tay",
    "DexNom/VIctory",
    "realblastex/flamingo",
    "LooperSalty/AnyXboxX",
    "Marvisak/VIctory",
    "Peppacodes/playablegames-peppa",
)

# Extra non-default branches to query explicitly in addition to the default branch
EXTRA_BRANCHES: dict[str, tuple[str, ...]] = {
    "belobug/victory-belobug-amd": ("rx9070xt", "a-publier"),
    "peppacodes/playablegames-peppa": ("nvidia/overnight", "nvidia/renderer-ports", "nvidia/4080-recipe"),
}

# Repos whose full active branch list is queried dynamically via GitHub API
ALL_ACTIVE_BRANCHES_REPOS: frozenset[str] = frozenset({
    "belobug/victory-belobug-amd",
})

# High-signal ecosystem repos also worth tracking if trackers are temporarily unreachable
FALLBACK_SEEDS: tuple[str, ...] = (
    "stellarr-studio/kelvaroq",
    "uiliannn/VItoriaBr",
    "Belobug/VIctory-Belobug-AMD",
    "avatarmanz/VIctory-7900XTX",
    "fxceless/victory-fork",
    "DoubleGhosty/cool_fox",
    "TaynanGT/Vice-Tay",
    "DexNom/VIctory",
    "realblastex/flamingo",
    "LooperSalty/AnyXboxX",
    "Marvisak/VIctory",
    "Peppacodes/playablegames-peppa",
    "Dolberthi/neptune-dolberth-nvidia-8gb",
    "JozefDeTremmerie/CCS03",
    "bolado-dev/mandy-port",
    "Linox-IDK/LNX-victory",
    "3cvil/M1LK",
    "lepotane/VIctory-leonida",
    "trustno1-coder/VIctory-NVIDIA",
    "vitutu1/victory-perf",
    "PastiiJay/klbf-lab",
    "edsuuu/VI-BR-PORT",
    "LoverCatys/ruff-lab",
    "proneonn/Vertex",
    "Fayberr/Victory-United",
)

# Categories in priority display order
CATEGORY_ORDER: tuple[str, ...] = (
    "perf",
    "visual_skin_face",
    "animation_mouth",
    "lighting_rt",
    "streaming_lod",
    "shader_wave",
    "stability",
)

CATEGORY_META: dict[str, dict[str, str]] = {
    "perf": {
        "label": "Performance / Frame Time / Memory",
        "short": "Perf",
    },
    "visual_skin_face": {
        "label": "Visuals: Skin / Face / Hair / Materials",
        "short": "Skin/Face",
    },
    "animation_mouth": {
        "label": "Animation / Mouth / Facial / Skinning",
        "short": "Anim/Mouth",
    },
    "lighting_rt": {
        "label": "Lighting / GI / Ray Tracing / Shadows",
        "short": "Lighting/RT",
    },
    "streaming_lod": {
        "label": "Streaming / LOD / Mips / Textures / Tiles",
        "short": "Stream/LOD",
    },
    "shader_wave": {
        "label": "Shaders / Wave64 / Wave32 / DXIL / VALU",
        "short": "Shader/Wave",
    },
    "stability": {
        "label": "Stability / TDR / Crashes / Races / Leaks",
        "short": "Stability",
    },
}

# Keyword regexes (weight, compiled regex, display tag) per category
CATEGORY_KEYWORDS: dict[str, list[tuple[int, re.Pattern[str], str]]] = {
    "perf": [
        (4, re.compile(r"\b(fps|frame\s*rate|frame\s*time|frametime|ms/frame|ms\s+per\s+frame)\b", re.I), "fps/frametime"),
        (3, re.compile(r"\b(latency|submit|submission|copy[-\s]?back|copyback|relay)\b", re.I), "submit/relay"),
        (3, re.compile(r"\b(watch\.rs|watch\b|heap[_\s]?pass|simd|avx2?|sse[24]?|coalesce)\b", re.I), "watch/heap/simd"),
        (3, re.compile(r"\b(memory\s*leak|memleak|rss\b|vram\b|in[-\s]?place|inplace|late[_\s]?wait)\b", re.I), "mem/inplace"),
        (2, re.compile(r"\b(cache|caching|staging|pool|indirect|bookkeep|throughput|bottleneck|stall)\b", re.I), "cache/staging"),
        (2, re.compile(r"\b(perf|optimiz\w*|speed\s*up|speedup|faster|overhead|hot\s*path|zero[-\s]?copy)\b", re.I), "perf"),
        (2, re.compile(r"\b(parallel|worker\s*thread|background\s*thread|async\s*upload|lock[-\s]?free|contention)\b", re.I), "threading"),
    ],
    "visual_skin_face": [
        (4, re.compile(r"\b(skin|face|faces|hair|beard|eyebrow|eyelash|eye[s]?\s*(shader|tex|render))\b", re.I), "skin/face/hair"),
        (4, re.compile(r"\b(bake|baked|baking|decoration|oily|wet|wetness|roughness|glossy|specular)\b", re.I), "bake/roughness"),
        (4, re.compile(r"\b(rainbow|noise|banding|bands?\s+on\s+skin|dark\s+face|purple|magenta)\b", re.I), "artifacts/noise"),
        (3, re.compile(r"\b(subsurface|sss\b|albedo|normal\s*map|bcpack|bc7|srgb|unorm|snorm|swizzle|decal)\b", re.I), "material/bc"),
        (2, re.compile(r"\b(dstorage\b.*\b(bake|tex|face|skin)|character\s*(render|tex|skin))\b", re.I), "dstorage-bake"),
    ],
    "animation_mouth": [
        (4, re.compile(r"\b(mouth|jaw|lip|lips|facial|blend\s*shape|blendshape|morph|morph\s*target)\b", re.I), "mouth/facial"),
        (4, re.compile(r"\b(skinning|skinned|skeleton|skeletal|bone\s*matrix|rigging|pose)\b", re.I), "skinning"),
        (3, re.compile(r"\b(vertex\s*buffer|vbv|buffer\s*view|buffer_view|raw\s*buffer|byte\s*address)\b", re.I), "vertex/bufview"),
        (3, re.compile(r"\b(anim\w*|deformation|vertex\s*(stride|offset|align\w*|fetch|format))\b", re.I), "anim/vertex"),
        (2, re.compile(r"\b(raw\b.*\balign\w*|align\w*\b.*\b(vertex|buffer|raw|view))\b", re.I), "raw/align"),
    ],
    "lighting_rt": [
        (4, re.compile(r"\b(ambient|gi\b|global\s+illumination|bvh|blas|tlas|ray\s*trac\w*|rayquery)\b", re.I), "gi/bvh/rt"),
        (4, re.compile(r"\b(reflection|reflections|ssr\b|shadow|shadows|corner|crevasse|probe|probes)\b", re.I), "refl/shadow/probe"),
        (3, re.compile(r"\b(ray\b|intersect|irradiance|stand[-\s]?in|standin|hdr|r9g9b9e5|tonemap)\b", re.I), "ray/standin/hdr"),
        (3, re.compile(r"\b(light\s*refresh|indoor\s*light|sky\s*light|sun\s*light|diffuse|ao\b|ssao|emissive)\b", re.I), "lighting"),
        (2, re.compile(r"\b(exposure|blown[-\s]?out|overexposed|black\s+shadow|dark\s+indoor|lighting)\b", re.I), "exposure/dark"),
    ],
    "streaming_lod": [
        (4, re.compile(r"\b(stream\w*|lod\b|lods\b|mip\b|mips\b|mipmap|tile\b|tiled|tiling)\b", re.I), "stream/lod/mip/tile"),
        (4, re.compile(r"\b(dstorage|directstorage|xmem|views[_\s]?remade|view[_\s]?guard|levels[_\s]?on[_\s]?demand)\b", re.I), "dstorage/xmem/views"),
        (3, re.compile(r"\b(invisible|pop[-\s]?in|popin|late\s+texture|missing\s+(wall|geo|tex|mesh)|subresource)\b", re.I), "pop-in/invisible"),
        (3, re.compile(r"\b(texture\s*(upload|sweep|gate|evict|residen|budget|reupload)|xg[_\s]?xs|tail\s+mip)\b", re.I), "texture-stream"),
        (2, re.compile(r"\b(texture|residency|resident|evict\w*|page[_\s]?write|guard\s*page)\b", re.I), "texture/residency"),
    ],
    "shader_wave": [
        (4, re.compile(r"\b(wave64|wave32|wave[_\s]?pairs?|wave[_\s]?wide|host[_\s]?wave|lane\s*count)\b", re.I), "wave64/32"),
        (4, re.compile(r"\b(dxil|lds\b|groupshared|tgsm|valu|salu|vop[123p]|sop[12ck]|sp3|gfx10|rdna[23]?)\b", re.I), "dxil/lds/rdna"),
        (3, re.compile(r"\b(shader|recompil\w*|hlsl|bitcode|sgpr|vgpr|spill|waterfall|ballot|shuffle)\b", re.I), "shader/recomp"),
        (3, re.compile(r"\b(barrier\b.*\b(wave|lds|group|shader)|shader[_\s]?opt|loop[_\s]?limit|pso\b|pipeline\s*state)\b", re.I), "shader-barrier/pso"),
        (2, re.compile(r"\b(v_[a-z0-9_]+|s_[a-z0-9_]+|buffer_load|image_sample|ds_read|ds_write)\b", re.I), "gcn/rdna-op"),
    ],
    "stability": [
        (4, re.compile(r"\b(tdr\b|device\s*removed|device[_\s]?lost|dxgi_error_device|gpu\s*hang|driver\s*hang)\b", re.I), "tdr/device-lost"),
        (4, re.compile(r"\b(hang|hung|crash|crashes|crashing|deadlock|freeze|freezes|dred)\b", re.I), "hang/crash"),
        (3, re.compile(r"\b(leak|leaks|leaking|oom\b|out\s+of\s+memory|access\s+violation|segfault|panic|unwrap)\b", re.I), "leak/oom/panic"),
        (3, re.compile(r"\b(race\b|data\s*race|race\s*condition|use[-\s]?after[-\s]?free|corrupt\w*|barrier)\b", re.I), "race/barrier"),
        (2, re.compile(r"\b(assert|exception|timeout|watchdog|retry|stability|uptime|fault)\b", re.I), "stability"),
    ],
}

# File-path heuristics: (regex on changed file path, category, weight, tag)
FILE_PATH_RULES: list[tuple[re.Pattern[str], str, int, str]] = [
    (re.compile(r"(watch|relay|heap_pass|indirect|staging|frame|pool|submit)\.rs$", re.I), "perf", 3, "file:perf"),
    (re.compile(r"(texture|tiles|xmem|dstorage|xg|residen|stream|mipmap)", re.I), "streaming_lod", 3, "file:streaming"),
    (re.compile(r"(bvh|shadow|intersect|ray|light|rt)", re.I), "lighting_rt", 3, "file:lighting"),
    (re.compile(r"(victory-shader|shader|emit/|wave|dxil|dxbc|gfx10|rdna|bitcode|valu|lds|pipeline)", re.I), "shader_wave", 3, "file:shader"),
    (re.compile(r"(bcpack|bc7|formats|swizzle|bake)", re.I), "visual_skin_face", 2, "file:formats"),
    (re.compile(r"(descriptor|resource|barrier|crash|dred|guard|sync)", re.I), "stability", 2, "file:stability"),
    (re.compile(r"(vertex|skin|morph|anim|buffer_view)", re.I), "animation_mouth", 2, "file:anim"),
]

# Generic env vars that exist everywhere and should NOT alone trigger LIKELY ALREADY HAVE
GENERIC_ENV_VARS: frozenset[str] = frozenset({
    "VICTORY_LOG",
    "VICTORY_DATA_DIR",
    "VICTORY_D3D12_DEBUG",
    "VICTORY_D3D12_GBV",
    "VICTORY_D3D12_DRED",
    "VICTORY_D3D12_WARP",
    "VICTORY_CAPTURE",
    "VICTORY_CAPTURE_FRAMES",
    "VICTORY_DUMP_TARGETS",
    "VICTORY_HOST_WAVE",
    "VICTORY_HOST_MAX_WAVE",
    "VICTORY_VIEW_LIMIT",
    "VICTORY_DXIL",
    "VICTORY_AGILITY_SDK",
    "VICTORY_WATCH",
    "VICTORY_KEYS",
    "VICTORY_STACKS",
})

# Stopwords for commit-subject fuzzy matching
STOPWORDS: frozenset[str] = frozenset({
    "a", "an", "the", "and", "or", "for", "of", "in", "on", "to", "at", "by", "from",
    "with", "without", "into", "onto", "as", "is", "are", "was", "were", "be", "been",
    "it", "its", "this", "that", "these", "those", "not", "no", "do", "does", "did",
    "add", "added", "fix", "fixed", "fixes", "feat", "perf", "refactor", "chore", "docs",
    "doc", "test", "tests", "ci", "wip", "update", "updated", "use", "using", "make",
    "set", "get", "new", "only", "when", "after", "before", "more", "less", "all",
    "merge", "pull", "request", "branch", "main", "master", "victory", "pr",
})

LOW_SIGNAL_SUBJECT_RE = re.compile(
    r"^(merge\s+(remote-tracking\s+)?branch|merge\s+pull\s+request|docs?(\([^)]*\))?:|"
    r"chore(\([^)]*\))?:|ci(\([^)]*\))?:|style(\([^)]*\))?:|bump\b|update\s+readme|"
    r"readme\b|typo|formatting|rustfmt|clippy\s+lint|changelog|license)\b",
    re.I,
)

METRIC_EXTRACT_RE = re.compile(
    r"(?:\b\d+(?:\.\d+)?(?:\s*[-–to]+\s*\d+(?:\.\d+)?)?\s*(?:fps|ms|gb|mb|kib|mib|%|min(?:utes?)?|h(?:ours?)?)\b|"
    r"\b\d+(?:\.\d+)?x\s+(?:faster|speedup|reduction)\b)",
    re.I,
)

ENV_VAR_RE = re.compile(r"\b(VICTORY_[A-Z0-9_]{3,})\b")
GITHUB_REPO_RE = re.compile(r"https?://github\.com/([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)")
FILE_PATH_MENTION_RE = re.compile(r"\b([A-Za-z0-9_/-]+\.(?:rs|hlsl|cpp|c|h|hpp|py|toml))\b")


# ---------------------------------------------------------------------------
# Data Models
# ---------------------------------------------------------------------------

@dataclass
class RepoProfile:
    repo: str  # canonical "owner/name"
    name: str = ""
    sources: list[str] = field(default_factory=list)
    is_priority_seed: bool = False
    is_victory_fork: bool = False
    stage: int | None = None
    rank_race: int | None = None
    kind: str = ""
    gpu: list[str] = field(default_factory=list)
    fps_low: float | None = None
    fps_high: float | None = None
    fps_text: str = ""
    stability_minutes: float | None = None
    stability_text: str = ""
    picture_step: int | None = None
    picture_text: str = ""
    tracker_status: str = ""
    tracker_changed: str = ""
    portwatch_tier: str = ""
    portwatch_render: float | None = None
    portwatch_fps: float | None = None
    portwatch_stability: float | None = None
    portwatch_highlights: list[str] = field(default_factory=list)
    portwatch_blockers: list[str] = field(default_factory=list)
    stars: int | None = None
    pushed_at: str = ""
    default_branch: str = ""
    extra_branches: list[str] = field(default_factory=list)
    readme_highlights: dict[str, Any] = field(default_factory=dict)
    scan_error: str = ""
    priority_score: float = 0.0


@dataclass
class ScoutItem:
    kind: str  # "commit" or "pr"
    repo: str
    item_id: str  # full SHA for commit, "PR#123" for PR
    short_id: str  # 7-char SHA or "#123"
    title: str
    branch: str = ""
    body_snippet: str = ""
    author: str = ""
    timestamp: str = ""
    url: str = ""
    state: str = ""  # "commit", "open", "merged", "closed"
    files: list[str] = field(default_factory=list)
    categories: list[str] = field(default_factory=list)
    category_scores: dict[str, int] = field(default_factory=dict)
    matched_tags: list[str] = field(default_factory=list)
    env_vars: list[str] = field(default_factory=list)
    metrics: list[str] = field(default_factory=list)
    local_status: str = "NEW"  # "NEW" or "LIKELY ALREADY HAVE"
    local_reason: str = ""
    seen_before: bool = False
    priority_score: float = 0.0


@dataclass
class LocalRepoIndex:
    repo_path: str
    origin_slug: str = ""
    head_sha: str = ""
    head_subject: str = ""
    commit_shas_full: set[str] = field(default_factory=set)
    commit_shas_short: set[str] = field(default_factory=set)
    commit_subjects: list[tuple[str, str, str, frozenset[str]]] = field(default_factory=list)
    # tuple: (short_sha, raw_subject, normalized_subject, token_set)
    env_vars: set[str] = field(default_factory=set)
    symbols: set[str] = field(default_factory=set)
    source_files_scanned: int = 0


# ---------------------------------------------------------------------------
# Utilities
# ---------------------------------------------------------------------------

def utc_now() -> datetime:
    return datetime.now(timezone.utc)


def utc_iso(dt: datetime | None = None) -> str:
    d = dt or utc_now()
    return d.strftime("%Y-%m-%dT%H:%M:%SZ")


def parse_iso_ts(value: str) -> float:
    if not value:
        return 0.0
    try:
        s = value.strip()
        if s.endswith("Z"):
            s = s[:-1] + "+00:00"
        return datetime.fromisoformat(s).timestamp()
    except (ValueError, TypeError):
        return 0.0


def format_age(iso_ts: str, now_ts: float | None = None) -> str:
    ts = parse_iso_ts(iso_ts)
    if ts <= 0:
        return "?"
    now = now_ts if now_ts is not None else time.time()
    delta = max(0.0, now - ts)
    if delta < 60:
        return f"{int(delta)}s ago"
    if delta < 3600:
        return f"{int(delta // 60)}m ago"
    if delta < 86400:
        return f"{delta / 3600:.1f}h ago"
    return f"{delta / 86400:.1f}d ago"


def atomic_write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f"{path.name}.{os.getpid()}.tmp")
    tmp.write_text(text, encoding="utf-8")
    os.replace(tmp, path)


def normalize_repo_slug(raw: str | None) -> str:
    if not raw:
        return ""
    s = raw.strip()
    m = GITHUB_REPO_RE.search(s)
    if m:
        s = m.group(1)
    s = s.strip("/")
    if s.endswith(".git"):
        s = s[:-4]
    parts = s.split("/")
    if len(parts) < 2:
        return ""
    owner, name = parts[0].strip(), parts[1].strip()
    if not owner or not name:
        return ""
    # Exclude GitHub meta pages
    if owner.lower() in {"topics", "search", "explore", "marketplace", "orgs", "users", "settings", "features"}:
        return ""
    return f"{owner}/{name}"


def normalize_subject(text: str) -> str:
    s = text.lower().strip()
    s = re.sub(r"^merge\s+pr\s+#\d+:\s*", "", s)
    s = re.sub(r"^(feat|fix|perf|refactor|chore|docs?|test|ci)(\([^)]*\))?:\s*", "", s)
    s = re.sub(r"\(#\d+\)\s*$", "", s)
    s = re.sub(r"\([^)]*\)\s*$", "", s)  # trailing author name e.g. "(KainanP)"
    s = re.sub(r"[^a-z0-9_]+", " ", s).strip()
    return re.sub(r"\s+", " ", s)


def extract_tokens(text: str) -> frozenset[str]:
    norm = normalize_subject(text)
    tokens = {
        tok
        for tok in norm.split()
        if len(tok) >= 3 and tok not in STOPWORDS and not tok.isdigit()
    }
    return frozenset(tokens)


def truncate_one_line(text: str, max_len: int = 140) -> str:
    clean = re.sub(r"\s+", " ", (text or "").strip())
    if len(clean) <= max_len:
        return clean
    return clean[: max_len - 1].rstrip() + "…"


def safe_float(value: Any) -> float | None:
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return float(value)
    if isinstance(value, str):
        try:
            return float(value.strip())
        except ValueError:
            return None
    return None


def configure_console_utf8() -> None:
    for stream in (sys.stdout, sys.stderr):
        try:
            if hasattr(stream, "reconfigure"):
                stream.reconfigure(encoding="utf-8", errors="replace")
        except Exception:
            pass


# ---------------------------------------------------------------------------
# GitHub Token Resolution
# ---------------------------------------------------------------------------

def resolve_github_token() -> tuple[str | None, str]:
    """Resolve GitHub token from GITHUB_TOKEN, GH_TOKEN, or `gh auth token`."""
    for env_name in ("GITHUB_TOKEN", "GH_TOKEN"):
        val = os.environ.get(env_name, "").strip()
        if val:
            return val, f"env:{env_name}"
    try:
        res = subprocess.run(
            ["gh", "auth", "token"],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=10,
        )
        if res.returncode == 0:
            tok = res.stdout.strip()
            if tok:
                return tok, "gh-cli"
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        pass
    return None, "unauthenticated (60 req/hr)"


# ---------------------------------------------------------------------------
# Polite HTTP Client with ETag / If-Modified-Since Caching & Rate Limiting
# ---------------------------------------------------------------------------

class HttpClient:
    """Thread-safe urllib HTTP client with disk ETag cache & GitHub rate-limit handling."""

    def __init__(
        self,
        token: str | None,
        token_source: str,
        cache_path: Path,
        timeout: int = 25,
    ) -> None:
        self.token = token
        self.token_source = token_source
        self.cache_path = cache_path
        self.timeout = timeout
        self._lock = threading.Lock()
        self._cache: dict[str, dict[str, Any]] = {}
        self._dirty = False

        # Rate-limit telemetry
        self.rate_limit: int | None = None
        self.rate_remaining: int | None = None
        self.rate_reset: int | None = None
        self.requests_made: int = 0
        self.cache_hits_304: int = 0
        self.cache_hits_immutable: int = 0
        self.http_errors: int = 0

        self._load_cache()

    def _load_cache(self) -> None:
        if not self.cache_path.is_file():
            return
        try:
            data = json.loads(self.cache_path.read_text(encoding="utf-8"))
            if isinstance(data, dict):
                self._cache = data
        except (OSError, json.JSONDecodeError):
            self._cache = {}

    def save_cache(self) -> None:
        with self._lock:
            if not self._dirty:
                return
            # Prune oldest entries if cache grows very large (> 1500 entries)
            if len(self._cache) > 1500:
                ordered = sorted(
                    self._cache.items(),
                    key=lambda kv: kv[1].get("cached_at", 0.0),
                    reverse=True,
                )
                self._cache = dict(ordered[:1200])
            snapshot = json.dumps(self._cache, ensure_ascii=False)
            self._dirty = False
        try:
            atomic_write_text(self.cache_path, snapshot)
        except OSError:
            pass

    @staticmethod
    def _url_key(url: str) -> str:
        return hashlib.sha256(url.encode("utf-8")).hexdigest()[:24]

    def _update_rate_headers(self, headers: Any) -> None:
        if headers is None:
            return
        rem = headers.get("X-RateLimit-Remaining")
        lim = headers.get("X-RateLimit-Limit")
        rst = headers.get("X-RateLimit-Reset")
        with self._lock:
            if rem is not None:
                try:
                    self.rate_remaining = int(rem)
                except ValueError:
                    pass
            if lim is not None:
                try:
                    self.rate_limit = int(lim)
                except ValueError:
                    pass
            if rst is not None:
                try:
                    self.rate_reset = int(rst)
                except ValueError:
                    pass

    def remaining_quota(self) -> int | None:
        with self._lock:
            return self.rate_remaining

    def get(
        self,
        url: str,
        *,
        is_github_api: bool = False,
        immutable: bool = False,
        max_bytes: int = 8 * 1024 * 1024,
    ) -> tuple[int, str | None, str]:
        """Fetch URL with ETag/If-Modified-Since caching.

        Returns: (status_code, body_text_or_none, error_message)
        Status 200 or 304 returns cached/fresh body text.
        """
        key = self._url_key(url)
        with self._lock:
            cached = self._cache.get(key)

        # Immutable resources (like a specific commit SHA detail) can be served straight from cache
        if immutable and cached and isinstance(cached.get("body"), str):
            with self._lock:
                self.cache_hits_immutable += 1
            return 200, cached["body"], ""

        # If GitHub API quota is critically low, fall back to cache if present
        if is_github_api:
            rem = self.remaining_quota()
            if rem is not None and rem < 10:
                if cached and isinstance(cached.get("body"), str):
                    return 200, cached["body"], ""
                return 429, None, "GitHub API rate limit nearly exhausted"

        headers: dict[str, str] = {
            "User-Agent": USER_AGENT,
            "Accept-Encoding": "identity",
        }
        if is_github_api:
            headers["Accept"] = "application/vnd.github+json"
            headers["X-GitHub-Api-Version"] = "2022-11-28"
            if self.token:
                headers["Authorization"] = f"Bearer {self.token}"

        if cached:
            if cached.get("etag"):
                headers["If-None-Match"] = cached["etag"]
            if cached.get("last_modified"):
                headers["If-Modified-Since"] = cached["last_modified"]

        attempts = 2
        for attempt in range(attempts):
            req = urllib.request.Request(url, headers=headers)
            try:
                with self._lock:
                    self.requests_made += 1
                with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                    if is_github_api:
                        self._update_rate_headers(resp.headers)
                    raw = resp.read(max_bytes + 1)
                    if len(raw) > max_bytes:
                        return 413, None, f"response exceeded {max_bytes} bytes"
                    body = raw.decode("utf-8", errors="replace")
                    etag = resp.headers.get("ETag") or ""
                    last_mod = resp.headers.get("Last-Modified") or ""
                    if etag or last_mod or immutable:
                        with self._lock:
                            self._cache[key] = {
                                "url": url,
                                "etag": etag,
                                "last_modified": last_mod,
                                "body": body,
                                "cached_at": time.time(),
                            }
                            self._dirty = True
                    return resp.status, body, ""

            except urllib.error.HTTPError as exc:
                if is_github_api:
                    self._update_rate_headers(exc.headers)
                if exc.code == 304 and cached and isinstance(cached.get("body"), str):
                    with self._lock:
                        self.cache_hits_304 += 1
                        self._cache[key]["cached_at"] = time.time()
                        self._dirty = True
                    return 304, cached["body"], ""

                if exc.code in (403, 429):
                    retry_after = exc.headers.get("Retry-After") if exc.headers else None
                    if retry_after and attempt + 1 < attempts:
                        try:
                            wait_s = min(15.0, max(1.0, float(retry_after)))
                        except ValueError:
                            wait_s = 3.0
                        time.sleep(wait_s)
                        continue
                    # Secondary rate limit or quota exhausted; return cache if available
                    if cached and isinstance(cached.get("body"), str):
                        return 200, cached["body"], ""
                    with self._lock:
                        self.http_errors += 1
                    return exc.code, None, f"HTTP {exc.code} (rate limited or forbidden)"

                if exc.code in (500, 502, 503, 504) and attempt + 1 < attempts:
                    time.sleep(1.5 * (attempt + 1))
                    continue

                with self._lock:
                    self.http_errors += 1
                reason = exc.reason or ""
                return exc.code, None, f"HTTP {exc.code} {reason}".strip()

            except (urllib.error.URLError, TimeoutError, OSError) as exc:
                if attempt + 1 < attempts:
                    time.sleep(1.0)
                    continue
                with self._lock:
                    self.http_errors += 1
                return 0, None, str(exc)

        return 0, None, "request failed"

    def get_json(
        self,
        url: str,
        *,
        is_github_api: bool = False,
        immutable: bool = False,
    ) -> tuple[int, Any, str]:
        status, body, err = self.get(url, is_github_api=is_github_api, immutable=immutable)
        if body is None:
            return status, None, err
        try:
            return status, json.loads(body), ""
        except json.JSONDecodeError as exc:
            return status, None, f"JSON decode error: {exc}"


# ---------------------------------------------------------------------------
# Local Repository Indexing & Cross-Checking
# ---------------------------------------------------------------------------

def _run_git(repo_path: Path, args: list[str], timeout: int = 20) -> str:
    try:
        res = subprocess.run(
            ["git", "-C", str(repo_path), *args],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
        )
        if res.returncode == 0:
            return res.stdout.strip()
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        pass
    return ""


def build_local_index(repo_path: Path) -> LocalRepoIndex:
    """Index local git history, VICTORY_* env vars, and key symbols for cross-checking."""
    idx = LocalRepoIndex(repo_path=str(repo_path))
    if not repo_path.is_dir():
        return idx

    origin = _run_git(repo_path, ["remote", "get-url", "origin"])
    idx.origin_slug = normalize_repo_slug(origin)

    log_out = _run_git(repo_path, ["log", "-n", "600", "--format=%H\x1f%s"])
    if log_out:
        for i, line in enumerate(log_out.splitlines()):
            parts = line.split("\x1f", 1)
            if len(parts) != 2:
                continue
            sha, subj = parts[0].strip(), parts[1].strip()
            if i == 0:
                idx.head_sha = sha
                idx.head_subject = subj
            idx.commit_shas_full.add(sha.lower())
            idx.commit_shas_short.add(sha[:7].lower())
            norm = normalize_subject(subj)
            toks = extract_tokens(subj)
            idx.commit_subjects.append((sha[:7], subj, norm, toks))
            for ev in ENV_VAR_RE.findall(subj):
                idx.env_vars.add(ev)

    # Scan source directories for VICTORY_* env vars and key identifiers
    scan_roots = [
        repo_path / "third_party" / "victory",
        repo_path / "runtime",
        repo_path / "rust",
        repo_path / "tools",
    ]
    skip_dirs = {"target", ".git", "__pycache__", "node_modules", "dist", "build", ".venv", "venv"}
    valid_exts = {".rs", ".py", ".c", ".cpp", ".h", ".hpp", ".hlsl", ".toml", ".md"}
    sym_re = re.compile(r"\b(?:pub\s+)?(?:async\s+)?(?:fn|struct|enum|const|static)\s+([A-Za-z_][A-Za-z0-9_]{5,})\b")

    for root in scan_roots:
        if not root.is_dir():
            continue
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames[:] = [d for d in dirnames if d not in skip_dirs]
            for fname in filenames:
                fpath = Path(dirpath) / fname
                if fpath.suffix.lower() not in valid_exts:
                    continue
                try:
                    if fpath.stat().st_size > 1_500_000:
                        continue
                    text = fpath.read_text(encoding="utf-8", errors="ignore")
                except OSError:
                    continue
                idx.source_files_scanned += 1
                for ev in ENV_VAR_RE.findall(text):
                    idx.env_vars.add(ev)
                if fpath.suffix.lower() == ".rs":
                    for sym in sym_re.findall(text):
                        idx.symbols.add(sym)

    return idx


def cross_check_local(item: ScoutItem, local: LocalRepoIndex) -> None:
    """Annotate item with [NEW] vs [LIKELY ALREADY HAVE] and reason."""
    # 1. Direct SHA match
    if item.kind == "commit":
        sha_low = item.item_id.lower()
        if sha_low in local.commit_shas_full or sha_low[:7] in local.commit_shas_short:
            item.local_status = "LIKELY ALREADY HAVE"
            item.local_reason = f"commit `{item.short_id}` present in local git history"
            return

    # 2. Exact or high-overlap commit subject match against local history
    norm_title = normalize_subject(item.title)
    title_tokens = extract_tokens(item.title)
    best_overlap = 0.0
    best_match: tuple[str, str] | None = None

    if norm_title and len(norm_title) >= 12:
        for l_sha, l_raw, l_norm, l_toks in local.commit_subjects:
            if norm_title == l_norm:
                item.local_status = "LIKELY ALREADY HAVE"
                item.local_reason = f"exact subject match with local `{l_sha}` ({truncate_one_line(l_raw, 65)})"
                return
            if len(title_tokens) >= 3 and len(l_toks) >= 3:
                inter = title_tokens & l_toks
                union = title_tokens | l_toks
                if len(inter) >= 3 and union:
                    jaccard = len(inter) / len(union)
                    if jaccard > best_overlap:
                        best_overlap = jaccard
                        best_match = (l_sha, l_raw)

    if best_overlap >= 0.62 and best_match is not None:
        item.local_status = "LIKELY ALREADY HAVE"
        item.local_reason = (
            f"similar to local `{best_match[0]}` ({int(best_overlap * 100)}% overlap: "
            f"{truncate_one_line(best_match[1], 60)})"
        )
        return

    # 3. Specific VICTORY_* env vars all already present in local codebase
    specific_vars = [v for v in item.env_vars if v not in GENERIC_ENV_VARS]
    if specific_vars:
        present = [v for v in specific_vars if v in local.env_vars]
        missing = [v for v in specific_vars if v not in local.env_vars]
        if present and not missing:
            item.local_status = "LIKELY ALREADY HAVE"
            item.local_reason = f"env var(s) already in local tree: {', '.join(present[:3])}"
            return

    # 4. Specific code symbols explicitly referenced in title (backticks) all present locally
    backtick_syms = re.findall(r"`([a-z][a-z0-9_]{8,})`", item.title)
    if backtick_syms:
        present_syms = [s for s in backtick_syms if s in local.symbols]
        if len(present_syms) == len(backtick_syms) and len(present_syms) >= 2:
            item.local_status = "LIKELY ALREADY HAVE"
            item.local_reason = f"symbols already in local tree: {', '.join(present_syms[:3])}"
            return

    item.local_status = "NEW"
    if specific_vars:
        missing = [v for v in specific_vars if v not in local.env_vars]
        if missing:
            item.local_reason = f"introduces/uses unseen env var(s): {', '.join(missing[:3])}"


# ---------------------------------------------------------------------------
# Tracker Discovery (gta6-pc-race + PortWatch VI + Seeds)
# ---------------------------------------------------------------------------

def fetch_gta6_pc_race(client: HttpClient) -> tuple[dict[str, Any], list[dict[str, Any]], str]:
    """Fetch gta6-pc-race structured data (JSON primary, HTML fallback)."""
    status, data, err = client.get_json(GTA6_PC_RACE_JSON_URL, is_github_api=False)
    if isinstance(data, dict) and data.get("projects"):
        meta = {
            "source": "gta6-pc-race (data.json)",
            "checked": data.get("checked", ""),
            "playable": data.get("playable", ""),
            "leader": data.get("leader", ""),
            "headline": data.get("headline", ""),
            "updates": data.get("updates", [])[:6],
        }
        return meta, data.get("projects", []), ""

    # Fallback: scrape HTML page
    h_status, html_body, h_err = client.get(GTA6_PC_RACE_HTML_URL, is_github_api=False)
    if html_body:
        repos_found: list[dict[str, Any]] = []
        seen: set[str] = set()
        for m in GITHUB_REPO_RE.finditer(html_body):
            slug = normalize_repo_slug(m.group(1))
            if not slug or slug.lower() in seen or slug.lower() == "vuyp/gta6-pc-race":
                continue
            seen.add(slug.lower())
            repos_found.append({"repo": slug, "name": slug.split("/")[1], "stage": None})
        return {"source": "gta6-pc-race (HTML fallback)", "checked": ""}, repos_found, ""

    return {"source": "gta6-pc-race (unavailable)"}, [], err or h_err


def fetch_portwatch_vi(client: HttpClient) -> tuple[dict[str, Any], list[dict[str, Any]], list[dict[str, Any]], str]:
    """Fetch PortWatch VI structured data (JSON primary, HTML fallback)."""
    status, data, err = client.get_json(PORTWATCH_JSON_URL, is_github_api=False)
    if isinstance(data, dict) and data.get("efforts"):
        efforts = data.get("efforts", [])
        standings: list[dict[str, Any]] = []
        for eff in efforts:
            if not isinstance(eff, dict):
                continue
            slug = normalize_repo_slug(eff.get("repo")) or str(eff.get("name") or "")
            gta = eff.get("gta") if isinstance(eff.get("gta"), dict) else {}
            stats = eff.get("stats") if isinstance(eff.get("stats"), dict) else {}
            standings.append(
                {
                    "name": str(eff.get("name") or slug),
                    "repo": slug,
                    "tier": str(eff.get("tier") or ""),
                    "branch": str(eff.get("branch") or ""),
                    "playable": safe_float(gta.get("playable")),
                    "fps": safe_float(gta.get("fps")),
                    "render": safe_float(gta.get("render")),
                    "stability": safe_float(gta.get("stability")),
                    "gpu": [str(g) for g in (eff.get("gpu") or []) if isinstance(g, str)],
                    "highlights": [str(x) for x in (eff.get("highlights") or [])[:3]],
                    "blockers": [str(x) for x in (eff.get("blockers") or [])[:2]],
                    "pushed_at": str(stats.get("pushedAt") or ""),
                    "stars": stats.get("stars") if isinstance(stats.get("stars"), int) else None,
                }
            )
        standings.sort(
            key=lambda e: (
                1 if e["tier"] == "tracked" else 0,
                e["playable"] if e["playable"] is not None else -1.0,
                e["fps"] if e["fps"] is not None else -1.0,
                e["render"] if e["render"] is not None else -1.0,
                e["stability"] if e["stability"] is not None else -1.0,
            ),
            reverse=True,
        )
        meta = {
            "source": "portwatch-vi (tracker.json)",
            "url": PORTWATCH_JSON_URL,
            "updated": data.get("updated", ""),
            "changes": data.get("changes", [])[:8],
            "standings": standings,
        }
        return meta, efforts, data.get("candidates", []), ""

    # Fallback: scrape HTML page
    h_status, html_body, h_err = client.get(PORTWATCH_HTML_URL, is_github_api=False)
    if html_body:
        efforts: list[dict[str, Any]] = []
        seen: set[str] = set()
        for m in GITHUB_REPO_RE.finditer(html_body):
            slug = normalize_repo_slug(m.group(1))
            if not slug or slug.lower() in seen or slug.lower() == "4lwayz/portwatch-vi":
                continue
            seen.add(slug.lower())
            efforts.append({"repo": slug, "name": slug.split("/")[1], "tier": "scraped"})
        return {"source": "portwatch-vi (HTML fallback)", "updated": "", "standings": []}, efforts, [], ""

    return {"source": "portwatch-vi (unavailable)", "updated": "", "standings": []}, [], [], err or h_err


def _is_victory_related(text_parts: list[str]) -> bool:
    blob = " ".join(p for p in text_parts if p).lower()
    return any(
        kw in blob
        for kw in (
            "victory",
            "el1xr",
            "klbf-lab",
            "vice-tay",
            "kelvaroq",
            "stellarr",
            "vi-br-port",
            "marvis",
            "cgbj",
            "ruff-lab",
            "vitoriabr",
            "vinividivici",
            "vilocity",
            "neptune",
            "ztrx",
            "cool_fox",
            "flamingo",
            "anyxboxx",
            "belobug",
            "dexnom",
            "fxceless",
            "avatarmanz",
            "peppa",
            "mandy-port",
            "ccs03",
            "m1lk",
            "dolberth",
            "lnx-victory",
            "lumen",
            "lepotane",
        )
    )


def compute_repo_priority(p: RepoProfile, now_ts: float) -> float:
    """Rank repos so top actionable Stage-6 Victory forks bubble to the top."""
    score = 0.0
    if p.is_priority_seed:
        score += 120.0
    if p.stage == 6:
        score += 65.0
    elif p.stage == 5:
        score += 20.0
    elif p.stage is not None:
        score += float(p.stage) * 3.0

    if p.is_victory_fork:
        score += 35.0
    if p.portwatch_tier == "tracked":
        score += 12.0
    if len(p.sources) >= 2:
        score += 8.0

    # Performance signal
    fps_best = max(
        (x for x in (p.fps_high, p.fps_low, p.portwatch_fps) if x is not None),
        default=0.0,
    )
    score += min(fps_best, 30.0) * 1.4

    # Stability signal
    if p.stability_minutes is not None:
        score += min(p.stability_minutes, 90.0) * 0.25
    elif p.portwatch_stability is not None:
        score += p.portwatch_stability * 15.0

    # Picture quality signal
    if p.picture_step is not None:
        score += p.picture_step * 5.0
    elif p.portwatch_render is not None:
        score += p.portwatch_render * 15.0

    # Recency boost
    latest_ts = max(parse_iso_ts(p.tracker_changed), parse_iso_ts(p.pushed_at))
    if latest_ts > 0:
        age_hours = max(0.0, (now_ts - latest_ts) / 3600.0)
        if age_hours <= 6:
            score += 16.0
        elif age_hours <= 24:
            score += 10.0
        elif age_hours <= 72:
            score += 4.0

    # Slight penalty for tracker-only candidate without stage confirmation
    if p.stage is None and not p.is_priority_seed and p.portwatch_tier != "tracked":
        score -= 10.0

    return round(score, 2)


def discover_repos(
    client: HttpClient,
    self_slug: str = "",
) -> tuple[list[RepoProfile], RepoProfile | None, dict[str, Any]]:
    """Discover, merge, and rank repositories across both trackers and seeds."""
    race_meta, race_projects, race_err = fetch_gta6_pc_race(client)
    pw_meta, pw_efforts, pw_candidates, pw_err = fetch_portwatch_vi(client)

    by_key: dict[str, RepoProfile] = {}

    def get_or_create(slug: str, display_name: str = "") -> RepoProfile:
        key = slug.lower()
        if key not in by_key:
            by_key[key] = RepoProfile(repo=slug, name=display_name or slug.split("/")[1])
        elif display_name and not by_key[key].name:
            by_key[key].name = display_name
        return by_key[key]

    # 1. Seed priority list
    for slug in PRIORITY_SEEDS:
        prof = get_or_create(slug)
        prof.is_priority_seed = True
        prof.is_victory_fork = True
        if "seed" not in prof.sources:
            prof.sources.append("seed")

    # 2. Merge gta6-pc-race projects
    for proj in race_projects:
        slug = normalize_repo_slug(proj.get("repo"))
        if not slug:
            continue
        prof = get_or_create(slug, proj.get("name") or "")
        if "gta6-pc-race" not in prof.sources:
            prof.sources.append("gta6-pc-race")
        prof.stage = proj.get("stage") if isinstance(proj.get("stage"), int) else prof.stage
        prof.rank_race = proj.get("pos") if isinstance(proj.get("pos"), int) else prof.rank_race
        prof.kind = proj.get("kind") or prof.kind
        prof.tracker_status = proj.get("status") or prof.tracker_status
        prof.tracker_changed = proj.get("changed") or prof.tracker_changed

        tested = proj.get("tested")
        if isinstance(tested, dict):
            cards = tested.get("cards") or []
            if isinstance(cards, list):
                for c in cards:
                    if isinstance(c, str) and c not in prof.gpu:
                        prof.gpu.append(c)

        lanes = proj.get("lanes")
        if isinstance(lanes, dict):
            pic = lanes.get("picture")
            if isinstance(pic, dict):
                prof.picture_step = pic.get("step") if isinstance(pic.get("step"), int) else prof.picture_step
                prof.picture_text = pic.get("text") or prof.picture_text
            spd = lanes.get("speed")
            if isinstance(spd, dict):
                prof.fps_low = safe_float(spd.get("low")) if spd.get("low") is not None else prof.fps_low
                prof.fps_high = safe_float(spd.get("high")) if spd.get("high") is not None else prof.fps_high
                prof.fps_text = spd.get("text") or prof.fps_text
            stab = lanes.get("stability")
            if isinstance(stab, dict):
                prof.stability_minutes = (
                    safe_float(stab.get("minutes"))
                    if stab.get("minutes") is not None
                    else prof.stability_minutes
                )
                prof.stability_text = stab.get("text") or prof.stability_text

        if _is_victory_related([prof.repo, prof.name, prof.kind, prof.tracker_status]):
            prof.is_victory_fork = True

    # 3. Merge PortWatch VI tracked efforts
    for eff in pw_efforts:
        slug = normalize_repo_slug(eff.get("repo"))
        if not slug:
            continue
        prof = get_or_create(slug, eff.get("name") or "")
        if "portwatch-vi" not in prof.sources:
            prof.sources.append("portwatch-vi")
        prof.portwatch_tier = eff.get("tier") or prof.portwatch_tier
        pw_branch = str(eff.get("branch") or "").strip()
        if pw_branch:
            if not prof.default_branch:
                prof.default_branch = pw_branch
            if pw_branch.lower() not in {"main", "master"} and pw_branch not in prof.extra_branches:
                prof.extra_branches.append(pw_branch)
        if isinstance(eff.get("gpu"), list):
            for g in eff["gpu"]:
                if isinstance(g, str) and g not in prof.gpu:
                    prof.gpu.append(g)
        if isinstance(eff.get("highlights"), list):
            prof.portwatch_highlights = [str(x) for x in eff["highlights"][:5]]
        if isinstance(eff.get("blockers"), list):
            prof.portwatch_blockers = [str(x) for x in eff["blockers"][:5]]
        if not prof.kind and eff.get("summary"):
            prof.kind = truncate_one_line(str(eff["summary"]), 140)

        gta = eff.get("gta")
        if isinstance(gta, dict):
            prof.portwatch_fps = safe_float(gta.get("fps"))
            prof.portwatch_render = safe_float(gta.get("render"))
            prof.portwatch_stability = safe_float(gta.get("stability"))
            if prof.stage is None and safe_float(gta.get("playable")) == 1.0:
                prof.stage = 6

        stats = eff.get("stats")
        if isinstance(stats, dict):
            if isinstance(stats.get("stars"), int):
                prof.stars = stats["stars"]
            if stats.get("pushedAt"):
                prof.pushed_at = str(stats["pushedAt"])

        based_on = eff.get("basedOn") or []
        based_str = " ".join(str(x) for x in based_on) if isinstance(based_on, list) else str(based_on)
        if _is_victory_related([prof.repo, prof.name, prof.kind, based_str, str(eff.get("summary", ""))]):
            prof.is_victory_fork = True

    # 4. Auto-discover all public repos listed in PortWatch VI candidates
    for cand in pw_candidates:
        if not isinstance(cand, dict):
            continue
        slug = normalize_repo_slug(cand.get("repo"))
        if not slug:
            continue
        desc = str(cand.get("description") or "")
        prof = get_or_create(slug, str(cand.get("name") or ""))
        if "portwatch-candidate" not in prof.sources:
            prof.sources.append("portwatch-candidate")
        if _is_victory_related([slug, str(cand.get("name") or ""), desc]):
            prof.is_victory_fork = True
        cand_branch = str(cand.get("branch") or "").strip()
        if cand_branch and cand_branch.lower() not in {"main", "master"} and cand_branch not in prof.extra_branches:
            prof.extra_branches.append(cand_branch)
        if not prof.kind and desc:
            prof.kind = truncate_one_line(desc, 140)
        if cand.get("pushedAt") and not prof.pushed_at:
            prof.pushed_at = str(cand["pushedAt"])
        if isinstance(cand.get("stars"), int) and prof.stars is None:
            prof.stars = cand["stars"]

    # 5. If both trackers failed completely, include fallback seeds so tool still works
    if race_err and pw_err:
        for slug in FALLBACK_SEEDS:
            prof = get_or_create(slug)
            prof.is_victory_fork = True
            if "fallback-seed" not in prof.sources:
                prof.sources.append("fallback-seed")

    # Attach configured extra branches (e.g. Belobug/VIctory-Belobug-AMD rx9070xt)
    for key, prof in by_key.items():
        for br in EXTRA_BRANCHES.get(key, ()):
            if br not in prof.extra_branches:
                prof.extra_branches.append(br)

    # Track self repo standing (e.g. LooperSalty/AnyXboxX), keeping it in the
    # scan list when explicitly listed in PRIORITY_SEEDS so upstream PRs/commits
    # are still monitored.
    self_key = self_slug.lower() if self_slug else ""
    self_profile: RepoProfile | None = None
    self_popped = False
    if self_key and self_key in by_key:
        if by_key[self_key].is_priority_seed:
            self_profile = by_key[self_key]
        else:
            self_profile = by_key.pop(self_key)
            self_popped = True

    now_ts = time.time()
    profiles = list(by_key.values())
    for p in profiles:
        p.priority_score = compute_repo_priority(p, now_ts)

    profiles.sort(key=lambda p: (-p.priority_score, p.repo.lower()))

    tracker_info = {
        "gta6_pc_race": race_meta,
        "gta6_pc_race_error": race_err,
        "portwatch_vi": pw_meta,
        "portwatch_vi_error": pw_err,
        "total_discovered": len(profiles) + (1 if self_popped else 0),
        "stage6_victory_count": sum(1 for p in profiles if p.stage == 6 and p.is_victory_fork),
    }
    return profiles, self_profile, tracker_info


# ---------------------------------------------------------------------------
# Categorization & Actionability Scoring
# ---------------------------------------------------------------------------

def categorize_item(item: ScoutItem) -> None:
    """Categorize a commit/PR into actionable buckets using keywords & file paths."""
    title_text = item.title or ""
    body_text = item.body_snippet or ""
    combined_text = f"{title_text}\n{body_text}"

    # Extract VICTORY_* env vars and concrete metrics
    env_vars = sorted(set(ENV_VAR_RE.findall(combined_text)))
    item.env_vars = env_vars
    metrics = []
    seen_m: set[str] = set()
    for m in METRIC_EXTRACT_RE.findall(combined_text):
        norm_m = re.sub(r"\s+", " ", m.strip())
        if norm_m.lower() not in seen_m:
            seen_m.add(norm_m.lower())
            metrics.append(norm_m)
    item.metrics = metrics[:6]

    # Also detect file paths mentioned in commit/PR text
    mentioned_files = FILE_PATH_MENTION_RE.findall(combined_text)
    all_files = list(dict.fromkeys([*item.files, *mentioned_files]))

    scores: dict[str, int] = defaultdict(int)
    tags: list[str] = []
    seen_tags: set[str] = set()

    for cat, rules in CATEGORY_KEYWORDS.items():
        for weight, regex, tag in rules:
            if regex.search(title_text):
                scores[cat] += weight * 2  # title matches carry extra weight
                if tag not in seen_tags:
                    seen_tags.add(tag)
                    tags.append(tag)
            elif body_text and regex.search(body_text):
                scores[cat] += weight
                if tag not in seen_tags:
                    seen_tags.add(tag)
                    tags.append(tag)

    for fpath in all_files:
        for regex, cat, weight, tag in FILE_PATH_RULES:
            if regex.search(fpath):
                scores[cat] += weight
                if tag not in seen_tags:
                    seen_tags.add(tag)
                    tags.append(tag)

    # Sort categories by descending score, breaking ties by CATEGORY_ORDER
    order_idx = {c: i for i, c in enumerate(CATEGORY_ORDER)}
    matched_cats = sorted(
        (c for c, s in scores.items() if s >= 2),
        key=lambda c: (-scores[c], order_idx.get(c, 99)),
    )
    item.categories = matched_cats
    item.category_scores = {c: scores[c] for c in matched_cats}
    item.matched_tags = tags[:8]


def score_item(item: ScoutItem, repo: RepoProfile, now_ts: float) -> float:
    """Compute an actionability score so the highest-value NEW developments sort first."""
    score = 0.0

    # Strong separation between [NEW] and [LIKELY ALREADY HAVE]
    if item.local_status == "NEW":
        score += 40.0
    else:
        score -= 25.0

    # Bonus for unseen since last watcher run
    if not item.seen_before:
        score += 18.0

    # Category match strength
    if item.categories:
        top_cat_score = max(item.category_scores.values(), default=0)
        score += min(top_cat_score, 18) * 2.0 + len(item.categories) * 3.0
    else:
        score -= 12.0

    # Kind/state bonus
    if item.kind == "pr":
        if item.state == "merged":
            score += 14.0
        elif item.state == "open":
            score += 10.0
        else:
            score -= 18.0

    # Concrete performance/memory metrics or new env vars are super actionable
    if item.metrics:
        score += min(len(item.metrics), 3) * 5.0
    new_env_vars = [v for v in item.env_vars if v not in GENERIC_ENV_VARS]
    if new_env_vars and item.local_status == "NEW":
        score += 9.0

    # Repo tier influence
    if repo.is_priority_seed:
        score += 10.0
    elif repo.stage == 6 and repo.is_victory_fork:
        score += 6.0

    # Recency boost
    ts = parse_iso_ts(item.timestamp)
    if ts > 0:
        age_h = max(0.0, (now_ts - ts) / 3600.0)
        if age_h <= 6:
            score += 14.0
        elif age_h <= 24:
            score += 9.0
        elif age_h <= 72:
            score += 4.0

    # Down-rank pure docs/chore/merge noise
    if LOW_SIGNAL_SUBJECT_RE.match(item.title.strip()):
        score -= 24.0

    return round(score, 2)


# ---------------------------------------------------------------------------
# README Highlight Extraction
# ---------------------------------------------------------------------------

def extract_readme_highlights(readme_text: str) -> dict[str, Any]:
    """Extract concise FPS, stability, picture/visual notes, and env switches from README."""
    if not readme_text:
        return {}

    html_tag_re = re.compile(r"<[^>]+>")
    md_link_re = re.compile(r"\[([^\]]+)\]\([^)]+\)")

    cleaned_lines: list[str] = []
    for raw_ln in readme_text.splitlines():
        s = raw_ln.strip()
        if not s:
            continue
        # Skip raw HTML blocks, image/badge lines, and pure table separators
        low = s.lower()
        if (
            s.startswith(("![", "[![", "<!--", "<picture", "</picture", "<source", "<img", "<div", "</div", "<p ", "<a "))
            or "img.shields.io" in low
            or re.fullmatch(r"[\s|:-]+", s)
        ):
            continue
        s = md_link_re.sub(r"\1", s)
        s = html_tag_re.sub(" ", s)
        s = html.unescape(s)
        s = s.replace("**", "").replace("__", "")
        s = re.sub(r"^[#>*\-\s|]+", "", s).strip(" |")
        s = re.sub(r"\s+", " ", s)
        if len(s) >= 16:
            cleaned_lines.append(s)

    fps_notes: list[str] = []
    stability_notes: list[str] = []
    picture_notes: list[str] = []

    fps_re = re.compile(r"\b\d+(?:\.\d+)?(?:\s*[-–to]+\s*\d+(?:\.\d+)?)?\s*fps\b", re.I)
    stab_re = re.compile(
        r"\b(\d+(?:\.\d+)?\s*(?:min(?:utes?)?|h(?:ours?|rs?))|no\s+crash|without\s+(?:a\s+)?crash|"
        r"tdr|device\s+(?:lost|removed)|stable|uptime|playtest)\b",
        re.I,
    )
    pic_re = re.compile(
        r"\b(face|faces|skin|hair|shadow|light|ray\s*trac|reflection|water|glass|"
        r"texture|flicker|magenta|blown[-\s]?out|dark|sharp|picture|visual|artifact)\b",
        re.I,
    )

    for clean in cleaned_lines:
        if len(fps_notes) < 3 and fps_re.search(clean):
            fps_notes.append(truncate_one_line(clean, 150))
        if len(stability_notes) < 3 and stab_re.search(clean):
            stability_notes.append(truncate_one_line(clean, 150))
        if len(picture_notes) < 3 and pic_re.search(clean):
            picture_notes.append(truncate_one_line(clean, 150))
        if len(fps_notes) >= 3 and len(stability_notes) >= 3 and len(picture_notes) >= 3:
            break

    env_vars = sorted(set(ENV_VAR_RE.findall(readme_text)))
    return {
        "fps_notes": fps_notes,
        "stability_notes": stability_notes,
        "picture_notes": picture_notes,
        "env_vars_count": len(env_vars),
        "sample_env_vars": env_vars[:12],
    }


# ---------------------------------------------------------------------------
# Concurrent Deep Repo Scanner
# ---------------------------------------------------------------------------

def scan_repo(
    profile: RepoProfile,
    client: HttpClient,
    local_index: LocalRepoIndex,
    seen_commits: set[str],
    seen_prs: dict[str, str],
    *,
    commits_limit: int = 25,
    prs_limit: int = 15,
    fetch_commit_files: bool = True,
    max_commit_file_fetches: int = 4,
) -> tuple[RepoProfile, list[ScoutItem], set[str], dict[str, str]]:
    """Deep-scan a single GitHub repository for commits, PRs, and README highlights."""
    repo = profile.repo
    items: list[ScoutItem] = []
    new_seen_commits: set[str] = set()
    new_seen_prs: dict[str, str] = {}
    now_ts = time.time()

    # 1. Dynamically discover all active non-default branches for configured repos
    if repo.lower() in ALL_ACTIVE_BRANCHES_REPOS:
        b_url = f"https://api.github.com/repos/{repo}/branches?per_page=30"
        _, b_data, _ = client.get_json(b_url, is_github_api=True)
        if isinstance(b_data, list):
            default_br = (profile.default_branch or "main").lower()
            for entry in b_data:
                if not isinstance(entry, dict):
                    continue
                br_name = str(entry.get("name") or "").strip()
                if br_name and br_name.lower() != default_br and br_name not in profile.extra_branches:
                    profile.extra_branches.append(br_name)

    # 2. Fetch latest commits (default branch + any configured/discovered extra branches)
    branch_queries: list[tuple[str, str]] = [
        ("", f"https://api.github.com/repos/{repo}/commits?per_page={commits_limit}"),
    ]
    for br in profile.extra_branches:
        q_br = urllib.parse.quote(br, safe="")
        branch_queries.append(
            (br, f"https://api.github.com/repos/{repo}/commits?sha={q_br}&per_page={commits_limit}")
        )

    file_fetches_done = 0
    seen_repo_shas: set[str] = set()
    for branch_name, c_url in branch_queries:
        c_status, c_data, c_err = client.get_json(c_url, is_github_api=True)
        if not branch_name:
            if c_status in (404, 451, 410):
                profile.scan_error = f"unavailable ({c_err})"
                return profile, items, new_seen_commits, new_seen_prs
            if c_err and not isinstance(c_data, list):
                profile.scan_error = c_err
        elif not isinstance(c_data, list):
            continue

        if not isinstance(c_data, list):
            continue
        if c_data:
            first_date = (
                (c_data[0].get("commit") or {}).get("committer", {}).get("date")
                or (c_data[0].get("commit") or {}).get("author", {}).get("date")
                or ""
            )
            if first_date and parse_iso_ts(first_date) > parse_iso_ts(profile.pushed_at):
                profile.pushed_at = first_date

        for entry in c_data:
            if not isinstance(entry, dict):
                continue
            sha = str(entry.get("sha") or "")
            if not sha or sha in seen_repo_shas:
                continue
            seen_repo_shas.add(sha)
            new_seen_commits.add(sha)
            commit_obj = entry.get("commit") or {}
            full_msg = str(commit_obj.get("message") or "").strip()
            msg_lines = full_msg.splitlines()
            title = msg_lines[0].strip() if msg_lines else "(no subject)"
            body = "\n".join(msg_lines[1:]).strip()
            author_name = (
                (entry.get("author") or {}).get("login")
                or (commit_obj.get("author") or {}).get("name")
                or ""
            )
            ts = (
                (commit_obj.get("committer") or {}).get("date")
                or (commit_obj.get("author") or {}).get("date")
                or ""
            )
            html_url = str(entry.get("html_url") or f"https://github.com/{repo}/commit/{sha}")
            parents = entry.get("parents") or []
            is_merge = len(parents) > 1 or title.lower().startswith("merge ")

            files: list[str] = []
            # Fetch exact changed files for a few newest unseen non-merge commits if quota permits
            rem = client.remaining_quota()
            quota_ok = rem is None or rem > 250
            if (
                fetch_commit_files
                and quota_ok
                and not is_merge
                and sha not in seen_commits
                and file_fetches_done < max_commit_file_fetches
                and not LOW_SIGNAL_SUBJECT_RE.match(title)
            ):
                d_url = f"https://api.github.com/repos/{repo}/commits/{sha}"
                _, d_data, _ = client.get_json(d_url, is_github_api=True, immutable=True)
                file_fetches_done += 1
                if isinstance(d_data, dict) and isinstance(d_data.get("files"), list):
                    files = [str(f.get("filename")) for f in d_data["files"][:20] if f.get("filename")]

            item = ScoutItem(
                kind="commit",
                repo=repo,
                item_id=sha,
                short_id=sha[:7],
                title=title,
                branch=branch_name,
                body_snippet=truncate_one_line(body, 360) if body else "",
                author=str(author_name),
                timestamp=str(ts),
                url=html_url,
                state="commit",
                files=files,
                seen_before=(sha in seen_commits),
            )
            categorize_item(item)
            cross_check_local(item, local_index)
            item.priority_score = score_item(item, profile, now_ts)
            items.append(item)

    # 2. Fetch recent PRs (open + merged/closed, sorted by updated)
    pr_url = (
        f"https://api.github.com/repos/{repo}/pulls"
        f"?state=all&sort=updated&direction=desc&per_page={prs_limit}"
    )
    _, pr_data, _ = client.get_json(pr_url, is_github_api=True)
    if isinstance(pr_data, list):
        for pr in pr_data:
            if not isinstance(pr, dict):
                continue
            num = pr.get("number")
            if num is None:
                continue
            pr_key = str(num)
            merged_at = pr.get("merged_at")
            raw_state = str(pr.get("state") or "open")
            eff_state = "merged" if merged_at else raw_state
            updated_at = str(pr.get("updated_at") or pr.get("created_at") or "")
            sig = f"{eff_state}:{updated_at}"
            new_seen_prs[pr_key] = sig

            # Skip closed-unmerged PRs that are old noise unless recently updated & technical
            title = str(pr.get("title") or "(untitled PR)").strip()
            body = str(pr.get("body") or "").strip()
            author_name = str((pr.get("user") or {}).get("login") or "")
            html_url = str(pr.get("html_url") or f"https://github.com/{repo}/pull/{num}")

            item = ScoutItem(
                kind="pr",
                repo=repo,
                item_id=f"PR#{num}",
                short_id=f"#{num}",
                title=title,
                body_snippet=truncate_one_line(body, 400) if body else "",
                author=author_name,
                timestamp=str(merged_at or updated_at),
                url=html_url,
                state=eff_state,
                seen_before=(seen_prs.get(pr_key) == sig),
            )
            categorize_item(item)
            cross_check_local(item, local_index)
            item.priority_score = score_item(item, profile, now_ts)
            items.append(item)

    # 3. Fetch README highlights
    readme_url = f"https://api.github.com/repos/{repo}/readme"
    _, r_data, _ = client.get_json(readme_url, is_github_api=True)
    if isinstance(r_data, dict) and r_data.get("content"):
        try:
            raw_bytes = base64.b64decode(r_data["content"])
            readme_text = raw_bytes.decode("utf-8", errors="replace")
            profile.readme_highlights = extract_readme_highlights(readme_text)
        except Exception:
            pass

    return profile, items, new_seen_commits, new_seen_prs


# ---------------------------------------------------------------------------
# Persistent State Management
# ---------------------------------------------------------------------------

def load_state(state_path: Path, reset: bool = False) -> dict[str, Any]:
    empty: dict[str, Any] = {
        "version": 1,
        "last_run_utc": "",
        "seen_commits": {},
        "seen_prs": {},
        "repo_snapshots": {},
    }
    if reset or not state_path.is_file():
        return empty
    try:
        data = json.loads(state_path.read_text(encoding="utf-8"))
        if not isinstance(data, dict):
            return empty
        for k, v in empty.items():
            data.setdefault(k, v)
        return data
    except (OSError, json.JSONDecodeError):
        return empty


def save_state(state_path: Path, state: dict[str, Any]) -> None:
    atomic_write_text(state_path, json.dumps(state, indent=2, ensure_ascii=False))


def compute_standing_deltas(
    profiles: list[RepoProfile],
    prev_snapshots: dict[str, dict[str, Any]],
) -> tuple[list[str], dict[str, dict[str, Any]]]:
    """Compare current tracker standings with previous run and build new snapshot."""
    deltas: list[str] = []
    new_snaps: dict[str, dict[str, Any]] = dict(prev_snapshots)

    for p in profiles:
        key = p.repo.lower()
        fps_best = max((x for x in (p.fps_high, p.fps_low, p.portwatch_fps) if x is not None), default=None)
        cur = {
            "stage": p.stage,
            "fps_best": fps_best,
            "stability_minutes": p.stability_minutes,
            "picture_step": p.picture_step,
            "tracker_changed": p.tracker_changed,
        }
        prev = prev_snapshots.get(key)
        if prev:
            changes: list[str] = []
            if cur["stage"] != prev.get("stage") and cur["stage"] is not None:
                changes.append(f"stage {prev.get('stage')} -> {cur['stage']}")
            if (
                cur["fps_best"] is not None
                and prev.get("fps_best") is not None
                and abs(cur["fps_best"] - prev["fps_best"]) >= 1.0
            ):
                changes.append(f"FPS {prev['fps_best']:.1f} -> {cur['fps_best']:.1f}")
            if (
                cur["picture_step"] is not None
                and prev.get("picture_step") is not None
                and cur["picture_step"] != prev["picture_step"]
            ):
                changes.append(f"picture {prev['picture_step']}/5 -> {cur['picture_step']}/5")
            if (
                cur["stability_minutes"] is not None
                and prev.get("stability_minutes") is not None
                and abs(cur["stability_minutes"] - prev["stability_minutes"]) >= 5.0
            ):
                changes.append(f"stability {prev['stability_minutes']:.0f}m -> {cur['stability_minutes']:.0f}m")
            if changes:
                deltas.append(f"**{p.repo}**: " + ", ".join(changes))
        new_snaps[key] = cur

    return deltas, new_snaps


# ---------------------------------------------------------------------------
# Report Generation (Markdown Briefing + JSON)
# ---------------------------------------------------------------------------

def _fmt_fps(p: RepoProfile) -> str:
    if p.fps_low is not None and p.fps_high is not None:
        if abs(p.fps_low - p.fps_high) < 0.05:
            return f"{p.fps_high:.1f} fps"
        return f"{p.fps_low:.1f}–{p.fps_high:.1f} fps"
    if p.portwatch_fps is not None:
        return f"~{p.portwatch_fps:.1f} fps"
    return "—"


def _fmt_stab(p: RepoProfile) -> str:
    if p.stability_minutes is not None:
        return f"{int(round(p.stability_minutes))} min"
    if p.portwatch_stability is not None:
        return f"{int(round(p.portwatch_stability * 100))}%"
    return "—"


def _fmt_pic(p: RepoProfile) -> str:
    if p.picture_step is not None:
        short_note = truncate_one_line(p.picture_text, 44) if p.picture_text else ""
        return f"{p.picture_step}/5" + (f" ({short_note})" if short_note else "")
    if p.portwatch_render is not None:
        return f"{int(round(p.portwatch_render * 100))}%"
    return "—"


def _fmt_item_line(item: ScoutItem, now_ts: float, show_body: bool = False) -> str:
    status_badge = "[NEW]" if item.local_status == "NEW" else "[LIKELY ALREADY HAVE]"
    fresh_badge = " ★" if not item.seen_before else ""
    branch_tag = f" @{item.branch}" if item.branch else ""
    kind_label = (
        f"PR {item.short_id} ({item.state})"
        if item.kind == "pr"
        else f"`{item.short_id}`{branch_tag}"
    )
    cats = ", ".join(item.categories) if item.categories else "uncategorized"
    age = format_age(item.timestamp, now_ts)
    extras: list[str] = []
    if item.metrics:
        extras.append("metrics: " + ", ".join(item.metrics[:3]))
    if item.env_vars:
        non_gen = [v for v in item.env_vars if v not in GENERIC_ENV_VARS]
        if non_gen:
            extras.append("env: " + ", ".join(f"`{v}`" for v in non_gen[:2]))
    if item.files:
        extras.append("files: " + ", ".join(f"`{Path(f).name}`" for f in item.files[:3]))
    if item.local_reason:
        extras.append(item.local_reason)

    line = (
        f"- **{status_badge}**{fresh_badge} `{item.repo}` {kind_label} "
        f"({age}) [{cats}] — {truncate_one_line(item.title, 115)} "
        f"<{item.url}>"
    )
    if extras:
        line += f"\n  - {'; '.join(extras)}"
    if show_body and item.body_snippet:
        line += f"\n  - Note: {truncate_one_line(item.body_snippet, 200)}"
    return line


def render_markdown_briefing(
    report: dict[str, Any],
    *,
    focus_categories: list[str] | None = None,
    full: bool = False,
) -> str:
    """Render a crisp, prioritized Markdown briefing (stdout or full report file)."""
    now_ts = parse_iso_ts(report["generated_utc"]) or time.time()
    meta = report["meta"]
    tracker = report["trackers"]
    scanned: list[RepoProfile] = report["_scanned_profiles"]
    self_prof: RepoProfile | None = report["_self_profile"]
    items: list[ScoutItem] = report["_items"]
    deltas: list[str] = report["standing_deltas"]

    # Apply --focus filter if specified
    active_cats = focus_categories if focus_categories else list(CATEGORY_ORDER)
    if focus_categories:
        focus_set = set(focus_categories)
        filtered_items = [it for it in items if any(c in focus_set for c in it.categories)]
    else:
        filtered_items = items

    new_items = [it for it in filtered_items if it.local_status == "NEW"]
    fresh_new_items = [it for it in new_items if not it.seen_before]
    already_items = [it for it in filtered_items if it.local_status != "NEW"]

    lines: list[str] = []
    lines.append("# El1xr — GTA VI / VIctory Ecosystem Intelligence Briefing")
    lines.append("")
    lines.append(
        f"- **Generated:** `{report['generated_utc']}` | "
        f"**Auth:** `{meta['token_source']}` | "
        f"**GitHub Quota:** `{meta['rate_remaining']}/{meta['rate_limit']}` "
        f"(reqs: {meta['requests_made']}, 304 cache hits: {meta['cache_hits_304']})"
    )
    lines.append(
        f"- **Local El1xr Repo:** `{meta['local_repo']}` "
        f"(HEAD `{meta['local_head_sha'][:7]}` — {truncate_one_line(meta['local_head_subject'], 70)}; "
        f"{meta['local_env_vars_count']} `VICTORY_*` vars indexed)"
    )
    if self_prof:
        lines.append(
            f"- **Our Tracker Standing (`{self_prof.repo}`):** Stage {self_prof.stage or '?'} "
            f"| Speed: {_fmt_fps(self_prof)} | Stability: {_fmt_stab(self_prof)} "
            f"| Picture: {_fmt_pic(self_prof)}"
        )
    lines.append(
        f"- **Coverage:** {tracker['total_discovered']} repos discovered "
        f"({tracker['stage6_victory_count']} external Stage-6 Victory forks), "
        f"**{len(scanned)}** deep-scanned | "
        f"**{len(new_items)} `[NEW]`** ({len(fresh_new_items)} fresh since last run ★), "
        f"{len(already_items)} `[LIKELY ALREADY HAVE]`"
    )
    if focus_categories:
        lines.append(f"- **Focus Filter:** `{', '.join(focus_categories)}`")
    pw_meta = tracker.get("portwatch_vi") or {}
    race_meta = tracker.get("gta6_pc_race") or {}
    lines.append(
        f"- **Live Trackers:** PortWatch VI (`{pw_meta.get('source', '?')}`, "
        f"updated: `{pw_meta.get('updated') or '?'}`) | "
        f"gta6-pc-race (`{race_meta.get('source', '?')}`, "
        f"checked: `{race_meta.get('checked') or '?'}`)"
    )
    if tracker.get("gta6_pc_race_error"):
        lines.append(f"- **Tracker Warning (gta6-pc-race):** {tracker['gta6_pc_race_error']}")
    if tracker.get("portwatch_vi_error"):
        lines.append(f"- **Tracker Warning (portwatch-vi):** {tracker['portwatch_vi_error']}")
    lines.append("")

    # Live PortWatch VI Standings Leaderboard
    pw_standings = pw_meta.get("standings") or []
    if pw_standings:
        pw_updated = pw_meta.get("updated") or "unknown"
        lines.append(f"## Live PortWatch VI Standings (updated: `{pw_updated}`)")
        lines.append("")
        lines.append("| # | Repo | Tier | FPS | Render | Stability | GPU(s) | Highlights |")
        lines.append("|---|------|------|-----|--------|-----------|--------|------------|")
        pw_limit = len(pw_standings) if full else min(15, len(pw_standings))
        for idx, entry in enumerate(pw_standings[:pw_limit], 1):
            fps_s = f"{entry['fps']:.1f} fps" if entry.get("fps") is not None else "—"
            ren_s = f"{int(round(entry['render'] * 100))}%" if entry.get("render") is not None else "—"
            stb_s = f"{int(round(entry['stability'] * 100))}%" if entry.get("stability") is not None else "—"
            gpu_s = ", ".join(entry.get("gpu") or []) or "—"
            hl = truncate_one_line("; ".join(entry.get("highlights") or []), 70) or "—"
            br_s = f" (`{entry['branch']}`)" if entry.get("branch") else ""
            lines.append(
                f"| {idx} | `{entry.get('repo') or entry.get('name')}`{br_s} | "
                f"{entry.get('tier') or '—'} | {fps_s} | {ren_s} | {stb_s} | "
                f"{truncate_one_line(gpu_s, 30)} | {hl} |"
            )
        lines.append("")
        pw_changes = pw_meta.get("changes") or []
        if pw_changes:
            lines.append("### Recent PortWatch VI Changes")
            for ch in pw_changes[: (6 if full else 4)]:
                if isinstance(ch, dict):
                    ch_t = ch.get("date") or ch.get("t") or ch.get("time") or ""
                    ch_msg = ch.get("summary") or ch.get("title") or ch.get("text") or str(ch)
                    lines.append(f"- **{ch_t}** — {truncate_one_line(str(ch_msg), 160)}")
                else:
                    lines.append(f"- {truncate_one_line(str(ch), 160)}")
            lines.append("")

    # Standing deltas since previous check
    if deltas:
        lines.append("## Standing Changes Since Last Check")
        for d in deltas[:12]:
            lines.append(f"- {d}")
        lines.append("")

    # Top Scanned Repos Standings & README Highlights
    lines.append(f"## Top {len(scanned)} Monitored Repositories")
    lines.append("")
    lines.append("| # | Repo | Stage | Speed | Stability | Picture | Fresh `[NEW]` | Notes |")
    lines.append("|---|------|-------|-------|-----------|---------|---------------|-------|")
    fresh_by_repo: dict[str, int] = defaultdict(int)
    for it in fresh_new_items:
        fresh_by_repo[it.repo] += 1

    for idx, p in enumerate(scanned, 1):
        seed_mark = " ★" if p.is_priority_seed else ""
        branch_mark = (
            f" (+{','.join(p.extra_branches)})" if p.extra_branches else ""
        )
        stage_s = f"S{p.stage}" if p.stage is not None else "—"
        note = p.scan_error if p.scan_error else truncate_one_line(p.fps_text or p.kind, 60)
        lines.append(
            f"| {idx} | `{p.repo}`{seed_mark}{branch_mark} | {stage_s} | {_fmt_fps(p)} | "
            f"{_fmt_stab(p)} | {_fmt_pic(p)} | {fresh_by_repo.get(p.repo, 0)} | {note} |"
        )
    lines.append("")

    # README Highlights (compact in stdout, detailed in full report)
    readme_limit = len(scanned) if full else min(6, len(scanned))
    readme_lines: list[str] = []
    for p in scanned[:readme_limit]:
        rh = p.readme_highlights or {}
        snippets: list[str] = []
        if rh.get("fps_notes"):
            snippets.append(f"FPS: {truncate_one_line(rh['fps_notes'][0], 110)}")
        if rh.get("stability_notes"):
            snippets.append(f"Stability: {truncate_one_line(rh['stability_notes'][0], 110)}")
        if rh.get("picture_notes"):
            snippets.append(f"Picture: {truncate_one_line(rh['picture_notes'][0], 110)}")
        if snippets:
            readme_lines.append(f"- **`{p.repo}`** — " + " | ".join(snippets))
    if readme_lines:
        lines.append("## README Highlights (FPS / Stability / Picture)")
        lines.extend(readme_lines)
        lines.append("")

    # Top Prioritized [NEW] Findings across all active buckets
    previous_run_utc = meta.get("previous_run_utc") or ""
    incremental_stdout = bool(previous_run_utc) and not full
    top_n = 35 if full else 18

    if incremental_stdout:
        prioritized_new = sorted(
            (it for it in fresh_new_items if it.categories),
            key=lambda it: (-it.priority_score, -parse_iso_ts(it.timestamp)),
        )
        lines.append(
            f"## Fresh `[NEW]` Developments Since `{previous_run_utc}` "
            f"(Showing {min(top_n, len(prioritized_new))} of {len(prioritized_new)})"
        )
        if not prioritized_new:
            lines.append(
                f"_No fresh `[NEW]` categorized developments since `{previous_run_utc}`. "
                f"Run with `--reset-state` to view all {len(new_items)} recent `[NEW]` items._"
            )
        else:
            for it in prioritized_new[:top_n]:
                lines.append(_fmt_item_line(it, now_ts, show_body=False))
        lines.append("")
    else:
        prioritized_new = sorted(
            (it for it in new_items if it.categories),
            key=lambda it: (-it.priority_score, -parse_iso_ts(it.timestamp)),
        )
        lines.append(
            f"## Top Actionable `[NEW]` Developments "
            f"(Showing {min(top_n, len(prioritized_new))} of {len(prioritized_new)})"
        )
        if not prioritized_new:
            lines.append("_No categorized `[NEW]` developments matched the current filters._")
        else:
            for it in prioritized_new[:top_n]:
                lines.append(_fmt_item_line(it, now_ts, show_body=full))
        lines.append("")

    # Breakdown by Category
    bucket_source = fresh_new_items if incremental_stdout else new_items
    bucket_lines: list[str] = []
    per_cat_limit = 15 if full else 6
    for cat in active_cats:
        cat_new = sorted(
            (it for it in bucket_source if cat in it.categories),
            key=lambda it: (-it.priority_score, -parse_iso_ts(it.timestamp)),
        )
        cat_have = [it for it in already_items if cat in it.categories]
        label = CATEGORY_META[cat]["label"]
        if incremental_stdout and not cat_new:
            continue
        bucket_lines.append(f"### `{cat}` — {label} ({len(cat_new)} `[NEW]`, {len(cat_have)} `[LIKELY ALREADY HAVE]`)")
        if not cat_new:
            bucket_lines.append("_No `[NEW]` items in this bucket._")
        else:
            for it in cat_new[:per_cat_limit]:
                bucket_lines.append(_fmt_item_line(it, now_ts, show_body=False))
            if len(cat_new) > per_cat_limit:
                bucket_lines.append(f"- _…and {len(cat_new) - per_cat_limit} more in full report._")
        if full and cat_have:
            bucket_lines.append("")
            bucket_lines.append(f"  _Likely already in local tree ({len(cat_have)}):_")
            for it in cat_have[:5]:
                bucket_lines.append(
                    f"  - `[LIKELY ALREADY HAVE]` `{it.repo}` {it.short_id} — "
                    f"{truncate_one_line(it.title, 85)} ({it.local_reason})"
                )
        bucket_lines.append("")

    if bucket_lines:
        bucket_heading = (
            f"## Fresh Developments by Engineering Bucket (Since `{previous_run_utc}`)"
            if incremental_stdout
            else "## Breakdown by Engineering Bucket"
        )
        lines.append(bucket_heading)
        lines.extend(bucket_lines)

    # Recent tracker updates
    race_updates = (tracker.get("gta6_pc_race") or {}).get("updates") or []
    if race_updates:
        lines.append("## Latest `gta6-pc-race` Tracker Updates")
        for upd in race_updates[: (5 if full else 3)]:
            t_str = upd.get("t", "")
            u_title = upd.get("title", "")
            lines.append(f"- **{t_str}** — {u_title}")
            if full and isinstance(upd.get("points"), list):
                for pt in upd["points"][:4]:
                    lines.append(f"  - {truncate_one_line(str(pt), 160)}")
        lines.append("")

    return "\n".join(lines).rstrip() + "\n"


# ---------------------------------------------------------------------------
# Single Scour Cycle
# ---------------------------------------------------------------------------

def run_scout_cycle(
    *,
    local_repo: Path,
    state_path: Path,
    out_dir: Path,
    top_n: int,
    commits_limit: int,
    prs_limit: int,
    focus_categories: list[str] | None,
    reset_state: bool,
    workers: int,
    fetch_commit_files: bool,
) -> dict[str, Any]:
    """Execute one complete ecosystem intelligence cycle and write reports/state."""
    t0 = time.time()
    token, token_source = resolve_github_token()
    cache_path = state_path.parent / "http_cache.json"
    client = HttpClient(token=token, token_source=token_source, cache_path=cache_path)

    state = load_state(state_path, reset=reset_state)
    previous_run_utc = "" if reset_state else str(state.get("last_run_utc") or "")
    local_index = build_local_index(local_repo)

    profiles, self_profile, tracker_info = discover_repos(client, self_slug=local_index.origin_slug)
    targets = profiles[: max(1, top_n)]

    seen_commits_map: dict[str, list[str]] = state.get("seen_commits", {})
    seen_prs_map: dict[str, dict[str, str]] = state.get("seen_prs", {})

    scanned_profiles: list[RepoProfile] = []
    all_items: list[ScoutItem] = []

    max_workers = max(1, min(workers, len(targets)))
    with concurrent.futures.ThreadPoolExecutor(max_workers=max_workers) as pool:
        future_map = {
            pool.submit(
                scan_repo,
                prof,
                client,
                local_index,
                set(seen_commits_map.get(prof.repo.lower(), [])),
                dict(seen_prs_map.get(prof.repo.lower(), {})),
                commits_limit=commits_limit,
                prs_limit=prs_limit,
                fetch_commit_files=fetch_commit_files,
            ): prof
            for prof in targets
        }
        results_by_repo: dict[str, RepoProfile] = {}
        for fut in concurrent.futures.as_completed(future_map):
            orig_prof = future_map[fut]
            try:
                upd_prof, repo_items, new_shas, new_prs = fut.result()
            except Exception as exc:
                orig_prof.scan_error = f"worker error: {exc}"
                results_by_repo[orig_prof.repo.lower()] = orig_prof
                continue

            key = upd_prof.repo.lower()
            results_by_repo[key] = upd_prof
            all_items.extend(repo_items)

            # Merge seen commits (keep newest 500 per repo)
            prev_list = seen_commits_map.get(key, [])
            merged_shas = list(dict.fromkeys([*new_shas, *prev_list]))[:500]
            seen_commits_map[key] = merged_shas

            # Merge seen PR signatures
            prev_prs = seen_prs_map.get(key, {})
            prev_prs.update(new_prs)
            if len(prev_prs) > 200:
                prev_prs = dict(list(prev_prs.items())[-200:])
            seen_prs_map[key] = prev_prs

        # Preserve priority order of targets
        for prof in targets:
            scanned_profiles.append(results_by_repo.get(prof.repo.lower(), prof))

    # Deduplicate identical commit SHAs that appear across multiple forks (keep highest-priority repo's copy)
    dedup_items: list[ScoutItem] = []
    seen_sha_global: dict[str, ScoutItem] = {}
    all_items.sort(key=lambda it: (-it.priority_score, -parse_iso_ts(it.timestamp)))
    for it in all_items:
        if it.kind == "commit":
            sha_key = it.item_id.lower()
            if sha_key in seen_sha_global:
                continue
            seen_sha_global[sha_key] = it
        dedup_items.append(it)

    deltas, new_snapshots = compute_standing_deltas(
        scanned_profiles,
        state.get("repo_snapshots", {}),
    )

    generated_utc = utc_iso()
    elapsed_s = round(time.time() - t0, 2)

    # Update & persist state + HTTP ETag cache
    state["last_run_utc"] = generated_utc
    state["seen_commits"] = seen_commits_map
    state["seen_prs"] = seen_prs_map
    state["repo_snapshots"] = new_snapshots
    save_state(state_path, state)
    client.save_cache()

    report: dict[str, Any] = {
        "schema_version": 1,
        "generated_utc": generated_utc,
        "elapsed_seconds": elapsed_s,
        "meta": {
            "previous_run_utc": previous_run_utc,
            "token_source": token_source,
            "rate_remaining": client.rate_remaining,
            "rate_limit": client.rate_limit,
            "rate_reset": client.rate_reset,
            "requests_made": client.requests_made,
            "cache_hits_304": client.cache_hits_304,
            "cache_hits_immutable": client.cache_hits_immutable,
            "http_errors": client.http_errors,
            "local_repo": str(local_repo),
            "local_origin": local_index.origin_slug,
            "local_head_sha": local_index.head_sha,
            "local_head_subject": local_index.head_subject,
            "local_env_vars_count": len(local_index.env_vars),
            "local_files_scanned": local_index.source_files_scanned,
            "top_n": top_n,
            "focus": focus_categories or [],
            "state_path": str(state_path),
        },
        "trackers": tracker_info,
        "self_standing": asdict(self_profile) if self_profile else None,
        "standing_deltas": deltas,
        "repos_scanned": [asdict(p) for p in scanned_profiles],
        "repos_discovered_summary": [
            {
                "repo": p.repo,
                "stage": p.stage,
                "is_victory_fork": p.is_victory_fork,
                "is_priority_seed": p.is_priority_seed,
                "priority_score": p.priority_score,
                "fps_high": p.fps_high or p.portwatch_fps,
                "stability_minutes": p.stability_minutes,
            }
            for p in profiles[:50]
        ],
        "items": [asdict(it) for it in dedup_items],
        # Internal transient references for markdown rendering (stripped before JSON dump)
        "_scanned_profiles": scanned_profiles,
        "_self_profile": self_profile,
        "_items": dedup_items,
    }

    # Write full Markdown + JSON reports to out_dir
    out_dir.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")
    full_md = render_markdown_briefing(report, focus_categories=focus_categories, full=True)
    stdout_md = render_markdown_briefing(report, focus_categories=focus_categories, full=False)

    json_payload = {k: v for k, v in report.items() if not k.startswith("_")}
    json_text = json.dumps(json_payload, indent=2, ensure_ascii=False)

    md_latest = out_dir / "briefing-latest.md"
    md_dated = out_dir / f"briefing-{stamp}.md"
    json_latest = out_dir / "report-latest.json"
    json_dated = out_dir / f"report-{stamp}.json"

    atomic_write_text(md_latest, full_md)
    atomic_write_text(md_dated, full_md)
    atomic_write_text(json_latest, json_text)
    atomic_write_text(json_dated, json_text)

    report["output_files"] = {
        "markdown_latest": str(md_latest),
        "markdown_dated": str(md_dated),
        "json_latest": str(json_latest),
        "json_dated": str(json_dated),
    }
    report["stdout_markdown"] = stdout_md
    return report


# ---------------------------------------------------------------------------
# CLI & Watch Loop
# ---------------------------------------------------------------------------

def parse_focus_arg(raw: str | None) -> list[str] | None:
    if not raw:
        return None
    parts = [p.strip().lower() for p in raw.split(",") if p.strip()]
    invalid = [p for p in parts if p not in CATEGORY_META]
    if invalid:
        valid = ", ".join(CATEGORY_ORDER)
        raise argparse.ArgumentTypeError(
            f"Unknown --focus category: {', '.join(invalid)}. Valid categories: {valid}"
        )
    return list(dict.fromkeys(parts))


def resolve_default_local_repo() -> Path:
    if DEFAULT_LOCAL_REPO.is_dir():
        return DEFAULT_LOCAL_REPO
    candidate = Path(__file__).resolve().parents[1]
    if (candidate / ".git").exists():
        return candidate
    return DEFAULT_LOCAL_REPO


def build_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="scout_ecosystem.py",
        description="El1xr ecosystem intelligence & watcher for GTA VI Xbox-on-PC (VIctory) ports.",
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--once",
        action="store_true",
        default=True,
        help="Run a single scour cycle and exit (default).",
    )
    mode.add_argument(
        "--watch",
        action="store_true",
        help="Run continuously in a watcher loop.",
    )
    parser.add_argument(
        "--interval",
        type=int,
        default=900,
        help="Seconds between watcher cycles in --watch mode (default: 900 = 15 min).",
    )
    parser.add_argument(
        "--top",
        type=int,
        default=18,
        help="Number of top repositories to deep-scan (default: 18).",
    )
    parser.add_argument(
        "--commits",
        type=int,
        default=25,
        help="Latest commits to fetch per repository (default: 25).",
    )
    parser.add_argument(
        "--prs",
        type=int,
        default=15,
        help="Recent pull requests to fetch per repository (default: 15).",
    )
    parser.add_argument(
        "--focus",
        type=parse_focus_arg,
        default=None,
        help=(
            "Comma-separated category filter: "
            + ",".join(CATEGORY_ORDER)
        ),
    )
    parser.add_argument(
        "--repo",
        type=Path,
        default=resolve_default_local_repo(),
        help=f"Local git repository path for cross-checking (default: {DEFAULT_LOCAL_REPO}).",
    )
    parser.add_argument(
        "--out-dir",
        type=Path,
        default=DEFAULT_OUT_DIR,
        help=f"Directory for Markdown + JSON reports (default: {DEFAULT_OUT_DIR}).",
    )
    parser.add_argument(
        "--state",
        type=Path,
        default=DEFAULT_STATE_PATH,
        help=f"Path to persistent state file (default: {DEFAULT_STATE_PATH}).",
    )
    parser.add_argument(
        "--reset-state",
        action="store_true",
        help="Ignore previously seen commits/PRs and treat all recent items as freshly discovered.",
    )
    parser.add_argument(
        "--workers",
        type=int,
        default=6,
        help="Concurrent worker threads for repo scanning (default: 6).",
    )
    parser.add_argument(
        "--no-commit-files",
        action="store_true",
        help="Skip fetching per-commit changed file lists (saves GitHub API calls).",
    )
    parser.add_argument(
        "--quiet",
        action="store_true",
        help="Suppress stdout briefing (still writes reports to --out-dir).",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    configure_console_utf8()
    parser = build_arg_parser()
    args = parser.parse_args(argv)

    stop_event = threading.Event()

    def _handle_signal(signum: int, _frame: Any) -> None:
        stop_event.set()
        print(f"\n[scout] Received signal {signum}; shutting down cleanly...", file=sys.stderr)

    signal.signal(signal.SIGINT, _handle_signal)
    if hasattr(signal, "SIGTERM"):
        signal.signal(signal.SIGTERM, _handle_signal)

    if not args.watch:
        report = run_scout_cycle(
            local_repo=args.repo,
            state_path=args.state,
            out_dir=args.out_dir,
            top_n=args.top,
            commits_limit=args.commits,
            prs_limit=args.prs,
            focus_categories=args.focus,
            reset_state=args.reset_state,
            workers=args.workers,
            fetch_commit_files=not args.no_commit_files,
        )
        if not args.quiet:
            print(report["stdout_markdown"])
            print(
                f"_Reports written to `{report['output_files']['markdown_latest']}` "
                f"and `{report['output_files']['json_latest']}` "
                f"in {report['elapsed_seconds']}s._"
            )
        return 0

    # Continuous --watch mode
    cycle = 0
    interval = max(30, args.interval)
    print(
        f"[scout] Starting ecosystem watcher (interval={interval}s, top={args.top}, "
        f"out={args.out_dir}, state={args.state}). Press Ctrl+C to stop.",
        file=sys.stderr,
    )
    while not stop_event.is_set():
        cycle += 1
        try:
            report = run_scout_cycle(
                local_repo=args.repo,
                state_path=args.state,
                out_dir=args.out_dir,
                top_n=args.top,
                commits_limit=args.commits,
                prs_limit=args.prs,
                focus_categories=args.focus,
                reset_state=(args.reset_state and cycle == 1),
                workers=args.workers,
                fetch_commit_files=not args.no_commit_files,
            )
            items: list[ScoutItem] = report["_items"]
            focus_set = set(args.focus) if args.focus else None
            fresh_high = [
                it
                for it in items
                if not it.seen_before
                and it.local_status == "NEW"
                and it.categories
                and (focus_set is None or any(c in focus_set for c in it.categories))
            ]
            fresh_high.sort(key=lambda it: -it.priority_score)

            if cycle == 1 and not args.quiet:
                print(report["stdout_markdown"])
            else:
                meta = report["meta"]
                print(
                    f"\n[scout] Cycle #{cycle} @ {report['generated_utc']} "
                    f"({report['elapsed_seconds']}s, quota {meta['rate_remaining']}/{meta['rate_limit']}, "
                    f"304 hits: {meta['cache_hits_304']}): "
                    f"{len(fresh_high)} fresh [NEW] categorized item(s), "
                    f"{len(report['standing_deltas'])} standing change(s)."
                )
                for delta in report["standing_deltas"][:5]:
                    print(f"  [DELTA] {delta}")
                for it in fresh_high[:10]:
                    cats = ",".join(it.categories[:3])
                    print(
                        f"  [NEW] {it.repo} {it.short_id} [{cats}] "
                        f"{truncate_one_line(it.title, 95)} -> {it.url}"
                    )
            print(
                f"[scout] Updated `{report['output_files']['markdown_latest']}` "
                f"& `{report['output_files']['json_latest']}`.",
                file=sys.stderr,
            )
        except Exception as exc:
            print(f"[scout] Cycle #{cycle} failed: {exc}", file=sys.stderr)

        # Interruptible sleep so Ctrl+C exits promptly on Windows
        slept = 0
        while slept < interval and not stop_event.is_set():
            stop_event.wait(timeout=1.0)
            slept += 1

    print("[scout] Watcher stopped cleanly.", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
