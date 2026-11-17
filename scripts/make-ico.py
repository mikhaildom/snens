#!/usr/bin/env python3
"""Собрать .ico из набора PNG (формат ICO с PNG-кадрами, Vista+).

Зачем свой скрипт: ImageMagick и Pillow на машине релиза не гарантированы, а
формат простой — заголовок и таблица кадров с данными PNG как есть.

Кадры обязаны быть RGBA: `tauri build` (tauri-codegen) падает с «icon … is not
RGBA» на любом другом цветотипе, а headless-chrome без
`--default-background-color=00000000` пишет RGB. Поэтому здесь же есть
`--rgba`: он дописывает альфа-канал (непрозрачный) к RGB-кадру, а PNG
пересобирается заново — без zlib-магии: фильтры сбрасываются в 0.
"""
import struct
import sys
import zlib
from pathlib import Path


def _chunks(blob: bytes):
    """Разобрать PNG на (тип, данные) — по кускам после сигнатуры."""
    pos = 8
    while pos + 8 <= len(blob):
        (length,) = struct.unpack(">I", blob[pos : pos + 4])
        ctype = blob[pos + 4 : pos + 8]
        data = blob[pos + 8 : pos + 8 + length]
        yield ctype, data
        pos += 12 + length


def _chunk(ctype: bytes, data: bytes) -> bytes:
    return (
        struct.pack(">I", len(data))
        + ctype
        + data
        + struct.pack(">I", zlib.crc32(ctype + data) & 0xFFFFFFFF)
    )


def to_rgba(blob: bytes) -> bytes:
    """Пересобрать PNG с цветотипом RGBA. Палитру/серое не поддерживаем: наш
    источник — chrome, он пишет либо RGB, либо RGBA."""
    ihdr = None
    idat = bytearray()
    rest = []
    for ctype, data in _chunks(blob):
        if ctype == b"IHDR":
            ihdr = data
        elif ctype == b"IDAT":
            idat += data
        elif ctype == b"IEND":
            continue
        else:
            rest.append((ctype, data))
    if ihdr is None:
        raise SystemExit("не PNG: нет IHDR")
    width, height, depth, color, comp, filt, interlace = struct.unpack(">IIBBBBB", ihdr)
    if (depth, color) == (8, 6):
        return blob
    if (depth, color) != (8, 2):
        raise SystemExit(f"ожидался RGB8/RGBA8, а тут depth={depth} color={color}")
    if interlace:
        raise SystemExit("interlaced PNG не поддерживается")

    raw = zlib.decompress(bytes(idat))
    stride = width * 3
    out = bytearray()
    prev = bytearray(stride)
    pos = 0
    for _ in range(height):
        ftype = raw[pos]
        line = bytearray(raw[pos + 1 : pos + 1 + stride])
        pos += 1 + stride
        if ftype == 1:  # Sub
            for i in range(3, stride):
                line[i] = (line[i] + line[i - 3]) & 0xFF
        elif ftype == 2:  # Up
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xFF
        elif ftype == 3:  # Average
            for i in range(stride):
                left = line[i - 3] if i >= 3 else 0
                line[i] = (line[i] + ((left + prev[i]) >> 1)) & 0xFF
        elif ftype == 4:  # Paeth
            for i in range(stride):
                a = line[i - 3] if i >= 3 else 0
                b = prev[i]
                c = prev[i - 3] if i >= 3 else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pred = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pred) & 0xFF
        elif ftype != 0:
            raise SystemExit(f"неизвестный фильтр PNG: {ftype}")
        prev = line
        # Непрозрачный альфа-канал: досыпаем 255 после каждого пикселя.
        out.append(0)  # фильтр None — так пересжатие проще и корректнее
        for x in range(width):
            out += line[x * 3 : x * 3 + 3]
            out.append(0xFF)
    ihdr_rgba = struct.pack(">IIBBBBB", width, height, 8, 6, comp, filt, 0)
    png = b"\x89PNG\r\n\x1a\n" + _chunk(b"IHDR", ihdr_rgba)
    for ctype, data in rest:
        png += _chunk(ctype, data)
    png += _chunk(b"IDAT", zlib.compress(bytes(out), 9))
    png += _chunk(b"IEND", b"")
    return png


def build(frames: list[Path], out: Path) -> None:
    blobs = [to_rgba(p.read_bytes()) for p in frames]
    n = len(blobs)
    header = struct.pack("<HHH", 0, 1, n)
    offset = 6 + 16 * n
    entries = b""
    for blob in blobs:
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
    args = sys.argv[1:]
    if args[:1] == ["--rgba"]:
        # Пересобрать один файл в RGBA: `make-icons.sh` зовёт это для icon.png.
        if len(args) != 3:
            raise SystemExit("использование: make-ico.py --rgba <in.png> <out.png>")
        src, dst = Path(args[1]), Path(args[2])
        dst.write_bytes(to_rgba(src.read_bytes()))
        raise SystemExit(0)
    if len(args) < 2:
        raise SystemExit("использование: make-ico.py <кадр.png>… <out.ico>")
    *inputs, target = args
    build([Path(p) for p in inputs], Path(target))
