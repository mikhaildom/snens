#!/bin/sh
# Контракты тула desk: коды возврата, --help, схема, отказ без машины.
# Живые проверки (окна, дерево, действия) требуют машину пользователя и ssh,
# поэтому в общий прогон не входят: они запускаются вручную с --tunnel.
# Запуск из корня репо: sh tools/desk/selftest.sh
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fails=0
ok() { echo "ok: $1"; }
bad() { echo "FAIL: $1"; fails=$((fails + 1)); }
want() { [ "$1" = "$2" ] && ok "$3" || bad "$3 (ожидалось '$1', получено '$2')"; }
has() {
  case "$2" in
    *"$1"*) ok "$3" ;;
    *) bad "$3 (нет '$1' в '$2')" ;;
  esac
}

BIN=$HERE/desk
[ -x "$BIN" ] || { echo "FAIL: нет бинарника $BIN — собери: snens build desk"; exit 1; }
[ -f "$HERE/schema.json" ] && ok "desk: schema.json рядом с бинарником" || bad "desk: нет schema.json"
has '"x-snens"' "$(cat "$HERE/schema.json")" "desk: в схеме есть ключ x-snens"
has '"command"' "$(cat "$HERE/schema.json")" "desk: схема объявляет command"
has '"tunnel"' "$(cat "$HERE/schema.json")" "desk: схема объявляет tunnel"

t() {
  rc=0
  out=$("$BIN" "$@" 2>"$TMP/err") || rc=$?
  err=$(cat "$TMP/err")
}

t --help
want 0 "$rc" "desk --help: код 0"
first=$(printf '%s\n' "$out" | sed -n 1p)
second=$(printf '%s\n' "$out" | sed -n 2p)
has "desk —" "$first" "desk --help: строка 1 объясняет суть"
has "desk " "$second" "desk --help: строка 2 — usage"

t
want 0 "$rc" "desk без аргументов: код 0 и справка"
has "desk —" "$out" "desk без аргументов печатает справку"

t nosuchcommand
want 2 "$rc" "неизвестная команда: код 2"

# Без --tunnel тул обязан сказать, чего не хватает, а не лезть в сеть.
SNENS_DESK_STATE=$TMP/state
export SNENS_DESK_STATE
unset SNENS_DESK_HOST 2>/dev/null || true
t apps
want 1 "$rc" "apps без --tunnel: код 1"
has "tunnel" "$err" "apps без --tunnel: подсказывает --tunnel"

t act --tunnel nosuchhost.invalid
want 1 "$rc" "act без --json: код 1"
has "json" "$err" "act без --json: подсказывает про json"

t act --tunnel nosuchhost.invalid --json 'не json'
want 1 "$rc" "act с битым JSON: код 1"
has "не JSON" "$err" "act с битым JSON: сказано, что не JSON"

t act --tunnel nosuchhost.invalid --json '{"click":"e1"}'
want 1 "$rc" "act с объектом вместо массива: код 1"
has "массив" "$err" "act с объектом: подсказывает про массив"

# Агент на недоступной машине: ошибка ssh, а не зависание.
t status --tunnel nosuchhost.invalid
want 0 "$rc" "status на недоступной машине: код 0, но пишет «недоступен»"
has "недоступен" "$out" "status: честно сказал, что агент недоступен"

if [ "$fails" -eq 0 ]; then
  echo "все проверки desk прошли"
else
  echo "провалов: $fails"
  exit 1
fi
