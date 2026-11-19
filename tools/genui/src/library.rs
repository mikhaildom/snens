//! Библиотека компонентов генеративного UI.
//!
//! Лежит в данных инстанса (`~/.local/share/snens/genui`), а не в репозитории:
//! это рабочий инструмент конкретного инстанса, он растёт из тикетов, а не из
//! релизов харнесса. Клиент тянет её через `GET /v1/genui` и инжектит в каждый
//! артефакт; `genui check` берёт ровно те же файлы.
//!
//! Соглашение об именах: имя файла в `components/` — это имя тега без префикса
//! (`quiz.js` → `<my-quiz>`). Так `INDEX.md` и список тегов не расходятся.

use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct File {
    /// Имя файла (`lib.js`, `components/quiz.js`).
    pub name: String,
    /// Исходник как есть: он уезжает в `srcdoc` инлайном.
    pub source: String,
}

/// Каталог библиотеки: `SNENS_GENUI_DIR`, иначе `<данные>/genui`.
pub fn dir() -> PathBuf {
    if let Ok(d) = std::env::var("SNENS_GENUI_DIR") {
        if !d.trim().is_empty() {
            return PathBuf::from(d);
        }
    }
    let base = std::env::var("SNENS_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".local/share/snens")
        });
    base.join("genui")
}

/// Прочитать библиотеку: `lib.js`, потом компоненты по алфавиту. Порядок важен —
/// компоненты могут пользоваться утилитами из `lib.js`.
pub fn read() -> Result<Vec<File>, String> {
    let dir = dir();
    let mut out = Vec::new();
    let lib = dir.join("lib.js");
    if lib.is_file() {
        out.push(File {
            name: "lib.js".into(),
            source: std::fs::read_to_string(&lib).map_err(|e| format!("{}: {e}", lib.display()))?,
        });
    }
    let comps = dir.join("components");
    if comps.is_dir() {
        // Ошибку обхода не глотаем: молча неполная библиотека даёт успешный
        // `check` на части компонентов и нестабильный отпечаток кэша.
        let mut names: Vec<PathBuf> = std::fs::read_dir(&comps)
            .map_err(|e| format!("{}: {e}", comps.display()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("{}: {e}", comps.display()))?
            .into_iter()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("js"))
            .collect();
        names.sort();
        for p in names {
            let name = format!(
                "components/{}",
                p.file_name().and_then(|n| n.to_str()).unwrap_or("?")
            );
            out.push(File {
                name,
                source: std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?,
            });
        }
    }
    Ok(out)
}

/// Теги, которые библиотека регистрирует: `customElements.define("my-quiz"`.
///
/// Читаем исходники, а не спрашиваем браузер: `lint` обязан работать и там, где
/// хрома нет (деградация до статики).
pub fn tags(files: &[File]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in files {
        // Комментарии и строки не должны давать фантомных тегов: пример вызова
        // `customElements.define(...)` в документации — не регистрация.
        let code = strip_comments(&f.source);
        let mut rest = code.as_str();
        while let Some(pos) = rest.find("customElements.define(") {
            let after = &rest[pos + "customElements.define(".len()..];
            let after = after.trim_start();
            // Кавычка обязательна: `define(myVar, …)` — не строковый тег.
            let Some(quote) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else {
                rest = after;
                continue;
            };
            let body = &after[1..];
            let end = body.find(quote).unwrap_or(body.len());
            let tag = body[..end].trim().to_ascii_lowercase();
            if !tag.is_empty() && valid_tag(&tag) && !out.contains(&tag) {
                out.push(tag);
            }
            rest = &body[end..];
        }
    }
    out
}

fn valid_tag(t: &str) -> bool {
    t.contains('-')
        && t.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

/// Исходник без комментариев: пример вызова в документации — не регистрация тега.
/// Строки не трогаем: имя тега само живёт в строковом литерале.
fn strip_comments(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        // Строчный комментарий.
        if c == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Блочный комментарий.
        if c == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            out.push(' ');
            continue;
        }
        // Строка или шаблон: копируем целиком, чтобы не спутать `//` внутри неё
        // с комментарием.
        if c == b'"' || c == b'\'' || c == b'`' {
            let quote = c;
            out.push(quote as char);
            i += 1;
            while i < b.len() && b[i] != quote {
                if b[i] == b'\\' && i + 1 < b.len() {
                    out.push(b[i] as char);
                    i += 1;
                }
                out.push(b[i] as char);
                i += 1;
            }
            if i < b.len() {
                out.push(quote as char);
                i += 1;
            }
            continue;
        }
        out.push(c as char);
        i += 1;
    }
    out
}

