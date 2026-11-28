"""tools/dev_profile.py: the game's sample profile (`VICTORY_SAMPLE_PROFILE`) parsed, named and reported."""

import struct
import sys
from pathlib import Path

TOOLS = Path(__file__).resolve().parents[1] / "tools"
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))

import dev
import dev_profile

BASE = 0x7FF600000000
NT = 0x7FFC00000000

# Addresses of a made-up victory.exe and ntdll.dll, named by NAMES below.
NAMES = {
    BASE + 0x101: "victory.exe!wide::flush",
    BASE + 0x201: "victory.exe!batch::record",
    BASE + 0x301: "victory.exe!submit::run",
    BASE + 0x401: "victory.exe!thread_start",
    BASE + 0x501: "victory.exe!heap::sync",
    NT + 0x11: "ntdll.dll!NtWaitForSingleObject",
    NT + 0x21: "kernelbase.dll!WaitForSingleObjectEx",
    BASE + 0x601: "game.exe!sub_1000",
}

SAMPLE = """victory-sample-profile\t1
interval_ms\t5
window_s\t10.000
ticks\t2000
suspended_us_mean\t12.5
suspended_us_max\t300
stuck\t0
module\t{base:#x}\t0x10000\tC:\\x\\victory.exe
module\t{nt:#x}\t0x1000\tC:\\Windows\\System32\\ntdll.dll
thread\t10\t8000.000\tvictory.exe+0x400\tvictory-submit
thread\t11\t3000.000\tvictory.exe+0x400\tvictory-wide-flush-0
thread\t12\t2000.000\tvictory.exe+0x400\tvictory-wide-flush-1
thread\t13\t0.000\tgame.exe+0x1000\t
stack\t0\t{f1:x} {r1:x} {s1:x} {t1:x}
stack\t1\t{r0:x} {s1:x} {t1:x}
stack\t2\t{w0:x} {kb:x} {s1:x} {t1:x}
stack\t3\t{h0:x} {t1:x}
stack\t4\t{w0:x} {kb:x} {g1:x}
sample\t10\t0\t1\t600
sample\t10\t1\t1\t200
sample\t10\t2\t1\t100
sample\t10\t3\t0\t50
sample\t11\t0\t1\t300
sample\t11\t2\t0\t1700
sample\t12\t3\t1\t400
sample\t13\t4\t0\t2000
"""


def text() -> str:
    # The first frame of a stack is the instruction (named +1 by `aggregate`), the rest return
    # addresses: both land on NAMES' keys.
    return SAMPLE.format(
        base=BASE, nt=NT, f1=BASE + 0x100, r0=BASE + 0x200, r1=BASE + 0x201, s1=BASE + 0x301, t1=BASE + 0x401,
        w0=NT + 0x10, kb=NT + 0x21, h0=BASE + 0x500, g1=BASE + 0x601,
    )


def name(address: int) -> str:
    return NAMES.get(address, f"{address:#x}")


def test_parse_reads_every_record():
    p = dev_profile.parse(text())
    assert p.meta["interval_ms"] == 5 and p.meta["window_s"] == 10 and p.meta["ticks"] == 2000
    assert p.modules[0] == (BASE, 0x10000, "C:\\x\\victory.exe")
    assert p.threads[10] == (8000.0, "victory.exe+0x400", "victory-submit")
    assert p.threads[13] == (0.0, "game.exe+0x1000", "")
    assert len(p.stacks) == 5 and p.stacks[3] == (BASE + 0x500, BASE + 0x401)
    assert (10, 0, True, 600) in p.samples and (13, 4, False, 2000) in p.samples
    assert p.module_of(NT + 5)[2].endswith("ntdll.dll")


def test_parse_refuses_another_file():
    try:
        dev_profile.parse("hello\n")
    except ValueError:
        return
    raise AssertionError("no ValueError")


def test_aggregate_splits_run_wait_ready_and_counts_functions():
    per_thread, per_group, total = dev_profile.aggregate(dev_profile.parse(text()), name)
    submit = per_thread[10]
    assert (submit.run, submit.wait, submit.ready) == (800, 100, 50)
    assert submit.excl["victory.exe!wide::flush"] == 600
    assert submit.excl["victory.exe!batch::record"] == 200
    # Inclusive: once per stack, every frame.
    assert submit.incl["victory.exe!submit::run"] == 800
    assert submit.incl["victory.exe!batch::record"] == 800
    assert submit.waits["ntdll.dll!NtWaitForSingleObject <- victory.exe!submit::run"] == 100
    flushers = per_group["victory-wide-flush-N"]
    assert (flushers.run, flushers.wait, flushers.ready) == (700, 1700, 0)
    assert total.excl.most_common(1) == [("victory.exe!wide::flush", 900)]
    assert total.run == 1500 and total.wait == 3800 and total.ready == 50
    assert per_group["(unnamed) game.exe"].waits["ntdll.dll!NtWaitForSingleObject <- game.exe!sub_1000"] == 2000


