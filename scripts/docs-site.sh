#!/bin/sh
# docs-site.sh — собрать и выложить сайт документации для агентов (docs.svuvi.ch).
#
# Первичный источник документации о snens для агентов: plaintext-файлы, которые
# агент читает fetch'ем или curl'ом. Контент лежит в репозитории
# (site/docs.svuvi.ch/), здесь только проверка, генерация tools.txt и копирование
# в корень статики, который раздаёт Caddy. Caddyfile правит человек: этот скрипт
# в /etc не ходит, sudo не зовёт и сервис не перезапускает.
#
# Использование:
#   scripts/docs-site.sh                     # собрать tools.txt, проверить, выложить
#   scripts/docs-site.sh --check             # только проверить (ничего не копировать)
#   scripts/docs-site.sh --no-gen            # не перегенерировать tools.txt
#   scripts/docs-site.sh --dry-run           # показать план, ничего не писать
#   SNENS_DOCS_WEBROOT=/tmp/docs-test scripts/docs-site.sh
#   SNENS_DOCS_URL=https://example.org scripts/docs-site.sh --check
#
# Переменные:
#   SNENS_DOCS_WEBROOT  куда копировать (дефолт /var/www/docs.svuvi.ch)
#   SNENS_DOCS_URL      базовый URL, попадает в вывод (дефолт https://docs.svuvi.ch)
set -eu

REPO="${SNENS_REPO:-$(cd "$(dirname "$0")/.." && pwd)}"
SRC="$REPO/site/docs.svuvi.ch"
WEBROOT="${SNENS_DOCS_WEBROOT:-/var/www/docs.svuvi.ch}"
BASE_URL="${SNENS_DOCS_URL:-https://docs.svuvi.ch}"

CHECK=0
GEN=1
DRY=0
while [ $# -gt 0 ]; do
    case "$1" in
        --check) CHECK=1 ;;
        --no-gen) GEN=0 ;;
        --dry-run) DRY=1 ;;
        -h | --help)
            sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "docs-site: неизвестный аргумент: $1" >&2
            exit 2
            ;;
    esac
    shift
done

die() {
    echo "docs-site: $*" >&2
    exit 1
}

[ -d "$SRC" ] || die "нет каталога контента: $SRC"

# Список — по порядку чтения: точка входа первой. Файл, забытый здесь, не уедет
# на сайт, поэтому список короткий и проверяется ниже по index.txt.
FILES="index.txt quickstart.txt tools.txt plugins.txt practices.txt"

# --- 1. tools.txt генерируется из исходников, а не пишется руками ------------
GEN_PY="$REPO/scripts/docs-tools.py"
if [ -f "$GEN_PY" ]; then
    command -v python3 >/dev/null 2>&1 || die "нужен python3 для генерации tools.txt"
    if [ "$GEN" = 1 ]; then
        if [ "$DRY" = 1 ]; then
            echo "сгенерировал бы tools.txt: python3 $GEN_PY --write $SRC/tools.txt"
        else
            python3 "$GEN_PY" --write "$SRC/tools.txt" ||
                die "генератор tools.txt упал"
        fi
    else
        # --no-gen не отменяет проверку: устаревший tools.txt хуже отсутствующего.
        if [ "$DRY" = 0 ]; then
            tmp=$(mktemp)
            trap 'rm -f "$tmp"' EXIT
            python3 "$GEN_PY" >"$tmp" || die "генератор tools.txt упал"
            if ! cmp -s "$tmp" "$SRC/tools.txt"; then
                die "tools.txt устарел относительно исходников: перезапусти без --no-gen"
            fi
        fi
    fi
else
    echo "docs-site: предупреждение: нет $GEN_PY — tools.txt не перегенерирован" >&2
fi

# --- 2. Проверка содержимого ------------------------------------------------
fail=0
for f in $FILES; do
    p="$SRC/$f"
    if [ ! -f "$p" ]; then
        echo "НЕТ     $f" >&2
        fail=1
        continue
    fi
    size=$(wc -c <"$p" | tr -d ' ')
    if [ "$size" -lt 200 ]; then
        echo "МАЛО    $f ($size Б) — файл пустой или обрезан" >&2
        fail=1
        continue
    fi
    # Plaintext по замыслу: ни NUL, ни html-обвязки, ни скриптов.
    if LC_ALL=C tr -d '\0' <"$p" | cmp -s - "$p"; then :; else
        echo "БИНАРЬ  $f — содержит NUL-байты" >&2
        fail=1
    fi
    if grep -qiE '<(!doctype|html|script)' "$p"; then
        echo "ВЁРСТКА $f — похоже на html, а сайт должен быть plaintext" >&2
        fail=1
    fi
    # index.txt — точка входа: в ней обязан быть полный URL каждого файла.
    if [ "$f" != index.txt ] && ! grep -qF "$BASE_URL/$f" "$SRC/index.txt"; then
        echo "НЕТ URL $f не упомянут в index.txt как $BASE_URL/$f" >&2
        fail=1
    fi
done

# Лишние файлы в каталоге контента тоже уезжают — скажем о них, но не валим:
# рядом может лежать временный мусор редактора, и это не повод не выкладывать.
for p in "$SRC"/*; do
    [ -f "$p" ] || continue
    n=$(basename "$p")
    case " $FILES " in
        *" $n "*) ;;
        *) echo "ВНИМАНИЕ $n есть в каталоге, но не в списке FILES — на сайт не попадёт" >&2 ;;
    esac
done

[ "$fail" = 0 ] || die "контент не прошёл проверку (см. выше)"

# --- 3. Выкладка ------------------------------------------------------------
if [ "$CHECK" = 1 ]; then
    echo "проверка пройдена: $SRC"
    exit 0
fi

if [ "$DRY" = 1 ]; then
    echo "скопировал бы в $WEBROOT:"
else
    mkdir -p "$WEBROOT" || die "не могу создать $WEBROOT (нужны права на запись)"
fi

echo "контент: $SRC"
echo "корень:  $WEBROOT"
echo
# Шапка без %-16s: dash считает ширину в байтах, кириллица ломает выравнивание.
echo "файл             байт  URL"
total=0
for f in $FILES; do
    size=$(wc -c <"$SRC/$f" | tr -d ' ')
    total=$((total + size))
    if [ "$DRY" = 0 ]; then
        # Копия через temp+mv: читатель не увидит полуфайл во время выкладки.
        cp "$SRC/$f" "$WEBROOT/.$f.tmp" && mv "$WEBROOT/.$f.tmp" "$WEBROOT/$f" ||
            die "не могу записать $WEBROOT/$f"
        # Caddy читает не нашим пользователем, а umask бывает 077 (файлы 0600):
        # без этого сайт отдаёт 403, и причина не видна из вывода скрипта.
        chmod 0644 "$WEBROOT/$f" || die "не могу выставить права на $WEBROOT/$f"
    fi
    printf '%-16s %8s  %s/%s\n' "$f" "$size" "$BASE_URL" "$f"
done
printf '%-16s %8s\n' "итого" "$total"

if [ "$DRY" = 1 ]; then
    echo
    echo "dry-run: ничего не записано"
else
    echo
    echo "выложено файлов: 5; проверка снаружи: curl -sI --max-time 8 $BASE_URL/index.txt"
fi
