#!/bin/sh
# Версия продукта в одном месте: читать, проверять, поднимать.
#
# Версия живёт в пяти файлах, и это не избыточность, а требования инструментов:
# Cargo.toml (ядро и тулы), web/Cargo.toml (клиент), desktop/Cargo.toml и
# desktop/tauri.conf.json + package.json (бандл Tauri обязан нести ту же
# версию, иначе updater не увидит обновление). Руками их синхронизировать —
# источник расхождений, который виден только при сборке установщика.
#
# Источник правды — корневой Cargo.toml. Остальные файлы — производные.
#
# Использование:
#   scripts/version.sh                # напечатать версию
#   scripts/version.sh --check        # только сверить, код 1 при расхождении
#   scripts/version.sh --set 0.3.0    # поднять во всех файлах
#   scripts/version.sh --tag          # проверить, что тег v<версия> = HEAD
set -eu

REPO="${SNENS_REPO:-$(cd "$(dirname "$0")/.." && pwd)}"
CORE="$REPO/Cargo.toml"
WEB="$REPO/web/Cargo.toml"
DESK="$REPO/desktop/Cargo.toml"
TAURI="$REPO/desktop/tauri.conf.json"
PKG="$REPO/desktop/package.json"

die() { printf 'version: %s\n' "$*" >&2; exit 1; }

current() { sed -n 's/^version = "\(.*\)"$/\1/p' "$CORE" | head -1; }

read_all() {
    printf 'core=%s\n' "$(current)"
    printf 'web=%s\n' "$(sed -n 's/^version = "\(.*\)"$/\1/p' "$WEB" | head -1)"
    printf 'desktop=%s\n' "$(sed -n 's/^version = "\(.*\)"$/\1/p' "$DESK" | head -1)"
    printf 'tauri=%s\n' "$(python3 -c "import json;print(json.load(open('$TAURI'))['version'])")"
    printf 'package=%s\n' "$(python3 -c "import json;print(json.load(open('$PKG'))['version'])")"
}

check() {
    want="$(current)"
    [ -n "$want" ] || die "в $CORE нет version"
    bad=0
    read_all | while IFS='=' read -r name got; do
        [ "$got" = "$want" ] || printf '  %s: %s (ожидалось %s)\n' "$name" "$got" "$want" >&2
    done | grep . >&2 && bad=1
    # Подстановка через отдельный файл: `while` в конвейере — подшеллелл, и
    # `bad=1` из него не виден снаружи.
    counts="$(read_all | awk -F= -v want="$want" '$2 != want {print}')"
    if [ -n "$counts" ]; then
        printf 'version: расходятся:\n%s\n' "$counts" >&2
        return 1
    fi
    printf '%s\n' "$want"
}

set_version() {
    new="$1"
    printf '%s' "$new" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' ||
        die "версия должна быть X.Y.Z: $new"
    # Cargo.toml: правим только первую строку version = "..." — это версия
    # пакета, а не зависимостей (у тех `version = "1"` внутри { }).
    for f in "$CORE" "$WEB" "$DESK"; do
        [ -f "$f" ] || continue
        python3 - "$f" "$new" <<'PY'
import re, sys
path, new = sys.argv[1], sys.argv[2]
text = open(path).read()
out, done = [], False
for line in text.splitlines(keepends=True):
    if not done and re.fullmatch(r'version = "[^"]*"\n?', line):
        line = f'version = "{new}"\n' if line.endswith('\n') else f'version = "{new}"'
        done = True
    out.append(line)
if not done:
    sys.exit(f"{path}: нет строки version = ...")
open(path, "w").write("".join(out))
PY
    done
    for f in "$TAURI" "$PKG"; do
        python3 - "$f" "$new" <<'PY'
import json, sys
path, new = sys.argv[1], sys.argv[2]
data = json.load(open(path))
data["version"] = new
open(path, "w").write(json.dumps(data, ensure_ascii=False, indent=2) + "\n")
PY
    done
    # Cargo.lock тоже держит версию своего пакета: без обновления `cargo test
    # --locked` ругается, а сборка релиза переписывает его молча.
    ( cd "$REPO" && cargo update --workspace --offline >/dev/null 2>&1 ) || true
    check >/dev/null || die "после правки версии всё ещё расходятся: запусти --check"
    printf 'версия: %s\n' "$new"
    printf 'дальше: cargo build --workspace && git commit -am "release: %s" && git tag v%s\n' "$new" "$new"
}

tagcheck() {
    v="$(current)"
    if ! git -C "$REPO" rev-parse -q --verify "refs/tags/v$v" >/dev/null; then
        die "нет тега v$v"
    fi
    want="$(git -C "$REPO" rev-list -n1 "v$v")"
    head="$(git -C "$REPO" rev-parse HEAD)"
    [ "$want" = "$head" ] || die "v$v указывает не на HEAD ($(printf '%.8s' "$want") ≠ $(printf '%.8s' "$head"))"
    printf 'v%s = HEAD\n' "$v"
}

case "${1:-}" in
    "") current ;;
    --check) check ;;
    --set) [ $# -ge 2 ] || die "--set требует версию"; set_version "$2" ;;
    --tag) tagcheck ;;
    -h|--help) sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//' ;;
    *) die "неизвестный аргумент: $1" ;;
esac
