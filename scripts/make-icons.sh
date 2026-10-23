#!/bin/sh
# Пересобрать иконки приложения из исходника.
#
# Логотип — знак из приветственного экрана (`src/feed/Feed.tsx`): жирная `s` на
# чёрном поле. Раньше иконка была пустым чёрным квадратом (так её однажды
# сгенерировал `tauri icon` из чего-то пустого), и в трее/на панели задач
# приложение выглядело безымянным.
#
# Источник — `desktop/icons/icon-source.html` (SVG в HTML): один файл, из него
# рендерятся все размеры. Рендерим headless-хромом, потому что ImageMagick,
# rsvg и Pillow на машине релиза не гарантированы, а хром уже нужен `shot`.
#
# Использование:
#   scripts/make-icons.sh            # пересобрать icons/icon.png и icon.ico
#   scripts/make-icons.sh --check    # только проверить, что инструменты есть
set -eu

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SIZES="16 32 48 64 128 256"
SRC="$REPO/desktop/icons/icon-source.html"
OUT="$REPO/desktop/icons"

say() { printf '>>> %s\n' "$*"; }
die() { printf 'ОШИБКА: %s\n' "$*" >&2; exit 1; }

# Где chrome: свой выбор, потом playwright, потом PATH.
pick_chrome() {
    for c in "${SNENS_CHROME:-}" \
        "$HOME"/.cache/ms-playwright/chromium_headless_shell-*/*/chrome-headless-shell \
        "$HOME"/.cache/ms-playwright/chromium-*/chrome-linux64/chrome \
        /usr/bin/chromium /usr/bin/google-chrome; do
        [ -n "$c" ] && [ -x "$c" ] && { printf '%s' "$c"; return 0; }
    done
    return 1
}

CHROME="$(pick_chrome)" || die "не нашёл chrome (задай SNENS_CHROME)"
[ -f "$SRC" ] || die "нет исходника $SRC"

if [ "${1:-}" = "--check" ]; then
    say "chrome: $CHROME"
    say "исходник: $SRC"
    exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

for s in $SIZES; do
    "$CHROME" --headless --disable-gpu --no-sandbox --hide-scrollbars \
        --screenshot="$TMP/$s.png" --window-size="$s,$s" "file://$SRC" >/dev/null 2>&1
    [ -s "$TMP/$s.png" ] || die "кадр $s не отрисовался"
    say "кадр $s"
done

# Большой PNG — иконка Linux/трея; .ico — Windows-инсталлятор и панель задач.
cp "$TMP/256.png" "$OUT/icon.png"
python3 "$REPO/scripts/make-ico.py" "$TMP"/16.png "$TMP"/32.png "$TMP"/48.png \
    "$TMP"/64.png "$TMP"/128.png "$TMP"/256.png "$OUT/icon.ico"
chmod 0644 "$OUT/icon.png" "$OUT/icon.ico"
say "готово: $(wc -c <"$OUT/icon.png") Б png, $(wc -c <"$OUT/icon.ico") Б ico"
