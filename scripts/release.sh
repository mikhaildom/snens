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
#   scripts/release.sh --check          # проверить окружение, ничего не собирать
set -eu

REPO="$(cd "$(dirname "$0")/.." && pwd)"
DESKTOP="$REPO/desktop"
OUT="${SNENS_RELEASE_DIR:-$REPO/desktop/target/release/bundle}"
BASE_URL="${SNENS_RELEASE_URL:-https://releases.svuvi.ch/snens}"
VERSION="$(python3 -c "import json;print(json.load(open('$DESKTOP/tauri.conf.json'))['version'])")"

say() { printf '>>> %s\n' "$*"; }
die() { printf 'ОШИБКА: %s\n' "$*" >&2; exit 1; }

check_only=0
[ "${1:-}" = "--check" ] && check_only=1

[ -n "${TAURI_SIGNING_PRIVATE_KEY_PATH:-}${TAURI_SIGNING_PRIVATE_KEY:-}" ] ||
    die "нет ключа подписи: задай TAURI_SIGNING_PRIVATE_KEY_PATH (updater.key)"

if [ "$check_only" = 1 ]; then
    say "версия: $VERSION"
    say "каталог бандлов: $OUT"
    say "адрес манифеста: $BASE_URL/latest.json"
    say "ключ: задан"
    command -v npx >/dev/null || die "нет npx (нужен @tauri-apps/cli)"
    exit 0
fi

say "1/3 раскладка sidecar'ов и клиента"
"$REPO/scripts/desktop-stage.sh"

say "2/3 сборка бандлов (nsis/appimage/deb) с подписью"
cd "$DESKTOP"
npx tauri build

say "3/3 манифест latest.json"
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
    for path in sorted(root.glob(pattern)):
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
        break

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
say "  rsync -av --mkpath $OUT/latest.json $OUT/$VERSION/ <host>:/var/www/releases/snens/"
