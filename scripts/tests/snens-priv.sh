#!/bin/sh
# Тест брокера привилегий: валидация без выполнения (`SNENS_PRIV_DRYRUN=1`).
#
# Зачем. У `snens-priv` вся работа — в отказах: пустой конфиг, путь вне корня,
# `..`, симлинк из-под корня, пустое имя юнита. Проверять это руками перед
# каждой правкой — единственный способ поймать регресс, а рук не хватает:
# регресс здесь означает root-код-исполнение.
#
# Использование: sh scripts/tests/snens-priv.sh
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
PRIV="$HERE/../snens-priv"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fails=0
ok()  { printf 'ok: %s\n' "$*"; }
bad() { printf 'FAIL: %s\n' "$*"; fails=$((fails + 1)); }

# run <ожидаемый код> <описание> -- <аргументы…>
run() {
    want="$1"; desc="$2"; shift 3
    set +e
    SNENS_PRIV_CONF="$CONF" SNENS_PRIV_DRYRUN=1 sh "$PRIV" "$@" >"$TMP/out" 2>"$TMP/err"
    rc=$?
    set -e
    if [ "$rc" = "$want" ]; then
        ok "$desc"
    else
        bad "$desc: код $rc, ожидался $want ($(cat "$TMP/err"))"
    fi
}

# --- 1. пустой конфиг: отказ во всём ---
CONF="$TMP/empty.conf"
printf '# только комментарии\nunits = ""\n' >"$CONF"
run 2 "пустой конфиг: systemctl отказ" - systemctl restart caddy
run 2 "пустой конфиг: mkdir отказ" - mkdir -p /var/www/x

# --- 2. нормальный конфиг ---
CONF="$TMP/ok.conf"
mkdir -p "$TMP/root/sub"
printf 'units = "caddy my-app"  # хвостовой комментарий\nroots = "%s"\nowner = "deploy:deploy"\n' "$TMP/root" >"$CONF"
run 0 "юнит из списка" - systemctl restart caddy
run 0 "юнит с суффиксом .service" - systemctl status my-app.service
run 2 "юнит вне списка" - systemctl restart nginx
run 2 "пустое имя юнита" - systemctl restart ""
run 2 "голый суффикс .service" - systemctl restart .service
run 2 "недопустимый глагол" - systemctl enable caddy
run 2 "лишний аргумент" - systemctl restart caddy extra

run 0 "mkdir внутри корня" - mkdir -p "$TMP/root/sub/x"
run 2 "mkdir выше корня" - mkdir -p "$TMP/root/../etc/x"
run 2 "mkdir вне корня" - mkdir -p /etc/x
run 2 "mkdir относительный путь" - mkdir -p var/www/x
run 2 "mkdir без -p" - mkdir "$TMP/root/new"

run 0 "chown внутри корня" - chown deploy:deploy "$TMP/root/sub"
run 2 "chown другой владелец" - chown root:root "$TMP/root/sub"
run 2 "chown вне корня" - chown deploy:deploy /etc/passwd

# --- 3. симлинк из-под корня: текстовая проверка `..` его не ловит ---
ln -sfn /etc "$TMP/root/link"
run 2 "mkdir через симлинк наружу" - mkdir -p "$TMP/root/link/x"

# --- 4. чужой/перезаписываемый конфиг ---
CONF="$TMP/open.conf"
printf 'roots = "%s"\n' "$TMP/root" >"$CONF"
chmod 0666 "$CONF"
run 2 "конфиг, открытый на запись всем" - mkdir -p "$TMP/root/x"

echo
if [ "$fails" -ne 0 ]; then
    printf 'провалено проверок: %s\n' "$fails"
    exit 1
fi
printf 'брокер привилегий: все проверки прошли\n'
