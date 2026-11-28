"""`dev.py profile`: read the game's sample profile (`VICTORY_SAMPLE_PROFILE`, victory-loader
`sample_profile.rs`), name its addresses offline and write a text report.

The file holds raw return addresses per stack, counts per (thread, stack, ran) and the module list.
Names come from dbghelp against each module's PDB (victory.exe's, written beside it), else the
function that holds the address by the module's own `.pdata` (title code and system DLLs without
symbols: `module!sub_<rva>`, or an export of that address), else `module+0x<rva>`.

Each sample is one of:
- run: the thread used cycles since the last tick and is not in a wait call: on a CPU.
- wait: its innermost frame is a wait system call (`WAIT_RE`).
- ready: not in a wait call, but no cycles since the last tick: preempted, waiting for a CPU.
Hot function tables count run samples; inclusive counts each function once per stack.
"""

from __future__ import annotations

import bisect
import ctypes
import re
import struct
import sys
from collections import Counter, defaultdict
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path

FORMAT = "victory-sample-profile"
WAIT_RE = re.compile(
    r"^(ntdll|win32u)\.dll!(Nt|Zw)\w*(Wait|Delay|RemoveIoCompletion|Yield|ReplyWaitReceive|AlpcSendWaitReceive)"
)


@dataclass
class Profile:
    meta: dict[str, float] = field(default_factory=dict)
    modules: list[tuple[int, int, str]] = field(default_factory=list)
    # tid -> (cpu ms in the window, start, name)
    threads: dict[int, tuple[float, str, str]] = field(default_factory=dict)
    stacks: list[tuple[int, ...]] = field(default_factory=list)
    # (tid, stack, ran, count)
    samples: list[tuple[int, int, bool, int]] = field(default_factory=list)

    def module_of(self, address: int) -> tuple[int, int, str] | None:
        for base, size, path in self.modules:
            if base <= address < base + size:
                return base, size, path
        return None


def parse(text: str) -> Profile:
    """The file `sample_profile::render` writes. Raises ValueError for another file."""
    lines = text.splitlines()
    if not lines or not lines[0].startswith(FORMAT + "\t"):
        raise ValueError(f"not a {FORMAT} file")
    p = Profile()
    stacks: dict[int, tuple[int, ...]] = {}
    for line in lines[1:]:
        parts = line.split("\t")
        kind = parts[0]
        if kind == "module" and len(parts) >= 4:
            p.modules.append((int(parts[1], 16), int(parts[2], 16), parts[3]))
        elif kind == "thread" and len(parts) >= 4:
            p.threads[int(parts[1])] = (float(parts[2]), parts[3], parts[4] if len(parts) > 4 else "")
        elif kind == "stack" and len(parts) >= 2:
            pcs = parts[2].split() if len(parts) > 2 else []
            stacks[int(parts[1])] = tuple(int(pc, 16) for pc in pcs)
        elif kind == "sample" and len(parts) == 5:
            p.samples.append((int(parts[1]), int(parts[2]), parts[3] == "1", int(parts[4])))
        elif len(parts) == 2:
            try:
                p.meta[kind] = float(parts[1])
            except ValueError:
                pass
    p.stacks = [stacks.get(i, ()) for i in range(max(stacks, default=-1) + 1)]
    return p


# ---------------------------------------------------------------------------------------------
# Names


