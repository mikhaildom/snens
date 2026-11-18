// Разбор файла в скелет: единственное место, где создаётся парсер. Языковые
// модули получают готовое дерево и не знают про tree-sitter::Parser.
use tree_sitter::Parser;

use crate::lang;
use crate::render;

/// Скелет исходника. `None` — язык не поддержан; `Err` — файл не разобрался
/// (тогда модель должна читать его целиком, а не получить пустоту).
pub fn skeleton_of(src: &str, ext: &str) -> Option<String> {
    let language = lang::language(ext)?;
    Some(skeleton_with(src, ext, &language))
}

fn skeleton_with(src: &str, ext: &str, language: &tree_sitter::Language) -> String {
    let mut parser = Parser::new();
    if parser.set_language(language).is_err() {
        return String::new();
    }
    let Some(tree) = parser.parse(src, None) else {
        return String::new();
    };
    let root = tree.root_node();
    // Файл с ошибкой разбора всё равно даёт полезные узлы: shell-скрипты с
    // heredoc, шаблоны и диалекты парсятся частично. Скелет печатаем, но
    // помечаем — модель должна знать, что часть файла могла не попасть.
    let lang = module_of(ext);
    let layout = match lang {
        "rust" => lang::rust::skeleton(root, src),
        "python" => lang::python::skeleton(root, src),
        "typescript" => lang::typescript::skeleton(root, src),
        "go" => lang::go::skeleton(root, src),
        "bash" => lang::bash::skeleton(root, src),
        _ => lang::generic::skeleton(root, src),
    };
    let mut out = render::render(&layout);
    if out.is_empty() {
        return String::new();
    }
    if root.has_error() {
        out.push_str("\n\n[файл разобран частично: есть синтаксические ошибки, скелет неполный — нужное дочитай через read]");
    }
    out
}

/// Какой языковой модуль звать по расширению файла. Отдельно от
/// `lang::language`: js/jsx/tsx выбирают одну грамматику, но модули у них
/// разные по смыслу (JSX-разворот нужен только TS-модулю).
fn module_of(ext: &str) -> &'static str {
    match ext {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "ts" | "mts" | "cts" | "tsx" | "jsx" | "js" | "mjs" | "cjs" => "typescript",
        "go" => "go",
        "sh" | "bash" | "zsh" => "bash",
        _ => "generic",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_language_is_none() {
        assert!(skeleton_of("x", "zzz").is_none());
    }

    #[test]
    fn empty_file_gives_empty_skeleton() {
        assert_eq!(skeleton_of("", "rust").unwrap_or_default(), "");
    }
}