/// Отпечаток содержимого: по нему клиент решает, что кэш устарел.
/// FNV-1a, а не `DefaultHasher`: у того алгоритм не зафиксирован и меняется
/// между версиями компилятора — отпечаток обязан быть воспроизводимым между
/// сборками харнесса и клиента.
pub fn hash(files: &[File]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for f in files {
        for b in f
            .name
            .as_bytes()
            .iter()
            .chain(b"\0")
            .chain(f.source.as_bytes())
        {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

/// Скелет компонента: то, с чего начинается новый файл в `components/`.
/// Тег и класс выводятся из того же slug, что и имя файла: `my-Poll Card` —
/// невалидное имя кастомного элемента, а `class 3dCard` — синтаксическая ошибка.
pub fn skeleton(name: &str) -> String {
    let slug = file_name(name);
    let slug = slug.trim_end_matches(".js");
    let tag = format!("my-{slug}");
    let cls = pascal(slug);
    format!(
        r#"// <{tag}> — что это и когда использовать.
//
// Контракт библиотеки:
//  - тема — только через CSS-переменные (--paper, --ink, --accent, --hair);
//  - данные — из атрибутов или `snens.data`, сети нет (CSP без connect-src);
//  - вопрос человеку — `snens.answer({{id, kind, question, value}})`.
class {cls} extends HTMLElement {{
  connectedCallback() {{
    if (this.dataset.ready) return;
    this.dataset.ready = "1";
    const root = this.attachShadow({{ mode: "open" }});
    root.innerHTML = `
      <style>
        :host {{ display: block; font-family: var(--mono); color: var(--ink); }}
        button {{
          font: inherit; padding: 2px 8px; cursor: pointer;
          background: var(--paper-2); color: var(--ink);
          border: 1px solid var(--line);
        }}
        button:hover {{ background: var(--accent); color: var(--accent-ink); }}
      </style>
      <slot></slot>
    `;
  }}
}}
customElements.define("{tag}", {cls});
"#
    )
}

fn pascal(name: &str) -> String {
    let mut out = String::new();
    for part in name.split(|c: char| !c.is_ascii_alphanumeric()) {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.extend(chars);
        }
    }
    // Имя класса не может начинаться с цифры: `3d-card` → `C3dCard`.
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, 'C');
    }
    if out.is_empty() {
        "Component".into()
    } else {
        out
    }
}

/// Имя файла по имени тега: `my-quiz` → `quiz.js`.
pub fn file_name(name: &str) -> String {
    let base = name.strip_prefix("my-").unwrap_or(name);
    let slug: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    // Серии разделителей схлопываем до одного: `replace` делает один проход,
    // и `a---b` превращалось в `a--b`.
    let mut slug = slug.trim_matches('-').to_string();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    if slug.is_empty() {
        "component.js".into()
    } else {
        format!("{slug}.js")
    }
}

/// Заголовок компонента для `INDEX.md`: первая строка **шапки** файла —
/// ведущего блока `//`-комментариев. Комментарий выше внутреннего помощника
/// описанием компонента не является, поэтому на первом же непустом
/// не-комментарии поиск прекращается.
pub fn describe(source: &str) -> String {
    for line in source.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Some(rest) = t.strip_prefix("//") else {
            return String::new();
        };
        let rest = rest.trim();
        if !rest.is_empty() && !rest.starts_with('-') {
            return rest.to_string();
        }
    }
    String::new()
}

/// Есть ли каталог библиотеки вообще (для `list`: пустая библиотека — норма).
pub fn exists(dir: &Path) -> bool {
    dir.is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_read_from_define_calls() {
        let files = vec![File {
            name: "components/quiz.js".into(),
            source: "customElements.define('my-quiz', class extends HTMLElement{});".into(),
        }];
        assert_eq!(tags(&files), vec!["my-quiz".to_string()]);
    }

    #[test]
    fn commented_define_is_not_a_tag() {
        let files = vec![File {
            name: "components/doc.js".into(),
            source: "// customElements.define('my-ghost', class {});\n\
                     /* customElements.define('my-ghost2', class {}); */\n\
                     customElements.define(\"my-real\", class extends HTMLElement{});"
                .into(),
        }];
        assert_eq!(tags(&files), vec!["my-real".to_string()]);
    }

    #[test]
    fn skeleton_registers_the_requested_tag() {
        let src = skeleton("quiz");
        assert!(src.contains("customElements.define(\"my-quiz\""));
        assert!(src.contains("class Quiz extends HTMLElement"));
    }

    #[test]
    fn skeleton_tag_and_class_follow_the_file_slug() {
        // Имя человеческое: файл — poll-card.js, тег — my-poll-card, класс —
        // PollCard. Раньше тег получался «my-Poll Card» (невалидное имя).
        let src = skeleton("Poll Card");
        assert!(
            src.contains("customElements.define(\"my-poll-card\""),
            "{src}"
        );
        assert!(src.contains("class PollCard extends HTMLElement"), "{src}");
        assert_eq!(file_name("Poll Card"), "poll-card.js");
        // Ведущая цифра: класс не может начинаться с неё.
        let src = skeleton("3d-card");
        assert!(src.contains("class C3dCard extends HTMLElement"), "{src}");
        assert!(
            src.contains("customElements.define(\"my-3d-card\""),
            "{src}"
        );
    }

    #[test]
    fn file_name_strips_the_prefix() {
        assert_eq!(file_name("my-quiz"), "quiz.js");
        assert_eq!(file_name("Poll Card"), "poll-card.js");
        assert_eq!(file_name("my-"), "component.js");
    }

    #[test]
    fn hash_changes_with_content() {
        let a = vec![File {
            name: "lib.js".into(),
            source: "1".into(),
        }];
        let b = vec![File {
            name: "lib.js".into(),
            source: "2".into(),
        }];
        assert_ne!(hash(&a), hash(&b));
        assert_eq!(hash(&a), hash(&a.clone()));
    }

    #[test]
    fn describe_takes_first_comment_line() {
        assert_eq!(describe("// Квиз\n// ещё"), "Квиз");
        assert_eq!(describe("class A {}"), "");
        // Комментарий глубоко в файле — не описание компонента.
        assert_eq!(describe("class A {}\n// helper\n"), "");
        // Шапка с пустой строкой внутри остаётся шапкой.
        assert_eq!(describe("// Квиз\n\n// ещё\nclass A {}"), "Квиз");
    }
}
