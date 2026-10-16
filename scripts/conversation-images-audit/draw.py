#!/usr/bin/env python3
"""Draw invented PNG images for the conversation image audit. No photos, no screenshots.

Usage: draw.py OUT_DIR

Writes:
  garden.png   320x200  rings over a diagonal gradient
  pond.png     240x240  a checkerboard with a disc
  mural.png    6000x4000  a large gradient, to check that a huge image is scaled before it is held
  tiny.png     1x1
  far.png      200x120  stripes, held only by the second host in the remote case
"""

import struct
import sys
import zlib
from pathlib import Path


def png(width, height, pixel):
    rows = bytearray()
    for y in range(height):
        rows.append(0)
        for x in range(width):
            rows.extend(pixel(x, y))

    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
            + chunk(b"IDAT", zlib.compress(bytes(rows), 9)) + chunk(b"IEND", b""))


def garden(x, y):
    ring = int(((x - 160) ** 2 + (y - 100) ** 2) ** 0.5) // 12 % 2
    return (40 + x * 200 // 320, 120 + 100 * ring, 60 + y * 180 // 200)


def pond(x, y):
    inside = (x - 120) ** 2 + (y - 120) ** 2 < 80 ** 2
    check = (x // 30 + y // 30) % 2
    return (30, 90, 200) if inside else ((230, 230, 210) if check else (90, 140, 60))


def mural_png(width, height):
    # Row-wise so a 6000x4000 image draws in a few seconds.
    rows = bytearray()
    for y in range(height):
        rows.append(0)
        shade = y * 255 // height
        row = bytearray(width * 3)
        for x in range(0, width, 1):
            row[x * 3] = x * 255 // width
            row[x * 3 + 1] = shade
            row[x * 3 + 2] = 128
        rows.extend(row)

    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
            + chunk(b"IDAT", zlib.compress(bytes(rows), 6)) + chunk(b"IEND", b""))


def main():
    out = Path(sys.argv[1])
    out.mkdir(parents=True, exist_ok=True)
    (out / "garden.png").write_bytes(png(320, 200, garden))
    (out / "pond.png").write_bytes(png(240, 240, pond))
    (out / "tiny.png").write_bytes(png(1, 1, lambda x, y: (200, 60, 60)))
    (out / "far.png").write_bytes(png(200, 120, lambda x, y: (20, 60 + (x // 20 % 2) * 120, 160)))
    (out / "mural.png").write_bytes(mural_png(6000, 4000))


if __name__ == "__main__":
    main()
