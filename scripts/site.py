#!/usr/bin/env python3
"""site.py — собрать статический сайт snens (snens.svuvi.ch).

Один домен и два контура:

  content/*.md   человеческие страницы  →  <webroot>/docs/<slug>.html + index.html
  llm/*.txt      машинные файлы для агентов → <webroot>/llm/*.txt
  asset/         style.css и прочее      →  <webroot>/

Контент — единственный источник правды. HTML и плейнтекст никто руками не правит,
поэтому страница и её машинная копия не расходятся.

Разметка — свой минимальный markdown-подмножество (см. RENDER): md-парсера в
зависимостях нет намеренно, а нужен ровно тот набор, который мы пишем.

  scripts/site.py                 # собрать в /var/www/snens.svuvi.ch
  scripts/site.py --out /tmp/site # собрать куда-то ещё (проверка вёрстки)
  scripts/site.py --check         # только проверить контент, ничего не писать
"""

import argparse
import html
import json
import re
import shutil
import sys
import unicodedata
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SITE = REPO / "site" / "snens.svuvi.ch"
CONTENT = SITE / "content"
ASSET = SITE / "asset"
LLM_SRC = SITE / "llm"
BASE_URL = "https://snens.svuvi.ch"

# Порядок страниц — порядок чтения: он же порядок в навигации и в llm/index.txt.
# Первый элемент группы — её заголовок в боковом меню.
GROUPS: list[tuple[str, list[tuple[str, str]]]] = [
    (
        "с нуля",
        [
            ("start", "С чего начать"),
            ("keys", "Где взять ключ"),
        ],
    ),
    (
        "начало",
        [
            ("install", "Установка"),
            ("first-run", "Первый запуск"),
            ("providers", "Провайдеры и ключи"),
        ],
    ),
    (
        "настройка",
        [
            ("config", "Конфиг"),
            ("cli", "Команды"),
            ("limits", "Лимиты и усечения"),
        ],
    ),
    (
        "как это работает",
        [
            ("agent-files", "Файлы инстанса"),
            ("context", "Контекст"),
            ("subagents", "Подагенты"),
            ("surfaces", "Поверхности"),
        ],
    ),
    (
        "расширение",
        [
            ("skills", "Скиллы"),
            ("tools", "Свои инструменты"),
            ("memory", "Долгая память"),
            ("plugins", "Плагины"),
        ],
    ),
    (
        "эксплуатация",
        [
            ("api", "HTTP API"),
            ("security", "Привилегии и доверие"),
            ("operations", "Эксплуатация"),
            ("desktop", "Десктоп"),
        ],
    ),
    (
        "ещё",
        [
            ("faq", "Если не работает"),
            ("license", "Лицензия"),
        ],
    ),
]

# Лендинг живёт вне групп: он не «страница документации».
LANDING = ("index", "snens")
PAGES: list[tuple[str, str]] = [LANDING] + [p for _, pages in GROUPS for p in pages]

# Машинный контур: имя файла → строка описания (она же в llm/index.txt).
LLM_FILES: list[tuple[str, str]] = [
    ("index.txt", "точка входа: что такое snens, что где лежит, правила чтения"),
    ("quickstart.txt", "цикл хода, `run`/`ctx`, env, каталоги, файлы инстанса"),
    ("tools.txt", "справочник по всем инструментам: `--help` и схемы (генерируется)"),
    ("plugins.txt", "расширения: память, скиллы, свой тул"),
    ("practices.txt", "контекст и сжатие, подагенты, тикеты, привилегии"),
]

# ─────────────────────────── markdown ───────────────────────────

FENCE = re.compile(r"^```(\w*)\s*$")
HEADING = re.compile(r"^(#{1,6})\s+(.*)$")
ULIST = re.compile(r"^\s*[-*]\s+(.*)$")
OLIST = re.compile(r"^\s*\d+[.)]\s+(.*)$")
TABLE_SEP = re.compile(r"^\|[-\s:|]+\|$")


LINK = re.compile(r"\[((?:[^\[\]]|\[[^\]]*\])*)\]\(([^)\s]+)\)(\{\.[\w-]+\})?")
CODE = re.compile(r"`([^`]+)`")


