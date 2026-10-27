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

# th <HOME-каталог> <тул> [args...] → $out, $err, $rc
# Отдельный HOME нужен тулу памяти: он читает свой конфиг и ключ оттуда.
th() {
  home=$1
  name=$2
  shift 2
  rc=0
  out=$(HOME="$home" "$HERE/$name/$name" "$@" 2>"$TMP/err") || rc=$?
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

# ---- схемы тулов (контракт для модели) ----
# Модель видит не generic args/stdin, а настоящие поля из tools/<имя>/schema.json.
# Проверяем файлы и то, что `snens tools` их показывает; сборку argv по схеме
# проверяют unit-тесты (src/dispatch.rs, src/registry.rs).
for tool in read write edit; do
  s=$HERE/$tool/schema.json
  if [ -f "$s" ]; then ok "schema: $tool — schema.json рядом с бинарником"; else bad "schema: у $tool нет schema.json"; fi
  if grep -q '"x-snens"' "$s" 2>/dev/null; then ok "schema: $tool — есть ключ x-snens"; else bad "schema: в $tool/schema.json нет x-snens"; fi
done
has '"path"' "$(cat "$HERE/read/schema.json")" "schema: read объявляет path"
has '"content"' "$(cat "$HERE/write/schema.json")" "schema: write объявляет content"
has '"edits"' "$(cat "$HERE/edit/schema.json")" "schema: edit объявляет edits"
has '"mode": "field"' "$(cat "$HERE/write/schema.json")" "schema: write берёт stdin из поля content"
has '"mode": "json"' "$(cat "$HERE/edit/schema.json")" "schema: edit собирает stdin как JSON"
# Минимальный валидный вызов по схеме не должен быть пустым: провайдер эмитил
# {path, replace_all} (required был только path) и девять раз подряд получал отказ.
has '"anyOf"' "$(cat "$HERE/edit/schema.json")" "schema: edit требует одну из двух форм (anyOf)"


# Что видит модель: определения тулов из ядра (dump), а не только файлы схем.
# Ядро само печатает definitions — так проверка не зависит от бинаря в PATH.
CORE=${SNENS_SELFTEST_BIN:-}
if [ -z "$CORE" ]; then
  # Каталог сборки берём у самого cargo: target-dir живёт в .cargo/config.toml
  # репозитория, и без этого брался бинарь из PATH — то есть production, а
  # проверялся бы не тот код.
  TARGET=$(cd "$HERE/.." && cargo metadata --no-deps --format-version 1 2>/dev/null \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
  for c in "${TARGET:-/nonexistent}/debug/snens" "${CARGO_TARGET_DIR:-/nonexistent}/debug/snens"; do
    [ -x "$c" ] && CORE=$c && break
  done
  if [ -z "$CORE" ]; then
    echo "selftest: ядро не собрано (cargo build) — проверки схем пропущены" >&2
  fi
fi
if [ -n "$CORE" ]; then
  # Свой конфиг: иначе команда читает ~/.config/snens и смотрит в чужой каталог тулов.
  scfg=$TMP/selftest.toml
  printf 'tools_dir = "%s"\nsessions_dir = "%s"\n' "$HERE" "$TMP/sessions" >"$scfg"
  tools_out=$("$CORE" tools --config "$scfg" 2>/dev/null || true)
  case "$tools_out" in
    *"[схема] поля:"*) ok "schema: snens tools помечает тулы со схемой" ;;
    *) bad "schema: snens tools не различает тулы со схемой" ;;
  esac
  for field in 'path: string' 'content: string' 'old_string: string'; do
    case "$tools_out" in
      *"$field"*) ok "schema: snens tools печатает поле '$field'" ;;
      *) bad "schema: snens tools не печатает поле '$field'" ;;
    esac
  done
  case "$tools_out" in
    *"аргументы:"*) ok "schema: snens tools оставляет пометку аргументов для тулов без схемы" ;;
    *) bad "schema: snens tools потерял пометку generic" ;;
  esac
