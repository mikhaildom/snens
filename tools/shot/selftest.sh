#!/bin/sh
# Контракты тула shot: коды возврата, --help, файлы PNG, JS-проба, таймаут.
# Сеть не нужна: страницы отдаёт локальный python3-сервер из этой же проверки.
# Запуск из корня репо: sh tools/shot/selftest.sh
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
TMP=$(mktemp -d)
SRV_PID=
trap '[ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null; rm -rf "$TMP"' EXIT

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
hasnt() {
  case "$2" in
    *"$1"*) bad "$3 (не должно быть '$1')" ;;
    *) ok "$3" ;;
  esac
}

BIN=$HERE/shot
[ -x "$BIN" ] || { echo "FAIL: нет бинарника $BIN — собери: cargo build -p shot"; exit 1; }
[ -f "$HERE/schema.json" ] && ok "shot: schema.json рядом с бинарником" || bad "shot: нет schema.json"
has '"x-snens"' "$(cat "$HERE/schema.json")" "shot: в схеме есть ключ x-snens"
has '"url"' "$(cat "$HERE/schema.json")" "shot: схема объявляет url"

# t [args...] → $out, $err, $rc
t() {
  rc=0
  out=$("$BIN" "$@" 2>"$TMP/err") || rc=$?
  err=$(cat "$TMP/err")
}

# ---- справка и разбор аргументов (без запуска Chrome) ----
t --help
want 0 "$rc" "shot: --help — код 0"
has "shot — " "$out" "shot: строка 1 = суть"
has "shot <url> [--size" "$out" "shot: строка 2 = usage"
has "--selector" "$out" "shot: хелп объявляет --selector"
has "--timeout" "$out" "shot: хелп объявляет --timeout"

t
want 2 "$rc" "shot: без URL — код 2"
has "shot <url>" "$err" "shot: без URL — usage в stderr"
want "" "$out" "shot: без URL — stdout пуст"

t ftp://example.com/x
want 2 "$rc" "shot: не http(s) — код 2"
has "только http/https" "$err" "shot: не http(s) — текст отказа"

t http://127.0.0.1:1/ --nosuch
want 2 "$rc" "shot: неизвестный аргумент — код 2"
has "--nosuch" "$err" "shot: неизвестный аргумент назван по имени"

t http://127.0.0.1:1/ --size 1440
want 2 "$rc" "shot: кривой --size — код 2"

t http://127.0.0.1:1/ --crop 1,2,3
want 2 "$rc" "shot: кривой --crop — код 2"

t http://127.0.0.1:1/ --selector 'a' --crop 1,2,3,4
want 2 "$rc" "shot: --selector вместе с --crop — код 2"

t http://127.0.0.1:1/ --script "$TMP/нет.js"
want 2 "$rc" "shot: --script без файла — код 2"

t http://127.0.0.1:1/ --no-shot
want 2 "$rc" "shot: --no-shot без --script — код 2"

t http://127.0.0.1:1/ --timeout 0
want 2 "$rc" "shot: --timeout 0 — код 2"

# ---- живой прогон: локальный сервер ----
if ! command -v python3 >/dev/null 2>&1; then
  # Живые проверки — почти весь контракт тула (PNG, --selector, --crop, --script,
  # внутренний таймаут). Молчаливый пропуск в CI даёт зелёный прогон, в котором
  # ничего не проверено, поэтому пропуск возможен только явным переключателем.
  if [ "${SHOT_SELFTEST_SKIP_LIVE:-0}" = 1 ]; then
    echo "selftest: python3 нет — живые проверки пропущены (SHOT_SELFTEST_SKIP_LIVE=1)"
  else
    bad "shot: python3 не найден — живые проверки (PNG, --selector, --crop, --script, таймаут) не выполнены; пропустить явно: SHOT_SELFTEST_SKIP_LIVE=1"
  fi
else
  mkdir -p "$TMP/www"
  printf '<html><head><title>shot selftest</title></head><body style="background:#eef"><h1>shot selftest</h1><div id="box" style="width:320px;height:140px;background:#4a90d9">box</div><textarea>typed</textarea></body></html>\n' >"$TMP/www/index.html"
  cat >"$TMP/srv.py" <<'PY'
import sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

root = sys.argv[1]

