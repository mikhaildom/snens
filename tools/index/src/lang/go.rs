// Go: пакет, импорты, типы (struct/interface с полями), функции и методы.
use tree_sitter::Node;

use crate::render::{collapse, deep, head, push, text, Item, Layout};

pub fn skeleton(root: Node<'_>, src: &str) -> Layout {
    let mut layout = Layout::new();
    let mut cur = root.walk();
    for node in root.children(&mut cur) {
        match node.kind() {
            "package_clause" => push(&mut layout, "package", plain(node, src)),
            "import_declaration" => {
                // Одна декларация может нести несколько путей: по строке на путь.
                let specs = deep(node, "import_spec");
                if specs.is_empty() {
                    push(&mut layout, "imports", plain(node, src));
                }
                for spec in specs {
                    push(
                        &mut layout,
                        "imports",
                        Item::new(collapse(text(spec, src)), spec.start_position().row + 1),
                    );
                }
            }
            "type_declaration" => {
                for spec in deep(node, "type_spec") {
                    push(&mut layout, "types", with_members(spec, src));
                }
            }
            "function_declaration" => push(&mut layout, "fns", plain(node, src)),
            "method_declaration" => push(&mut layout, "methods", plain(node, src)),
            "const_declaration" | "var_declaration" => push(&mut layout, "vars", plain(node, src)),
            _ => {}
        }
    }
    layout
}

fn plain(node: Node<'_>, src: &str) -> Item {
    Item::new(head(node, src), node.start_position().row + 1)
}

fn with_members(spec: Node<'_>, src: &str) -> Item {
    let mut item = plain(spec, src);
    let mut members = Vec::new();
    for kind in ["field_declaration", "method_elem"] {
        for m in deep(spec, kind) {
            members.push(Item::new(head(m, src), m.start_position().row + 1));
        }
    }
    members.sort_by_key(|m| m.line);
    item.children = members;
    item
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeleton_of;

    #[test]
    fn package_types_and_methods() {
        let src = "package main\n\nimport (\n\t\"fmt\"\n\t\"os\"\n)\n\ntype T struct {\n\tX int\n}\n\nfunc (t *T) M() error { return nil }\n\nfunc F() {}\n";
        let out = skeleton_of(src, "go").unwrap();
        assert!(out.contains("package main"), "{out}");
        assert!(out.contains("imports:"), "{out}");
        assert!(out.contains("\"fmt\""), "{out}");
        assert!(out.contains("types:"), "{out}");
        assert!(out.contains("X int"), "{out}");
        assert!(out.contains("methods:"), "{out}");
        assert!(out.contains("fns:"), "{out}");
    }
}
