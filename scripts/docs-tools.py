#!/usr/bin/env python3
"""Собрать справку по инструментам snens в один plaintext.

Источники фактов (и только они):
  * tools/<имя>/src/*.rs           — const HELP (Rust-строковый литерал);
  * tools/<имя>/schema.json        — properties / required / x-snens;
  * src/dispatch.rs::builtin_defs  — описания встроенных run и ctx.

Печатает в stdout; с `--write PATH` пишет атомарно (temp + rename).
Только stdlib, вывод детерминирован: ни дат, ни путей с домашним каталогом.
"""

import argparse
import json
import os
import re
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TOOLS_DIR = os.path.join(ROOT, "tools")
DISPATCH = os.path.join(ROOT, "src", "dispatch.rs")

HEADER = [
    "tools.txt — справочник по инструментам snens",
    "Источник: --help тулов (tools/*/src), tools/*/schema.json, встроенные run и ctx "
    "(src/dispatch.rs). Сгенерировано scripts/docs-tools.py — руками не править. "
    "URL: https://snens.svuvi.ch/llm/tools.txt",
]

GENERIC = (
    "Контракт: generic — args (array|string) + stdin (string); "
    "разбор argv делает сам тул."
)

_SIMPLE_ESCAPES = {
    "n": "\n",
    "r": "\r",
    "t": "\t",
    "\\": "\\",
    '"': '"',
    "'": "'",
    "0": "\0",
}


class DocsError(Exception):
    """Ошибка сборки: справка не нашлась или источник не разобрался."""


def decode_rust_string(src, i, where):
    """Декодировать Rust-строковый литерал; i — сразу за открывающей кавычкой.

    Понимает escapes \\" \\\\ \\n \\t \\r \\0 \\xNN \\u{...} и продолжение строки
    (обратный слэш + перевод строки + ведущие пробелы следующей строки).
    """
    out = []
    n = len(src)
    while i < n:
        ch = src[i]
        if ch == '"':
            return "".join(out)
        if ch != "\\":
            out.append(ch)
            i += 1
            continue
        i += 1
        if i >= n:
            break
        e = src[i]
        if e == "\n":  # продолжение строки: хвост среза выбрасывается
            i += 1
            while i < n and src[i] in " \t\r\n":
                i += 1
            continue
        if e in _SIMPLE_ESCAPES:
            out.append(_SIMPLE_ESCAPES[e])
            i += 1
            continue
        if e == "x":
            # Битый hex — не DocsError, а ValueError, и main() печатал бы сырой
            # traceback вместо одной строки `docs-tools: ...`.
            try:
                out.append(chr(int(src[i + 1 : i + 3], 16)))
            except ValueError:
                raise DocsError("%s: некорректный escape \\x" % where)
            i += 3
            continue
        if e == "u":
            close = src.find("}", i)
            if src[i + 1 : i + 2] != "{" or close < 0:
                raise DocsError("%s: неизвестный escape \\u" % where)
            try:
                out.append(chr(int(src[i + 2 : close], 16)))
            except ValueError:
                # В том числе code point > 0x10FFFF и суррогаты.
                raise DocsError("%s: некорректный escape \\u{...}" % where)
            i = close + 1
            continue
        raise DocsError("%s: неизвестный escape \\%s" % (where, e))
    raise DocsError("%s: незакрытый строковый литерал" % where)


def const_str(src, name, where):
    """Значение `const <name>: &str = "...";` или None, если константы нет."""
    m = re.search(r"\bconst\s+%s\s*:\s*&str\s*=\s*" % re.escape(name), src)
    if not m:
        return None
    start = m.end()
    if src[start : start + 1] != '"':
        raise DocsError("%s: const %s — не строковый литерал" % (where, name))
    return decode_rust_string(src, start + 1, where)


def read_text(path):
    with open(path, "r", encoding="utf-8") as f:
        return f.read()


def tool_dirs():
    dirs = sorted(
        d
        for d in os.listdir(TOOLS_DIR)
        if os.path.isdir(os.path.join(TOOLS_DIR, d))
    )
    # Каталог без src/main.rs — библиотечный крейт (`tools/cdp`: транспорт для
    # `shot` и `genui`), а не тул реестра. То же правило, что в src/registry.rs.
    return [
        d
        for d in dirs
        if not (
            os.path.isfile(os.path.join(TOOLS_DIR, d, "Cargo.toml"))
            and not os.path.isfile(os.path.join(TOOLS_DIR, d, "src", "main.rs"))
        )
    ]


