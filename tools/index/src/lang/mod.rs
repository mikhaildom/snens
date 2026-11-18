// Языки: скелет наполняет либо свой модуль (там нужны имена полей грамматики),
// либо общий обход по таблице видов узлов — для языков, где хватает секций.
pub mod bash;
pub mod generic;
pub mod go;
pub mod python;
pub mod rust;
pub mod typescript;

use tree_sitter::Language;

/// Грамматика по расширению. `None` — язык не поддержан: тул отказывает и
/// советует `read`, а не отдаёт пустой скелет (пустой выглядел бы как
/// «в файле ничего нет»).
pub fn language(ext: &str) -> Option<Language> {
    Some(match ext {
        "rs" => tree_sitter_rust::LANGUAGE.into(),
        "py" | "pyi" => tree_sitter_python::LANGUAGE.into(),
        "ts" | "mts" | "cts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        // JSX и TSX — только TSX-грамматика: она надмножество JavaScript и
        // единственная понимает JSX (на plain TS JSX уезжает в ERROR-узел).
        "tsx" | "jsx" | "js" | "mjs" | "cjs" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "sh" | "bash" | "zsh" => tree_sitter_bash::LANGUAGE.into(),
        _ => return None,
    })
}

/// Список расширений для `--help` и для текста отказа: модель должна знать,
/// что поддержано, не пробуя наугад.
pub const EXTENSIONS: &[&str] = &[
    "rs", "py", "pyi", "ts", "tsx", "js", "jsx", "mjs", "cjs", "go", "sh", "bash", "zsh",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsx_routes_to_tsx_grammar() {
        // Регрессия maki: на plain TS JSX попадал в ERROR, скелет выходил пустым.
        assert!(language("jsx").is_some());
        assert!(language("tsx").is_some());
    }

    #[test]
    fn unknown_extension_has_no_grammar() {
        assert!(language("zzz").is_none());
    }
}
