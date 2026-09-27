#!/bin/sh
# Контракты тула fetch: коды возврата, форма stdout, живой прогон, отказы.
# Сеть нужна (example.com, httpbin.org). Запуск из корня репо: sh tools/fetch/selftest.sh
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

BIN=$HERE/fetch
[ -x "$BIN" ] || { echo "FAIL: нет бинарника $BIN — собери: cargo build -p fetch && snens build"; exit 1; }

# t [args...] → $out, $err, $rc
t() {
  rc=0
  out=$("$BIN" "$@" 2>"$TMP/err") || rc=$?
  err=$(cat "$TMP/err")
}

# ---- справка и разбор аргументов ----
t --help
want 0 "$rc" "fetch: --help — код 0"
has "fetch — прочитать веб-страницу как markdown." "$out" "fetch: строка 1 = суть"
has "fetch <url> [--raw] [--fallback auto|never|always]" "$out" "fetch: строка 2 = usage"

t
want 2 "$rc" "fetch: без URL — код 2"
has "fetch <url>" "$err" "fetch: без URL — usage в stderr"
want "" "$out" "fetch: без URL — stdout пуст"

t ftp://x
want 2 "$rc" "fetch: ftp-схема — код 2"
has "http/https" "$err" "fetch: ftp-схема — текст отказа"

t https://example.com --fallback=какой-то
want 2 "$rc" "fetch: кривой --fallback — код 2"

# ---- живой прогон: свой путь ----
t https://example.com
want 0 "$rc" "fetch: example.com — код 0"
has "Example Domain" "$out" "fetch: example.com — текст страницы"
has "documentation examples" "$out" "fetch: example.com — абзац"
case "$out" in
  *'{"'*) bad "fetch: вывод — markdown, не JSON" ;;
  *) ok "fetch: вывод — markdown, не JSON" ;;
esac

t https://example.com --fallback never
want 0 "$rc" "fetch: example.com без фолбэка — код 0"
has "# Example Domain" "$out" "fetch: свой путь — заголовок как markdown"
case "$out" in
  *"URL Source"*) bad "fetch: свой путь не должен нести обёртку jina" ;;
  *) ok "fetch: свой путь не несёт обёртку jina" ;;
esac

t https://example.com --max-chars 20
want 0 "$rc" "fetch: --max-chars — код 0"
has "обрезано" "$out" "fetch: --max-chars — пометка об обрезке"

# ---- HTTP-ошибка: свой путь не должен печатать половину страницы ----
t https://httpbin.org/status/403 --fallback never
want 1 "$rc" "fetch: 403 с --fallback never — код 1"
want "" "$out" "fetch: 403 с --fallback never — stdout пуст"
has "403" "$err" "fetch: 403 с --fallback never — код в stderr"
has "httpbin.org" "$err" "fetch: 403 с --fallback never — URL в stderr"

# ---- фолбэк r.jina.ai ----
t https://example.com --fallback always
want 0 "$rc" "fetch: example.com с --fallback always — код 0"
has "documentation examples" "$out" "fetch: --fallback always — текст страницы"
case "$out" in
  *"URL Source"*) bad "fetch: jina — служебная шапка срезана" ;;
  *) ok "fetch: jina — служебная шапка срезана" ;;
esac

# Цель ответила 403: jina приходит кодом 200, но в шапке пишет про ошибку цели.
# Молча вернуть пустоту с кодом 0 — соврать об успехе, поэтому ждём код 1.
t https://httpbin.org/status/403 --fallback always
want 1 "$rc" "fetch: цель 403 через jina — код 1"
want "" "$out" "fetch: цель 403 через jina — stdout пуст"
has "403" "$err" "fetch: цель 403 через jina — код в stderr"

# ---- не-html: слить в файл ----
t https://www.orimi.com/pdf-test.pdf --save "$TMP/p.pdf"
want 0 "$rc" "fetch: pdf с --save — код 0"
want "$TMP/p.pdf" "$out" "fetch: pdf с --save — печатает путь"
[ -s "$TMP/p.pdf" ] && ok "fetch: pdf с --save — файл не пуст" || bad "fetch: pdf с --save — файл пуст"

# Из сессии агента наследуется SNENS_ATTACH_DIR — для этой проверки он должен молчать.
t_clean() {
  rc=0
  out=$(env -u SNENS_ATTACH_DIR "$BIN" "$@" 2>"$TMP/err") || rc=$?
  err=$(cat "$TMP/err")
}

t_clean https://httpbingo.org/image/png
want 1 "$rc" "fetch: бинарь без --save и SNENS_ATTACH_DIR — код 1"
has "--save" "$err" "fetch: бинарь без места — подсказка про --save"

echo
if [ "$fails" -eq 0 ]; then
  echo "fetch: все проверки прошли"
else
  echo "fetch: провалов $fails"
  exit 1
fi