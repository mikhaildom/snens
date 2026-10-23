#!/bin/sh
# Проверки перед релизом: «можно релизить» или «нельзя, и вот почему».
#
# Зачем отдельный скрипт. До него проверки жили в головах: cargo test, смоук,
# тесты клиента, сайт — и каждый раз что-то забывалось. Версия уезжала вперёд
# кода, а релиз собирался с непроверенного worktree. Гейт — единственное место,
# где записано, что считается проверенным.
#
# Что проверяется (все шаги обязательны, порядок — от дешёвого к дорогому):
#   1. версия одна на всё: Cargo.toml = desktop = tauri.conf.json = web
#   2. worktree чистый: релиз из незакоммиченных правок не воспроизводим
#   3. тег v<версия> существует и указывает на HEAD
#   4. cargo test --workspace (ядро и тулы)
#   5. смоук поверхности и TUI на мок-провайдере (scripts/production-smoke.py)
#   6. snens-web: tsc + тесты + сборка бандла (если репозиторий клиента рядом)
#   7. сайт: python3 scripts/site.py --check (ссылки, плейсхолдеры, манифест)
#   8. selftest ядра: реестр тулов, контракт --help, argv/stdin
#
# Использование:
#   scripts/release-check.sh              # полный гейт
#   scripts/release-check.sh --fast       # без cargo test и смоука (5 минут)
#   scripts/release-check.sh --skip-site  # без проверки контента сайта
#   scripts/release-check.sh --list       # что будет запущено
#
# Код возврата: 0 — релизить можно; 1 — есть провал (печатается, какой именно).
set -eu

REPO="${SNENS_REPO:-$(cd "$(dirname "$0")/.." && pwd)}"
WEB="${SNENS_WEB_REPO:-$HOME/projects/snens-web}"
SITE="$REPO/site/snens.svuvi.ch"

FAST=0
SKIP_SITE=0
LIST=0
while [ $# -gt 0 ]; do
    case "$1" in
        --fast) FAST=1 ;;
        --skip-site) SKIP_SITE=1 ;;
        --list) LIST=1 ;;
        -h|--help) sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "release-check: неизвестный аргумент: $1" >&2; exit 2 ;;
    esac
    shift
done

say()  { printf '>>> %s\n' "$*"; }
ok()   { printf '  ok   %s\n' "$*"; }
bad()  { printf '  ПРОВАЛ %s\n' "$*" >&2; FAILED="${FAILED} $*"; }
have() { command -v "$1" >/dev/null 2>&1; }

# Прогон с хвостом вывода и честным кодом возврата.
#
# Почему не `cmd | tail -n`: в POSIX sh код возврата конвейера — код последней
# команды, то есть `tail`. Провал `cargo test` или `selftest` тонул: гейт видел 0
# и печатал «ok» на упавшем шаге (проверено — selftest падал с rc=101, а гейт
# писал ok). Пишем в файл, читаем код отдельно.
run_tail() {
    _log="$1"
    _lines="$2"
    shift 2
    "$@" >"$_log" 2>&1
    _rc=$?
    tail -n "$_lines" "$_log" | sed 's/^/  /'
    return $_rc
}

FAILED=""

# --- 1. версия --------------------------------------------------------------
say "1/8 версия"
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO/Cargo.toml" | head -1)"
[ -n "$VERSION" ] || bad "Cargo.toml: не нашёл version"
profile() { printf '%s\n' "$1"; }
DESK_VER="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO/desktop/Cargo.toml" | head -1)"
TAURI_VER="$(python3 -c "import json;print(json.load(open('$REPO/desktop/tauri.conf.json'))['version'])")"
PKG_VER="$(python3 -c "import json;print(json.load(open('$REPO/desktop/package.json'))['version'])")"
WEB_VER="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO/web/Cargo.toml" | head -1)"
if [ "$VERSION" = "$DESK_VER" ] && [ "$VERSION" = "$TAURI_VER" ] && [ "$VERSION" = "$PKG_VER" ] && [ "$VERSION" = "$WEB_VER" ]; then
    ok "версия $VERSION везде"
else
    bad "версии расходятся: core=$VERSION desktop=$DESK_VER tauri=$TAURI_VER pkg=$PKG_VER web=$WEB_VER"
fi

# --- 2. worktree ------------------------------------------------------------
say "2/8 worktree"
if [ -n "$(git -C "$REPO" status --porcelain)" ]; then
    git -C "$REPO" status --porcelain | sed 's/^/  /' >&2
    bad "worktree ядра грязный: закоммить или спрячь правки"
else
    ok "ядро: чисто"
fi
if [ -d "$WEB/.git" ]; then
    if [ -n "$(git -C "$WEB" status --porcelain)" ]; then
        git -C "$WEB" status --porcelain | sed 's/^/  /' >&2
        bad "worktree клиента грязный: $WEB"
    else
        ok "клиент: чисто"
    fi