def inline(text: str, depth: int = 0) -> str:
    """Инлайн-разметка: код, ссылки, жирный. Один проход с плейсхолдерами.

    Код и ссылки прячутся в маркеры `\x00depth:N\x00` ДО правил жирного: иначе
    `**… `--help` …**` ломается — жирный открывается до кода, а закрывается после,
    и в каждом куске по отдельности пары нет. Содержимое кода больше не
    участвует в разборе; текст ссылки рендерится рекурсивно, и глубина в маркере
    не даёт внутреннему вызову перепутать чужие маркеры со своими.
    Класс в фигурных скобках (`](url){.cta}`) — кнопка: HTML в контент не пускаем.
    """
    stash: list[str] = []
    token = re.compile(rf"\x00{depth}:(\d+)\x00")

    def keep(fragment: str) -> str:
        stash.append(fragment)
        return f"\x00{depth}:{len(stash) - 1}\x00"

    def code(m: re.Match) -> str:
        return keep(f"<code>{m.group(1)}</code>")

    def link(m: re.Match) -> str:
        label = inline(m.group(1), depth + 1)
        cls = m.group(3)
        attr = f' class="{cls[2:-1]}"' if cls else ""
        return keep(f'<a{attr} href="{m.group(2)}">{label}</a>')

    src = html.escape(text, quote=False)
    # Автоссылка `<https://…>` — как в markdown; после escape угловые скобки
    # стали `&lt;`/`&gt;`, ловим их именно в этом виде.
    src = re.sub(
        r"&lt;(https?://[^\s&]+)&gt;",
        lambda m: keep(f'<a href="{m.group(1)}">{m.group(1)}</a>'),
        src,
    )
    src = CODE.sub(code, src)
    src = LINK.sub(link, src)
    src = re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", src)
    while token.search(src):
        src = token.sub(lambda m: stash[int(m.group(1))], src)
    return src


def render(md: str) -> str:
    lines = md.splitlines()
    blocks: list[str] = []
    para: list[str] = []
    i = 0

    def flush() -> None:
        if para:
            text = inline(" ".join(para))
            # Абзац-определение: начинается с жирного, которое кончается точкой
            # (`**Тула нет: … код 1.**`). Так пишутся пункты FAQ и преимущества
            # на лендинге — это единицы чтения, их надо отделять визуально.
            # Жирное с двоеточием (`**ядро**: …`) — обычная проза в начале фразы.
            head = re.match(r"^<strong>(.*?)</strong>", text)
            if head and re.search(r"[.!?…]$", head.group(1)):
                blocks.append(f'<p class="def">{text}</p>')
            else:
                blocks.append(f"<p>{text}</p>")
            para.clear()

    while i < len(lines):
        line = lines[i]
        stripped = line.strip()

        if not stripped:
            flush()
            i += 1
            continue

        m = FENCE.match(stripped)
        if m:
            flush()
            i += 1
            code: list[str] = []
            while i < len(lines) and not FENCE.match(lines[i].strip()):
                code.append(lines[i])
                i += 1
            i += 1  # закрывающий ```
            blocks.append(
                "<pre><code>" + html.escape("\n".join(code), quote=False) + "</code></pre>"
            )
            continue

        m = HEADING.match(stripped)
        if m:
            flush()
            level = len(m.group(1))
            blocks.append(f"<h{level}>{inline(m.group(2))}</h{level}>")
            i += 1
            continue

        # Таблица: строка с |, затем разделитель. Без разделителя это просто текст.
        if stripped.startswith("|") and i + 1 < len(lines) and TABLE_SEP.match(lines[i + 1].strip()):
            flush()
            head = [c.strip() for c in stripped.strip("|").split("|")]
            i += 2
            rows: list[list[str]] = []
            while i < len(lines) and lines[i].strip().startswith("|"):
                rows.append([c.strip() for c in lines[i].strip().strip("|").split("|")])
                i += 1
            out = ["<div class=\"tw\"><table><thead><tr>"]
            out += [f"<th>{inline(c)}</th>" for c in head]
            out.append("</tr></thead><tbody>")
            for r in rows:
                out.append("<tr>" + "".join(f"<td>{inline(c)}</td>" for c in r) + "</tr>")
            out.append("</tbody></table></div>")
            blocks.append("".join(out))
            continue

        if ULIST.match(line) or OLIST.match(line):
            flush()
            pattern = ULIST if ULIST.match(line) else OLIST
            items: list[str] = []
            while i < len(lines):
                m = pattern.match(lines[i])
                if m:
                    items.append(m.group(1))
                    i += 1
                    # Продолжение пункта — последующие строки с отступом. Без
                    # этого они выпадали из списка отдельным абзацем, и текст
                    # вроде «цикл → запись» терял связь с пунктом, к которому
                    # относится. Пустая строка закрывает пункт.
                    while i < len(lines) and lines[i].startswith("  ") and lines[i].strip():
                        nxt = lines[i].strip()
                        if ULIST.match(lines[i]) or OLIST.match(lines[i]):
                            break
                        items[-1] += " " + nxt
                        i += 1
                    continue
                break
            tag = "ul" if pattern is ULIST else "ol"
            blocks.append(f"<{tag}>" + "".join(f"<li>{inline(t)}</li>" for t in items) + f"</{tag}>")
            continue

        if stripped.startswith(">"):
            flush()
            quote: list[str] = []
            while i < len(lines) and lines[i].strip().startswith(">"):
                quote.append(lines[i].strip().lstrip(">").strip())
                i += 1
            blocks.append("<blockquote><p>" + inline(" ".join(quote)) + "</p></blockquote>")
            continue

        para.append(stripped)
        i += 1

    flush()
    return "\n".join(blocks)


