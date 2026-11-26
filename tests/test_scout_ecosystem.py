"""Unit tests for tools/scout_ecosystem.py."""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path
from typing import Any

TOOLS_DIR = Path(__file__).resolve().parents[1] / "tools"


def _load_scout():
    if "scout_ecosystem" in sys.modules:
        return sys.modules["scout_ecosystem"]
    spec = importlib.util.spec_from_file_location("scout_ecosystem", TOOLS_DIR / "scout_ecosystem.py")
    assert spec and spec.loader
    mod = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = mod
    spec.loader.exec_module(mod)
    return mod


class _FakeHttpClient:
    def __init__(self, responses: dict[str, Any]) -> None:
        self._responses = responses
        self.requested_urls: list[str] = []

    def remaining_quota(self) -> int | None:
        return 4500

    def get_json(self, url: str, *, is_github_api: bool = False, immutable: bool = False):
        self.requested_urls.append(url)
        if url in self._responses:
            return 200, self._responses[url], ""
        return 404, None, "HTTP 404 Not Found"

    def get(self, url: str, *, is_github_api: bool = False, immutable: bool = False, max_bytes: int = 0):
        self.requested_urls.append(url)
        return 404, None, "HTTP 404 Not Found"


def test_priority_seeds_cover_all_leading_forks():
    scout = _load_scout()
    seeds_lower = {s.lower() for s in scout.PRIORITY_SEEDS}
    expected = {
        "stellarr-studio/kelvaroq",
        "uiliannn/vitoriabr",
        "belobug/victory-belobug-amd",
        "avatarmanz/victory-7900xtx",
        "fxceless/victory-fork",
        "doubleghosty/cool_fox",
        "taynangt/vice-tay",
        "dexnom/victory",
        "realblastex/flamingo",
        "loopersalty/anyxboxx",
        "marvisak/victory",
    }
    assert expected.issubset(seeds_lower)
    assert "fablinoo/kelvaroq" not in seeds_lower


def test_discover_repos_auto_discovers_all_public_tracker_json_repos():
    scout = _load_scout()
    fake_tracker = {
        "updated": "2026-11-26T22:07:44Z",
        "efforts": [
            {
                "name": "BrandNewEffort",
                "repo": "neworg/brand-new-port",
                "tier": "tracked",
                "branch": "nvidia-fast",
                "gta": {"playable": 1, "fps": 18, "render": 0.7, "stability": 0.9},
                "gpu": ["NVIDIA"],
                "stats": {"stars": 42, "pushedAt": "2026-11-26T21:00:00Z"},
            },
            {
                "name": "PrivateEffort",
                "repo": None,
                "tier": "tracked",
                "gta": {"playable": 1, "fps": 25},
            },
        ],
        "candidates": [
            {
                "name": "low-sub-public",
                "repo": "freshuser/fresh-public-repo",
                "substance": 45,
                "stars": 3,
                "pushedAt": "2026-11-26T20:00:00Z",
                "description": "Brand new public Xbox Series runtime experiment",
                "branch": "exp-perf",
            }
        ],
    }
    client = _FakeHttpClient({scout.PORTWATCH_JSON_URL: fake_tracker})
    profiles, _self_prof, _info = scout.discover_repos(client)
    by_slug = {p.repo.lower(): p for p in profiles}

    assert "neworg/brand-new-port" in by_slug
    assert "nvidia-fast" in by_slug["neworg/brand-new-port"].extra_branches

    assert "freshuser/fresh-public-repo" in by_slug
    assert "exp-perf" in by_slug["freshuser/fresh-public-repo"].extra_branches
    assert "portwatch-candidate" in by_slug["freshuser/fresh-public-repo"].sources


def test_scan_repo_dynamically_discovers_all_belobug_branches():
    scout = _load_scout()
    repo = "Belobug/VIctory-Belobug-AMD"
    client = _FakeHttpClient(
        {
            f"https://api.github.com/repos/{repo}/branches?per_page=30": [
                {"name": "main"},
                {"name": "rx9070xt"},
                {"name": "a-publier"},
                {"name": "future-rdna4"},
            ],
            f"https://api.github.com/repos/{repo}/commits?per_page=5": [],
            f"https://api.github.com/repos/{repo}/commits?sha=rx9070xt&per_page=5": [],
            f"https://api.github.com/repos/{repo}/commits?sha=a-publier&per_page=5": [],
            f"https://api.github.com/repos/{repo}/commits?sha=future-rdna4&per_page=5": [],
            f"https://api.github.com/repos/{repo}/pulls?state=all&sort=updated&direction=desc&per_page=5": [],
        }
    )
    prof = scout.RepoProfile(repo=repo, default_branch="main", extra_branches=["rx9070xt"])
    local = scout.LocalRepoIndex(repo_path=".")

    upd, _items, _shas, _prs = scout.scan_repo(
        prof,
        client,
        local,
        set(),
        {},
        commits_limit=5,
        prs_limit=5,
        fetch_commit_files=False,
    )
    assert set(upd.extra_branches) >= {"rx9070xt", "a-publier", "future-rdna4"}
    assert "main" not in upd.extra_branches
    assert any("sha=future-rdna4" in u for u in client.requested_urls)