def test_report_has_threads_groups_tables_and_stacks():
    r = dev_profile.report(dev_profile.parse(text()), name, source="p.txt")
    assert r.startswith("# victory sample profile p.txt\n")
    assert "1 resumed by the watchdog" not in r and "0 resumed by the watchdog" in r
    lines = r.splitlines()
    threads = lines[lines.index("## Threads by CPU") + 2:]
    assert threads[0].split()[:4] == ["10", "submit", "8000.0", "80.0"]
    assert "| victory.exe!wide::flush" in threads[0]
    assert any(" recording " in line for line in threads[:3])
    excl = lines[lines.index("### Top 40 functions, exclusive") + 2]
    assert excl.split() == ["900", "60.0", "60.0", "victory.exe!wide::flush"]
    assert "### victory-wide-flush-N: top 20 exclusive" in r
    assert "### victory.exe!wide::flush: 900 samples (60.0% of run)" in r
    at = lines.index("### victory.exe!wide::flush: 900 samples (60.0% of run)")
    assert lines[at + 1].split()[0] == "900"
    assert lines[at + 2:at + 6] == [
        "      victory.exe!wide::flush", "      victory.exe!batch::record",
        "      victory.exe!submit::run", "      victory.exe!thread_start",
    ]
    assert "victory-wide-flush-N: where it waits" in r
    assert dev_profile.summary_lines(dev_profile.parse(text()), name)[0].endswith("victory.exe!wide::flush")


def test_groups_and_roles():
    assert dev_profile.thread_group("victory-pipelines-12", "victory.exe+0x10") == "victory-pipelines-N"
    assert dev_profile.thread_group("", "game.exe+0x1a2b") == "(unnamed) game.exe"
    assert dev_profile.thread_role("victory-submit", "") == "submit"
    assert dev_profile.thread_role("victory-wide-flush-3", "") == "recording"
    assert dev_profile.thread_role("victory-heap-sync", "victory.exe+0x1") == "helper"
    assert dev_profile.thread_role("", "game_scarlett_master.exe+0x1") == "title"
    assert dev_profile.thread_role("title-main", "victory.exe+0x1") == "title"
    assert dev_profile.thread_role("", "ntdll.dll+0x24990") == "system"


def test_shorten_drops_generics_hashes_and_long_paths():
    assert dev_profile.shorten("victory_gpu::draw::State::heap<victory_gpu::Foo>::h0123456789abcdef") == \
        "draw::State::heap"
    assert dev_profile.shorten("NtWaitForSingleObject") == "NtWaitForSingleObject"


def minimal_pe(entries: list[tuple[int, int, int]], unwind: dict[int, bytes]) -> bytes:
    """A PE32+ with one section at rva 0x1000 holding `.pdata` at 0x1000 and unwind records."""
    pe = 0x40
    opt_size = 240
    sec = pe + 24 + opt_size
    raw = 0x200
    body = bytearray(0x400)
    for i, (b, e, u) in enumerate(entries):
        struct.pack_into("<III", body, 12 * i, b, e, u)
    for rva, data in unwind.items():
        body[rva - 0x1000:rva - 0x1000 + len(data)] = data
    head = bytearray(raw)
    head[0:2] = b"MZ"
    struct.pack_into("<I", head, 0x3C, pe)
    head[pe:pe + 4] = b"PE\0\0"
    struct.pack_into("<HH", head, pe + 4, 0x8664, 1)
    struct.pack_into("<H", head, pe + 20, opt_size)
    struct.pack_into("<H", head, pe + 24, 0x20B)
    struct.pack_into("<II", head, pe + 24 + 112 + 3 * 8, 0x1000, 12 * len(entries))
    struct.pack_into("<8sIIII", head, sec, b".pdata", len(body), 0x1000, len(body), raw)
    return bytes(head + body)


def test_pe_functions_follow_chained_unwind(tmp_path):
    # f1 0x2000-0x2100 (unwind 0x1100, plain); its part 0x2200-0x2280 chains back to f1 (unwind 0x1110).
    plain = bytes([1, 0, 0, 0])
    chained = bytes([1 | (4 << 3), 0, 0, 0]) + struct.pack("<III", 0x2000, 0x2100, 0x1100)
    path = tmp_path / "x.dll"
    path.write_bytes(minimal_pe([(0x2000, 0x2100, 0x1100), (0x2200, 0x2280, 0x1110)], {0x1100: plain, 0x1110: chained}))
    assert dev_profile.pe_functions(str(path)) == [(0x2000, 0x2100, 0x2000), (0x2200, 0x2280, 0x2000)]
    profile = dev_profile.Profile(modules=[(0x10000000, 0x10000, str(path))])
    namer = dev_profile.Namer(profile, use_dbghelp=False)
    assert namer(0x10000000 + 0x2051) == "x.dll!sub_2000"
    assert namer(0x10000000 + 0x2241) == "x.dll!sub_2000"
    assert namer(0x10000000 + 0x2181) == "x.dll+0x2181"
    assert namer(0x20000000) == "0x20000000"


def test_dev_profile_report_names_a_file(tmp_path, capsys):
    data = tmp_path / "profile.txt"
    data.write_text(text(), encoding="utf-8")
    assert dev.main(["profile", "--report", str(data)]) == 0
    out = capsys.readouterr().out
    assert "DEV profile-report PASS samples=5350" in out
    report = (tmp_path / "report.txt").read_text(encoding="utf-8")
    assert "## Threads by CPU" in report and "## Hottest call stacks" in report


def test_profile_parser_defaults():
    args = dev.build_parser().parse_args(["profile", "--window-s", "30", "--override", "VICTORY_X=1"])
    assert (args.interval_ms, args.start_s, args.window_s, args.seconds) == (5.0, 150.0, 30.0, 0)
    assert args.profile == "release" and args.fast_boot and args.override == ["VICTORY_X=1"]
