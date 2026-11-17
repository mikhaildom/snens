#!/bin/sh
# Контракты тула web: коды возврата, --help, схема, привязка к вкладке и
# отказ без подключения. Сеть не нужна: страницу отдаёт локальный python3.
# Запуск из корня репо: sh tools/web/selftest.sh
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
TMP=$(mktemp -d)
SRV_PID=
CH_PID=
trap '[ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null; [ -n "$CH_PID" ] && kill "$CH_PID" 2>/dev/null; sleep 0.5; rm -rf "$TMP" 2>/dev/null || true' EXIT

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

BIN=$HERE/web
[ -x "$BIN" ] || { echo "FAIL: нет бинарника $BIN — собери: snens build web"; exit 1; }
[ -f "$HERE/schema.json" ] && ok "web: schema.json рядом с бинарником" || bad "web: нет schema.json"
has '"x-snens"' "$(cat "$HERE/schema.json")" "web: в схеме есть ключ x-snens"
has '"command"' "$(cat "$HERE/schema.json")" "web: схема объявляет command"

# t [args...] → $out, $err, $rc
t() {
  rc=0
  out=$("$BIN" "$@" 2>"$TMP/err") || rc=$?
  err=$(cat "$TMP/err")
}

# --help: строка 1 — суть, строка 2 — usage (их ядро увозит модели)
t --help
want 0 "$rc" "web --help: код 0"
first=$(printf '%s\n' "$out" | sed -n 1p)
second=$(printf '%s\n' "$out" | sed -n 2p)
has "web —" "$first" "web --help: строка 1 объясняет суть"
has "web connect" "$second" "web --help: строка 2 — usage"

t
want 0 "$rc" "web без аргументов: код 0 и справка"
has "web —" "$out" "web без аргументов печатает справку"

t nosuchcommand
want 2 "$rc" "неизвестная команда: код 2"

t open
want 1 "$rc" "open без URL: ошибка"
has "нужен URL" "$err" "open без URL: сказано, чего не хватает"

# Без привязки тул обязан сказать «сначала connect», а не падать на соединении.
SNENS_WEB_STATE=$TMP/state
export SNENS_WEB_STATE
t snap
want 1 "$rc" "snap без подключения: код 1"
has "web connect" "$err" "snap без подключения: подсказывает connect"

# connect к мёртвому порту: понятная подсказка про тумблер, а не стектрейс.
t connect --port 9
want 1 "$rc" "connect на закрытый порт: код 1"
has "chrome://inspect" "$err" "connect на закрытый порт: подсказывает тумблер"

# launch без --tunnel: сказать, чего не хватает, а не лезть в сеть.
t launch
want 1 "$rc" "launch без --tunnel: код 1"
has "tunnel" "$err" "launch без --tunnel: подсказывает --tunnel"

# --host уходит в ssh одним вызовом; без доступного ssh — ошибка, не паника.
t --host 127.0.0.1:1 status
# ssh отдаёт свой код (255) — пробрасываем как есть, это честнее «своего» 1:
# человек видит, что упал именно ssh, а не web.
[ "$rc" -ne 0 ] && ok "--host с недоступной машиной: код $rc ≠ 0" || bad "--host: код 0 на недоступной машине"
has "ssh" "$err" "--host: сказал про ssh"

# --- живой Chrome (если есть) ---
CHROME=${SNENS_CHROME:-}
if [ -z "$CHROME" ]; then
  for c in "$HOME/.cache/ms-playwright/chromium-"*/chrome-linux64/chrome; do
    [ -x "$c" ] && CHROME=$c && break
  done
fi
if [ -z "$CHROME" ]; then
  echo "skip: живые проверки — Chrome не найден (нет SNENS_CHROME и ~/.cache/ms-playwright)"
else
  PORT=$((9200 + $$ % 400))
  "$CHROME" --headless=new --remote-debugging-port=$PORT \
    --user-data-dir="$TMP/prof" --no-first-run --no-sandbox --window-size=1280,800 \
    about:blank >"$TMP/ch.log" 2>&1 &
  CH_PID=$!
  i=0
  while [ $i -lt 40 ]; do
    # --max-time: порт может открыться и не ответить, и тогда прогон висел бы.
    curl -fsS --max-time 2 "http://127.0.0.1:$PORT/json/version" >/dev/null 2>&1 && break
    i=$((i + 1)); sleep 0.25
  done
  if ! curl -fsS --max-time 2 "http://127.0.0.1:$PORT/json/version" >/dev/null 2>&1; then
    bad "web: Chrome не поднял отладку на порту $PORT — живые проверки не выполнены"
    kill "$CH_PID" 2>/dev/null || true
    echo "лого: $(tail -3 "$TMP/ch.log" 2>/dev/null)"
    exit 1
  fi

  printf '<html><head><meta charset="utf-8"><title>Проба</title></head><body><h1>Заголовок</h1><form><input aria-label="Имя"><button>Отправить</button></form><a href="/x">Ссылка</a></body></html>' >"$TMP/p.html"
  # Порт страницы — свободный: захардкоженный 8877 мог достаться чужому серверу.
  if ! command -v python3 >/dev/null 2>&1; then
    # «Сеть не нужна» держится на локальном python3: без него живые проверки
    # не выполнить, и молчаливая зелень тут хуже красного.
    bad "web: python3 не найден — живые проверки (страница, snap, act) не выполнены"
    kill "$CH_PID" 2>/dev/null || true
    exit 1
  fi
  PAGE_PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
  ( cd "$TMP" && python3 -m http.server "$PAGE_PORT" >/dev/null 2>&1 ) &
  SRV_PID=$!
  sleep 1

  t connect --port $PORT
  want 0 "$rc" "connect к живому Chrome: код 0"
  has "подключено" "$out" "connect: сказал, что подключился"

  t open "http://127.0.0.1:$PAGE_PORT/p.html"
  want 0 "$rc" "open: код 0"
  has "Проба" "$out" "open: показал заголовок страницы"

  t snap
  want 0 "$rc" "snap: код 0"
  has "дерево:" "$out" "snap: печатает счётчики"
  has "textbox [e1]" "$out" "snap: поле ввода получило адрес e1"
  has "button" "$out" "snap: кнопка попала в дерево"
  # дерево в файл, а не в вывод: в контексте не должно быть всего дерева
  lines=$(printf '%s\n' "$out" | wc -l)
  [ "$lines" -le 20 ] && ok "snap: вывод короткий ($lines строк), дерево ушло в файл" || bad "snap: вывод слишком длинный ($lines строк)"

  t act --json '[{"type":{"ref":"e1","text":"привет"}},{"eval":"document.querySelector(\"input\").value"}]'
  want 0 "$rc" "act: батч из двух действий прошёл"
  has "привет" "$out" "act: ввод доехал до поля"

  t eval 'document.title'
  want 0 "$rc" "eval: код 0"
  has "Проба" "$out" "eval: вернул значение"

  t text
  want 0 "$rc" "text: код 0"
  has "Заголовок" "$out" "text: отдал видимый текст"

  t shot --out "$TMP/s.png"
  want 0 "$rc" "shot: код 0"
  [ -s "$TMP/s.png" ] && ok "shot: PNG записан" || bad "shot: PNG пустой"

  t probe
  want 0 "$rc" "probe: ввод доставляется"

  t eval 'this is not js'
  want 1 "$rc" "eval с синтаксической ошибкой: код 1"
fi

if [ "$fails" -eq 0 ]; then
  echo "все проверки web прошли"
else
  echo "провалов: $fails"
  exit 1
fi