def rust_sources(tool_dir):
    src = os.path.join(TOOLS_DIR, tool_dir, "src")
    if not os.path.isdir(src):
        return []
    names = sorted(n for n in os.listdir(src) if n.endswith(".rs"))
    # main.rs первым: HELP обычно там, остальные — по алфавиту.
    names.sort(key=lambda n: (n != "main.rs", n))
    return [os.path.join(src, n) for n in names]


def tool_help(name):
    """Полный HELP-текст тула; нет справки — ошибка."""
    for path in rust_sources(name):
        text = const_str(read_text(path), "HELP", "tools/%s/src/%s" % (name, os.path.basename(path)))
        if text is not None:
            return text
    raise DocsError("tools/%s: не найдена справка (const HELP)" % name)


def schema_pairs(path, where):
    """JSON с сохранением порядка ключей и запретом дублей."""
    def hook(pairs):
        keys = [k for k, _ in pairs]
        if len(keys) != len(set(keys)):
            raise DocsError("%s: повторяющийся ключ в объекте JSON" % where)
        return dict(pairs)

    try:
        return json.loads(read_text(path), object_pairs_hook=hook)
    except DocsError:
        raise
    except ValueError as e:
        raise DocsError("%s: schema.json не парсится как JSON: %s" % (where, e))


def field_lines(schema, where):
    props = schema.get("properties") or {}
    if not isinstance(props, dict):
        raise DocsError("%s: properties — не объект" % where)
    required = schema.get("required") or []
    if not isinstance(required, list):
        raise DocsError("%s: required — не массив" % where)
    lines = []
    for fname, spec in props.items():
        if not isinstance(spec, dict):
            raise DocsError("%s: поле %s — не объект схемы" % (where, fname))
        ftype = spec.get("type", "")
        if isinstance(ftype, list):
            ftype = "|".join(str(t) for t in ftype)
        head = "- %s: %s" % (fname, ftype)
        if fname in required:
            head += ", обязательное"
        desc = spec.get("description")
        if desc:
            head += " — %s" % desc
        enum = spec.get("enum")
        if isinstance(enum, list) and enum:
            head += "; значения: %s" % "|".join(str(v) for v in enum)
        lines.append(head)
    return lines


def stdin_line(schema, where):
    xsnens = schema.get("x-snens") or {}
    if not isinstance(xsnens, dict):
        raise DocsError("%s: x-snens — не объект" % where)
    rule = xsnens.get("stdin")
    # «Ключ отсутствует» и «ключ есть, но пустой» — разные вещи: `"stdin": {}`
    # или `null` — ошибка автора схемы, а не «stdin: none».
    if rule is None:
        rule = {"mode": "none"}
    if not isinstance(rule, dict):
        raise DocsError("%s: x-snens.stdin — не объект" % where)
    if not rule:
        raise DocsError("%s: x-snens.stdin — пустой объект" % where)
    if "mode" not in rule:
        raise DocsError("%s: x-snens.stdin без mode" % where)
    mode = rule.get("mode")
    if mode == "none":
        return "stdin: none"
    if mode == "field":
        field = rule.get("field")
        if not field:
            raise DocsError("%s: x-snens.stdin.mode=field без поля" % where)
        return "stdin: field (сырое значение поля %s)" % field
    if mode == "json":
        fields = rule.get("fields")
        if not isinstance(fields, list) or not fields:
            raise DocsError("%s: x-snens.stdin.mode=json без списка полей" % where)
        return "stdin: json (объект из полей %s)" % ", ".join(str(f) for f in fields)
    raise DocsError("%s: неизвестный x-snens.stdin.mode: %s" % (where, mode))


def tool_contract(name):
    """Блок контракта: поля схемы + stdin, либо generic."""
    path = os.path.join(TOOLS_DIR, name, "schema.json")
    if not os.path.isfile(path):
        return GENERIC
    where = "tools/%s/schema.json" % name
    schema = schema_pairs(path, where)
    if not isinstance(schema, dict):
        raise DocsError("%s: схема — не объект JSON" % where)
    lines = ["Поля (schema.json):"]
    lines.extend(field_lines(schema, where))
    lines.append(stdin_line(schema, where))
    return "\n".join(lines)


