#!/bin/sh
# Контракты тулов: коды возврата, форма stdout, отказы.
# Ни python, ни других интерпретаторов не предполагается.
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

# t <тул> [args...]  → $out, $err, $rc
t() {
  name=$1
  shift
  rc=0
  out=$("$HERE/$name/$name" "$@" 2>"$TMP/err") || rc=$?
  err=$(cat "$TMP/err")
}

# ti <тул> <stdin> [args...] → $out, $err, $rc
ti() {
  name=$1
  input=$2
  shift 2
  rc=0
  out=$(printf '%s' "$input" | "$HERE/$name/$name" "$@" 2>"$TMP/err") || rc=$?
  err=$(cat "$TMP/err")
}

f=$TMP/a.txt
printf 'alpha\nbeta\ngamma\ndelta\n' >"$f"

# ---- read ----
t read "$f"
want 0 "$rc" "read: код 0"
has "$(printf '     2\tbeta')" "$out" "read: формат cat -n"

t read "$f" --offset 3 --limit 1
has "gamma" "$out" "read: offset/limit"
has "показаны строки 3-3 из 4" "$out" "read: футер диапазона"

t read "$f" --limit=2
has "показаны строки 1-2 из 4" "$out" "read: инлайн-значение и футер усечения"

t read "$TMP/нет"
want 1 "$rc" "read: отсутствующий файл — код 1"
has "не найден" "$err" "read: отсутствующий файл — текст"

printf '\000\001\002' >"$TMP/bin"
t read "$TMP/bin"
want 1 "$rc" "read: бинарник — код 1"
has "бинарный" "$err" "read: бинарник — текст"

t read "$TMP"
has "каталог" "$err" "read: каталог — отказ с подсказкой"

t read --help
has "read —" "$out" "read: --help, строка 1 = суть"

# ---- edit ----
JSON='{"old_string":"beta","new_string":"BETA"}'
ti edit "$JSON" "$f"
want 0 "$rc" "edit: код 0"
has '"replaced":1' "$out" "edit: одна замена в событии"
want "alpha
BETA
gamma
delta" "$(cat "$f")" "edit: файл изменён точно"

ti edit '{"old_string":"нетакого","new_string":"x"}' "$f"
want 1 "$rc" "edit: нет совпадений — код 1"
has "не найден" "$err" "edit: нет совпадений — текст"

ti edit '{"old_string":"  gamma","new_string":"x"}' "$f"
want 1 "$rc" "edit: отступ не совпал — код 1"
has "похоже на строку 3" "$err" "edit: подсказка про отступ"

printf 'dup\ndup\n' >"$TMP/dup.txt"
ti edit '{"old_string":"dup","new_string":"x"}' "$TMP/dup.txt"
want 1 "$rc" "edit: неоднозначность — код 1"
has "2 раз" "$err" "edit: неоднозначность — число совпадений"
has "replace_all" "$err" "edit: неоднозначность — подсказка про replace_all"

ti edit '{"old_string":"dup","new_string":"x","replace_all":true}' "$TMP/dup.txt"
has '"replaced":2' "$out" "edit: replace_all — две замены"
want "x
x" "$(cat "$TMP/dup.txt")" "edit: replace_all — результат"

printf 'one\n' >"$TMP/dry.txt"
ti edit '{"old_string":"one","new_string":"two"}' "$TMP/dry.txt" --dry-run
has '"dry_run":true' "$out" "edit: --dry-run в событии"
want "one" "$(cat "$TMP/dry.txt")" "edit: --dry-run не пишет"
has "-- " "$err" "edit: --dry-run печатает участок"

chmod 600 "$TMP/dry.txt"
ti edit '{"old_string":"one","new_string":"one2"}' "$TMP/dry.txt"
want "600" "$(stat -c %a "$TMP/dry.txt")" "edit: права файла сохраняются"

ti edit '{"old_string":"","new_string":"x"}' "$TMP/dry.txt"
want 1 "$rc" "edit: пустой old_string — код 1"
has "write" "$err" "edit: пустой old_string — подсказка про write"

ti edit 'не json' "$TMP/dry.txt"
want 1 "$rc" "edit: мусор на stdin — код 1"
has "не JSON" "$err" "edit: мусор на stdin — текст"

t edit "$f"
want 1 "$rc" "edit: пустой stdin — код 1"

# ---- write ----
newf=$TMP/sub/n.txt
ti write "привет" "$newf"
want 0 "$rc" "write: код 0"
has '"created":true' "$out" "write: created в событии"
want "привет" "$(cat "$newf")" "write: создаёт каталоги и пишет"

ti write "ещё" "$newf"
has '"overwrote":true' "$out" "write: перезапись отмечена"
want "ещё" "$(cat "$newf")" "write: перезапись"

ti write "!" "$newf" --append
want "ещё!" "$(cat "$newf")" "write: --append"

if [ "$fails" -ne 0 ]; then
  echo
  echo "провалено проверок: $fails"
  exit 1
fi
echo
echo "все проверки тулов прошли"