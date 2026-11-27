"""Minimal PNG reading and writing for tools/dev.py (stdlib only): 8-bit RGB or RGBA, not
interlaced, as victory-gpu's `screenshot.rs` and `capture.rs` write them."""

from __future__ import annotations

import struct
import zlib
from itertools import accumulate
from pathlib import Path

SIGNATURE = b"\x89PNG\r\n\x1a\n"
_AND_255 = (255).__and__


class Image:
    """`rows` hold 8-bit RGB bytes, `width * 3` per row."""

    def __init__(self, width: int, height: int, rows: list[bytes]) -> None:
        self.width, self.height, self.rows = width, height, rows


def _chunks(data: bytes):
    at = len(SIGNATURE)
    while at + 8 <= len(data):
        length, kind = struct.unpack_from(">I4s", data, at)
        yield kind, data[at + 8 : at + 8 + length]
        at += 12 + length


def _unfilter(kind: int, raw: bytes, prev: bytes, bpp: int) -> bytes:
    n = len(raw)
    if kind == 0:
        return raw
    if kind == 1:
        out = bytearray(n)
        for c in range(bpp):
            out[c::bpp] = bytes(map(_AND_255, accumulate(raw[c::bpp])))
        return bytes(out)
    if kind == 2:
        # Byte-wise addition of two rows without carries between bytes.
        lo, hi = int.from_bytes(b"\x7f" * n, "little"), int.from_bytes(b"\x80" * n, "little")
        a, b = int.from_bytes(raw, "little"), int.from_bytes(prev, "little")
        return (((a & lo) + (b & lo)) ^ ((a ^ b) & hi)).to_bytes(n, "little")
    out = bytearray(raw)
    if kind == 3:
        for i in range(n):
            left = out[i - bpp] if i >= bpp else 0
            out[i] = (out[i] + ((left + prev[i]) >> 1)) & 255
        return bytes(out)
    if kind == 4:
        for i in range(n):
            if i >= bpp:
                a, c = out[i - bpp], prev[i - bpp]
            else:
                a = c = 0
            b = prev[i]
            p = a + b - c
            pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
            out[i] = (out[i] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
        return bytes(out)
    raise ValueError(f"unknown PNG row filter {kind}")


def read_png(path: Path) -> Image:
    data = path.read_bytes()
    if not data.startswith(SIGNATURE):
        raise ValueError(f"{path}: not a PNG")
    header, idat = b"", []
    for kind, body in _chunks(data):
        if kind == b"IHDR":
            header = body
        elif kind == b"IDAT":
            idat.append(body)
    width, height, depth, color, _, _, interlace = struct.unpack(">IIBBBBB", header)
    if depth != 8 or color not in (2, 6) or interlace:
        raise ValueError(f"{path}: only 8-bit RGB/RGBA non-interlaced PNGs are read")
    bpp = 3 if color == 2 else 4
    stride = width * bpp
    raw = zlib.decompress(b"".join(idat))
    rows, prev = [], bytes(stride)
    for y in range(height):
        at = y * (stride + 1)
        prev = _unfilter(raw[at], raw[at + 1 : at + 1 + stride], prev, bpp)
        if bpp == 4:
            rgb = bytearray(width * 3)
            for c in range(3):
                rgb[c::3] = prev[c::4]
            rows.append(bytes(rgb))
        else:
            rows.append(prev)
    return Image(width, height, rows)


def write_png(path: Path, image: Image) -> None:
    def chunk(kind: bytes, body: bytes) -> bytes:
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))

    header = struct.pack(">IIBBBBB", image.width, image.height, 8, 2, 0, 0, 0)
    raw = b"".join(b"\x00" + row for row in image.rows)
    path.write_bytes(
        SIGNATURE + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(raw, 6)) + chunk(b"IEND", b"")
    )


def side_by_side(left: Image, right: Image, gap: int = 8, gap_rgb: bytes = b"\xff\x00\xff") -> Image:
    """`left` and `right` next to each other with a `gap`-pixel stripe between, the shorter one
    padded with black below."""
    height = max(left.height, right.height)
    stripe = gap_rgb * gap

    def row(image: Image, y: int) -> bytes:
        return image.rows[y] if y < image.height else bytes(image.width * 3)

    rows = [row(left, y) + stripe + row(right, y) for y in range(height)]
    return Image(left.width + gap + right.width, height, rows)
