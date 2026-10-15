"""Генератор PNG-фикстуры для теста /api/export/png.

Однопиксельный RGB PNG собирается вручную (sig + IHDR + IDAT + IEND),
без сторонних зависимостей. Запуск: python make_png.py
"""
import os
import struct
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))


def chunk(tag: bytes, data: bytes) -> bytes:
    body = tag + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)


def build_png(width: int, height: int, rgb: tuple) -> bytes:
    raw = b"".join(b"\x00" + bytes(rgb) * width for _ in range(height))
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


if __name__ == "__main__":
    for name, size, color in [("pixel.png", 1, (255, 0, 0)), ("chart.png", 64, (30, 122, 204))]:
        data = build_png(size, size, color)
        path = os.path.join(HERE, name)
        with open(path, "wb") as fh:
            fh.write(data)
        print(f"{name}: {len(data)} bytes")
