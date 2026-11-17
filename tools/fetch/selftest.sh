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

# t [args...] → $out, $err, $rc. Запрос ограничен по времени: зависший хост
# должен стать провалом проверки, а не зависанием всего прогона.
NET_TIMEOUT=${NET_TIMEOUT:-30}
t() {
  rc=0
  out=$(timeout "$NET_TIMEOUT" "$BIN" "$@" 2>"$TMP/err") || rc=$?
  err=$(cat "$TMP/err")
}

# t_clean — тот же прогон без унаследованного SNENS_ATTACH_DIR: из сессии агента
# он приходит, и «куда сохранить» иначе оказывается заданным.
t_clean() {
  rc=0
  out=$(env -u SNENS_ATTACH_DIR timeout "$NET_TIMEOUT" "$BIN" "$@" 2>"$TMP/err") || rc=$?
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

# Живые проверки ходят на чужие хосты: недоступность хоста — не регрессия тула.
# По умолчанию это громкий пропуск его группы проверок; при FETCH_SELFTEST_LIVE=1
# — провал (в CI молчаливая зелень хуже красного).
L() {
  if timeout 15 "$BIN" "https://$1" --fallback never --max-chars 1 >/dev/null 2>&1; then
    return 0
  fi
  if [ "${FETCH_SELFTEST_LIVE:-0}" = 1 ]; then
    bad "fetch: $1 недоступен — живые проверки не выполнены"
  fi
  echo "пропуск: $1 недоступен — его живые проверки не выполнены"
  return 1
}

# ---- живой прогон: свой путь ----
if L example.com; then
t https://example.com
want 0 "$rc" "fetch: example.com — код 0"
has "documentation examples" "$out" "fetch: example.com — текст страницы"
# Тело страницы живёт своей жизнью: проверяем форму вывода, а не её текст.
case "$out" in
  *"<p>"*|*"</p>"*) bad "fetch: вывод — markdown, а не html" ;;
  *) [ -n "$out" ] && ok "fetch: вывод — markdown, не html" || bad "fetch: вывод пуст" ;;
esac
case "$out" in
  *'{"'*) bad "fetch: вывод — markdown, не JSON" ;;
  *) ok "fetch: вывод — markdown, не JSON" ;;
esac

t https://example.com --fallback never
want 0 "$rc" "fetch: example.com без фолбэка — код 0"
[ -n "$out" ] && ok "fetch: свой путь — непустой вывод" || bad "fetch: свой путь — пустой вывод"
case "$out" in
  *"URL Source"*) bad "fetch: свой путь не должен нести обёртку jina" ;;
  *) ok "fetch: свой путь не несёт обёртку jina" ;;
esac

t https://example.com --max-chars 20
want 0 "$rc" "fetch: --max-chars — код 0"
has "обрезано" "$out" "fetch: --max-chars — пометка об обрезке"
# Пометка без реального среза — тоже регрессия: тело должно быть короче лимита+пометки.
[ "${#out}" -le 200 ] && ok "fetch: --max-chars — тело действительно обрезано" \
  || bad "fetch: --max-chars — тело длиннее лимита (${#out})"

# --raw отключает чистку DOM, но вывод всё равно markdown — проверяем, что он есть.
t https://example.com --raw
want 0 "$rc" "fetch: --raw — код 0"
[ -n "$out" ] && ok "fetch: --raw — непустой вывод" || bad "fetch: --raw — пустой вывод"

fi # example.com

# ---- HTTP-ошибка: свой путь не должен печатать половину страницы ----
if L httpbin.org; then
t https://httpbin.org/status/403 --fallback never
want 1 "$rc" "fetch: 403 с --fallback never — код 1"
want "" "$out" "fetch: 403 с --fallback never — stdout пуст"
has "403" "$err" "fetch: 403 с --fallback never — код в stderr"
has "httpbin.org" "$err" "fetch: 403 с --fallback never — URL в stderr"

fi # httpbin.org

# ---- фолбэк r.jina.ai ----
if L example.com; then
t https://example.com --fallback always
want 0 "$rc" "fetch: example.com с --fallback always — код 0"
has "documentation examples" "$out" "fetch: --fallback always — текст страницы"
case "$out" in
  *"URL Source"*) bad "fetch: jina — служебная шапка срезана" ;;
  *) ok "fetch: jina — служебная шапка срезана" ;;
esac

fi # example.com (jina)

if L httpbin.org; then
# Цель ответила 403: jina приходит кодом 200, но в шапке пишет про ошибку цели.
# Молча вернуть пустоту с кодом 0 — соврать об успехе, поэтому ждём код 1.
t https://httpbin.org/status/403 --fallback always
want 1 "$rc" "fetch: цель 403 через jina — код 1"
want "" "$out" "fetch: цель 403 через jina — stdout пуст"
has "403" "$err" "fetch: цель 403 через jina — код в stderr"

fi # httpbin.org (jina)

# ---- не-html: слить в файл ----
if L www.orimi.com; then
t https://www.orimi.com/pdf-test.pdf --save "$TMP/p.pdf"
want 0 "$rc" "fetch: pdf с --save — код 0"
want "$TMP/p.pdf" "$out" "fetch: pdf с --save — печатает путь"
[ -s "$TMP/p.pdf" ] && ok "fetch: pdf с --save — файл не пуст" || bad "fetch: pdf с --save — файл пуст"

fi # www.orimi.com

if L httpbingo.org; then
t_clean https://httpbingo.org/image/png
want 1 "$rc" "fetch: бинарь без --save и SNENS_ATTACH_DIR — код 1"
has "--save" "$err" "fetch: бинарь без места — подсказка про --save"
fi # httpbingo.org


echo
if [ "$fails" -eq 0 ]; then
  echo "fetch: все проверки прошли"
else
  echo "fetch: провалов $fails"
  exit 1
fi