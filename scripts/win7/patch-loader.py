"""Правка WebView2Loader.dll для Windows 7 без обновления KB2533623.

Загрузчик подключает системные библиотеки (version.dll, advapi32, shell32) так:
    LoadLibraryExW(имя, NULL, LOAD_LIBRARY_SEARCH_SYSTEM32 /* 0x800 */)
Этот флаг Windows 7 понимает только с KB2533623, которого в каталоге Microsoft
для Win7 больше нет. Без него вызов падает, и загрузчик отвечает 0x80070032
(«не поддерживается») — окно пишет «Could not find the WebView2 Runtime».

Правка меняет флаг на 0 (обычный поиск библиотеки) только в таких вызовах:
    lea rcx,[имя] ; xor edx,edx ; mov r8d,0x800 ; call [LoadLibraryExW]
Вызовы из библиотеки C у загрузчика сами повторяют попытку без флага — их не трогаем.

python patch-loader.py <вход.dll> <выход.dll>
"""
import struct
import sys


def sections(d):
    pe = struct.unpack_from('<I', d, 0x3c)[0]
    if d[pe:pe + 4] != b'PE\0\0':
        raise SystemExit('не PE-файл')
    nsec = struct.unpack_from('<H', d, pe + 6)[0]
    optsz = struct.unpack_from('<H', d, pe + 20)[0]
    opt = pe + 24
    if struct.unpack_from('<H', d, opt)[0] != 0x20b:
        raise SystemExit('нужен 64-битный загрузчик')
    base = struct.unpack_from('<Q', d, opt + 24)[0]
    imp_rva = struct.unpack_from('<I', d, opt + 112 + 8)[0]
    secs = []
    o = opt + optsz
    for _ in range(nsec):
        vsz, va, rsz, raw = struct.unpack_from('<IIII', d, o + 8)
        secs.append((va, max(vsz, rsz), raw))
        o += 40
    return base, imp_rva, secs


def rva2off(secs, rva):
    for va, sz, raw in secs:
        if va <= rva < va + sz:
            return raw + rva - va
    return None


def iat_slot(d, secs, imp_rva, want):
    """RVA ячейки импорта функции want (например b'LoadLibraryExW')."""
    o = rva2off(secs, imp_rva)
    while True:
        ilt, _, _, name_rva, iat = struct.unpack_from('<IIIII', d, o)
        if not name_rva:
            return None
        t = rva2off(secs, ilt or iat)
        i = 0
        while True:
            e = struct.unpack_from('<Q', d, t + 8 * i)[0]
            if not e:
                break
            if not e >> 63:
                n = rva2off(secs, (e & 0x7fffffff) + 2)
                if d[n:n + len(want) + 1] == want + b'\0':
                    return iat + 8 * i
            i += 1
        o += 20


def main():
    src, dst = sys.argv[1], sys.argv[2]
    d = bytearray(open(src, 'rb').read())
    base, imp_rva, secs = sections(d)
    slot = iat_slot(d, secs, imp_rva, b'LoadLibraryExW')
    if slot is None:
        raise SystemExit('в загрузчике нет импорта LoadLibraryExW')
    # Ищем mov r8d,0x800 (41 B8 00 08 00 00), за которым call [rip+Y] (FF 15 ....).
    seq = bytes.fromhex('41b800080000ff15')
    fixed = 0
    pos = d.find(seq)
    while pos >= 0:
        # перед mov — xor edx,edx (31 D2 или 33 D2)
        if d[pos - 1] != 0xd2 or d[pos - 2] not in (0x31, 0x33):
            pos = d.find(seq, pos + 1)
            continue
        call_end = pos + len(seq) + 4
        disp = struct.unpack_from('<i', d, pos + len(seq))[0]
        # RVA конца инструкции call -> RVA ячейки, на которую она прыгает
        for va, sz, raw in secs:
            if raw <= pos < raw + sz:
                target = va + (call_end - raw) + disp
                break
        else:
            target = None
        # lea rcx,[имя] — сразу перед xor edx,edx или ещё через xor edi,edi
        lea = d[pos - 9:pos - 6] == b'\x48\x8d\x0d' or d[pos - 11:pos - 8] == b'\x48\x8d\x0d'
        if target == slot and lea:
            d[pos + 3] = 0   # 41 B8 00 [08] 00 00 -> 0
            fixed += 1
        pos = d.find(seq, pos + 1)
    if not fixed:
        raise SystemExit('не нашла вызовов с флагом 0x800 — другая версия загрузчика?')
    open(dst, 'wb').write(d)
    sys.stdout.reconfigure(encoding='utf-8')
    print(f'исправлено вызовов: {fixed}')


main()
