"""tools/dev_driver.py tells a job when the GPU driver has reset (runs on that boot are unreliable)."""

import sys
from pathlib import Path

TOOLS = Path(__file__).resolve().parents[1] / "tools"
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))

import dev_driver  # noqa: E402


def test_parse_count_reads_the_last_number():
    assert dev_driver.parse_count("0\r\n") == 0
    assert dev_driver.parse_count("noise\n72\n") == 72
    assert dev_driver.parse_count("") is None
    assert dev_driver.parse_count("Get-WinEvent : error\n") is None


def test_no_warning_on_a_healthy_boot():
    assert dev_driver.warning(0, 0) == ""
    assert dev_driver.warning(None, None) == ""


def test_warns_when_the_driver_reset_during_the_run():
    assert "reset 1 time(s) during this run (3 since boot)" in dev_driver.warning(2, 3)


def test_warns_to_reboot_after_earlier_resets():
    line = dev_driver.warning(5, 5)
    assert "5 time(s) since boot" in line
    assert "reboot" in line