fi

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
# Правила матчинга — часть контракта тула: по хелпу модель решает, как дробить
# правку. Проверяются до t has, который перезаписывает $out.
help=$out
has "хвостовых пробелов" "$help" "edit: хелп объявляет fuzzy-матчинг"
has "ИСХОДНОГО файла" "$help" "edit: хелп объявляет батч от исходного файла"
t has "stdin" "$help" "edit: хелп объявляет stdin (признак для реестра)"

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

# Вызов без самой правки: по схеме он валиден (обязателен только path), поэтому
# отказ обязан назвать обе формы и перечислить пришедшие поля — на коротком
# «нужны строковые old_string и new_string» модель повторяла тот же скелет.
ti edit '{"path":"x","replace_all":false}' "$TMP/multi.txt"
want 1 "$rc" "edit: вызов без правки — код 1"
has "edits" "$err" "edit: отказ называет вторую форму (edits)"
has "Пришло в вызове: path, replace_all" "$err" "edit: отказ перечисляет пришедшие поля"

# Fuzzy: хвостовые пробелы/CRLF/юникод-кавычки — совпадение с пометкой, файл
# переписывается только в затронутом месте.
printf 'head  \nvalue = 1;   \ntail\t\n' >"$TMP/fz.txt"
ti edit '{"old_string":"value = 1;\ntail","new_string":"value = 2;\ntail"}' "$TMP/fz.txt"
want 0 "$rc" "edit: fuzzy по хвостовым пробелам — код 0"
has '"fuzzy":true' "$out" "edit: fuzzy отмечен в событии"
want "head  
value = 2;
tail	" "$(cat "$TMP/fz.txt")" "edit: fuzzy — изменён только участок"

printf 'alpha\r\nbeta\r\n' >"$TMP/crlf.txt"
ti edit '{"old_string":"alpha\nbeta","new_string":"ALPHA\nBETA"}' "$TMP/crlf.txt"
want 0 "$rc" "edit: fuzzy по CRLF — код 0"
want "ALPHA:BETA:" "$(tr -d '\r' <"$TMP/crlf.txt" | tr '\n' ':')" "edit: fuzzy — текст после CRLF в порядке"
want 2 "$(tr -cd '\r' <"$TMP/crlf.txt" | wc -c | tr -d ' ')" "edit: fuzzy — CRLF сохранён (два CR)"

printf 'let s = \342\200\234x\342\200\235; // a \342\200\224 b\n' >"$TMP/fq.txt"
ti edit '{"old_string":"let s = \"x\"; // a - b","new_string":"let s = \"y\";"}' "$TMP/fq.txt"
want 0 "$rc" "edit: fuzzy по кривым кавычкам — код 0"
want 'let s = "y";' "$(cat "$TMP/fq.txt")" "edit: fuzzy — кавычки заменены"

# Батч: old_string матчатся против исходного файла и не должны пересекаться.
printf 'abcdef\n' >"$TMP/ov.txt"
ti edit '{"edits":[{"old_string":"abc","new_string":"X"},{"old_string":"bcd","new_string":"Y"}]}' "$TMP/ov.txt"
want 1 "$rc" "edit: пересечение правок — код 1"
has "задевают один и тот же участок" "$err" "edit: пересечение правок — названо"
want "abcdef" "$(cat "$TMP/ov.txt")" "edit: пересечение — файл не тронут"

printf 'let a = 1;\n' >"$TMP/seq.txt"
ti edit '{"edits":[{"old_string":"let a = 1;","new_string":"let a = 10;"},{"old_string":"let a = 10;","new_string":"x"}]}' "$TMP/seq.txt"
want 1 "$rc" "edit: вторая правка не видит результата первой — код 1"
has "правка 2" "$err" "edit: батч от исходника — номер правки"
has "ни одна правка не применена" "$err" "edit: батч от исходника — атомарность названа"
has "правка 1 сошлась бы" "$err" "edit: батч — сказано, какие правки сошлись"

printf 'one\ntwo\n' >"$TMP/one.txt"
ti edit '{"edits":{"old_string":"one","new_string":"1"}}' "$TMP/one.txt"
want 0 "$rc" "edit: edits одиночным объектом — код 0"
want "1
two" "$(cat "$TMP/one.txt")" "edit: edits одиночным объектом — применён"


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
has "неизвестная команда nosuch" "$err" "memory: неизвестная команда — названа в stderr"

