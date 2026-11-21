#!/usr/bin/env bash
# Проверка: не зовут ли exe/dll функции, которых нет в Windows 7.
# objdump из w64devkit читает таблицу импорта PE (как dumpbin /imports).
#
# Проверяется ядро с тулами (каталог сборки или первый аргумент) и, если оно
# собрано, настоящее окно snens-desktop.exe с WebView2Loader.dll. Окну на Win7
# нужны две DLL из Windows 8 — их заменяют наши w7compat/out/*.dll: такие
# импорты не считаются ошибкой, но каждая функция обязана быть в заменителе.
set -uo pipefail
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
W64DEVKIT="${W64DEVKIT:-$REPO/../toolchain/w64devkit}"
OBJDUMP="$W64DEVKIT/bin/objdump"
DIR="${1:-$REPO/target/x86_64-win7-windows-gnu/release}"
DESKTOP="$REPO/target/desktop-win7/x86_64-win7-windows-gnu/release"
COMPAT="${W7COMPAT_DIR:-$REPO/scripts/win7/w7compat/out}"
BAD='GetSystemTimePreciseAsFileTime|ProcessPrng|WaitOnAddress|WakeByAddressSingle|WakeByAddressAll|SetThreadDescription|GetThreadDescription|GetHostNameW|PathCch[A-Za-z]*|RoInitialize|RoGetActivationFactory|WindowsCreateString|CreateFile2|CreatePseudoConsole|ClosePseudoConsole|ResizePseudoConsole|GetSystemCpuSetInformation|GetProcessMitigationPolicy|SetProcessMitigationPolicy|GetCurrentThreadStackLimits|PrefetchVirtualMemory'
fail=0

# Импорты файла строками «dll функция» (dll — в нижнем регистре).
imports() {
  awk '/DLL Name:/ { dll = tolower($NF); next }
       dll != "" && NF >= 3 && $1 ~ /^[0-9a-f]+$/ { print dll, $NF }
       /^[ 	]*$/ { dll = "" }' <<<"$1"
}

# Экспорты заменителя (имена функций).
exports() {
  "$OBJDUMP" -p "$1" | awk '/\[Ordinal\/Name Pointer\] Table/ { on = 1; next }
                             on && /^[ 	]*\[/ { print $NF; next }
                             on && /^[ 	]*$/ { on = 0 }'
}

check() {
  local file="$1" name dump dlls bad_dll bad_fn compat_dll dll fn missing=""
  name="$(basename "$file")"
  if ! dump="$("$OBJDUMP" -p "$file")" || ! grep -q "DLL Name:" <<<"$dump"; then
    echo "ПЛОХО $name: objdump не прочитал таблицу импорта"; fail=1; return
  fi
  dlls="$(sed -n 's/^\s*DLL Name: //p' <<<"$dump" | sort -u | tr '\n' ' ')"
  bad_dll=""
  for dll in $(tr ' ' '\n' <<<"$dlls" | grep -iE '^api-ms-win-|^bcryptprimitives|^combase|^ext-ms-' || true); do
    compat_dll="$(find "$COMPAT" -maxdepth 1 -iname "$dll" 2>/dev/null | head -1)"
    if [ -z "$compat_dll" ]; then
      bad_dll="$bad_dll$dll "
      continue
    fi
    # Заменитель есть — каждая импортируемая функция должна в нём быть.
    while read -r _ fn; do
      grep -qxF "$fn" <<<"$(exports "$compat_dll")" || missing="$missing$dll!$fn "
    done < <(imports "$dump" | awk -v d="$(tr 'A-Z' 'a-z' <<<"$dll")" '$1 == d')
  done
  bad_fn="$(grep -E "\s($BAD)\b" <<<"$dump" | awk '{print $NF}' | sort -u | tr '\n' ' ')"
  if [ -n "$bad_dll$bad_fn$missing" ]; then
    echo "ПЛОХО $name: DLL [$bad_dll] функции [$bad_fn] нет в заменителях [$missing]"; fail=1
  else
    echo "ok    $name: $dlls"
  fi
}

shopt -s nullglob
exes=("$DIR"/*.exe)
[ ${#exes[@]} -gt 0 ] || { echo "нет ни одного exe в $DIR" >&2; exit 1; }
for exe in "${exes[@]}"; do check "$exe"; done

if [ $# -eq 0 ] && [ -f "$DESKTOP/snens-desktop.exe" ]; then
  [ -d "$COMPAT" ] || { echo "ПЛОХО: нет $COMPAT — собери w7compat/build.sh"; exit 1; }
  # Сами заменители не проверяем: они переадресуют вызовы и своих импортов
  # почти не имеют.
  for f in "$DESKTOP/snens-desktop.exe" "$DESKTOP/WebView2Loader.dll"; do
    [ -f "$f" ] && check "$f"
  done
fi
exit $fail
