#!/bin/sh
# Контракты тула search. Живой поиск требует локальный SearXNG (дефолт 127.0.0.1:8888):
# нет сервера — соответствующие проверки пропускаются, а не падают.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
# Бинарник сначала ищем рядом (`snens build` кладёт его в tools/<имя>/), иначе —
# в target-dir (он может быть вынесен из репозитория, как в этом checkout'е).
BIN=$HERE/search
if [ ! -x "$BIN" ]; then
  TD=$(cd "$REPO" && cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
  [ -n "$TD" ] && [ -x "$TD/debug/search" ] && BIN=$TD/debug/search
fi
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

# t [args...] → $out, $err, $rc
t() {
  rc=0
  out=$("$BIN" "$@" 2>"$TMP/err") || rc=$?
  err=$(cat "$TMP/err")
}

[ -x "$BIN" ] || { echo "нет бинарника $BIN — собрать: cargo build -p search"; exit 1; }

# ---- help ----
t --help
want 0 "$rc" "search: --help — код 0"
first=$(printf '%s\n' "$out" | head -n 1)
has "search" "$first" "search: строка 1 хелпа содержит 'search'"
has "поиск" "$first" "search: строка 1 — суть про поиск"
second=$(printf '%s\n' "$out" | sed -n 2p)
has "search <запрос>" "$second" "search: строка 2 — usage"

# ---- нет аргумента-запроса ----
t
want 2 "$rc" "search: без запроса — код 2"
has "search <запрос>" "$err" "search: без запроса — usage в stderr"

# ---- битый URL ----
rc=0
out=$(SEARXNG_URL=http://127.0.0.1:1 "$BIN" "test" 2>"$TMP/err") || rc=$?
err=$(cat "$TMP/err")
want 1 "$rc" "search: недоступный SearXNG — код 1"
want "" "$out" "search: недоступный SearXNG — stdout пуст"
has "127.0.0.1:1" "$err" "search: недоступный SearXNG — URL в stderr"

# ---- живой запрос ----
if curl -s -m 3 -o /dev/null 'http://127.0.0.1:8888/search?q=x&format=json'; then
  t "rust async runtime" --n 5
  want 0 "$rc" "search: живой запрос — код 0"
  has "1." "$out" "search: живой запрос — нумерация результатов"
  has "найдено" "$out" "search: живой запрос — футер"
  has "движков" "$out" "search: живой запрос — счётчик движков"

  t "rust" --json
  want 0 "$rc" "search: --json — код 0"
  case "$out" in
    '{'*'"results"'*'}'*) ok "search: --json — форма ответа" ;;
    *) bad "search: --json — форма ответа ($out)" ;;
  esac
  lines=$(printf '%s\n' "$out" | wc -l | tr -d ' ')
  want 1 "$lines" "search: --json — одна строка"
else
  echo "skip: локальный SearXNG (127.0.0.1:8888) недоступен — живые проверки пропущены"
fi

if [ "$fails" -ne 0 ]; then
  echo
  echo "провалено проверок: $fails"
  exit 1
fi
echo
echo "все проверки search прошли"