#!/bin/sh
# Собрать релиз десктоп-приложения и подготовить то, что нужно для обновления.
#
# Одно обновление на всё приложение: установщик несёт ядро, тулы, плагин памяти и
# клиент. Поэтому «обновить» = скачать новый `-setup.exe` и переустановить его
# поверх — отдельного канала для ядра нет и не нужно (см. desktop/README.md).
#
# Что делает скрипт:
#   1. раскладывает sidecar'ы и клиент (scripts/desktop-stage.sh)
#   2. собирает бандлы с подписью updater'а (`createUpdaterArtifacts`)
#   3. генерирует `latest.json` — манифест, который читает tauri-plugin-updater
#
# Подпись обязательна: без ключа приложение откажется устанавливать обновление
# (`pubkey` в tauri.conf.json). Ключ живёт только на машине релиза, в git его нет.
#
# Использование:
#   TAURI_SIGNING_PRIVATE_KEY_PATH=~/.config/snens/updater.key scripts/release.sh
#   scripts/release.sh --check               # проверить окружение, ничего не собирать
#   scripts/release.sh --skip-gate           # собрать без гейта (только для отладки)
#
# Гейт — `scripts/release-check.sh --skip-site`: тесты, смоук, версия, чистый
# worktree, тег. Без него релиз не собирается: артефакт с непроверенного дерева
# уже один раз уехал людям (версия 0.2.4 была собрана с правок, проверенных
# только на двух машинах).
set -eu

REPO="$(cd "$(dirname "$0")/.." && pwd)"
DESKTOP="$REPO/desktop"
OUT="${SNENS_RELEASE_DIR:-$REPO/desktop/target/release/bundle}"
BASE_URL="${SNENS_RELEASE_URL:-https://snens.svuvi.ch/releases}"
VERSION="$(python3 -c "import json;print(json.load(open('$DESKTOP/tauri.conf.json'))['version'])")"

say() { printf '>>> %s\n' "$*"; }
die() { printf 'ОШИБКА: %s\n' "$*" >&2; exit 1; }

check_only=0
skip_gate=0
while [ $# -gt 0 ]; do
    case "$1" in
        --check) check_only=1 ;;
        --skip-gate) skip_gate=1 ;;
        -h|--help) sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) printf 'release: неизвестный аргумент: %s\n' "$1" >&2; exit 2 ;;
    esac
    shift
done

[ -n "${TAURI_SIGNING_PRIVATE_KEY_PATH:-}${TAURI_SIGNING_PRIVATE_KEY:-}" ] ||
    die "нет ключа подписи: задай TAURI_SIGNING_PRIVATE_KEY_PATH (updater.key)"

if [ "$check_only" = 0 ] && [ "$skip_gate" = 0 ]; then
    say "0/4 гейт: scripts/release-check.sh --skip-site"
    "$REPO/scripts/release-check.sh" --skip-site ||
        die "гейт не пройден — релиз не собираю (--skip-gate только для отладки)"
fi

if [ "$check_only" = 1 ]; then
    say "версия: $VERSION"
    say "каталог бандлов: $OUT"
    say "адрес манифеста: $BASE_URL/latest.json"
    say "ключ: задан"
    command -v npx >/dev/null || die "нет npx (нужен @tauri-apps/cli)"
    exit 0
fi

say "1/5 раскладка sidecar'ов и клиента"
"$REPO/scripts/desktop-stage.sh"

say "2/5 сборка бандлов (nsis/appimage/deb) с подписью"
cd "$DESKTOP"
npx tauri build

say "3/5 манифест latest.json"
python3 - "$OUT" "$VERSION" "$BASE_URL" <<'PY'
import json, sys
from pathlib import Path

out, version, base = sys.argv[1], sys.argv[2], sys.argv[3]
root = Path(out)
platforms = {}

# Что именно прикладывает Tauri: подписанные артефакты идут рядом с бандлами.
# Имена стабильны для каждого формата, поэтому ищем по шаблону, а не угадываем.
patterns = {
    "windows-x86_64-nsis": "nsis/*-setup.exe",
    "linux-x86_64-appimage": "appimage/*.AppImage",
}
for key, pattern in patterns.items():
    # Только артефакт НЫНЕШНЕЙ версии: `target/release/bundle` не чистится между
    # релизами, и первый по алфавиту файл часто оказывается прошлогодним —
    # манифест тогда рекламирует новую версию, а отдаёт старый билд.
    matches = [p for p in sorted(root.glob(pattern)) if version in p.name]
    if not matches:
        print(f"  {key}: нет артефакта версии {version} ({pattern}) — пропускаю")
        continue
    if len(matches) > 1:
        names = ", ".join(p.name for p in matches)
        sys.exit(f"{key}: под {pattern} подходит несколько артефактов этой версии: {names} — почисти каталог бандлов")
    path = matches[0]
    sig = path.with_name(path.name + ".sig")
    if not sig.is_file():
        print(f"  {key}: нет подписи {sig.name} — пропускаю")
        continue
    platforms[key] = {
        "url": f"{base}/{version}/{path.name}",
        "signature": sig.read_text().strip(),
        "size": path.stat().st_size,
    }
    print(f"  {key}: {path.name} ({path.stat().st_size} байт)")

