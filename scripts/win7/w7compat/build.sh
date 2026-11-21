#!/usr/bin/env bash
# Две DLL-заменителя для snens-desktop.exe на Windows 7 (кладутся рядом с exe).
# На Windows 8+ они не мешают: combase — системная известная DLL, а
# api-ms-win-* система всегда берёт из своего набора, а не из папки программы.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="${1:-$HERE/out}"
mkdir -p "$OUT"
gcc -shared -O2 -s -nostdlib -e DllMain "$HERE/combase.c" "$HERE/combase.def" -lkernel32 -o "$OUT/combase.dll"
gcc -shared -O2 -s -nostdlib -e DllMain "$HERE/winrt-error.c" "$HERE/winrt-error.def" -lkernel32 -o "$OUT/api-ms-win-core-winrt-error-l1-1-0.dll"
ls -la "$OUT"
