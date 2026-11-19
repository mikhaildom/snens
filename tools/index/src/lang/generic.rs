// Общий обход для языков без своего модуля: набор «вид узла → секция» из
// таблицы. Полей и методов здесь нет — только объявления верхнего уровня и
// (на один уровень) содержимое контейнеров.
use tree_sitter::Node;

use crate::render::{head, push, Item, Layout};

/// (вид узла, секция). Порядок строк значения не имеет: секции выстраиваются по
/// первому появлению.
const KINDS: &[(&str, &str)] = &[
    ("function_item", "fns"),
    ("function_definition", "fns"),
    ("function_declaration", "fns"),
    ("method_definition", "methods"),
    ("method_declaration", "methods"),
    ("struct_item", "types"),
    ("class_definition", "types"),
    ("class_declaration", "types"),
    ("interface_declaration", "types"),
    ("type_declaration", "types"),
    ("enum_item", "types"),
    ("impl_item", "impls"),
    ("trait_item", "traits"),
    ("use_declaration", "imports"),
    ("import_statement", "imports"),
    ("import_declaration", "imports"),
    ("module", "imports"),
    ("mod_item", "mod"),
    ("const_item", "consts"),
    ("static_item", "consts"),
    ("lexical_declaration", "consts"),
    ("macro_definition", "macros"),
];

pub fn skeleton(root: Node<'_>, src: &str) -> Layout {
    let mut layout = Layout::new();
    walk(root, src, &mut layout, 0);
    layout
}

fn walk(node: Node<'_>, src: &str, layout: &mut Layout, depth: usize) {
    if depth > 2 {
        return;
    }
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        let kind = child.kind();
        if let Some((_, section)) = KINDS.iter().find(|(k, _)| *k == kind) {
            // Секции — &'static str: имена из таблицы, а не из файла.
            push_static(layout, section, plain(child, src));
        } else if matches!(
            kind,
            "declaration_list" | "block" | "class_body" | "statement_block" | "export_statement"
        ) {
            walk(child, src, layout, depth + 1);
        }
    }
}

/// push принимает &'static str; таблица даёт именно их, но тип приходит из
/// итератора — приводим явно, чтобы не менять сигнатуру render::push.
fn push_static(layout: &mut Layout, section: &str, item: Item) {
    let name: &'static str = match section {
        "fns" => "fns",
        "methods" => "methods",
        "types" => "types",
        "impls" => "impls",
        "traits" => "traits",
        "imports" => "imports",
        "mod" => "mod",
        "consts" => "consts",
        "macros" => "macros",
        _ => "other",
    };
    push(layout, name, item);
}

fn plain(node: Node<'_>, src: &str) -> Item {
    Item::new(head(node, src), node.start_position().row + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_walks_into_containers() {
        // Модуль проверяем напрямую: дерево из rust-грамматики, обход — общий.
        // Через `skeleton_of` сюда не попасть: он выбирает модуль по расширению,
        // и для .rs всегда уходит в rust-модуль.
        let src = "fn outer() {\n    fn inner() {}\n}\nuse std::io;\n";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();
        let layout = super::skeleton(tree.root_node(), src);
        let text = crate::render::render(&layout);
        assert!(text.contains("imports:"), "{text}");
        assert!(text.contains("outer"), "{text}");
    }
}