if not platforms:
    sys.exit("нет ни одного подписанного артефакта: обновление собрать не из чего")

manifest = {
    "version": version,
    "pub_date": __import__("datetime").datetime.now(
        __import__("datetime").timezone.utc
    ).strftime("%Y-%m-%dT%H:%M:%SZ"),
    # Заметки читает человек перед обновлением: пишем то, что важно ему.
    "notes": "Обновление snens: ядро, инструменты, память и клиент одним пакетом.",
    "platforms": platforms,
}
dest = root / "latest.json"
dest.write_text(json.dumps(manifest, ensure_ascii=False, indent=2))
print(f"  манифест: {dest}")
PY

say "готово. Выложи на $BASE_URL: latest.json и каталог $VERSION/ с артефактами"
# Каталог версии рядом с бандлами. `tauri build` кладёт артефакты в `nsis/` и
# `appimage/`, а не в `<версия>/`, и раньше всё ниже (выкладка и печатаемый
# rsync) ходило по несуществующему пути: копировалось ноль файлов, а манифест
# обещал URL, которые отдают 404. Собираем здесь ровно то, что попало в
# манифест; фильтр по версии обязателен — в каталоге бандлов лежат артефакты
# прошлых релизов (`tauri` его не чистит).
rm -rf "$OUT/$VERSION"
mkdir -p "$OUT/$VERSION"
for f in "$OUT"/nsis/*-setup.exe "$OUT"/nsis/*-setup.exe.sig \
         "$OUT"/appimage/*.AppImage "$OUT"/appimage/*.AppImage.sig; do
    [ -f "$f" ] || continue
    case "$(basename "$f")" in
        *"$VERSION"*) cp -p "$f" "$OUT/$VERSION/" ;;
    esac
done
[ -n "$(ls -A "$OUT/$VERSION")" ] ||
    die "в $OUT нет артефактов версии $VERSION (nsis/appimage) — публиковать нечего"
# Путь назначения — тот же корень, что раздаёт Caddy (deploy/caddy-sites.conf):
# сайт берёт `/releases/*` из каталога релизов, один домен на всё.
WEBROOT="${SNENS_RELEASE_WEBROOT:-/var/www/snens.svuvi.ch}"
SUBDIR="$(printf '%s' "$BASE_URL" | sed -n 's#^[a-z]*://[^/]*/*\(.*\)$#\1#p')"
[ -n "$SUBDIR" ] || SUBDIR="releases"
DEST="$WEBROOT/$SUBDIR"

# Выкладка. Два пути, потому что релиз собирается на Windows, а раздаёт сервер:
#  - каталог назначения доступен на запись (сервер или общий том) — копируем;
#  - задан SNENS_RELEASE_HOST — печатаем готовый rsync, который человек
#    запустит (на Windows rsync есть в Git Bash, а ключ и доступ — его дело).
if [ "${SNENS_RELEASE_PUBLISH:-0}" = 1 ] && [ -d "$WEBROOT" ] && [ -w "$WEBROOT" ]; then
    say "4/5 выкладка в $DEST"
    mkdir -p "$DEST"
    # Каталог версии неизменяем: обновление кладёт новый, а не перезаписывает старый
    # (люди с прошлой версией могут ещё качать её артефакт).
    mkdir -p "$DEST/$VERSION"
    for f in "$OUT"/"$VERSION"/*; do
        [ -f "$f" ] && cp -p "$f" "$DEST/$VERSION/"
    done
    cp -p "$OUT/latest.json" "$DEST/latest.json"
    # Caddy читает не нашим пользователем, а umask бывает 077.
    # Каталоги — 0755, файлы — 0644. Раньше здесь был только `chmod -R a+rX`,
    # а umask 077 делал новые каталоги 0700: Caddy отдавал 403 на установщик,
    # а причина не видна ни в логе сборки, ни в `ls` файлов. Проверено живьём.
    find "$DEST" -type d -exec chmod 0755 {} +
    find "$DEST" -type f -exec chmod 0644 {} +
    say "выложено: $DEST"
    ls -l "$DEST/latest.json" "$DEST/$VERSION" | sed 's/^/  /'
else
    say "выложить руками:"
    say "  rsync -av --mkpath \"$OUT/latest.json\" \"$OUT/$VERSION/\" \"${SNENS_RELEASE_HOST:-<host>}:$DEST/\""
    [ "$WEBROOT" = /var/www/snens.svuvi.ch ] && [ -d "$WEBROOT" ] && [ "${SNENS_RELEASE_PUBLISH:-0}" != 1 ] &&
        say "  (каталог доступен локально: тот же скрипт с SNENS_RELEASE_PUBLISH=1 скопирует сам)"
fi