def title_of(md: str) -> str:
    for line in md.splitlines():
        if line.startswith("# "):
            return line[2:].strip()
    return "snens"


def summary_of(md: str) -> str:
    """Первый абзац — описание страницы для <meta> и для списка в llm/index.txt."""
    seen_heading = False
    for raw in md.splitlines():
        line = raw.strip()
        if not line:
            continue
        if line.startswith("#"):
            seen_heading = True
            continue
        if seen_heading and not line.startswith(("|", "-", "```", ">")):
            return re.sub(r"[`*\[\]]|\([^)]*\)", "", line)[:200]
    return ""


# ─────────────────────────── вёрстка ───────────────────────────

TEMPLATE = """<!doctype html>
<html lang="ru">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<meta name="description" content="{desc}">
<link rel="stylesheet" href="/style.css">
<link rel="alternate" type="text/plain" href="/llm/index.txt" title="документация для агентов">
</head>
<body>
<header class="bar">
<a class="mark" href="/">snens</a>
<nav class="crumbs">{crumbs}</nav>
</header>
<div class="page{with_side}">
{aside}<main class="{klass}">
{body}
</main>
</div>
<footer class="foot">
<p class="foot__ask">Не знаешь, умеет ли он то, что тебе надо? Спроси агента — он знает свои тулы и ограничения.</p>
<p><a href="/llm/index.txt">документация для агентов (плейнтекст)</a> · <a href="/releases/latest.json">манифест обновлений</a></p>
<p>snens — MIT. Источник: <a href="https://git.svuvi.ch/svuvich/snens">git.svuvi.ch/svuvich/snens</a></p>
</footer>
</body>
</html>
"""


def aside_html(current: str) -> str:
    """Боковое меню страниц документации: группы и активный пункт."""
    out = ['<aside class="side">']
    for title, pages in GROUPS:
        out.append(f'<p class="side__head">{title}</p><ul>')
        for slug, label in pages:
            cls = ' class="on"' if slug == current else ""
            out.append(f'<li><a href="/docs/{slug}.html"{cls}>{label}</a></li>')
        out.append("</ul>")
    out.append("</aside>\n")
    return "".join(out)


def crumbs_html(current: str, label: str) -> str:
    """Шапка — не меню, а признак места: справка и обратная дорога."""
    if current == "index":
        return (
            '<a href="/docs/install.html">документация</a>'
            ' <a href="https://git.svuvi.ch/svuvich/snens">исходники</a>'
        )
    return f'<a href="/">snens</a> / <span>{label}</span>'


def build_page(slug: str, label: str, md: str) -> str:
    body = render(md)
    title = "snens" if slug == "index" else f"{label} — snens"
    landing = slug == "index"
    return TEMPLATE.format(
        title=html.escape(title),
        desc=html.escape(summary_of(md)),
        crumbs=crumbs_html(slug, label),
        with_side="" if landing else " with-side",
        aside="" if landing else aside_html(slug),
        klass="landing" if landing else "doc",
        body=body,
    )


# ─────────────────────────── сборка ───────────────────────────

def latest_installer(releases: Path) -> str | None:
    """Ссылка на установщик из манифеста, переписанная на свой домен.

    Манифест собирает release.sh и кладёт артефакты в `/releases/` этого же
    сайта: один домен на всё (лендинг, доки, релизы). Старые адреса, если они
    остались в манифесте, сохраняют только путь — домен у нас один.
    """
    manifest = releases / "latest.json"
    if not manifest.is_file():
        return None
    try:
        data = json.loads(manifest.read_text())
    except json.JSONDecodeError:
        return None
    for info in data.get("platforms", {}).values():
        url = info.get("url", "")
        if not url:
            continue
        # Один домен на всё: из манифеста берём только путь. Старый префикс
        # `/snens/` (до переезда релизов под сайт) убираем.
        m = re.match(r"^https?://[^/]+(/.*)$", url)
        path = m.group(1) if m else url
        if path.startswith("/snens/"):
            path = "/releases/" + path[len("/snens/") :]
        return path
    return None


