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

# Тул исполняется без оболочки, `~` доходит буквально — разворачивать должен он сам.
homefile=$HOME/.snens-selftest-$$
printf 'домашний\n' >"$homefile"
t read '~/.snens-selftest-'$$
want 0 "$rc" "read: ~ разворачивается в \$HOME"
has "домашний" "$out" "read: ~ прочитал файл из \$HOME"
rm -f "$homefile"

# ---- edit ----
# Реестр читает из хелпа признак «нужен stdin» по слову stdin в сути/usage.
# Потеряется — требование stdin пропадёт из схемы тула.
t edit --help
t has "stdin" "$out" "edit: хелп объявляет stdin (признак для реестра)"

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

# Кавычка внутри значения не экранирована моделью — правится, а не отправляет на write.
printf 'func envBool() {\n' >"$TMP/q.go"
ti edit '{"old_string": "func envBool() {", "new_string": "func envInt() int {\n\tif v := f(); v != "" {\n\t\treturn 1\n\t}\n\treturn 0\n}\n\nfunc envBool() {"}' "$TMP/q.go"
want 0 "$rc" "edit: кавычки внутри значения — код 0"
has 'v != ""' "$(cat "$TMP/q.go")" "edit: кавычки внутри значения сохранены"

# Оборванный вход назван оборванным, а не «кривой JSON».
ti edit '{"old_string": "abc' "$TMP/a.txt"
want 1 "$rc" "edit: оборванный вход — код 1"
has "оборван" "$err" "edit: оборванный вход — так и назван"

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

printf 'one\ntwo\nthree\n' >"$TMP/multi.txt"
ti edit '{"edits":[{"old_string":"one","new_string":"1"},{"old_string":"three","new_string":"3"}]}' "$TMP/multi.txt"
want 0 "$rc" "edit: пачка правок — код 0"
has '"edits":2' "$out" "edit: пачка — число правок в событии"
has '"replaced":2' "$out" "edit: пачка — счётчик замен"
want "1
two
3" "$(cat "$TMP/multi.txt")" "edit: пачка — обе правки применены"

printf 'one\ntwo\nthree\n' >"$TMP/multi.txt"
ti edit '{"edits":[{"old_string":"one","new_string":"1"},{"old_string":"нетакого","new_string":"3"}]}' "$TMP/multi.txt"
want 1 "$rc" "edit: пачка с промахом — код 1"
has "правка 2" "$err" "edit: пачка — номер несовпавшей правки"
want "one
two
three" "$(cat "$TMP/multi.txt")" "edit: пачка с промахом — файл не тронут (атомарность)"

ti edit '{"edits":[]}' "$TMP/multi.txt"
want 1 "$rc" "edit: пустая пачка — код 1"

printf 'x\nx\n' >"$TMP/multi2.txt"
ti edit '{"edits":[{"old_string":"x","new_string":"y"}]}' "$TMP/multi2.txt"
want 1 "$rc" "edit: пачка — неоднозначность внутри правки"
has "правка 1" "$err" "edit: пачка — номер в ошибке неоднозначности"

ti edit '{"old_string":"","new_string":"x"}' "$TMP/dry.txt"
want 1 "$rc" "edit: пустой old_string — код 1"
has "write" "$err" "edit: пустой old_string — подсказка про write"

ti edit 'не json' "$TMP/dry.txt"
want 1 "$rc" "edit: мусор на stdin — код 1"
has "не JSON" "$err" "edit: мусор на stdin — текст"

t edit "$f"
want 1 "$rc" "edit: пустой stdin — код 1"

# ---- write ----
# Суть «содержимое берётся со stdin» — тот же признак для реестра.
t write --help
t has "stdin" "$out" "write: хелп объявляет stdin (признак для реестра)"

newf=$TMP/sub/n.txt
ti write "привет" "$newf"
want 0 "$rc" "write: код 0"
has '"created":true' "$out" "write: created в событии"
want "привет" "$(cat "$newf")" "write: создаёт каталоги и пишет"

ti write "ещё" "$newf"
has '"overwrote":true' "$out" "write: перезапись отмечена"
want "ещё" "$(cat "$newf")" "write: перезапись"

ti write "!" "$newf" --append
want "ещё
!" "$(cat "$newf")" "write: --append"

# Хвост без перевода строки: append не должен склеивать строки (`}/// c`).
printf 'fn a(){}' >"$TMP/glue.rs"
ti write '/// c' "$TMP/glue.rs" --append
has '"separator_added":true' "$out" "write: append сам добавил разделитель"
want "fn a(){}
/// c" "$(cat "$TMP/glue.rs")" "write: append не склеивает строки"

# ---- memory ----
# Живого сервера тесты не требуют: контракт проверяется на мёртвом адресе.
t memory --help
want 0 "$rc" "memory: --help — код 0"
has "memory —" "$out" "memory: --help, строка 1 = суть"
has "memory recall" "$out" "memory: --help, строка 2 = usage"

t memory nosuch
want 2 "$rc" "memory: неизвестная команда — код 2"
has "memory recall <запрос>" "$err" "memory: неизвестная команда — usage в stderr"

t memory recall
want 2 "$rc" "memory: recall без запроса — код 2"

ti memory "" --context c
want 2 "$rc" "memory: retain без текста — код 2"

SNENS_MEMORY_URL=http://127.0.0.1:1
export SNENS_MEMORY_URL
t memory recall "что-нибудь"
want 1 "$rc" "memory: сервер недоступен (recall) — код 1"
has "Hindsight" "$err" "memory: сервер недоступен — текст ошибки"
ti memory '{"user":"q","assistant":"a"}' retain --context c
want 1 "$rc" "memory: сервер недоступен (retain) — код 1"
t memory stats
want 1 "$rc" "memory: сервер недоступен (stats) — код 1"
unset SNENS_MEMORY_URL

