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
        let mut names: Vec<PathBuf> = std::fs::read_dir(&comps)
            .map_err(|e| format!("{}: {e}", comps.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
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
        let mut rest = f.source.as_str();
        while let Some(pos) = rest.find("customElements.define(") {
            let after = &rest[pos + "customElements.define(".len()..];
            let after = after.trim_start();
            let (quote, after) = match after.chars().next() {
                Some(c @ ('"' | '\'')) => (Some(c), &after[1..]),
                _ => (None, after),
            };
            let _ = quote;
            let end = after.find(['"', '\'', ')', ',']).unwrap_or(after.len());
            let tag = after[..end].trim().to_ascii_lowercase();
            if !tag.is_empty() && !out.contains(&tag) {
                out.push(tag);
            }
            rest = &after[end..];
        }
    }
    out
}

/// Отпечаток содержимого: по нему клиент решает, что кэш устарел. Штатный
/// `DefaultHasher` (SipHash) — не криптография, но для кэша этого хватает, и
/// лишней зависимости не требует.
pub fn hash(files: &[File]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    for f in files {
        f.name.hash(&mut h);
        f.source.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

/// Скелет компонента: то, с чего начинается новый файл в `components/`.
pub fn skeleton(name: &str) -> String {
    let tag = if name.starts_with("my-") {
        name.to_string()
    } else {
        format!("my-{name}")
    };
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
"#,
        cls = pascal(name)
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
    let slug = slug.trim_matches('-').replace("--", "-");
    if slug.is_empty() {
        "component.js".into()
    } else {
        format!("{slug}.js")
    }
}

/// Заголовок компонента для `INDEX.md`: первая строка комментария файла.
pub fn describe(source: &str) -> String {
    for line in source.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("//") {
            let rest = rest.trim();
            if !rest.is_empty() && !rest.starts_with('-') {
                return rest.to_string();
            }
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
    fn skeleton_registers_the_requested_tag() {
        let src = skeleton("quiz");
        assert!(src.contains("customElements.define(\"my-quiz\""));
        assert!(src.contains("class Quiz extends HTMLElement"));
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
    }
}
