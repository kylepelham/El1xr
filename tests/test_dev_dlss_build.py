"""tools/dev.py builds with `--features dlss` only when VICTORY_DLSS_SDK is set."""

import importlib.util
import sys
from pathlib import Path

TOOLS = Path(__file__).resolve().parents[1] / "tools"
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))


def load_dev():
    spec = importlib.util.spec_from_file_location("dev", TOOLS / "dev.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_release_build_has_no_dlss_feature_without_the_sdk():
    dev = load_dev()
    assert "--features" not in dev.release_build_cmd({})
    assert "--features" not in dev.release_build_cmd({"VICTORY_DLSS_SDK": "  "})


def test_release_build_adds_dlss_with_the_sdk():
    dev = load_dev()
    cmd = dev.release_build_cmd({"VICTORY_DLSS_SDK": "E:/sdk/DLSS"})
    assert cmd[:3] == ["cargo", "build", "--release"]
    assert cmd[-2:] == ["--features", "dlss"]
