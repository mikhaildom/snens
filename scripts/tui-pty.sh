#!/bin/sh
# pty-проверка TUI: строка, введённая во время хода, должна долететь до сессии
# как steer. Не-tty путь (`printf … | snens tui`) этого не проверяет: там нет
# приглашения и вход читается последовательно.
#
# Почему так, а не через capture-pane: панель схлопывает длинные строки и не
# источник правды. Правда — в jsonl сессии. Поэтому id сессии фиксирован
# (`--session`), а ожидание идёт опросом лога, не экрана.
#
# Мок-провайдер: SSE-ответ, который держит ход открытым ~2.5 с. Так окно для
# steer детерминировано и токены не тратятся. Мок пишет свой stdout в файл и
# закрывает stdin — иначе фоновый процесс наследует stdout `run` и тот висит
# до таймаута (тикет run-background-holds-stdout).
#
# Использование: sh scripts/tui-pty.sh
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/.." && pwd)
TMP=$(mktemp -d)
# Имя сессии уникально на прогон: фиксированное `tuipty` ломалось о чужую сессию
# (свою у разработчика или параллельного прогона) — `new-session` падал, а trap
# затем убивал ЧУЖУЮ сессию.
# Имя сессии уникально на прогон: фиксированное `tuipty` ломалось о чужую сессию
# (свою у разработчика или параллельного прогона) — `new-session` падал, а trap
# затем убивал ЧУЖУЮ сессию. Формат `<цифры>-<pid>` — ровно то, что ядро
# принимает как допустимый id сессии (`surface_runtime::valid_id`); `tuipty-…`
# оно отвергало с «invalid session id», и лог сессии не создавался вовсе.
SESS="$(date +%s)-$$"
OWN_SESSION=
PORT=18765
MOCK_PID=
cleanup() {
  [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
  # Не `pkill -f`: он матчит собственную командную строку и убивает сам прогон.
  # `=` — точное совпадение имени, без tmux-фолбэка на префикс.
  [ -n "$OWN_SESSION" ] && tmux kill-session -t "=$SESS" 2>/dev/null || true
  # Каталог не сносим при провалах: jsonl сессии — единственное доказательство,
  # почему steer не долетел. Адрес печатается в конце. Без проверки на непустой
  # `TMP` `rm -rf ""` мог бы тронуть текущий каталог.
  if [ "${fails:-0}" -eq 0 ] && [ -n "${TMP:-}" ]; then rm -rf "$TMP"; fi
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT TERM

ok() { echo "ok: $1"; }
bad() { echo "FAIL: $1"; fails=$((fails + 1)); }
fails=0

# --- мок-провайдер: SSE, ход держится открытым ---
cat >"$TMP/mock.py" <<'PY'
import json, http.server, socketserver, sys, time

def frame(**delta):
    return "data: " + json.dumps({"id": "mock-1", "choices": [
        {"index": 0, "delta": delta, "finish_reason": None}]}) + "\n\n"

class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass
    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length", 0)))
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        def w(s):
            self.wfile.write(s.encode())
            self.wfile.flush()
        w(frame(content="начал"))
        # Держим ход открытым: окно, в которое влезает steer.
        time.sleep(float(sys.argv[1]) if len(sys.argv) > 1 else 2.5)
        w(frame(content="готово"))
        w("data: " + json.dumps({"id": "mock-1", "choices": [
            {"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10}}) + "\n\n")
        w("data: [DONE]\n\n")

socketserver.ThreadingTCPServer.allow_reuse_address = True
with socketserver.ThreadingTCPServer(("127.0.0.1", int(sys.argv[2]) if len(sys.argv) > 2 else 18765), H) as s:
    s.serve_forever()
PY

SESS_DIR="$TMP/sessions"
mkdir -p "$SESS_DIR"
cat >"$TMP/mock.toml" <<TOML
base_url = "http://127.0.0.1:$PORT/v1"
model = "mock/model"
key_env = "SNENS_API_KEY"
sessions_dir = "$SESS_DIR"
tools_dir = "$REPO/tools"
review_min_turns = 100000
[memory]
TOML

# stdio мока закрыт от вызывающего `run` — иначе фоновый процесс держит его stdout.
nohup python3 "$TMP/mock.py" 2.5 "$PORT" </dev/null >"$TMP/mock.log" 2>&1 &
MOCK_PID=$!
# Ждём готовности мока, а не фиксированного сна: на медленной машине порт ещё
# не занят, и `snens tui` падает с невнятной ошибкой провайдера. Заодно ловим
# смерть мока сразу, а не через таймаут хода.
i=0
while ! python3 -c 'import socket,sys; socket.create_connection(("127.0.0.1", int(sys.argv[1])), 0.2).close()' "$PORT" 2>/dev/null; do
  kill -0 "$MOCK_PID" 2>/dev/null || { echo "FAIL: мок-провайдер не поднялся (см. $TMP/mock.log)"; exit 1; }
  i=$((i + 1))
  [ "$i" -gt 50 ] && { echo "FAIL: мок не слушает порт $PORT за 5 с (см. $TMP/mock.log)"; exit 1; }
  sleep 0.1
done

SNENS_API_KEY=mock
export SNENS_API_KEY

LOG="$SESS_DIR/$SESS.jsonl"

# --- поднять TUI в pty с фиксированным id ---
# `-c` с рабочим каталогом вместо вклейки его в командную строку: путь с
# пробелами или `;` иначе разваливал бы `tmux new-session`, а кавычки в
# команде не спасают при вложенном шелле.
tmux new-session -d -s "$SESS" -c "$REPO" -x 100 -y 40 \
  "SNENS_API_KEY=mock snens tui --config $TMP/mock.toml --session $SESS"
OWN_SESSION=1

# ждём, что поверхность поднялась: появился лог сессии
i=0
while [ ! -f "$LOG" ]; do
  i=$((i + 1))
  [ "$i" -gt 50 ] && { bad "TUI не создал $LOG за 5 с"; exit 1; }
  sleep 0.1
done
ok "TUI поднялся в pty (сессия $SESS)"

# --- первый ход: задача уходит агенту ---
# send-keys -l: без -l tmux трактует ';' и прочее как свои команды и режет ввод.
tmux send-keys -t "=$SESS" -l "первый ход"
tmux send-keys -t "=$SESS" Enter

# --- steer во время хода: строка должна уйти в ящик, а не начать новый ход ---
# Ждём НАБЛЮДАЕМЫЙ факт (первый ход записан), а не фиксированные 1.2 с: на
# загруженной машине TUI мог ещё не начать запрос, и текст ложился бы новым
# ходом, а тест падал недетерминированно.
wait_for() { # wait_for <подстрока> <секунды> <описание>
  n=0
  while [ "$n" -lt "$(($2 * 10))" ]; do
    if grep -qF -- "$1" "$LOG" 2>/dev/null; then
      ok "$3"
      return 0
    fi
    n=$((n + 1))
    sleep 0.1
  done
  bad "$3 (нет '$1' в логе за $2 с)"
  return 1
}

wait_for '"kind":"user"' 10 "первый ход записан в сессию" || true

tmux send-keys -t "=$SESS" -l "вот это steer"
tmux send-keys -t "=$SESS" Enter

# --- правда — в jsonl, не в панели ---
wait_for '"kind":"steer"' 15 "строка во время хода долетела как steer" || true

# steer несёт наш текст, а не обрывок (проверка, что ';'/пробелы не съедены tmux)
if grep '"kind":"steer"' "$LOG" 2>/dev/null | grep -qF "вот это steer"; then
  ok "steer донёс текст целиком"
else
  bad "steer потерял текст (send-keys без -l?)"
fi

# --- выход ---
# `|| true`: если TUI уже умер, send-keys падает, а `set -e` обрывает прогон до
# сводки — и настоящая причина теряется.
tmux send-keys -t "=$SESS" -l "/quit" 2>/dev/null || true
tmux send-keys -t "=$SESS" Enter 2>/dev/null || true
sleep 0.5

echo
if [ "$fails" -ne 0 ]; then
  echo "провалено проверок: $fails"
  echo "улики (jsonl сессии и лог мока): $TMP"
  exit 1
fi
echo "pty-проверка TUI прошла"