def main() -> int:
    ap = argparse.ArgumentParser(description="собрать сайт snens.svuvi.ch")
    ap.add_argument("--out", default="/var/www/snens.svuvi.ch")
    ap.add_argument("--check", action="store_true", help="только проверить контент")
    ap.add_argument(
        "--releases",
        default="/var/www/snens.svuvi.ch/releases",
        help="каталог с latest.json (для ссылки на установщик)",
    )
    args = ap.parse_args()

    fail: list[str] = []
    pages: dict[str, str] = {}

    for slug, label in PAGES:
        path = CONTENT / f"{slug}.md"
        if not path.is_file():
            fail.append(f"нет страницы: {path.relative_to(REPO)}")
            continue
        md = path.read_text()
        if len(md) < 200:
            fail.append(f"страница подозрительно короткая: {slug}.md ({len(md)} Б)")
        # Разрешён только один плейсхолдер — адрес установщика из latest.json.
        leftovers = md.replace("{{INSTALLER}}", "")
        if "{{" in leftovers:
            fail.append(f"в {slug}.md остался плейсхолдер {{{{…}}}}")
        if not md.lstrip().startswith("# "):
            fail.append(f"{slug}.md должен начинаться с заголовка `# `")
        pages[slug] = md

    for name, _ in LLM_FILES:
        path = LLM_SRC / name
        if not path.is_file():
            fail.append(f"нет машинного файла: {path.relative_to(REPO)}")
        elif len(path.read_text()) < 150:
            fail.append(f"машинный файл пуст: {name}")

    llm_index = LLM_SRC / "index.txt"
    if llm_index.is_file():
        text = llm_index.read_text()
        # index.txt — точка входа: он перечисляет все остальные файлы и
        # сам себя в списке не дублирует.
        for name, _ in LLM_FILES:
            if name == "index.txt":
                continue
            if f"{BASE_URL}/llm/{name}" not in text:
                fail.append(f"llm/index.txt не ссылается на /llm/{name}")

    # Внутренние ссылки: битая ссылка на сайте хуже её отсутствия, а поймать её
    # глазами по 21 странице нельзя.
    known = {slug for slug, _ in PAGES}
    for slug, _, body in [(s, l, (CONTENT / f"{s}.md").read_text()) for s, l in PAGES if s in pages]:
        for target in re.findall(r"\]\((/docs/([a-z-]+)\.html)\)", body):
            if target[1] not in known:
                fail.append(f"{slug}.md ссылается на несуществующую страницу {target[0]}")
        for target in re.findall(r"\]\(/llm/([a-z]+\.txt)\)", body):
            if target not in {n for n, _ in LLM_FILES}:
                fail.append(f"{slug}.md ссылается на несуществующий файл /llm/{target}")

    if fail:
        print("контент не готов:", file=sys.stderr)
        for f in fail:
            print(f"  {f}", file=sys.stderr)
        return 1

    if args.check:
        print(f"контент в порядке: {len(pages)} страниц, {len(LLM_FILES)} машинных файлов")
        return 0

    out = Path(args.out)
    docs = out / "docs"
    docs.mkdir(parents=True, exist_ok=True)
    (out / "llm").mkdir(parents=True, exist_ok=True)

    installer = latest_installer(Path(args.releases))
    subs = {"INSTALLER": installer or "/docs/install.html"}

    written: list[Path] = []
    for slug, label in PAGES:
        md = pages[slug].replace("{{INSTALLER}}", subs["INSTALLER"])
        target = out / "index.html" if slug == "index" else docs / f"{slug}.html"
        target.write_text(build_page(slug, label, md))
        written.append(target)

    for name, _ in LLM_FILES:
        shutil.copyfile(LLM_SRC / name, out / "llm" / name)
        written.append(out / "llm" / name)

    for asset in ASSET.iterdir():
        if asset.is_file():
            shutil.copyfile(asset, out / asset.name)
            written.append(out / asset.name)

    for path in written:
        path.chmod(0o644)
    for d in (out, docs, out / "llm"):
        d.chmod(0o755)

    print(f"собрано: {out}")
    print(f"  страниц: {len(PAGES)}  машинных файлов: {len(LLM_FILES)}")
    print(f"  установщик: {installer or '— (нет latest.json, кнопка ведёт на страницу установки)'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
