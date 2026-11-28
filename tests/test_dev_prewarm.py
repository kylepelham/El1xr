"""Tests for `dev.py prewarm` (tools/dev_prewarm.py and its launcher command)."""

import importlib.util
import sys
from pathlib import Path

TOOLS = Path(__file__).resolve().parents[1] / "tools"
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))

import dev_prewarm  # noqa: E402

DEV_PATH = TOOLS / "dev.py"


def load_dev():
    spec = importlib.util.spec_from_file_location("el1xr_dev_prewarm", DEV_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def test_host_facts_from_a_run_log_match_the_defaults():
    log = (
        "2026-11-27T13:12:48.752206Z  INFO victory_gpu::host: host wave lanes min=32 max=32\n"
        "2026-11-27T13:12:49.146700Z  INFO victory_gpu::descriptor::host: host view heaps hold 1000000 views; "
        "view indices are clamped to the null slot at the end from the first shader on views=1000000\n"
    )
    assert dev_prewarm.host_facts_from_log(log) == (dev_prewarm.DEFAULT_HOST_WAVE, dev_prewarm.DEFAULT_VIEW_LIMIT)
    assert dev_prewarm.host_facts_from_log("no facts here") is None
    assert dev_prewarm.device_made(log)
    assert not dev_prewarm.device_made("warm         10 translated, 0 refused")


def test_translation_only_gives_the_host_facts_and_keeps_the_arms_overrides():
    arm = {"VICTORY_WAVE_PAIRS": "1", "VICTORY_MUTE": "1"}
    merged = dev_prewarm.prewarm_overrides(arm, pipelines=False, host_wave=32, view_limit=999999)
    assert merged == {
        "VICTORY_WAVE_PAIRS": "1",
        "VICTORY_MUTE": "1",
        "VICTORY_HOST_WAVE": "32",
        "VICTORY_VIEW_LIMIT": "999999",
    }
    assert arm == {"VICTORY_WAVE_PAIRS": "1", "VICTORY_MUTE": "1"}
    # With pipelines the device states the facts itself.
    assert dev_prewarm.prewarm_overrides(arm, pipelines=True, host_wave=32, view_limit=999999) == arm


def test_launcher_extra_warms_and_stops_before_the_title():
    assert dev_prewarm.launcher_extra(False, 0) == ["-Precompile", "-NoGame", "-WarmNoPipelines"]
    # Recipes, not a compute pipeline for every compute shader shipped.
    assert dev_prewarm.launcher_extra(True, 8) == [
        "-Precompile", "-NoGame", "-WarmJobs", "8", "-WarmNoPipelines", "-WarmRecipes",
    ]
    assert dev_prewarm.launcher_extra(False, 0, True) == [
        "-Precompile", "-NoGame", "-WarmNoPipelines", "-PrecompileRayTracing",
    ]


def test_the_other_ray_tracing_variant_flips_the_one_setting():
    assert dev_prewarm.other_ray_tracing({"VICTORY_MUTE": "1"}) == {"VICTORY_MUTE": "1", "VICTORY_RAY_TRACING": "on"}
    assert dev_prewarm.other_ray_tracing({"VICTORY_RAY_TRACING": "on"}) == {"VICTORY_RAY_TRACING": "off"}
    assert dev_prewarm.other_ray_tracing({"VICTORY_RAY_TRACING": "off"}) == {"VICTORY_RAY_TRACING": "on"}


def test_parse_summary_reads_the_warm_lines():
    text = (
        "warm         9123 translated, 41 refused; 0 compute pipelines, 0 refused; 57 s\n"
        "warm cache   3700 from the cache, 5423 translated now\n"
    )
    summary = dev_prewarm.parse_summary(text)
    assert {k: summary[k] for k in ("shaders", "refused", "pipelines", "pipelines_refused", "warm_s", "hits",
                                    "translated")} == {
        "shaders": 9123,
        "refused": 41,
        "pipelines": 0,
        "pipelines_refused": 0,
        "warm_s": 57,
        "hits": 3700,
        "translated": 5423,
    }
    assert summary["recipes"] == 0
    assert dev_prewarm.parse_summary("")["shaders"] == 0


def test_parse_summary_reads_the_recipes_line():
    text = (
        "warm recipes 3412 recipes: 3390 host pipelines (3101 compiled, 289 from the library), 4 refused, "
        "18 left out, 0 unreadable, 0 with another key; 96 s\n"
    )
    s = dev_prewarm.parse_summary(text)
    assert (s["recipes"], s["pipelines_made"], s["compiled"], s["from_library"]) == (3412, 3390, 3101, 289)
    assert (s["recipes_refused"], s["left_out"], s["unreadable"], s["other_key"], s["recipes_s"]) == (4, 18, 0, 0, 96)


def test_prewarm_launcher_command_is_the_bench_one_plus_the_warm_switches(tmp_path):
    dev = load_dev()
    args = dev.build_parser().parse_args(["prewarm", "--override", "VICTORY_X=1", "--jobs", "4"])
    assert args.command == "prewarm"
    assert args.override == ["VICTORY_X=1"] and args.jobs == 4 and not args.pipelines and not args.diag
    assert args.host_wave == 32 and args.view_limit == 999999
    overrides = dev_prewarm.prewarm_overrides({"VICTORY_X": "1"}, False, 32, 999999)
    cmd = dev.build_launcher_command(
        game_dir=tmp_path,
        diag=False,
        overrides=overrides,
        mute=True,
        skip_intro=True,
        extra=dev_prewarm.launcher_extra(False, 4),
    )
    line = cmd[-1]
    assert "-Precompile -NoGame -WarmJobs 4 -WarmNoPipelines" in line
    assert "'VICTORY_HOST_WAVE' = '32'" in line and "'VICTORY_VIEW_LIMIT' = '999999'" in line
    assert "-Diag" not in line


def test_needed_files_are_what_precompile_reads(tmp_path):
    names = [p.name for p in dev_prewarm.needed_files(tmp_path)]
    assert names == [
        "shaders.rpf",
        "game_scarlett_master.exe",
        "XboxBvhBuilder.dll",
        "amd_fidelityfx_upscaler_xbox.dll",
    ]
