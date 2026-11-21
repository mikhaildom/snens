#!/usr/bin/env bash
# Раскладка поставки для Windows 7 после build.sh:
#   <out>/snens.exe, snens-app.exe (окно-приложение), tools/<тул>/<тул>.exe (+schema.json), web/ (dist клиента),
#   start-snens.cmd (CP866 — кодировка консоли Win7), README.txt (UTF-8 с BOM).
# Аргументы: [каталог dist snens-web] [выходной каталог]
set -euo pipefail
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$REPO/scripts/win7"
WEB="${1:-$REPO/../snens-web/dist}"
OUT="${2:-$REPO/target/dist-win7/snens}"
BIN="$REPO/target/x86_64-win7-windows-gnu/release"
[ -f "$WEB/index.html" ] || { echo "нет $WEB/index.html — собери клиент (SNENS_PROXY=0 npm run build)" >&2; exit 1; }
case "$OUT" in */snens) ;; *) echo "выходной каталог должен кончаться на /snens: $OUT" >&2; exit 1 ;; esac
rm -rf "$OUT"; mkdir -p "$OUT/tools" "$OUT/web"
cp "$BIN/snens.exe" "$BIN/snens-app.exe" "$OUT/"
for d in "$REPO"/tools/*/; do
  n="$(basename "$d")"
  [ -f "$d/Cargo.toml" ] || continue
  [ -f "$d/src/lib.rs" ] && [ ! -f "$d/src/main.rs" ] && continue   # cdp — библиотека
  [ -f "$BIN/$n.exe" ] || { echo "нет $BIN/$n.exe" >&2; exit 1; }
  mkdir -p "$OUT/tools/$n"; cp "$BIN/$n.exe" "$OUT/tools/$n/"
  [ -f "$d/schema.json" ] && cp "$d/schema.json" "$OUT/tools/$n/"
done
cp -r "$WEB"/. "$OUT/web/"
iconv -f UTF-8 -t CP866 "$HERE/start-snens.cmd.utf8" | sed 's/$/\r/' > "$OUT/start-snens.cmd"
{ printf '\xEF\xBB\xBF'; sed 's/$/\r/' "$HERE/README.txt"; } > "$OUT/README.txt"
echo "готово: $OUT ($(du -sh "$OUT" | cut -f1))"
