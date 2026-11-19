// TypeScript/JS (включая TSX): импорты, типы, классы, функции, экспорты верхнего
// уровня. `export` — обёртка над объявлением, её надо развернуть, иначе весь файл
// окажется одной строкой `export …`.
use tree_sitter::Node;

use crate::render::{collapse, deep, head, name_of, push, text, Item, Layout};

pub fn skeleton(root: Node<'_>, src: &str) -> Layout {
    let mut layout = Layout::new();
    let mut cur = root.walk();
    for node in root.children(&mut cur) {
        visit(node, src, &mut layout);
    }
    layout
}

fn visit(node: Node<'_>, src: &str, layout: &mut Layout) {
    match node.kind() {
        // Обёртки: разворачиваем, иначе внутри export оказалось бы тело функции.
        "export_statement" | "ambient_declaration" | "internal_module" => {
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                visit(child, src, layout);
            }
            // `export default foo;` и `export { a, b };` — не объявления: их
            // показываем строкой, иначе из скелета пропадёт публичная поверхность.
            if !has_declaration(node) {
                push(layout, "exports", plain(node, src));
            }
        }
        "import_statement" => push(layout, "imports", plain(node, src)),
        "interface_declaration"
        | "type_alias_declaration"
        | "enum_declaration"
        | "class_declaration"
        | "abstract_class_declaration" => push(layout, "types", with_members(node, src)),
        "function_declaration" | "generator_function_declaration" => {
            push(layout, "fns", plain(node, src))
        }
        "lexical_declaration" | "variable_declaration" => {
            for item in bindings(node, src) {
                push(layout, "consts", item);
            }
        }
        "expression_statement" | "statement_block" => {}
        _ => {}
    }
}

fn has_declaration(node: Node<'_>) -> bool {
    let mut found = false;
    let mut cur = node.walk();
    for c in node.children(&mut cur) {
        if matches!(
            c.kind(),
            "function_declaration"
                | "class_declaration"
                | "interface_declaration"
                | "lexical_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
                | "abstract_class_declaration"
        ) {
            found = true;
            break;
        }
    }
    found
}

/// `const a = 1, b = () => {}` → по строке на имя: тело стрелки в контексте не
/// нужно, а имена нужны.
fn bindings(node: Node<'_>, src: &str) -> Vec<Item> {
    let mut out = Vec::new();
    let mut cur = node.walk();
    for decl in node.children(&mut cur) {
        if decl.kind() != "variable_declarator" {
            continue;
        }
        let name = name_of(decl, src);
        let kind = text(node, src).split_whitespace().next().unwrap_or("const");
        // У стрелки показываем параметры: `const f = (a: T) => …` → `f = (a: T) =>`.
        let params = decl
            .child_by_field_name("value")
            .and_then(|v| {
                v.child_by_field_name("parameters")
                    .or_else(|| v.child_by_field_name("parameter"))
            })
            .map(|p| collapse(text(p, src)))
            .unwrap_or_default();
        let line = format!("{kind} {name}{params}");
        out.push(Item::new(line, decl.start_position().row + 1));
    }
    if out.is_empty() {
        out.push(plain(node, src));
    }
    out
}

fn with_members(node: Node<'_>, src: &str) -> Item {
    let mut item = plain(node, src);
    let mut members = Vec::new();
    for kind in [
        "method_definition",
        "method_signature",
        "property_signature",
        "public_field_definition",
    ] {
        for m in deep(node, kind) {
            members.push(Item::new(head(m, src), m.start_position().row + 1));
        }
    }
    members.sort_by_key(|m| m.line);
    item.children = members;
    item
}

fn plain(node: Node<'_>, src: &str) -> Item {
    Item::new(head(node, src), node.start_position().row + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeleton_of;

    #[test]
    fn export_wrapper_is_unwrapped() {
        let src =
            "import { a } from 'x';\nexport function f(x: number): void {}\nexport const y = 1;\n";
        let out = skeleton_of(src, "ts").unwrap();
        assert!(out.contains("imports:"), "{out}");
        assert!(out.contains("fns:"), "{out}");
        assert!(out.contains("function f(x: number)"), "{out}");
        assert!(out.contains("consts:"), "{out}");
        assert!(out.contains("const y"), "{out}");
    }

    #[test]
    fn interface_members_are_listed() {
        let src = "export interface P {\n  x: number;\n  y: string;\n}\n";
        let out = skeleton_of(src, "ts").unwrap();
        assert!(out.contains("interface P"), "{out}");
        assert!(out.contains("x: number"), "{out}");
    }
}
