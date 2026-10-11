#!/bin/sh
# Собрать релиз ядра, поставить его immutable-релизом, переключить launcher и
# перезапустить production-сервис snens. Операторский скрипт: запускать руками.
#
# Почему не просто `systemctl restart`: юнит запускает не сборку из worktree, а
# неизменяемый релиз `~/.local/lib/snens/releases/<sha>/snens`. Без этого шага
# рестарт поднимет старый бинарник, и правки в ядре не вступят в силу.
#
# Использование:
#   scripts/prod-restart.sh            # собрать, поставить, перезапустить
#   scripts/prod-restart.sh --dry-run  # показать, что будет сделано, ничего не менять
#   scripts/prod-restart.sh --no-restart
#   scripts/prod-restart.sh --service snens-next   # другой юнит (по умолчанию snens)
set -eu

REPO="${SNENS_REPO:-$HOME/projects/snens}"
LAUNCHER="$HOME/.local/bin/snens"
RELEASES="$HOME/.local/lib/snens/releases"
SERVICE="snens"
DRY=0
RESTART=1

while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY=1 ;;
        --no-restart) RESTART=0 ;;
        --service) SERVICE="$2"; shift ;;
        --repo) REPO="$2"; shift ;;
        -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
        *) echo "неизвестный флаг: $1" >&2; exit 2 ;;
    esac
    shift
done

say() { printf '>>> %s\n' "$*"; }
die() { printf 'ОШИБКА: %s\n' "$*" >&2; exit 1; }

[ -d "$REPO" ] || die "нет репозитория $REPO"
[ -f "$REPO/Cargo.toml" ] || die "$REPO не похож на репозиторий snens"

command -v cargo >/dev/null || die "cargo не в PATH"
command -v systemctl >/dev/null || die "systemctl не в PATH"

# Старый релиз — для отката. Печатаем до любых изменений.
OLD_TARGET="$(readlink -f "$LAUNCHER" 2>/dev/null || true)"
[ -n "$OLD_TARGET" ] && say "текущий launcher → $OLD_TARGET"

if [ "$DRY" = 1 ]; then
    say "DRY-RUN: cargo build --release --bin snens в $REPO"
    say "DRY-RUN: install релиза в $RELEASES/<sha>/snens, chmod 0555"
    say "DRY-RUN: атомарно переключить $LAUNCHER"
    [ "$RESTART" = 1 ] && say "DRY-RUN: systemctl --user restart $SERVICE"
    exit 0
fi

say "сборка релиза (cargo build --release --bin snens)…"
( cd "$REPO" && cargo build --release --bin snens )

TARGET_DIR="$(cd "$REPO" && cargo metadata --no-deps --format-version 1 \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
BIN="$TARGET_DIR/release/snens"
[ -x "$BIN" ] || die "нет исполняемого $BIN"

HASH="$(sha256sum "$BIN" | cut -d' ' -f1)"
RELEASE="$RELEASES/$HASH"
say "sha256=$HASH"

if [ -f "$RELEASE/snens" ]; then
    say "этот релиз уже установлен — переустанавливаю артефакт"
    chmod 0755 "$RELEASE/snens"
fi
install -d -m 0755 "$RELEASE"
install -m 0555 "$BIN" "$RELEASE/snens"
[ "$(sha256sum "$RELEASE/snens" | cut -d' ' -f1)" = "$HASH" ] || die "артефакт не совпал по хэшу"
chmod 0555 "$RELEASE"

say "переключаю launcher $LAUNCHER → $RELEASE/snens"
ln -s "$RELEASE/snens" "$LAUNCHER.new"
mv -Tf "$LAUNCHER.new" "$LAUNCHER"
readlink -f "$LAUNCHER" | grep -qx "$RELEASE/snens" || die "launcher не переключился"

if [ "$RESTART" = 0 ]; then
    say "готово: релиз установлен, сервис НЕ перезапущен (--no-restart)"
    exit 0
fi

say "перезапуск сервиса systemctl --user restart $SERVICE"
systemctl --user restart "$SERVICE"

# Ждём готовности: auth-слой отвечает 401 без токена — этого достаточно,
# чтобы отличить «сервер поднялся» от «порт молчит».
TOKEN_FILE="$HOME/.local/share/snens/api.token"
i=0
while [ "$i" -lt 30 ]; do
    i=$((i + 1))
    CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 \
        http://127.0.0.1:7420/v1/model || true)"
    case "$CODE" in
        200|401) say "сервис отвечает (HTTP $CODE)"; break ;;
    esac
    [ "$i" -ge 30 ] && die "сервис не отвечает на 127.0.0.1:7420 после рестарта; смотри journalctl --user -u $SERVICE"
    sleep 1
done

say "статус: systemctl --user is-active $SERVICE"
systemctl --user is-active "$SERVICE" || true

if [ -n "$OLD_TARGET" ] && [ "$OLD_TARGET" != "$RELEASE/snens" ]; then
    cat <<EOF

Готово. Откат, если что-то не так:
  ln -sfn "$OLD_TARGET" "$LAUNCHER" && systemctl --user restart $SERVICE
Старый релиз сохранён на месте (releases не перезаписываются).
EOF
fi