t memory recall
want 2 "$rc" "memory: recall без запроса — код 2"

t memory forget
want 2 "$rc" "memory: forget без uid — код 2"

# Пустой ход — не работа для памяти: код 0 и ничего не обещаем.
ti memory '{"user":"","assistant":""}' retain
want 0 "$rc" "memory: пустой ход — код 0"
has "" "$out" "memory: пустой ход — молчит"

# Мусор на stdin — ошибка ввода, а не повод работать.
ti memory 'не json' retain
want 1 "$rc" "memory: retain с мусором на stdin — код 1"

# Локальные команды не требуют ключа: ход человека не падает без провайдера.
MEMHOME="$TMP/home"
mkdir -p "$MEMHOME/.config/snens"
th "$MEMHOME" memory stats
want 0 "$rc" "memory: stats без ключа — код 0"
has "фактов: 0" "$out" "memory: stats на чистой базе"

# Сеть — только в recall: без ключа он честно отказывает кодом 1.
th "$MEMHOME" memory recall "что-нибудь"
want 1 "$rc" "memory: recall без ключа — код 1"
has "ключ" "$err" "memory: recall без ключа — текст ошибки"

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

# ---- desk ----
# Живые проверки (окна, дерево, действия) требуют машину пользователя и ssh:
# полный набор — tools/desk/selftest.sh с --tunnel. Здесь только контракт.
t desk --help
want 0 "$rc" "desk: --help — код 0"
has "desk — " "$out" "desk: --help, строка 1 = суть"

t desk nosuchcommand
want 2 "$rc" "desk: неизвестная команда — код 2"

t desk apps
want 1 "$rc" "desk: apps без --tunnel — код 1"
has "tunnel" "$err" "desk: apps без --tunnel — подсказка"

# ---- web ----
# Полный набор живых проверок — рядом (tools/web/selftest.sh, поднимает Chrome и
# локальную страницу). Здесь — только контракт, который видит модель.
t web --help
want 0 "$rc" "web: --help — код 0"
has "web — " "$out" "web: --help, строка 1 = суть"
has "web connect" "$out" "web: --help, строка 2 = usage"

t web nosuchcommand
want 2 "$rc" "web: неизвестная команда — код 2"

# Без привязки тул обязан сказать «connect», а не падать на соединении.
SNENS_WEB_STATE=$TMP/webstate
export SNENS_WEB_STATE
unset SNENS_CDP_PORT
t web snap
want 1 "$rc" "web: snap без подключения — код 1"
has "web connect" "$err" "web: snap без подключения — подсказка"
unset SNENS_WEB_STATE

# ---- grep ----
# Структурированный поиск вместо ad-hoc `run 'grep -rn ...'`: путь, строка, текст.
g=$TMP/g
mkdir -p "$g/sub" "$g/target" "$g/skipme"
printf 'alpha\nneedle one\nomega\n' >"$g/a.rs"
printf 'needle two\n' >"$g/b.txt"
printf 'needle three\n' >"$g/sub/c.rs"
printf 'needle в служебном каталоге\n' >"$g/target/d.rs"
printf 'needle \000 бинарник\n' >"$g/bin.dat"

# Находка: формат `путь:строка: текст`, путь относительно переданного корня.
t grep needle "$g" --glob '*.rs'
want 0 "$rc" "grep: код 0 на находке"
has "a.rs:2: needle one" "$out" "grep: формат путь:строка: текст"
has "sub/c.rs:1: needle three" "$out" "grep: обход рекурсивный"
case "$out" in
  *target*) bad "grep: каталог target не пропущен" ;;
  *) ok "grep: каталог target пропущен" ;;
esac
case "$out" in
  *b.txt*) bad "grep: --glob не отфильтровал .txt" ;;
  *) ok "grep: --glob фильтрует по имени" ;;
