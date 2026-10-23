#!/bin/sh
# Разложить бинарники и клиент для сборки десктоп-приложения.
#
# Tauri требует, чтобы sidecar назывался `<имя>-<target-triple><.exe>` и лежал в
# `desktop/binaries/`. Здесь же копируется собранный клиент: это единственное
# место, где записано соответствие «что куда кладётся» — иначе знание разъедется
# между README, CI и руками.
#
# Использование:
#   scripts/desktop-stage.sh              # всё: ядро, тулы, плагин памяти, клиент
#   scripts/desktop-stage.sh --no-client  # без пересборки клиента
set -eu

REPO="$(cd "$(dirname "$0")/.." && pwd)"
DESKTOP="$REPO/desktop"
BIN_SRC="${SNENS_BIN_DIR:-}"
CLIENT_SRC="${SNENS_WEB_DIST:-$HOME/projects/snens-web/dist}"
WITH_CLIENT=1

while [ $# -gt 0 ]; do
    case "$1" in
        --no-client) WITH_CLIENT=0 ;;
        -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
        *) echo "неизвестный флаг: $1" >&2; exit 2 ;;
    esac
    shift
done

say() { printf '>>> %s\n' "$*"; }
die() { printf 'ОШИБКА: %s\n' "$*" >&2; exit 1; }

# Тройка цели — из самого rustc, а не из догадки: на Windows это
# x86_64-pc-windows-msvc, на Linux x86_64-unknown-linux-gnu.
TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
[ -n "$TRIPLE" ] || die "не удалось определить target triple (rustc -vV)"

if [ -z "$BIN_SRC" ]; then
    # target-каталог спрашиваем у cargo: он может быть переопределён
    # CARGO_TARGET_DIR или ~/.cargo/config.toml.
    BIN_SRC="$(cd "$REPO" && cargo metadata --no-deps --format-version 1 \
        | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/release"
fi
[ -d "$BIN_SRC" ] || die "нет каталога сборки $BIN_SRC — сначала cargo build --release"

mkdir -p "$DESKTOP/binaries"
say "target triple: $TRIPLE"
say "бинарники: $BIN_SRC"

# Ядро — sidecar: едет в бандле и обновляется вместе с ним.
#
# Плагина памяти здесь НЕТ намеренно: он тул (`tools/memory/memory`), и
# sidecar'ом ехал вторым таким же файлом (5.3 МБ в бандле). Одна роль — одно
# место: ядро запускает его по argv-контракту, значит он в реестре тулов.
for name in snens; do
    src="$BIN_SRC/$name"
    [ -f "$src" ] || die "нет $src — собери: cargo build --release --workspace"
    dst="$DESKTOP/binaries/$name-$TRIPLE"
    cp "$src" "$dst"
    say "$name → $(basename "$dst") ($(wc -c <"$dst") байт)"
done

# Тулы — ресурсы, а не sidecar'ы: их много, и запускает их ядро, а не окно.
TOOLS_DST="$DESKTOP/resources/tools"
rm -rf "$TOOLS_DST"
mkdir -p "$TOOLS_DST"
staged=0
for d in "$REPO"/tools/*/; do
    name="$(basename "$d")"
    [ -f "$d/Cargo.toml" ] || continue
    bin="$BIN_SRC/$name"
    if [ ! -f "$bin" ]; then
        say "тул $name: нет бинаря в $BIN_SRC — пропускаю"
        continue
    fi
    mkdir -p "$TOOLS_DST/$name"
    cp "$bin" "$TOOLS_DST/$name/$name"
    # Схема — часть контракта тула: без неё модель увидит generic args+stdin.
    [ -f "$d/schema.json" ] && cp "$d/schema.json" "$TOOLS_DST/$name/schema.json"
    staged=$((staged + 1))
done
say "тулов разложено: $staged"

if [ "$WITH_CLIENT" = 1 ]; then
    [ -f "$CLIENT_SRC/index.html" ] || die "нет $CLIENT_SRC/index.html — собери клиент: (cd ../snens-web && npm run build)"
    rm -rf "$DESKTOP/resources/web"
    mkdir -p "$DESKTOP/resources/web"
    cp -r "$CLIENT_SRC"/. "$DESKTOP/resources/web/"
    say "клиент → resources/web ($(du -sh "$DESKTOP/resources/web" | cut -f1))"
fi

say "готово. Дальше: cd desktop && npx tauri build"
