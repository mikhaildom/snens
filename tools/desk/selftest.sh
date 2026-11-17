#!/bin/sh
# Контракты тула desk: коды возврата, --help, схема, отказ без машины.
# Живые проверки (окна, дерево, действия) требуют машину пользователя и ssh:
# они идут только при явном `--tunnel user@machine`, иначе пропускаются с
# громкой строкой (а не молча: «все проверки прошли» без живых — неправда).
# Запуск из корня репо: sh tools/desk/selftest.sh [--tunnel user@machine]
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

LIVE_HOST=""
while [ $# -gt 0 ]; do
  case "$1" in
    --tunnel) LIVE_HOST=${2:-}; [ -n "$LIVE_HOST" ] || { echo "FAIL: --tunnel без адреса"; exit 2; }; shift 2 ;;
    --tunnel=*) LIVE_HOST=${1#--tunnel=}; shift ;;
    *) echo "FAIL: неизвестный аргумент $1"; exit 2 ;;
  esac
done

BIN=$HERE/desk
[ -x "$BIN" ] || { echo "FAIL: нет бинарника $BIN — собери: snens build desk"; exit 1; }
[ -f "$HERE/schema.json" ] && ok "desk: schema.json рядом с бинарником" || bad "desk: нет schema.json"
has '"x-snens"' "$(cat "$HERE/schema.json")" "desk: в схеме есть ключ x-snens"
# Анкер на ключ в properties: подстрока '"command"' встречается и в x-snens.argv,
# поэтому прежняя проверка была зелёной даже без верхнеуровневых свойств.
flat=$(tr -d '\n' <"$HERE/schema.json" | sed 's/[[:space:]]\{1,\}/ /g')
has '"properties": { "command": {' "$flat" "desk: схема объявляет command в properties"
has '"tunnel": {' "$flat" "desk: схема объявляет tunnel в properties"
has '"required": ["command", "tunnel"]' "$flat" "desk: command и tunnel обязательны"

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

# Неизвестный флаг — ошибка аргументов, а не тихо проигнорированный флаг.
t apps --windw 0 --tunnel nosuchhost.invalid
want 2 "$rc" "desk: неизвестный флаг — код 2"
has "--windw" "$err" "desk: неизвестный флаг назван"

# Кривой порт — ошибка, а не тихий дефолт/насыщение.
t apps --port abc --tunnel nosuchhost.invalid
want 1 "$rc" "desk: --port abc — код 1 (ошибка выполнения разбора)"
has "port" "$err" "desk: --port abc назван в ошибке"
want "" "$out" "desk: --port abc — stdout пуст"

# Все команды отвечают без машины: код 1 и подсказка про --tunnel в stderr.
for cmd in apps snap act shot focus launch stop status; do
  t "$cmd"
  want 1 "$rc" "desk: $cmd без --tunnel — код 1"
  want "" "$out" "desk: $cmd без --tunnel — stdout пуст"
  has "tunnel" "$err" "desk: $cmd без --tunnel — подсказка про tunnel"
done

# Агент на недоступной машине: ошибка ssh, а не зависание и не код 0.
t status --tunnel nosuchhost.invalid
want 1 "$rc" "status на недоступной машине: код 1"
has "недоступен" "$err" "status: честно сказал, что агент недоступен"

# ---- живые проверки: только с --tunnel ----------------------------------
if [ -n "$LIVE_HOST" ]; then
  t apps --tunnel "$LIVE_HOST"
  want 0 "$rc" "живой: apps — код 0"
  has "окон" "$out" "живой: apps перечисляет окна"

  t snap --tunnel "$LIVE_HOST"
  want 0 "$rc" "живой: snap — код 0"

  t status --tunnel "$LIVE_HOST"
  want 0 "$rc" "живой: status — код 0"
  has "жив" "$out" "живой: status говорит, что агент жив"
else
  echo "пропуск: живые проверки (окна, snap, status) не выполнялись — нет --tunnel user@machine"
fi

if [ "$fails" -eq 0 ]; then
  echo "все проверки desk прошли"
else
  echo "провалов: $fails"
  exit 1
fi
