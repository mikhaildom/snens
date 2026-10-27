#!/bin/sh
# Контракты тула genui: --help, list, new, check на фикстурах и деградация без браузера.
# Сеть не нужна, браузер — по возможности: без хрома check обязан деградировать до
# lint, а не падать (в десктопной поставке хрома может не быть).
# Запуск из корня репо: sh tools/genui/selftest.sh
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
hasnt() {
  case "$2" in
    *"$1"*) bad "$3 (не должно быть '$1')" ;;
    *) ok "$3" ;;
  esac
}

BIN=$HERE/genui
[ -x "$BIN" ] || { echo "FAIL: нет бинарника $BIN — собери: cargo build --release -p genui"; exit 1; }
[ -f "$HERE/schema.json" ] && ok "genui: schema.json рядом с бинарником" || bad "genui: нет schema.json"
has '"x-snens"' "$(cat "$HERE/schema.json")" "genui: в схеме есть ключ x-snens"
has '"command"' "$(cat "$HERE/schema.json")" "genui: схема объявляет command"
has '"enum"' "$(cat "$HERE/schema.json")" "genui: подкоманды перечислены enum-ом (модель не выдумает свою)"

# Библиотека и каталоги — только свои, во временном каталоге.
export SNENS_GENUI_DIR="$TMP/genui"
export SNENS_SESSIONS_DIR="$TMP/sessions"
export SNENS_ATTACH_DIR="$TMP/attach"
mkdir -p "$SNENS_SESSIONS_DIR"

t() {
  rc=0
  out=$("$BIN" "$@" 2>"$TMP/err") || rc=$?
  err=$(cat "$TMP/err")
}

# --- --help ---------------------------------------------------------------
t --help
want 0 "$rc" "genui: --help — код 0"
has "genui list" "$out" "genui: --help описывает подкоманды"
has "session>:<seq>" "$out" "genui: --help объясняет форму из журнала"

# --- list без библиотеки --------------------------------------------------
t list
want 0 "$rc" "genui: list без библиотеки — не ошибка"
has "каталога нет" "$out" "genui: list говорит, что библиотеки ещё нет"

# --- new создаёт скелет и lib.js ------------------------------------------
t new quiz
want 0 "$rc" "genui: new — код 0"
[ -f "$SNENS_GENUI_DIR/components/quiz.js" ] && ok "genui: new создал components/quiz.js" \
  || bad "genui: new не создал компонент"
[ -f "$SNENS_GENUI_DIR/lib.js" ] && ok "genui: new создал lib.js" || bad "genui: нет lib.js"
has 'customElements.define("my-quiz"' "$(cat "$SNENS_GENUI_DIR/components/quiz.js")" \
  "genui: скелет регистрирует my-quiz"

t new quiz
want 1 "$rc" "genui: new поверх существующего — отказ"
has "уже есть" "$err" "genui: отказ объясняет, что делать"

t new quiz --force
want 0 "$rc" "genui: new --force перезаписывает"

# --- list видит компонент -------------------------------------------------
t list
want 0 "$rc" "genui: list с компонентом — код 0"
has "my-quiz" "$out" "genui: list показывает тег компонента"

t list --json
want 0 "$rc" "genui: list --json — код 0"
has '"tag":"my-quiz"' "$(printf '%s' "$out" | tr -d ' \n')" "genui: list --json отдаёт тег"

# --- check: чистая фикстура ----------------------------------------------
cat >"$TMP/ok.html" <<'EOF'
```artifact id=quiz
<!doctype html><html><body><h3>Квиз</h3><button onclick="snens.answer({id:'q1',value:'2'})">2</button></body></html>
```
EOF
t check "$TMP/ok.html"
want 0 "$rc" "genui: check чистого артефакта — код 0"
has "чисто" "$out" "genui: check говорит «чисто»"

# --- check: чужой тег -----------------------------------------------------
cat >"$TMP/bad.html" <<'EOF'
```artifact id=broken
<div><my-nosuch>текст</my-nosuch></div>
```
EOF
t check "$TMP/bad.html"
want 1 "$rc" "genui: check чужого тега — код 1"
has "my-nosuch" "$out" "genui: проверка назвала незарегистрированный тег"

# --- check: приватная сеть ------------------------------------------------
cat >"$TMP/net.html" <<'EOF'
```artifact id=net net=localhost
<div>x</div>
```
EOF
t check "$TMP/net.html"
want 1 "$rc" "genui: check приватной сети — код 1"
has "приватная сеть" "$out" "genui: проверка объясняет запрет приватной сети"

# --- check: незакрытый фенс ----------------------------------------------
cat >"$TMP/open.html" <<'EOF'
```artifact id=half
<div>печатаю…
EOF
t check "$TMP/open.html"
want 1 "$rc" "genui: незакрытый фенс — ошибка"
has "не закрыт" "$out" "genui: проверка называет незакрытый фенс"

# --- check: артефакт из журнала ------------------------------------------
SESSION=11111111-2222-4333-8444-555555555555
cat >"$SNENS_SESSIONS_DIR/$SESSION.jsonl" <<'EOF'
{"kind":"meta","ts":1,"model":"x","surface":"web"}
{"kind":"system","ts":1,"text":"system"}
{"kind":"assistant","ts":2,"text":"Вот:\n\n```artifact id=quiz\n<div>ok</div>\n```","tool_calls":[]}
EOF
t check "$SESSION:3"
want 0 "$rc" "genui: check по журналу — код 0"
has "из $SESSION:3" "$out" "genui: проверка назвала источник из журнала"

t check "$SESSION:1"
want 1 "$rc" "genui: check по не-ответу — ошибка"
has "не ответ модели" "$err" "genui: отказ объясняет, что строка не ответ"

# --- check: деградация без браузера --------------------------------------
t check "$TMP/bad.html" --no-browser
want 1 "$rc" "genui: --no-browser всё равно находит чужой тег"
has "проверена только разметка" "$out" "genui: --no-browser честно говорит, что рендер не проверен"

t check "$TMP/ok.html" --chrome /nonexistent/chrome
want 0 "$rc" "genui: без браузера чистая разметка — не ошибка"
has "браузер не проверен" "$out" "genui: отсутствие браузера — предупреждение, а не провал"

# --- check: неизвестная подкоманда ---------------------------------------
t nosuch
want 1 "$rc" "genui: неизвестная подкоманда — ошибка"

echo
if [ "$fails" -eq 0 ]; then
  echo "genui: все проверки прошли"
else
  echo "genui: провалов $fails"
  exit 1
fi