esac
case "$out" in
  *bin.dat*) bad "grep: бинарный файл не пропущен" ;;
  *) ok "grep: бинарный файл пропущен молча" ;;
esac

# --no-ignore возвращает служебные каталоги.
t grep needle "$g" --no-ignore
has "target/d.rs" "$out" "grep: --no-ignore снимает пропуск каталогов"

# --context: соседние строки с префиксом `-`.
t grep 'needle one' "$g/a.rs" --context 1
has "a.rs:1- alpha" "$out" "grep: контекст с префиксом -"
has "a.rs:2: needle one" "$out" "grep: совпадение с префиксом :"

# --files-only: только имена файлов.
t grep needle "$g" --files-only
want "a.rs
b.txt
sub/c.rs" "$out" "grep: --files-only — одни имена"

# --limit обрезает с явной пометкой.
t grep needle "$g" --limit 1
want 0 "$rc" "grep: --limit — код 0"
has "обрезано: показано 1 из 3" "$out" "grep: --limit — пометка об обрезке"

# Пустой результат — ответ, а не отказ: код 0.
t grep 'нетакогослова' "$g"
want 0 "$rc" "grep: нет совпадений — код 0, а не 1"
has "нет совпадений: нетакогослова в $g" "$out" "grep: нет совпадений — текст"

t grep needle "$TMP/нет-такого"
want 1 "$rc" "grep: отсутствующий путь — код 1"
has "нет такого пути" "$err" "grep: отсутствующий путь — текст"

t grep needle "$g" --nosuch
want 1 "$rc" "grep: неизвестный аргумент — код 1"
has "--nosuch" "$err" "grep: неизвестный аргумент назван по имени"

# `~` доходит буквально — разворачивает сам тул.
ghome=$HOME/.snens-selftest-grep-$$
printf 'домашняя иголка\n' >"$ghome"
t grep иголка '~/.snens-selftest-grep-'$$
want 0 "$rc" "grep: ~ разворачивается в \$HOME"
has "домашняя иголка" "$out" "grep: ~ — файл из \$HOME найден"
rm -f "$ghome"

t grep --help
want 0 "$rc" "grep: --help — код 0"
has "grep — " "$out" "grep: --help, строка 1 = суть"
has "grep <шаблон>" "$out" "grep: --help, строка 2 = usage"

# ---- agent ----
# Тесты могут идти изнутри сессии агента, где dispatch уже выставил SNENS_SESSION
# и SNENS_MAIL_DIR: снимаем их, иначе проверки «без контекста» видят чужой контекст.
unset SNENS_SESSION SNENS_MAIL_DIR SNENS_PARENT 2>/dev/null || true

# Родитель не задан — ask некуда слать.
t agent ask "вопрос"
want 1 "$rc" "agent: ask без родителя — код 1"
has "некого спрашивать" "$err" "agent: ask без родителя — текст"

# Без SNENS_SESSION дерево тоже печатается: себя в нём не знаешь, но структуру
# сессий Mail-ящика видно — это осознанное поведение, а не отказ.
t agent list
want 0 "$rc" "agent: list без сессии — код 0"
has "себя в дереве не знаю" "$out" "agent: list без сессии — объясняет, что себя не знает"

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

# genui: интерфейсные артефакты. Полный набор (рендер в браузере, фикстуры
# песочницы) — рядом, tools/genui/selftest.sh; здесь контракт реестра.
t genui --help
want 0 "$rc" "genui: --help — код 0"
has "genui check" "$out" "genui: --help описывает проверку"

ghome=$TMP/genui-lib
SNENS_GENUI_DIR=$ghome t genui new card
want 0 "$rc" "genui: new — код 0"
[ -f "$TMP/genui-lib/components/card.js" ] && ok "genui: new создал компонент" \
  || bad "genui: new не создал компонент"
SNENS_GENUI_DIR=$ghome t genui list
want 0 "$rc" "genui: list — код 0"
has "my-card" "$out" "genui: list показывает новый тег"

if [ "$fails" -ne 0 ]; then
  echo
  echo "провалено проверок: $fails"
  exit 1
fi
echo
echo "все проверки тулов прошли"