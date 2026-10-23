#!/usr/bin/env python3
"""Собрать .ico из набора PNG (формат ICO с PNG-кадрами, Vista+).

Зачем свой скрипт: ImageMagick и Pillow на машине релиза не гарантированы, а
формат простой — заголовок и таблица кадров с данными PNG как есть.
"""
import struct
import sys
from pathlib import Path


def build(frames: list[Path], out: Path) -> None:
    blobs = [p.read_bytes() for p in frames]
    n = len(blobs)
    header = struct.pack("<HHH", 0, 1, n)
    offset = 6 + 16 * n
    entries = b""
    for path, blob in zip(frames, blobs):
        # Размер кадра читаем из самого PNG: IHDR, смещение 16 — width/height.
        w, h = struct.unpack(">II", blob[16:24])
        # 256 в ICO записывается нулём — это историческое ограничение поля.
        entries += struct.pack(
            "<BBBBHHII",
            w if w < 256 else 0,
            h if h < 256 else 0,
            0, 0, 1, 32, len(blob), offset,
        )
        offset += len(blob)
    out.write_bytes(header + entries + b"".join(blobs))


if __name__ == "__main__":
    *inputs, target = sys.argv[1:]
    build([Path(p) for p in inputs], Path(target))
