"""GPU driver health for tools/dev.py: how many times the display driver reset the GPU since boot.

Once the NVIDIA driver has reset the GPU (System log, provider nvlddmkm, event 153), every later
run on this boot tends to hang too, whatever the code (seen 2026-11-26: 72 resets, cured by a
reboot; then ~70 more from 22:56 on). Bench and capture report the count so a job notices at once
instead of bisecting code that is not at fault.
"""

from __future__ import annotations

import os
import subprocess

RESET_EVENT_ID = 153
QUERY = (
    "$boot=(Get-CimInstance Win32_OperatingSystem).LastBootUpTime; "
    "@(Get-WinEvent -FilterHashtable @{LogName='System'; ProviderName='nvlddmkm'; Id=%d; StartTime=$boot} "
    "-ErrorAction SilentlyContinue).Count" % RESET_EVENT_ID
)


def parse_count(text: str) -> int | None:
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    if not lines or not lines[-1].isdigit():
        return None
    return int(lines[-1])


def resets_since_boot() -> int | None:
    """The count, or None where it cannot be read (not Windows, no PowerShell)."""
    if os.name != "nt":
        return None
    try:
        proc = subprocess.run(
            ["powershell", "-NoProfile", "-Command", QUERY],
            capture_output=True, text=True, timeout=60,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    return parse_count(proc.stdout)


def warning(before: int | None, after: int | None) -> str:
    """A line for the job when the driver reset during this run or already had on this boot."""
    if after is None:
        return ""
    if before is not None and after > before:
        return f"WARNING: the GPU driver reset {after - before} time(s) during this run ({after} since boot)."
    if after > 0:
        return (
            f"NOTE: the GPU driver has reset {after} time(s) since boot. One isolated reset is survivable; "
            "if this run or the next one hangs (device removed) or crawls under 5 fps settled, stop game runs "
            "and tell the coordinator: the PC needs a reboot."
        )
    return ""
