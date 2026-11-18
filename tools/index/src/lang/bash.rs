// Bash/sh: функции и объявления верхнего уровня. Скелет нужен редко, но
// тул, который на `.sh` говорит «не поддержано», модель читает целиком — а
// правила брокера привилегий читать целиком не надо.
use tree_sitter::Node;

use crate::render::{collapse, head, push, text, Item, Layout};

pub fn skeleton(root: Node<'_>, src: &str) -> Layout {
    let mut layout = Layout::new();
    let mut cur = root.walk();
    for node in root.children(&mut cur) {
        match node.kind() {
            "function_definition" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| collapse(text(n, src)))
                    .unwrap_or_default();
                push(&mut layout, "fns", Item::new(name, node.start_position().row + 1));
            }
            "variable_assignment" => push(&mut layout, "vars", plain(node, src)),
            "command" => {
                // Верхнеуровневые команды: только первое слово, чтобы видеть
                // источник/точку входа (`source lib.sh`), а не тела скриптов.
                let t = collapse(text(node, src));
                let word = t.split_whitespace().next().unwrap_or("").to_string();
                if matches!(word.as_str(), "source" | "." | "export" | "set") {
                    push(&mut layout, "vars", Item::new(t, node.start_position().row + 1));
                }
            }
            _ => {}
        }
    }
    layout
}

fn plain(node: Node<'_>, src: &str) -> Item {
    Item::new(head(node, src), node.start_position().row + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeleton_of;

    #[test]
    fn functions_and_vars() {
        let src = "set -euo pipefail\nFOO=1\nsource lib.sh\n\nrun() {\n  echo hi\n}\n";
        let out = skeleton_of(src, "sh").unwrap();
        assert!(out.contains("vars:"), "{out}");
        assert!(out.contains("FOO=1"), "{out}");
        assert!(out.contains("source lib.sh"), "{out}");
        assert!(out.contains("fns:"), "{out}");
        assert!(out.contains("run"), "{out}");
    }
}
