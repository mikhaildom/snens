#!/usr/bin/env bash
# Проверка: не зовут ли exe функции, которых нет в Windows 7.
# objdump из w64devkit читает таблицу импорта PE (как dumpbin /imports).
set -uo pipefail
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
W64DEVKIT="${W64DEVKIT:-$REPO/../toolchain/w64devkit}"
OBJDUMP="$W64DEVKIT/bin/objdump"
DIR="${1:-$REPO/target/x86_64-win7-windows-gnu/release}"
BAD='GetSystemTimePreciseAsFileTime|ProcessPrng|WaitOnAddress|WakeByAddressSingle|WakeByAddressAll|SetThreadDescription|GetThreadDescription|GetHostNameW|PathCch[A-Za-z]*|RoInitialize|RoGetActivationFactory|WindowsCreateString|CreateFile2|CreatePseudoConsole|ClosePseudoConsole|ResizePseudoConsole|GetSystemCpuSetInformation|GetProcessMitigationPolicy|SetProcessMitigationPolicy|GetCurrentThreadStackLimits|PrefetchVirtualMemory'
fail=0
for exe in "$DIR"/*.exe; do
  name="$(basename "$exe")"
  dump="$("$OBJDUMP" -p "$exe")"
  dlls="$(printf '%s\n' "$dump" | sed -n 's/^\s*DLL Name: //p' | sort -u | tr '\n' ' ')"
  bad_dll="$(printf '%s\n' "$dlls" | tr ' ' '\n' | grep -iE '^api-ms-win-|^bcryptprimitives|^combase|^ext-ms-' || true)"
  bad_fn="$(printf '%s\n' "$dump" | grep -E "\s($BAD)\b" | awk '{print $NF}' | sort -u | tr '\n' ' ')"
  if [ -n "$bad_dll$bad_fn" ]; then
    echo "ПЛОХО $name: DLL [$bad_dll] функции [$bad_fn]"; fail=1
  else
    echo "ok    $name: $dlls"
  fi
done
exit $fail