else
    printf '  --   клиент не git-репозиторий (%s) — пропускаю чистоту\n' "$WEB"
fi

# --- 3. тег -----------------------------------------------------------------
say "3/8 тег"
if git -C "$REPO" rev-parse -q --verify "refs/tags/v$VERSION" >/dev/null; then
    TAG_SHA="$(git -C "$REPO" rev-list -n1 "v$VERSION")"
    HEAD_SHA="$(git -C "$REPO" rev-parse HEAD)"
    if [ "$TAG_SHA" = "$HEAD_SHA" ]; then
        ok "v$VERSION = HEAD"
    else
        bad "v$VERSION указывает на ${TAG_SHA%"${TAG_SHA#????????}"}, а HEAD ${HEAD_SHA%"${HEAD_SHA#????????}"}"
    fi
else
    bad "нет тега v$VERSION: релиз без тега не воспроизводим"
fi

if [ "$LIST" = 1 ]; then
    say "дальше: cargo test --workspace, production-smoke, клиент, сайт, selftest"
    [ "$FAST" = 1 ] && say "(с --fast шаг cargo test и смоук пропускаются)"
    exit 0
fi

# --- 4. тесты ядра и тулов --------------------------------------------------
say "4/8 cargo test --workspace"
if [ "$FAST" = 1 ]; then
    printf '  --   пропущено (--fast)\n'
elif have cargo; then
    if ( cd "$REPO" && run_tail /tmp/snens-gate-test.log 20 cargo test --workspace --quiet ); then
        ok "тесты зелёные"
    else
        bad "cargo test провалился"
    fi
else
    bad "нет cargo в PATH"
fi

# --- 5. смоук поверхности ---------------------------------------------------
say "5/8 смоук поверхности (мок-провайдер)"
if [ "$FAST" = 1 ]; then
    printf '  --   пропущено (--fast)\n'
elif have python3; then
    SNENS_BIN="${SNENS_BIN:-$(cd "$REPO" && cargo metadata --no-deps --format-version 1 \
        | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/debug/snens}"
    if [ ! -x "$SNENS_BIN" ]; then
        bad "нет $SNENS_BIN — собери: cargo build --workspace"
    elif ( SNENS_BIN="$SNENS_BIN"; export SNENS_BIN; run_tail /tmp/snens-gate-smoke.log 12 python3 "$REPO/scripts/production-smoke.py" ); then
        ok "смоук прошёл"
    else
        bad "смоук поверхности провалился"
    fi
else
    bad "нет python3"
fi

# --- 6. клиент --------------------------------------------------------------
say "6/8 клиент (snens-web)"
if [ -d "$WEB" ]; then
    if have npm; then
        if ( cd "$WEB" && npm test --silent ) >/tmp/snens-release-web-test.log 2>&1; then
            ok "тесты клиента"
        else
            tail -20 /tmp/snens-release-web-test.log | sed 's/^/  /' >&2
            bad "тесты клиента провалились"
        fi
        if ( cd "$WEB" && npm run build --silent ) >/tmp/snens-release-web-build.log 2>&1; then
            ok "бандл собран"
        else
            tail -20 /tmp/snens-release-web-build.log | sed 's/^/  /' >&2
            bad "сборка клиента провалилась"
        fi
    else
        bad "нет npm — клиент не проверить"
    fi
else
    bad "нет каталога клиента $WEB (SNENS_WEB_REPO)"
fi

# --- 7. сайт ----------------------------------------------------------------
say "7/8 сайт"
if [ "$SKIP_SITE" = 1 ]; then
    printf '  --   пропущено (--skip-site)\n'
elif have python3; then
    if python3 "$REPO/scripts/docs-tools.py" --write "$SITE/llm/tools.txt" >/dev/null 2>&1 &&
       run_tail /tmp/snens-gate-site.log 6 python3 "$REPO/scripts/site.py" --check --releases "${SNENS_RELEASES:-/var/www/snens.svuvi.ch/releases}"; then
        ok "контент сайта сходится"
    else
        bad "сайт: проверка контента провалилась"
    fi
else
    bad "нет python3"
fi

# --- 8. selftest ------------------------------------------------------------
say "8/8 selftest ядра"
if [ -x "$SNENS_BIN" ]; then
    if run_tail /tmp/snens-gate-selftest.log 4 "$SNENS_BIN" selftest; then
        ok "selftest ok"
    else
        bad "selftest провалился"
    fi
else
    bad "нет собранного ядра: cargo build --workspace"
fi

if [ -n "$FAILED" ]; then
    printf '\nРЕЛИЗ НЕЛЬЗЯ. Провалено:%s\n' "$FAILED" >&2
    exit 1
fi
printf '\nМожно релизить %s: проверки пройдены.\n' "$VERSION"
printf 'Дальше: сборка артефактов (scripts/release.sh — десктоп на Windows,\n'
printf 'веб — scripts/web-release.sh), выкладка, смоук на скачанном файле.\n'
