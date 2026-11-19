// Python: классы с методами, функции (async тоже), импорты. Декораторы
// сохраняются, `@pytest`-подобные не выделяем в отдельную секцию: в питоне тесты
// и код живут вперемешку, дробить файл по ним бессмысленно.
use tree_sitter::Node;

use crate::render::{collapse, deep, head_until, push, text, Item, Layout};

pub fn skeleton(root: Node<'_>, src: &str) -> Layout {
    let mut layout = Layout::new();
    let mut decorators: Vec<String> = Vec::new();
    let mut cur = root.walk();
    for node in root.children(&mut cur) {
        match node.kind() {
            "decorator" => decorators.push(collapse(text(node, src))),
            "import_statement" | "import_from_statement" | "future_import_statement" => {
                push(&mut layout, "imports", plain(node, src, &[]))
            }
            "class_definition" => {
                let item = with_methods(node, src, &decorators);
                decorators.clear();
                push(&mut layout, "types", item);
            }
            "function_definition" => {
                let item = plain(node, src, &decorators);
                decorators.clear();
                let is_async = node.child_by_field_name("async").is_some()
                    || text(node, src).starts_with("async ");
                let section = if is_async { "async" } else { "fns" };
                push(&mut layout, section, item);
            }
            // `if __name__ == "__main__"` и прочие блоки верхнего уровня:
            // показываем их заголовок, чтобы видеть, что файл — скрипт.
            "if_statement" | "try_statement" | "with_statement" | "for_statement" => {
                push(&mut layout, "script", plain(node, src, &[]))
            }
            _ => {}
        }
    }
    layout
}

fn plain(node: Node<'_>, src: &str, decorators: &[String]) -> Item {
    let mut text = head_until(node, src, ':');
    if !decorators.is_empty() {
        text = format!("{} {}", decorators.join(" "), text);
    }
    Item::new(text, node.start_position().row + 1)
}

fn with_methods(node: Node<'_>, src: &str, decorators: &[String]) -> Item {
    let mut item = plain(node, src, decorators);
    let body = node.child_by_field_name("body");
    let mut methods = Vec::new();
    if let Some(body) = body {
        for m in deep(body, "function_definition") {
            // Только методы самого класса: вложенные функции внутри методов не
            // нужны, а глубина здесь — единственный признак.
            if m.parent().map(|p| p.id()) != Some(body.id()) {
                continue;
            }
            let own = own_decorators(m, src);
            methods.push(Item::new(
                format!("{} {}", own, head_until(m, src, ':'))
                    .trim()
                    .to_string(),
                m.start_position().row + 1,
            ));
        }
    }
    item.children = methods;
    item
}

/// Декоратор, приклеенный к методу: в разборе он идёт отдельным узлом-соседом
/// либо обёрткой `decorated_definition`.
fn own_decorators(node: Node<'_>, src: &str) -> String {
    let mut raw = text(node, src);
    if let Some(parent) = node.parent() {
        if parent.kind() == "decorated_definition" {
            raw = text(parent, src);
        }
    }
    let mut out = Vec::new();
    for line in raw.lines() {
        let t = line.trim();
        if t.starts_with('@') {
            out.push(t.to_string());
        }
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeleton_of;

    #[test]
    fn classes_functions_and_imports() {
        let src = "import os\nfrom a import b\n\nclass A:\n    def m(self):\n        pass\n\nasync def f(x):\n    return x\n";
        let out = skeleton_of(src, "py").unwrap();
        assert!(out.contains("imports:"), "{out}");
        assert!(out.contains("import os"), "{out}");
        assert!(out.contains("types:"), "{out}");
        assert!(out.contains("class A"), "{out}");
        assert!(out.contains("def m(self)"), "{out}");
        assert!(out.contains("async:"), "{out}");
        assert!(out.contains("async def f(x)"), "{out}");
    }
}