# ---- view ----
t view --help
has "view —" "$out" "view: --help, строка 1 = суть"

# 1x1 PNG белого цвета — минимальная валидная картинка (PNG: IHDR 1x1, 8 бит, RGB).
png=$TMP/px.png
printf '\211PNG\r\n\032\n\000\000\000\015IHDR\000\000\000\001\000\000\000\001\010\002\000\000\000\220wS\336\000\000\000\014IDATx\234c\370\377\377\077\000\005\376\002\376\015\357F\270\000\000\000\000IEND\256B\140\202' >"$png"

at=$TMP/attach
SNENS_ATTACH_DIR=$at
export SNENS_ATTACH_DIR
t view "$png"
want 0 "$rc" "view: код 0 на PNG"
has "image/png" "$out" "view: тип по магии"
has "1x1" "$out" "view: размеры из заголовка"
[ -f "$at/px.png" ] && ok "view: файл в канале вложений" || bad "view: файла нет в SNENS_ATTACH_DIR"
has "вложение px.png" "$out" "view: имя вложения в ответе"

# Повтор не затирает: второе имя получает индекс.
t view "$png"
[ -f "$at/px-1.png" ] && ok "view: коллизия имён разводится" || bad "view: коллизия имён потеряна"

printf 'не картинка' >"$TMP/notimg.txt"
t view "$TMP/notimg.txt"
want 1 "$rc" "view: не изображение — код 1"
has "не изображение" "$err" "view: не изображение — текст"

t view "$TMP/нет.png"
want 1 "$rc" "view: отсутствующий файл — код 1"

# Без канала view не падает: метаданные есть, вложение не уезжает.
unset SNENS_ATTACH_DIR
t view "$png"
want 0 "$rc" "view: без канала — код 0"
has "канал вложений недоступен" "$out" "view: без канала — пометка"

# ---- search ----
# Свой набор проверок у тула лежит рядом (tools/search/selftest.sh): он длиннее
# общего и требует живого SearXNG. Здесь — только контракт, который видит модель.
t search --help
want 0 "$rc" "search: --help — код 0"
has "search — " "$out" "search: --help, строка 1 = суть"
has "search <запрос>" "$out" "search: --help, строка 2 = usage"

t search
want 2 "$rc" "search: без запроса — код 2"
has "search <запрос>" "$err" "search: без запроса — usage в stderr"

# ---- fetch ----
t fetch --help
want 0 "$rc" "fetch: --help — код 0"
has "fetch — " "$out" "fetch: --help, строка 1 = суть"
has "fetch <url>" "$out" "fetch: --help, строка 2 = usage"

t fetch
want 2 "$rc" "fetch: без url — код 2"
has "fetch <url>" "$err" "fetch: без url — usage в stderr"

t fetch "ftp://example.com/x"
want 2 "$rc" "fetch: не http(s) — код 2"
has "только http/https" "$err" "fetch: не http(s) — текст"

# ---- agent ----
# Тесты могут идти изнутри сессии агента, где dispatch уже выставил SNENS_SESSION
# и SNENS_MAIL_DIR: снимаем их, иначе проверки «без контекста» видят чужой контекст.
unset SNENS_SESSION SNENS_MAIL_DIR SNENS_PARENT 2>/dev/null || true

# Родитель не задан — ask некуда слать.
t agent ask "вопрос"
want 1 "$rc" "agent: ask без родителя — код 1"
has "я не подагент" "$err" "agent: ask без родителя — текст"

t agent list
want 1 "$rc" "agent: list без сессии — код 1"
has "нет SNENS_SESSION" "$err" "agent: list без сессии — текст"

t agent wait "нет-такого" --timeout 1
want 1 "$rc" "agent: wait несуществующего — код 1"

# status — неблокирующий снимок по логу; ждать живого подагента не нужно.
mkdir -p "$TMP/sessions"
printf '{"kind":"assistant","ts":%s,"text":null,"tool_calls":[{"id":"c1","name":"run","args":"{\\"command\\":\\"echo hi\\"}"}]}\n' "$(date +%s)" >"$TMP/sessions/kid.jsonl"
SNENS_SESSIONS_DIR=$TMP/sessions t agent status kid
want 0 "$rc" "agent: status — код 0"
has "ходов: 1" "$out" "agent: status — число ходов"
has "echo hi" "$out" "agent: status — последний вызов с хинтом"
has "с назад" "$out" "agent: status — свежесть лога"

# Таймаут wait — не ошибка: код 3 и снимок, а не «сбой» и не тишина.
SNENS_SESSIONS_DIR=$TMP/sessions t agent wait kid --timeout 1 --progress 5
want 3 "$rc" "agent: wait по таймауту — код 3, а не 1"
has "ещё работает" "$err" "agent: wait по таймауту — говорит, что жив"

# Свой ящик: send кладёт сообщение туда, откуда его прочитает сессия.
SNENS_SESSION=me
SNENS_MAIL_DIR="$TMP/mail"
export SNENS_SESSION SNENS_MAIL_DIR
t agent send kid "привет"
want 0 "$rc" "agent: send — код 0"
ls "$TMP/mail/kid/new"/*.json >/dev/null 2>&1 && ok "agent: send положил сообщение в ящик" || bad "agent: send не положил сообщение в ящик"
has "\"kind\":\"steer\"" "$(cat "$TMP/mail/kid/new"/*.json)" "agent: send — вид steer"

if [ "$fails" -ne 0 ]; then
  echo
  echo "провалено проверок: $fails"
  exit 1
fi
echo
echo "все проверки тулов прошли"