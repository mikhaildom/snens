// Rust: секции по видам объявлений, у impl/struct/enum/trait — вложенные
// сигнатуры. Атрибуты не теряются: `derive`/`cfg` печатаются припиской к
// объявлению, `#[test]` уводит функцию в секцию `tests` (иначе тестовый модуль
// съедает половину скелета).
use tree_sitter::Node;

use crate::render::{collapse, deep, head, push, text, Item, Layout};

/// Что оставляем от атрибута: остальное (doc-комментарии, serde) — шум.
const KEEP_ATTRS: [&str; 2] = ["derive", "cfg"];

pub fn skeleton(root: Node<'_>, src: &str) -> Layout {
    let mut layout = Layout::new();
    let mut attrs: Vec<String> = Vec::new();
    let mut cur = root.walk();
    for node in root.children(&mut cur) {
        if node.kind() == "attribute_item" {
            let t = text(node, src);
            if KEEP_ATTRS.iter().any(|k| t.contains(k)) {
                attrs.push(collapse(t));
            }
            continue;
        }
        dispatch(node, src, &mut layout, &attrs);
        attrs.clear();
    }
    layout
}

fn dispatch(node: Node<'_>, src: &str, layout: &mut Layout, attrs: &[String]) {
    match node.kind() {
        "use_declaration" => push(
            layout,
            "imports",
            Item::new(clean_use(text(node, src)), node_line(node)),
        ),
        "const_item" | "static_item" => push(layout, "consts", plain(node, src, attrs)),
        "struct_item" | "enum_item" | "union_item" | "type_item" => {
            push(layout, "types", with_fields(node, src, attrs))
        }
        "trait_item" => push(layout, "traits", with_fields(node, src, attrs)),
        "impl_item" => push(layout, "impls", with_fields(node, src, attrs)),
        "function_item" => {
            let section = if attrs.iter().any(|a| is_test(a)) {
                "tests"
            } else {
                "fns"
            };
            push(layout, section, plain(node, src, attrs));
        }
        "mod_item" => push(layout, "mod", plain(node, src, attrs)),
        "macro_definition" => push(layout, "macros", plain(node, src, attrs)),
        _ => {}
    }
}

fn is_test(attr: &str) -> bool {
    attr.contains("test")
}

/// `use std::{a::b, c};` → `std::{a::b, c}`: без ключевого слова и точки с
/// запятой, чтобы строка была короче и однороднее.
fn clean_use(raw: &str) -> String {
    let s = raw.trim();
    let s = s.strip_prefix("use ").unwrap_or(s);
    let s = s.strip_prefix("pub ").unwrap_or(s);
    collapse(s.trim_end_matches(';'))
}

fn node_line(node: Node<'_>) -> usize {
    node.start_position().row + 1
}

/// Объявление без вложенности: заголовок до `{` плюс сохранённые атрибуты.
fn plain(node: Node<'_>, src: &str, attrs: &[String]) -> Item {
    let mut text = head(node, src);
    if !attrs.is_empty() {
        text = format!("{} {}", attrs.join(" "), text);
    }
    Item::new(text, node_line(node))
}

/// Объявление с телом: у struct — поля, у enum — имена вариантов, у trait —
/// сигнатуры, у impl — методы.
fn with_fields(node: Node<'_>, src: &str, attrs: &[String]) -> Item {
    let mut item = plain(node, src, attrs);
    // У enum только имена вариантов: `deep` по `field_declaration` добирал бы
    // поля вариантов, и вместо пяти имён в скелете оказывались десятки строк.
    if node.kind() == "enum_item" {
        item.children = enum_variants(node, src);
        return item;
    }
    let kid_kinds: &[&str] = match node.kind() {
        "struct_item" | "union_item" => &["field_declaration"],
        "trait_item" | "impl_item" => &["function_item", "function_signature_item"],
        _ => &[],
    };
    if kid_kinds.is_empty() {
        return item;
    }
    // Поля и методы лежат внутри списка тела; ищем на любой глубине, потому что
    // у impl перед declaration_list бывает where-клауза.
    let mut children = Vec::new();
    for kind in kid_kinds {
        for child in deep(node, kind) {
            children.push(Item::new(head(child, src), node_line(child)));
        }
    }
    children.sort_by_key(|c| c.line);
    item.children = children;
    item
}

/// Имена вариантов enum'а — по одному на строку, без полей.
fn enum_variants(node: Node<'_>, src: &str) -> Vec<Item> {
    let mut out = Vec::new();
    for body_kind in ["enum_variant_list"] {
        let bodies = deep(node, body_kind);
        for body in bodies {
            let mut cur = body.walk();
            for child in body.children(&mut cur) {
                if child.kind() != "enum_variant" {
                    continue;
                }
                let name = child.child_by_field_name("name").map(|n| crate::render::text(n, src));
                if let Some(name) = name {
                    out.push(Item::new(name.to_string(), node_line(child)));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeleton_of;

    #[test]
    fn sections_and_line_numbers() {
        let src = "use std::io;\n\npub struct A { pub x: u8 }\n\npub fn f() {}\n\nimpl A {\n    fn g(&self) {}\n}\n";
        let out = skeleton_of(src, "rs").unwrap();
        assert!(out.contains("imports:"), "{out}");
        assert!(out.contains("types:"), "{out}");
        assert!(out.contains("pub struct A [3-]"), "{out}");
        assert!(out.contains("x: u8"), "{out}");
        assert!(out.contains("fns:"), "{out}");
        assert!(out.contains("impls:"), "{out}");
        assert!(out.contains("g(&self)"), "{out}");
    }

    #[test]
    fn test_fn_goes_to_tests_section() {
        let src = "#[cfg(test)]\nfn helper() {}\n#[test]\nfn it_works() {}\n";
        let out = skeleton_of(src, "rs").unwrap();
        assert!(out.contains("tests:"), "{out}");
        assert!(out.contains("it_works"), "{out}");
        assert!(out.contains("cfg(test)"), "{out}");
    }
}