def builtins():
    """Пары (имя, description) из src/dispatch.rs::builtin_defs() — в порядке исходника."""
    where = "src/dispatch.rs"
    src = read_text(DISPATCH)
    m = re.search(r"\bfn\s+builtin_defs\s*\(", src)
    if not m:
        raise DocsError("%s: не найдена функция builtin_defs()" % where)
    # Тело функции — до следующего определения верхнего уровня. Список шире,
    # чем было: пропущенный `const`/`use`/`struct` расширял `body` на остаток
    # файла, и оттуда подхватывались чужие "name"/"description".
    tail = re.search(
        r"\n(?:pub |fn |async |impl |#\[|const |static |struct |enum |type |mod |use |trait )",
        src[m.end() :],
    )
    body_end = m.end() + tail.start() if tail else len(src)
    body = src[m.end() : body_end]
    out = []
    pos = 0
    while True:
        # Только начало строки: внутри уже съеденного description литерал
        # `"name": "foo"` иначе был бы принят за следующий встроенный тул.
        nm = re.search(r'\n\s*"name"\s*:\s*(?="[^"]*")', body[pos:])
        if not nm:
            break
        # `+1` — перешагнуть открывающую кавычку: decode_rust_string ждёт индекс
        # сразу за ней.
        name_at = pos + nm.end() + 1
        name = decode_rust_string(body, name_at, where)
        dm = re.search(r'"description"\s*:\s*(?=")', body[name_at:])
        if not dm:
            raise DocsError("%s: у %s нет description" % (where, name))
        desc_at = name_at + dm.end() + 1
        desc = decode_rust_string(body, desc_at, where)
        # Ядро подставляет реальную оболочку из конфига из кода; в документации
        # показываем дефолт unix — символ в тексте был бы неправдой.
        desc = desc.replace("$shell", "sh -c")
        out.append((name, desc))
        # Дальше — ЗА конец description, а не в его начало: иначе повторный
        # поиск ходил бы по тексту уже прочитанного литерала.
        pos = desc_at
    if not out:
        raise DocsError("%s: в builtin_defs() не нашлось ни одного описания" % where)
    return out


def render():
    names = tool_dirs()
    if not names:
        raise DocsError("tools/: нет ни одного тула")

    parts = ["\n".join(HEADER)]
    parts.append("## Встроенные в ядро")
    # Список встроенных читаем один раз: он нужен и телу, и итоговой строке —
    # захардкоженная «2» врала бы после любого добавления.
    builtin = builtins()
    for name, desc in builtin:
        parts.append("### %s\n\n%s" % (name, desc))
    parts.append("## Тулы реестра")
    for name in names:
        parts.append("### %s\n\n%s\n\n%s" % (name, tool_help(name), tool_contract(name)))
    parts.append(
        "Всего: %d тулов реестра + %d встроенных." % (len(names), len(builtin))
    )
    return "\n\n".join(parts) + "\n"


def write_atomic(path, text):
    directory = os.path.dirname(os.path.abspath(path)) or "."
    os.makedirs(directory, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=directory, prefix=".docs-tools-", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as f:
            f.write(text)
        # `mkstemp` создаёт файл 0600, а `os.replace` сохраняет режим inode:
        # публичный tools.txt остался бы `-rw-------`. Ставим обычные 0644 по
        # umask — так файл читается и человеком, и раздатчиком.
        umask = os.umask(0)
        os.umask(umask)
        os.chmod(tmp, 0o666 & ~umask)
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def main(argv):
    ap = argparse.ArgumentParser(
        prog="docs-tools.py",
        description="Справка по инструментам snens (tools.txt) из --help тулов, схем и ядра.",
    )
    ap.add_argument("--write", metavar="PATH", help="записать вывод в файл атомарно (temp + rename)")
    args = ap.parse_args(argv)

    try:
        text = render()
    except DocsError as e:
        sys.stderr.write("docs-tools: %s\n" % e)
        return 1
    except OSError as e:
        sys.stderr.write("docs-tools: %s\n" % e)
        return 1

    if args.write:
        try:
            write_atomic(args.write, text)
        except OSError as e:
            sys.stderr.write("docs-tools: %s: %s\n" % (args.write, e))
            return 1
        return 0

    out = getattr(sys.stdout, "buffer", None)
    data = text.encode("utf-8")
    try:
        if out is not None:
            out.write(data)
            out.flush()
        else:
            sys.stdout.write(text)
    except BrokenPipeError:
        # `docs-tools.py | head -1`: труба уже закрыта — это не ошибка, но без
        # перехвата печатался бы трейсбек и «Exception ignored».
        try:
            sys.stdout.close()
        except OSError:
            pass
        return 0
    except OSError as e:
        sys.stderr.write("docs-tools: не записать вывод: %s\n" % e)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
