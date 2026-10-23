#!/bin/sh
# Собрать и выложить релиз веб-клиента snens-web.
#
# Клиент с момента перехода на бинарь — не «dist, который отдаёт ядро», а
# отдельный исполняемый файл: `snens-web` + каталог `web/` рядом с ним. Один
# архив, распаковал — работает; в юните systemd вместо `npx vite` стоит он.
#
# Что делает:
#   1. зовёт release-check.sh (--skip-site: сайт проверяется отдельно)
#   2. собирает бандл клиента (npm run build в репозитории snens-web)
#   3. собирает бинарь (cargo build --release -p snens-web)
#   4. пакует tar.gz: snens-web, web/, snens-web.service, README
#   5. кладёт в /var/www/<домен>/releases/web/<версия>/ и печатает контрольную сумму
#
# Использование:
#   scripts/web-release.sh --check      # только гейт, ничего не собирать
#   scripts/web-release.sh              # собрать и выложить
#   scripts/web-release.sh --no-publish # собрать архив, никуда не класть
#
# Переменные:
#   SNENS_WEB_REPO      репозиторий клиента (дефолт ~/projects/snens-web)
#   SNENS_RELEASE_WEBROOT  корень сайта (дефолт /var/www/snens.svuvi.ch)
set -eu

REPO="${SNENS_REPO:-$(cd "$(dirname "$0")/.." && pwd)}"
WEB="${SNENS_WEB_REPO:-$HOME/projects/snens-web}"
WEBROOT="${SNENS_RELEASE_WEBROOT:-/var/www/snens.svuvi.ch}"
OUT="${SNENS_WEB_RELEASE_DIR:-$REPO/target/web-release}"

CHECK=0
PUBLISH=1
while [ $# -gt 0 ]; do
    case "$1" in
        --check) CHECK=1 ;;
        --no-publish) PUBLISH=0 ;;
        -h|--help) sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "web-release: неизвестный аргумент: $1" >&2; exit 2 ;;
    esac
    shift
done

say() { printf '>>> %s\n' "$*"; }
die() { printf 'ОШИБКА: %s\n' "$*" >&2; exit 1; }

VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO/web/Cargo.toml" | head -1)"
[ -n "$VERSION" ] || die "не нашёл версию в web/Cargo.toml"

say "0/5 гейт (release-check.sh --skip-site)"
if ! "$REPO/scripts/release-check.sh" --skip-site; then
    die "гейт не пройден — релиз не собираю"
fi
[ "$CHECK" = 1 ] && { say "--check: останавливаюсь после гейта"; exit 0; }

say "1/5 бандл клиента"
[ -d "$WEB" ] || die "нет репозитория клиента: $WEB"
( cd "$WEB" && npm run build ) || die "сборка клиента провалилась"

say "2/5 бинарь snens-web"
( cd "$REPO" && cargo build --release -p snens-web ) || die "cargo build провалился"
TARGET="$(cd "$REPO" && cargo metadata --no-deps --format-version 1 \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
BIN="$TARGET/release/snens-web"
[ -x "$BIN" ] || die "нет $BIN"

say "3/5 упаковка"
STAGE="$OUT/$VERSION"
rm -rf "$STAGE"
mkdir -p "$STAGE/web"
cp "$BIN" "$STAGE/snens-web"
cp -r "$WEB/dist/." "$STAGE/web/"
cp "$REPO/deploy/snens-web.service" "$STAGE/"
cat > "$STAGE/README.txt" <<'EOF'
snens-web — веб-клиент агента snens одним исполняемым файлом.

Запуск:
  ./snens-web --listen 127.0.0.1:5180 --api http://127.0.0.1:7420 \
              --token-file ~/.local/share/snens/api.token

Каталог web/ должен лежать рядом с бинарём (он ищется по умолчанию) — это
собранный клиент. Прокси /api нужен затем, чтобы браузер не знал токен
инстанса; без него клиент подключается сам по коду пейринга (--no-proxy).

Как сервис: cp snens-web.service ~/.config/systemd/user/ && systemctl --user
enable --now snens-web.service (пути в юните — %h, поправь, если ставишь не в
~/.local/bin).

Обновление: распакуй новый архив поверх (cat — процесс переживёт замену файла
только после рестарта) и перезапусти юнит.
EOF

ARCHIVE="snens-web_${VERSION}_linux-x86_64.tar.gz"
( cd "$STAGE" && tar czf "../../$ARCHIVE" . ) || die "tar провалился"
SHA="$(sha256sum "$OUT/$ARCHIVE" | cut -d' ' -f1)"
printf '%s  %s\n' "$SHA" "$ARCHIVE" > "$OUT/$ARCHIVE.sha256"
say "архив: $OUT/$ARCHIVE ($(wc -c <"$OUT/$ARCHIVE") байт)"
say "sha256: $SHA"

say "4/5 выкладка"
if [ "$PUBLISH" = 0 ]; then
    say "--no-publish: архив остался в $OUT"
    exit 0
fi
DEST="$WEBROOT/releases/web/$VERSION"
[ -d "$WEBROOT" ] || die "нет $WEBROOT"
mkdir -p "$DEST"
# Каталог версии неизменяем: люди со старой версией могут её ещё качать.
cp "$OUT/$ARCHIVE" "$OUT/$ARCHIVE.sha256" "$DEST/"
# Caddy читает не нашим пользователем, а umask бывает 077.
find "$DEST" -type d -exec chmod 0755 {} +
find "$DEST" -type f -exec chmod 0644 {} +
say "выложено: $DEST"

say "5/5 проверка ссылки"
URL="https://snens.svuvi.ch/releases/web/$VERSION/$ARCHIVE"
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 "$URL" || true)"
[ "$CODE" = 200 ] || die "файл не отдаётся ($CODE): $URL"
say "скачивается: $URL"
say "готово. Проверь: скачай архив, распакуй в пустой каталог, запусти --help"
