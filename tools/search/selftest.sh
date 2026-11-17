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
  # JSON может быть отформатирован как угодно: режем по запятым, а не по всей
  # строке. Пустой результат означает «cargo не смог» — так и скажем ниже.
  TD=$(cd "$REPO" && cargo metadata --format-version 1 --no-deps 2>"$TMP/meta.err" \
    | tr ',' '\n' | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | head -1)
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

[ -x "$BIN" ] || {
  echo "нет бинарника search: cargo build -p search"
  [ -s "$TMP/meta.err" ] && { echo "cargo metadata:"; cat "$TMP/meta.err"; }
  exit 1
}

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

# ---- контракты разбора: код 2 ----
t query --nosuchflag
want 2 "$rc" "search: неизвестный флаг — код 2"

t query --n abc
want 2 "$rc" "search: --n без числа — код 2"

t query --page x
want 2 "$rc" "search: --page без числа — код 2"

t query --fresh всегда
want 2 "$rc" "search: кривой --fresh — код 2"

t query --lang
want 2 "$rc" "search: --lang без значения — код 2"

t "a" "b"
want 2 "$rc" "search: запрос дважды — код 2"

# ---- битый URL ----
rc=0
out=$(SEARXNG_URL=http://127.0.0.1:1 "$BIN" "test" 2>"$TMP/err") || rc=$?
err=$(cat "$TMP/err")
want 1 "$rc" "search: недоступный SearXNG — код 1"
want "" "$out" "search: недоступный SearXNG — stdout пуст"
has "127.0.0.1:1" "$err" "search: недоступный SearXNG — URL в stderr"

# ---- живой запрос ----
# Адрес инстанса — тот же, что у бинаря: SEARXNG_URL → конфиг → дефолт.
SEARX=${SEARXNG_URL:-}
if [ -z "$SEARX" ] && [ -f "${HOME:-/nonexistent}/.config/snens/search.toml" ]; then
  SEARX=$(sed -n 's/^[[:space:]]*url[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' \
    "${HOME}/.config/snens/search.toml" | head -1)
fi
SEARX=${SEARX:-http://127.0.0.1:8888}
SEARX=${SEARX%/}
if curl -fs -m 3 -o /dev/null "$SEARX/search?q=x&format=json"; then
  t "rust async runtime" --n 5
  want 0 "$rc" "search: живой запрос — код 0"
  # Движки SearXNG регулярно упираются в 429/CAPTCHA: «ничего не найдено» с
  # перечнем отвалившихся — это контракт (код 0), а не регрессия тула.
  case "$out" in
    *"ничего не найдено"*)
      echo "skip: движки SearXNG не ответили — проверки содержимого пропущены"
      break_live=1 ;;
    *) break_live=0 ;;
  esac
  if [ "$break_live" = 0 ]; then
  # Анкер на начало строки: «1.» внутри сниппета или URL ничего не доказывает.
  case "$out" in
    "1. "*|*"
1. "*) ok "search: живой запрос — нумерация результатов" ;;
    *) bad "search: живой запрос — нумерация результатов ($out)" ;;
  esac
  has "найдено" "$out" "search: живой запрос — футер"
  has "движков" "$out" "search: живой запрос — счётчик движков"
  fi

  t "rust" --json
  want 0 "$rc" "search: --json — код 0"
  case "$out" in
    '{'*'"results"'*'}'*) ok "search: --json — форма ответа" ;;
    *) bad "search: --json — форма ответа ($out)" ;;
  esac
  lines=$(printf '%s\n' "$out" | wc -l | tr -d ' ')
  want 1 "$lines" "search: --json — одна строка"
  # Тело — валидный JSON, а не «похоже на».
  if printf '%s' "$out" | python3 -c 'import json,sys; json.load(sys.stdin)' 2>/dev/null; then
    ok "search: --json — тело валидный JSON"
  else
    bad "search: --json — тело не разбирается как JSON"
  fi
else
  echo "skip: SearXNG ($SEARX) недоступен — живые проверки пропущены"
fi

if [ "$fails" -ne 0 ]; then
  echo
  echo "провалено проверок: $fails"
  exit 1
fi
echo
echo "все проверки search прошли"