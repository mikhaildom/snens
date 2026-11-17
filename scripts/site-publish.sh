#!/bin/sh
# site-publish.sh — собрать и выложить snens.svuvi.ch: лендинг, документация,
# plaintext-копии для агентов.
#
# Один домен, два контура (см. scripts/site.py):
#   content/*.md → /docs/*.html + /            (человек)
#   llm/*.txt    → /llm/*.txt                  (агент: те же факты, машинный формат)
# Релизы (установщик и latest.json) лежат отдельным корнем и раздаются тем же
# доменом из /releases/ — их кладёт scripts/release.sh, этот скрипт их не трогает.
#
# Использование:
#   scripts/site-publish.sh                  # собрать, проверить, выложить
#   scripts/site-publish.sh --check          # только проверить контент
#   scripts/site-publish.sh --dry-run        # собрать во временный каталог
#   SNENS_SITE_WEBROOT=/tmp/site scripts/site-publish.sh
#
# Переменные:
#   SNENS_SITE_WEBROOT  куда собирать (дефолт /var/www/snens.svuvi.ch)
#   SNENS_RELEASES      каталог с latest.json для ссылки на установщик
set -eu

REPO="${SNENS_REPO:-$(cd "$(dirname "$0")/.." && pwd)}"
WEBROOT="${SNENS_SITE_WEBROOT:-/var/www/snens.svuvi.ch}"
RELEASES="${SNENS_RELEASES:-/var/www/snens.svuvi.ch/releases}"
BASE_URL="https://snens.svuvi.ch"

CHECK=0
DRY=0
while [ $# -gt 0 ]; do
    case "$1" in
        --check) CHECK=1 ;;
        --dry-run) DRY=1 ;;
        -h | --help)
            sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "site-publish: неизвестный аргумент: $1" >&2
            exit 2
            ;;
    esac
    shift
done

die() {
    echo "site-publish: $*" >&2
    exit 1
}

command -v python3 >/dev/null 2>&1 || die "нужен python3"

# Справка тулов — генерируемая: устаревший tools.txt хуже отсутствующего.
# При --check не пишем в дерево: проверка контента не должна пачкать worktree
# (иначе `git diff --exit-code` в CI падает от самой проверки).
TOOLS_TXT="$REPO/site/snens.svuvi.ch/llm/tools.txt"
if [ "$CHECK" = 1 ] || [ "$DRY" = 1 ]; then
    TMP_TOOLS="$(mktemp)"
    trap 'rm -f "$TMP_TOOLS"' EXIT
    python3 "$REPO/scripts/docs-tools.py" --write "$TMP_TOOLS" || die "генератор tools.txt упал"
    cmp -s "$TMP_TOOLS" "$TOOLS_TXT" ||
        die "tools.txt устарел: запусти scripts/site-publish.sh (без флагов) и закоммить"
else
    python3 "$REPO/scripts/docs-tools.py" --write "$TOOLS_TXT" || die "генератор tools.txt упал"
fi

if [ "$CHECK" = 1 ]; then
    python3 "$REPO/scripts/site.py" --check --releases "$RELEASES"
    echo "проверка пройдена: $REPO/site/snens.svuvi.ch"
    exit 0
fi

if [ "$DRY" = 1 ]; then
    OUT=$(mktemp -d)
    # Без trap: каталог обещан в сообщении как «где посмотреть результат» —
    # удалять его сразу после печати пути бессмысленно.
    python3 "$REPO/scripts/site.py" --out "$OUT" --releases "$RELEASES"
    echo "dry-run: собрано в $OUT (ничего не выложено, каталог не удаляю)"
    exit 0
fi

[ -d "$WEBROOT" ] || mkdir -p "$WEBROOT" || die "не могу создать $WEBROOT (нужны права на запись)"
[ -w "$WEBROOT" ] || die "$WEBROOT не доступен на запись"

# Собираем в отдельный каталог и подменяем содержимое только целиком: провал
# сборки на середине не должен оставить сайт из смеси старых и новых страниц.
# `releases/` — чужой корень (его кладёт release.sh) и в подмену не входит.
STAGE="$WEBROOT/.site-new"
rm -rf "$STAGE"
python3 "$REPO/scripts/site.py" --out "$STAGE" --releases "$RELEASES" ||
    { rm -rf "$STAGE"; die "сборка сайта провалилась"; }

# Caddy читает не нашим пользователем, а umask бывает 077: без этого сайт
# отдаёт 403, и причина не видна ни из вывода сборки, ни из `ls` файлов.
# Права ставим до подмены и только на своё: `find` по всему $WEBROOT задевал
# бы `releases/` (его права — дело release.sh).
find "$STAGE" -type d -exec chmod 0755 {} +
find "$STAGE" -type f -exec chmod 0644 {} +

for item in "$STAGE"/*; do
    name="$(basename "$item")"
    [ "$name" = releases ] && continue
    rm -rf "$WEBROOT/$name"
    mv "$item" "$WEBROOT/$name"
done
rm -rf "$STAGE"

echo
echo "проверка снаружи: curl -sI --max-time 8 $BASE_URL/ | head -1"
echo "                   curl -s  --max-time 8 $BASE_URL/llm/index.txt | head -3"
