#!/usr/bin/env bash
# Сборка настоящего окна snens (desktop/, Tauri) под Windows 7.
#
# Окружение — как в build.sh. Плюс две заплатки крейтов (scripts/win7/patches):
#   webview2-com — просит движок не новее 109 (последний для Win7);
#   wry          — передаёт загрузчику WebView2 папку движка из
#                  WEBVIEW2_BROWSER_EXECUTABLE_FOLDER (её ставит desktop/src/main.rs,
#                  если рядом с exe лежит WebView2\msedgewebview2.exe).
# Сначала build.sh (ядро и тулы под Win7) и сборка клиента snens-web.
# Аргумент: [каталог dist snens-web]. Готовое окно раскладывает stage.sh.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
W64DEVKIT="${W64DEVKIT:-$REPO/../toolchain/w64devkit}"
[ -x "$W64DEVKIT/bin/gcc" ] || { echo "нет gcc в $W64DEVKIT/bin — задай W64DEVKIT" >&2; exit 1; }
SHIM="$REPO/target/win7-shim"
mkdir -p "$SHIM"
[ -f "$SHIM/libgcc_eh.a" ] || "$W64DEVKIT/bin/ar" rcs "$SHIM/libgcc_eh.a"
export PATH="$W64DEVKIT/bin:$HOME/.cargo/bin:$PATH"
export LIBRARY_PATH="$(cygpath -w "$SHIM" 2>/dev/null || echo "$SHIM")"
export CC_x86_64_win7_windows_gnu=gcc CXX_x86_64_win7_windows_gnu=g++ AR_x86_64_win7_windows_gnu=ar
export CARGO_TARGET_X86_64_WIN7_WINDOWS_GNU_LINKER=gcc
export CARGO_TARGET_X86_64_WIN7_WINDOWS_GNU_RUSTFLAGS="-C target-cpu=x86-64 -C link-args=-static"
WEB="${1:-$REPO/../snens-web/dist}"
BIN="$REPO/target/x86_64-win7-windows-gnu/release"
D="$REPO/desktop"
[ -f "$WEB/index.html" ] || { echo "нет $WEB/index.html — собери клиент" >&2; exit 1; }
[ -f "$BIN/snens.exe" ] || { echo "нет $BIN/snens.exe — сначала build.sh" >&2; exit 1; }
# Входные файлы окна (не в git): ядро-спутник, тулы и клиент.
mkdir -p "$D/binaries"
cp "$BIN/snens.exe" "$D/binaries/snens-x86_64-win7-windows-gnu.exe"
rm -rf "$D/resources"; mkdir -p "$D/resources/tools" "$D/resources/web"
for d in "$REPO"/tools/*/; do
  n="$(basename "$d")"
  [ -f "$d/Cargo.toml" ] || continue
  [ -f "$d/src/lib.rs" ] && [ ! -f "$d/src/main.rs" ] && continue   # cdp — библиотека
  [ -f "$BIN/$n.exe" ] || { echo "нет $BIN/$n.exe" >&2; exit 1; }
  mkdir -p "$D/resources/tools/$n"; cp "$BIN/$n.exe" "$D/resources/tools/$n/"
  [ -f "$d/schema.json" ] && cp "$d/schema.json" "$D/resources/tools/$n/"
done
cp -r "$WEB"/. "$D/resources/web/"
P="$(cygpath -m "$REPO/scripts/win7/patches" 2>/dev/null || echo "$REPO/scripts/win7/patches")"
cd "$REPO/desktop"
# Заплатки переписывают Cargo.lock (источник крейта — папка). Общий lock окна
# не трогаем: после сборки возвращаем как было.
cp Cargo.lock "$REPO/target/desktop-Cargo.lock.orig"
trap 'cp "$REPO/target/desktop-Cargo.lock.orig" "$REPO/desktop/Cargo.lock"' EXIT
cargo +nightly-x86_64-pc-windows-gnu build -Z build-std=std,panic_abort \
  --release --target x86_64-win7-windows-gnu --target-dir ../target/desktop-win7 \
  --config "patch.crates-io.webview2-com.path=\"$P/webview2-com-0.39.1\"" \
  --config "patch.crates-io.wry.path=\"$P/wry-0.57.1\""
# Заменители DLL Windows 8 (combase, winrt-error), без которых exe не стартует на Win7.
bash "$REPO/scripts/win7/w7compat/build.sh"