def pe_functions(path: str) -> list[tuple[int, int, int]]:
    """(begin rva, end rva, primary begin rva) of every `.pdata` entry of a PE32+ file, sorted; chained
    entries (a function's split parts) lead to their primary function. Empty if unreadable."""
    try:
        data = Path(path).read_bytes()
    except OSError:
        return []
    try:
        pe = struct.unpack_from("<I", data, 0x3C)[0]
        if data[pe:pe + 4] != b"PE\0\0":
            return []
        sections_n = struct.unpack_from("<H", data, pe + 6)[0]
        opt_size = struct.unpack_from("<H", data, pe + 20)[0]
        opt = pe + 24
        if struct.unpack_from("<H", data, opt)[0] != 0x20B:
            return []
        exc_rva, exc_size = struct.unpack_from("<II", data, opt + 112 + 3 * 8)
        sections = []
        for i in range(sections_n):
            s = opt + opt_size + 40 * i
            vsize, va, raw_size, raw = struct.unpack_from("<IIII", data, s + 8)
            sections.append((va, max(vsize, raw_size), raw))
    except struct.error:
        return []

    def offset(rva: int) -> int | None:
        for va, size, raw in sections:
            if va <= rva < va + size:
                return raw + rva - va
        return None

    start = offset(exc_rva) if exc_size else None
    if start is None:
        return []
    out = []
    for i in range(exc_size // 12):
        try:
            begin, end, unwind = struct.unpack_from("<III", data, start + 12 * i)
        except struct.error:
            break
        primary = begin
        for _ in range(32):
            at = offset(unwind & ~1)
            if at is None or at + 4 > len(data) or not (data[at] >> 3) & 4:
                break
            count = data[at + 2]
            chained = at + 4 + ((count + 1) & ~1) * 2
            try:
                primary, _e, unwind = struct.unpack_from("<III", data, chained)
            except struct.error:
                break
        out.append((begin, end, primary))
    out.sort()
    return out


def shorten(name: str, keep: int = 3) -> str:
    """A Rust or C++ name without generics, hash suffix and all but its last `keep` path parts."""
    depth = 0
    plain = []
    for c in name:
        if c == "<":
            depth += 1
        elif c == ">":
            depth = max(0, depth - 1)
        elif depth == 0:
            plain.append(c)
    parts = [p for p in "".join(plain).split("::") if not (len(p) == 17 and p.startswith("h"))]
    return "::".join(parts[-keep:]) if parts else name


class _SymbolInfo(ctypes.Structure):
    _fields_ = [
        ("SizeOfStruct", ctypes.c_uint32), ("TypeIndex", ctypes.c_uint32),
        ("Reserved", ctypes.c_uint64 * 2), ("Index", ctypes.c_uint32), ("Size", ctypes.c_uint32),
        ("ModBase", ctypes.c_uint64), ("Flags", ctypes.c_uint32), ("Value", ctypes.c_uint64),
        ("Address", ctypes.c_uint64), ("Register", ctypes.c_uint32), ("Scope", ctypes.c_uint32),
        ("Tag", ctypes.c_uint32), ("NameLen", ctypes.c_uint32), ("MaxNameLen", ctypes.c_uint32),
        ("Name", ctypes.c_wchar * 1),
    ]


SYMFLAG_EXPORT = 0x200
NAME_CHARS = 1024


class DbgHelp:
    """dbghelp over the modules of a profile, loaded at their recorded bases (no live process)."""

    def __init__(self, modules: list[tuple[int, int, str]]):
        self.ok = False
        if sys.platform != "win32":
            return
        kits = Path("C:/Program Files (x86)/Windows Kits/10/Debuggers/x64/dbghelp.dll")
        try:
            self.dll = ctypes.WinDLL(str(kits)) if kits.exists() else ctypes.WinDLL("dbghelp")
        except OSError:
            return
        d = self.dll
        d.SymSetOptions.argtypes = [ctypes.c_uint32]
        d.SymInitializeW.argtypes = [ctypes.c_void_p, ctypes.c_wchar_p, ctypes.c_int]
        d.SymLoadModuleExW.argtypes = [
            ctypes.c_void_p, ctypes.c_void_p, ctypes.c_wchar_p, ctypes.c_wchar_p,
            ctypes.c_uint64, ctypes.c_uint32, ctypes.c_void_p, ctypes.c_uint32,
        ]
        d.SymLoadModuleExW.restype = ctypes.c_uint64
        d.SymFromAddrW.argtypes = [ctypes.c_void_p, ctypes.c_uint64, ctypes.POINTER(ctypes.c_uint64), ctypes.c_void_p]
        d.SymCleanup.argtypes = [ctypes.c_void_p]
        self.process = ctypes.c_void_p(0x5EED)
        # SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS | SYMOPT_FAIL_CRITICAL_ERRORS | SYMOPT_NO_PROMPTS
        d.SymSetOptions(0x2 | 0x4 | 0x200 | 0x80000)
        dirs = list(dict.fromkeys(str(Path(path).parent) for _b, _s, path in modules))
        if not d.SymInitializeW(self.process, ";".join(dirs), 0):
            return
        for base, size, path in modules:
            d.SymLoadModuleExW(self.process, None, path, None, base, size, None, 0)
        self.ok = True
        self.buffer = ctypes.create_string_buffer(ctypes.sizeof(_SymbolInfo) + NAME_CHARS * 2)

    def lookup(self, address: int) -> tuple[str, int, bool] | None:
        """(name, displacement, from exports) of `address`, or None."""
        if not self.ok:
            return None
        info = _SymbolInfo.from_buffer(self.buffer)
        info.SizeOfStruct = ctypes.sizeof(_SymbolInfo)  # 88, as C's sizeof(SYMBOL_INFOW)
        info.MaxNameLen = NAME_CHARS
        disp = ctypes.c_uint64(0)
        if not self.dll.SymFromAddrW(self.process, address, ctypes.byref(disp), ctypes.addressof(info)):
            return None
        name_at = ctypes.addressof(info) + _SymbolInfo.Name.offset
        name = ctypes.wstring_at(name_at, min(info.NameLen, NAME_CHARS))
        return name, disp.value, bool(info.Flags & SYMFLAG_EXPORT)

    def close(self) -> None:
        if self.ok:
            self.dll.SymCleanup(self.process)
            self.ok = False


class Namer:
    """Function names of addresses: dbghelp (PDB names), then `.pdata` functions, then module+rva."""

    def __init__(self, profile: Profile, use_dbghelp: bool = True):
        self.profile = profile
        self.bases = sorted(profile.modules)
        self.starts = [b for b, _s, _p in self.bases]
        self.functions: dict[str, list[tuple[int, int, int]]] = {}
        self.dbghelp = DbgHelp(profile.modules) if use_dbghelp else None
        self.cache: dict[int, str] = {}

    def _module(self, address: int) -> tuple[int, int, str] | None:
        i = bisect.bisect_right(self.starts, address) - 1
        if i >= 0:
            base, size, path = self.bases[i]
            if address < base + size:
                return base, size, path
        return None

    def __call__(self, address: int) -> str:
        if address in self.cache:
            return self.cache[address]
        self.cache[address] = name = self._name(address)
        return name

    def _name(self, address: int) -> str:
        module = self._module(address)
        if module is None:
            return f"{address:#x}"
        base, _size, path = module
        file = path.replace("\\", "/").rsplit("/", 1)[-1]
        rva = address - base
        # A return address names the call's function: look one byte back (`aggregate` passes the
        # innermost frame plus one).
        found = self.dbghelp.lookup(address - 1) if self.dbghelp else None
        if found and not found[2]:
            return f"{file}!{shorten(found[0])}"
        funcs = self.functions.get(path)
        if funcs is None:
            funcs = self.functions[path] = pe_functions(path)
        i = bisect.bisect_right(funcs, (rva - 1, 1 << 62, 0)) - 1 if funcs else -1
        if i >= 0 and rva - 1 < funcs[i][1]:
            primary = funcs[i][2]
            if found and found[2] and found[1] == rva - 1 - primary:
                return f"{file}!{found[0]}"
            return f"{file}!sub_{primary:x}"
        if found and found[1] < 0x1000:
            return f"{file}!{found[0]}"
        return f"{file}+{rva:#x}"


# ---------------------------------------------------------------------------------------------
# Report


def thread_group(name: str, start: str) -> str:
    """Threads that differ only by a number share a group; unnamed ones go by their start."""
    if name:
        return re.sub(r"\d+", "N", name)
    return f"(unnamed) {re.sub(r'[+]0x[0-9a-f]+$', '', start) if start else '?'}"


def thread_role(name: str, start: str) -> str:
    low = start.lower()
    if name == "victory-submit":
        return "submit"
    if name.startswith("victory-wide"):
        return "recording"
    if name.startswith("title") or (name == "" and low and not low.startswith(("victory.exe", "ntdll", "kernel"))
                                    and ".dll" not in low.split("+")[0]):
        return "title"
    if name.startswith("victory") or low.startswith("victory.exe"):
        return "helper"
    if not name and low.startswith(("ntdll", "kernel")):
        return "system"
    return "other"


@dataclass
class Tally:
    run: int = 0
    wait: int = 0
    ready: int = 0
    excl: Counter = field(default_factory=Counter)
    incl: Counter = field(default_factory=Counter)
    stacks: Counter = field(default_factory=Counter)  # (names tuple) -> run samples
    waits: Counter = field(default_factory=Counter)   # wait site -> samples

    @property
    def total(self) -> int:
        return self.run + self.wait + self.ready


def _add(t: Tally, names: tuple[str, ...], state: str, n: int) -> None:
    if state == "wait":
        t.wait += n
        site = next((f for f in names[1:] if not WAIT_RE.match(f) and not _wait_wrapper(f)), names[0])
        t.waits[f"{names[0]} <- {site}"] += n
        return
    if state == "ready":
        t.ready += n
        return
    t.run += n
    if names:
        t.excl[names[0]] += n
        for f in set(names):
            t.incl[f] += n
        t.stacks[names] += n


def _wait_wrapper(name: str) -> bool:
    return name.lower().startswith(("kernelbase.dll!", "kernel32.dll!")) and "wait" in name.lower() \
        or name.lower().startswith("ntdll.dll!rtl") and "wait" in name.lower()


def classify(names: tuple[str, ...], ran: bool) -> str:
    if names and WAIT_RE.match(names[0]):
        return "wait"
    return "run" if ran else "ready"


def collapse(names: list[str]) -> tuple[str, ...]:
    """Frames named, recursion into the same function folded."""
    out: list[str] = []
    for n in names:
        if not out or out[-1] != n:
            out.append(n)
    return tuple(out)


def aggregate(profile: Profile, name: Callable[[int], str]) -> tuple[dict[int, Tally], dict[str, Tally], Tally]:
    named: dict[int, tuple[str, ...]] = {}
    per_thread: dict[int, Tally] = defaultdict(Tally)
    per_group: dict[str, Tally] = defaultdict(Tally)
    total = Tally()
    for tid, stack, ran, n in profile.samples:
        if stack not in named:
            pcs = profile.stacks[stack] if stack < len(profile.stacks) else ()
            # Frames past the first are return addresses, named by the byte before (the call);
            # the first is the instruction itself.
            named[stack] = collapse([name(pc + 1) if i == 0 else name(pc) for i, pc in enumerate(pcs)])
        names = named[stack]
        state = classify(names, ran)
        _cpu, start, tname = profile.threads.get(tid, (0.0, "", ""))
        for t in (per_thread[tid], per_group[thread_group(tname, start)], total):
            _add(t, names, state, n)
    return per_thread, per_group, total


def _pct(n: int, d: int) -> str:
    return f"{100.0 * n / d:5.1f}" if d else "  0.0"


def _table(title: str, counter: Counter, denom: int, top: int, total_run: int) -> list[str]:
    out = [f"### {title}", f"{'samples':>9} {'%group':>6} {'%all':>6}  function"]
    for f, n in counter.most_common(top):
        out.append(f"{n:>9} {_pct(n, denom)} {_pct(n, total_run)}  {f}")
    return out + [""]


def report(profile: Profile, name: Callable[[int], str], top: int = 40, groups: int = 8,
           group_top: int = 20, stacks_for: int = 10, source: str = "") -> str:
    per_thread, per_group, total = aggregate(profile, name)
    m = profile.meta
    window_ms = m.get("window_s", 0.0) * 1e3
    expected = int(window_ms / m["interval_ms"]) if m.get("interval_ms") else 0
    out = [
        f"# victory sample profile {source}".rstrip(),
        "",
        f"interval {m.get('interval_ms', 0):g} ms, window {m.get('window_s', 0):.1f} s, {int(m.get('ticks', 0))} ticks "
        f"(of {expected} at the interval: fewer = the sampler fell behind); "
        f"{total.total} samples of {len(profile.threads)} threads, {len(profile.stacks)} distinct stacks; "
        f"the sampler held a thread {m.get('suspended_us_mean', 0):.1f} us mean ({int(m.get('suspended_us_max', 0))} us most), "
        f"{int(m.get('stuck', 0))} resumed by the watchdog",
        "run = on a CPU (cycles since the last tick, not in a wait call); wait = in a wait system call; "
        "ready = no cycles since the last tick and not in a wait call (preempted). busy = CPU time / window. "
        "Function tables count run samples; %group of the group's run samples, %all of every thread's.",
        "",
        "## Threads by CPU",
        f"{'tid':>6} {'role':<9} {'cpu_ms':>9} {'busy%':>6} {'run%':>6} {'wait%':>6} {'ready%':>6}  name / start  | top function",
    ]
    order = sorted(profile.threads.items(), key=lambda kv: -kv[1][0])
    for tid, (cpu, start, tname) in order:
        t = per_thread.get(tid, Tally())
        if cpu < 0.5 and t.run == 0:
            continue
        hot = t.excl.most_common(1)[0][0] if t.excl else "-"
        out.append(
            f"{tid:>6} {thread_role(tname, start):<9} {cpu:>9.1f} {_pct(int(cpu * 1000), int(window_ms * 1000))} "
            f"{_pct(t.run, t.total)} {_pct(t.wait, t.total)} {_pct(t.ready, t.total)}  "
            f"{tname or '(unnamed)'} @ {start}  | {hot}"
        )
    idle = sum(1 for tid, (cpu, _s, _n) in order if cpu < 0.5 and per_thread.get(tid, Tally()).run == 0)
    out += [f"({idle} threads under 0.5 ms CPU and no run samples left out)", ""]

    group_cpu: dict[str, float] = defaultdict(float)
    group_n: Counter = Counter()
    for tid, (cpu, start, tname) in profile.threads.items():
        g = thread_group(tname, start)
        group_cpu[g] += cpu
        group_n[g] += 1
    ranked = sorted(per_group, key=lambda g: -group_cpu.get(g, 0.0))
    out += ["## Thread groups by CPU (cores = CPU time / window)",
            f"{'threads':>7} {'cpu_ms':>10} {'cores':>6} {'run%':>6} {'wait%':>6} {'ready%':>6}  group"]
    for g in ranked:
        t = per_group[g]
        cores = group_cpu[g] / window_ms if window_ms else 0.0
        out.append(f"{group_n[g]:>7} {group_cpu[g]:>10.1f} {cores:>6.2f} {_pct(t.run, t.total)} "
                   f"{_pct(t.wait, t.total)} {_pct(t.ready, t.total)}  {g}")
    out.append("")

    out += ["## All threads", ""]
    out += _table(f"Top {top} functions, exclusive", total.excl, total.run, top, total.run)
    out += _table(f"Top {top} functions, inclusive", total.incl, total.run, top, total.run)

    out += [f"## Top {groups} thread groups by CPU", ""]
    for g in ranked[:groups]:
        t = per_group[g]
        if not t.run:
            continue
        out.append(f"## {g}: {group_cpu[g]:.0f} ms CPU, {t.run} run / {t.wait} wait / {t.ready} ready samples")
        out.append("")
        out += _table(f"{g}: top {group_top} exclusive", t.excl, t.run, group_top, total.run)
        out += _table(f"{g}: top {group_top} inclusive", t.incl, t.run, group_top, total.run)
        if t.waits:
            out += _table(f"{g}: where it waits (wait call <- caller)", t.waits, t.wait, 8, total.wait)

    out += [f"## Hottest call stacks of the top {stacks_for} exclusive functions (innermost first)", ""]
    for f, n in total.excl.most_common(stacks_for):
        out.append(f"### {f}: {n} samples ({_pct(n, total.run).strip()}% of run)")
        by_stack: Counter = Counter()
        for names, c in total.stacks.items():
            if names and names[0] == f:
                by_stack[names[:14]] += c
        for names, c in by_stack.most_common(3):
            out.append(f"  {c:>7} ({_pct(c, n).strip()}%)")
            out.extend(f"      {frame}" for frame in names)
        out.append("")
    return "\n".join(out).rstrip() + "\n"


def summary_lines(profile: Profile, name: Callable[[int], str], top: int = 10) -> list[str]:
    """The top exclusive functions, one line each, for the terminal."""
    _pt, _pg, total = aggregate(profile, name)
    return [f"{n:>8} {_pct(n, total.run)}%  {f}" for f, n in total.excl.most_common(top)]


def report_file(path: Path, out: Path | None = None, use_dbghelp: bool = True) -> tuple[Path, Profile, Namer]:
    profile = parse(path.read_text(encoding="utf-8", errors="replace"))
    namer = Namer(profile, use_dbghelp=use_dbghelp)
    out = out or path.with_name("report.txt")
    out.write_text(report(profile, namer, source=str(path)), encoding="utf-8", newline="\n")
    return out, profile, namer
