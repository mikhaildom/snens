// HTML → markdown: выбросить мусор DOM, предпочесть main/article, сконвертировать
// и подчистить пустые строки. Отдельный модуль — main.rs держим читаемым.
use scraper::{Html, Selector};

/// Теги, которые в markdown не нужны: навигация, скрипты, каркас страницы.
const DROP: &[&str] = &[
    "script", "style", "noscript", "svg", "nav", "header", "footer", "aside", "form", "iframe",
];

/// Очистить DOM и вернуть markdown. `raw` отключает очистку (конвертируется всё body).
pub fn to_markdown(html: &str, raw: bool) -> String {
    let mut doc = Html::parse_document(html);

    if !raw {
        let sel = Selector::parse(DROP.join(",").as_str()).expect("селектор drop");
        let ids: Vec<_> = doc.select(&sel).map(|e| e.id()).collect();
        for id in ids {
            // detach отвязывает узел вместе с поддеревом: повторно к детям не ходим.
            if let Some(mut n) = doc.tree.get_mut(id) {
                n.detach();
            }
        }
    }

    let body = Selector::parse("body").expect("body");
    let root = if raw {
        doc.select(&body).next()
    } else {
        // main/article — если есть; иначе body; иначе корень документа.
        ["main", "article"]
            .iter()
            .find_map(|s| doc.select(&Selector::parse(s).unwrap()).next())
            .or_else(|| doc.select(&body).next())
    };

    let inner = match root {
        Some(el) => el.inner_html(),
        None => doc.html(),
    };
    let md = htmd::convert(&inner).unwrap_or_default();
    squeeze_table_padding(collapse_blank_lines(&md).trim())
}

fn align_marker(cell: &str) -> &'static str {
    match (cell.starts_with(':'), cell.ends_with(':')) {
        (true, true) => ":---:",
        (true, false) => ":---",
        (false, true) => "---:",
        _ => "---",
    }
}

/// htmd выравнивает таблицы пробелами: в инфобоксе Википедии это больше половины
/// объёма markdown впустую. Выравнивание колонок в markdown ничего не значит —
/// ячейки разделяет `|`, так что серии подряд идущих пробелов сжимаем в один.
/// Трогаем только строки таблиц: двойной пробел в абзаце — это перенос строки.
fn squeeze_table_padding(md: &str) -> String {
    md.lines()
        .map(|line| {
            if !line.starts_with('|') {
                return line.to_string();
            }
            // Строка-разделитель шапки состоит из „|“, „-“, „:“ и пробелов: длину
            // дефисов markdown не значит, а в инфобоксе она растянута на весь текст.
            if line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')) && line.contains('-') {
                return line
                    .split('|')
                    .map(|cell| {
                        let t = cell.trim();
                        if t.is_empty() {
                            String::new()
                        } else {
                            format!(" {} ", align_marker(t))
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("|");
            }
            let mut out = String::with_capacity(line.len());
            let mut run = false;
            for c in line.chars() {
                if c == ' ' {
                    if !run {
                        out.push(' ');
                    }
                    run = true;
                } else {
                    run = false;
                    out.push(c);
                }
            }
            out
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Сколько символов в тексте без пробелов: порог «страница пустая, нужен фолбэк».
pub fn text_len(s: &str) -> usize {
    s.chars().filter(|c| !c.is_whitespace()).count()
}

/// Не более одной пустой строки подряд: хтмл-конвертеры щедры на пустоту.
pub fn collapse_blank_lines(s: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut blanks = 0usize;
    for line in s.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            continue;
        }
        // Перед непустой строкой отдаём не больше одного перевода строки.
        if !out.is_empty() && blanks > 0 {
            out.push("");
        }
        blanks = 0;
        out.push(line.trim_end());
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!doctype html><html><head>
        <style>body{color:red}</style><script>alert(1)</script></head>
        <body>
        <nav><a href="/">главная</a><a href="/x">ещё</a></nav>
        <header>шапка сайта</header>
        <main>
          <h1>Заголовок</h1>
          <p>Первый абзац.</p>
          <p>Второй абзац.</p>
        </main>
        <footer>подвал</footer>
        </body></html>"#;

    #[test]
    fn drops_nav_and_scripts_keeps_main() {
        let md = to_markdown(PAGE, false);
        assert!(md.contains("Заголовок"), "заголовок потерян: {md}");
        assert!(md.contains("Первый абзац."), "текст потерян: {md}");
        assert!(md.contains("Второй абзац."), "второй абзац потерян: {md}");
        assert!(!md.contains("главная"), "навигация не выброшена: {md}");
        assert!(!md.contains("шапка"), "header не выброшен: {md}");
        assert!(!md.contains("подвал"), "footer не выброшен: {md}");
        assert!(!md.contains("alert"), "script не выброшен: {md}");
        assert!(!md.contains("color:red"), "style не выброшен: {md}");
    }

    #[test]
    fn raw_keeps_navigation() {
        let md = to_markdown(PAGE, true);
        assert!(md.contains("главная"), "raw должен сохранять nav: {md}");
    }

    #[test]
    fn collapses_runs_of_blank_lines() {
        assert_eq!(collapse_blank_lines("a\n\n\n\nb"), "a\n\nb");
        assert_eq!(collapse_blank_lines("a\n\nb"), "a\n\nb");
        assert_eq!(collapse_blank_lines("a\n\n\nb\n\n\nc"), "a\n\nb\n\nc");
        assert_eq!(collapse_blank_lines("\n\n\na"), "a");
    }

    #[test]
    fn table_padding_squeezed_only_in_table_rows() {
        let md = "| Rust     | Team   |\n| -------- | -----: |\nабзац  с  двойным пробелом";
        let out = squeeze_table_padding(md);
        assert!(
            out.contains("| Rust | Team |"),
            "паддинг таблицы не сжат: {out}"
        );
        assert!(out.contains("| --- | ---: |"), "разделитель не сжат: {out}");
        assert!(out.contains("абзац  с  двойным"), "абзац тронут: {out}");
    }

    #[test]
    fn text_len_ignores_whitespace() {
        assert_eq!(text_len("  \n\t a b "), 2);
    }
}