class H(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/hang"):
            # Открытый поток: заголовки отданы, тело не кончается — ровно то,
            # на чём headless-хром висит без внутреннего таймаута.
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            # Размер чанка — ровно длина данных: "<html><body>hang" = 16 = 0x10,
            # а не 0x14. Ошибка в размере ломает chunked-ответ, и /hang перестаёт
            # проверять внутренний таймаут (хром падает на сети, а не висит).
            self.wfile.write(b"10\r\n<html><body>hang\r\n")
            try:
                while True:
                    self.wfile.write(b"4\r\n....\r\n")
                    self.wfile.flush()
                    time.sleep(1)
            except Exception:
                pass
            return
        try:
            with open(root + "/index.html", "rb") as f:
                body = f.read()
        except OSError:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass

srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
print(srv.server_address[1], flush=True)
threading.Thread(target=srv.serve_forever, daemon=True).start()
time.sleep(600)
PY
  python3 "$TMP/srv.py" "$TMP/www" >"$TMP/port" 2>"$TMP/srv.err" &
  SRV_PID=$!
  port=
  for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
    port=$(head -1 "$TMP/port" 2>/dev/null || true)
    [ -n "$port" ] && break
    sleep 0.2
  done
  if [ -z "$port" ]; then
    bad "shot: локальный сервер страниц не поднялся ($(cat "$TMP/srv.err" 2>/dev/null))"
  else
    URL="http://127.0.0.1:$port/"
    mkdir -p "$TMP/attach" "$TMP/out"

    # Полный кадр дефолтного размера: файл в канале вложений и он не пустой.
    rc=0
    out=$(SNENS_ATTACH_DIR="$TMP/attach" "$BIN" "$URL" --wait-ms 100 --out-dir "$TMP/out" 2>"$TMP/err") || rc=$?
    err=$(cat "$TMP/err")
    want 0 "$rc" "shot: обычный кадр — код 0"
    has "image/png" "$out" "shot: в stdout тип вложения"
    has "→ вложение" "$out" "shot: в stdout имя вложения"
    f=$(ls "$TMP/attach"/*.png 2>/dev/null | head -1)
    if [ -n "$f" ]; then ok "shot: PNG лёг в SNENS_ATTACH_DIR"; else bad "shot: PNG не лёг в SNENS_ATTACH_DIR"; fi
    size=$(wc -c <"$f" 2>/dev/null | tr -d ' ')
    [ "${size:-0}" -gt 5000 ] && ok "shot: кадр больше 5 КБ ($size Б)" || bad "shot: кадр подозрительно мал ($size Б)"
    head -c 8 "$f" | od -An -tx1 | tr -d ' \n' >"$TMP/magic"
    want "89504e470d0a1a0a" "$(cat "$TMP/magic")" "shot: файл — настоящий PNG (магия)"

    # Селектор: кроп ровно элемента, а не окна.
    rc=0
    out=$(SNENS_ATTACH_DIR="$TMP/attach" "$BIN" "$URL" --selector '#box' --wait-ms 100 --out-dir "$TMP/out" 2>"$TMP/err") || rc=$?
    err=$(cat "$TMP/err")
    want 0 "$rc" "shot: --selector — код 0"
    has "рамка" "$out" "shot: --selector печатает измеренную рамку"
    has "320x140" "$out" "shot: рамка элемента измерена, а не угадана"

    # Явный кроп.
    rc=0
    out=$(SNENS_ATTACH_DIR="$TMP/attach" "$BIN" "$URL" --crop 0,0,200,100 --wait-ms 100 --out-dir "$TMP/out" 2>"$TMP/err") || rc=$?
    err=$(cat "$TMP/err")
    want 0 "$rc" "shot: --crop — код 0"
    has "200x100" "$out" "shot: --crop даёт заказанный размер"

    # Два размера за запуск.
    rc=0
    out=$(SNENS_ATTACH_DIR="$TMP/attach" "$BIN" "$URL" --size 800x600,390x844 --wait-ms 100 --out-dir "$TMP/out" 2>"$TMP/err") || rc=$?
    err=$(cat "$TMP/err")
    want 0 "$rc" "shot: два --size — код 0"
    has "800x600" "$out" "shot: первый размер в выводе"
    has "390x844" "$out" "shot: второй размер в выводе"

    # Нет такого элемента — отказ, а не пустой PNG.
    t "$URL" --selector '#нетакого' --wait-ms 100 --out-dir "$TMP/out"
    want 1 "$rc" "shot: селектор без элемента — код 1"
    has "нет на странице" "$err" "shot: селектор без элемента — текст"

    # JS-проба: JSON в stdout, картинок не надо.
    printf 'return {title: document.title, box: !!document.querySelector("#box"), n: 1 + 1};\n' >"$TMP/probe.js"
    t "$URL" --script "$TMP/probe.js" --no-shot --wait-ms 100
    want 0 "$rc" "shot: --script — код 0"
    has '"box":true' "$out" "shot: --script вернул DOM-факт"
    has '"n":2' "$out" "shot: --script вернул вычисленное значение"
    has '"title":"shot selftest"' "$out" "shot: --script вернул заголовок"
    hasnt "image/png" "$out" "shot: --no-shot не снимает кадр"

    # Ошибка в JS — код 1 и текст исключения, а не «успех с null».
    printf 'throw new Error("boom");\n' >"$TMP/bad.js"
    t "$URL" --script "$TMP/bad.js" --no-shot --wait-ms 100
    want 1 "$rc" "shot: исключение в --script — код 1"
    has "boom" "$err" "shot: исключение в --script — текст ошибки"

    # Недоступный порт: отказ за секунды, без висящего хрома.
    t http://127.0.0.1:1/ --timeout 20
    want 1 "$rc" "shot: недоступный URL — код 1"
    has "страница не открылась" "$err" "shot: недоступный URL — текст"

    # Открытый поток: внутренний таймаут обязан вернуть тул, а не повесить.
    started=$(date +%s)
    t "http://127.0.0.1:$port/hang" --timeout 4
    elapsed=$(( $(date +%s) - started ))
    want 1 "$rc" "shot: висящая страница — код 1"
    has "таймаут 4 с" "$err" "shot: висящая страница — назван таймаут"
    [ "$elapsed" -le 10 ] && ok "shot: вернулся за ${elapsed} с (лимит 4 с + запуск)" \
      || bad "shot: таймаут не сработал (${elapsed} с)"

    # Временные каталоги тула (shot-<pid>-<nanos>) убираются за собой.
    left=$(ls -d /tmp/shot-[0-9]* 2>/dev/null | wc -l | tr -d ' ')
    [ "${left:-0}" -eq 0 ] && ok "shot: временные каталоги убраны" \
      || bad "shot: остались временные каталоги ($left)"
  fi
fi

echo
if [ "$fails" -eq 0 ]; then
  echo "shot: все проверки прошли"
else
  echo "shot: провалов $fails"
  exit 1
fi
