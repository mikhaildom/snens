// Сборка и печать скелета: структура (секции → записи → потомки) и её текстовый
// вид. Языковые модули только наполняют структуру, формат вывода живёт здесь —
// иначе у каждого языка поехали бы отступы и номера строк.
use tree_sitter::Node;

/// Одна запись: строка скелета, номер строки в файле и вложенные записи
/// (поля структуры, методы impl/класса).
pub struct Item {
    pub text: String,
    pub line: usize,
    pub children: Vec<Item>,
    /// Сколько вложенных не влезло в потолок — печатается одной строкой.
    pub hidden: usize,
}

impl Item {
    pub fn new(text: String, line: usize) -> Self {
        Item {
            text,
            line,
            children: Vec::new(),
            hidden: 0,
        }
    }
}

pub struct Section {
    pub name: &'static str,
    pub items: Vec<Item>,
}

impl Section {
    pub fn new(name: &'static str) -> Self {
        Section {
            name,
            items: Vec::new(),
        }
    }
}

/// Порядок секций задаёт язык: печатаются только непустые, в этом порядке.
pub type Layout = Vec<Section>;

pub fn push(layout: &mut Layout, name: &'static str, item: Item) {
    match layout.iter_mut().find(|s| s.name == name) {
        Some(s) => s.items.push(item),
        None => {
            let mut s = Section::new(name);
            s.items.push(item);
            layout.push(s);
        }
    }
}

/// Сколько вложенных записей показываем: за этим пределом — счётчик.
const MAX_CHILDREN: usize = 8;

pub fn render(layout: &Layout) -> String {
    let mut out: Vec<String> = Vec::new();
    for section in layout {
        if section.items.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(String::new());
        }
        out.push(format!("{}:", section.name));
        for item in &section.items {
            out.push(format!("  {} {}", item.text, range(item.line, item)));
            let shown = item.children.len().min(MAX_CHILDREN);
            for child in &item.children[..shown] {
                out.push(format!("    {}", child.text));
            }
            let rest = item.hidden + item.children.len() - shown;
            if rest > 0 {
                out.push(format!("    [+{rest} скрыто]"));
            }
        }
    }
    out.join("\n")
}

/// `[12]`, а записи с потомками — `[12-40]`: модель видит, что читать
/// `read --offset 12 --limit 29`, не гадая о границе.
fn range(line: usize, item: &Item) -> String {
    if item.children.is_empty() && item.hidden == 0 {
        format!("[{line}]")
    } else {
        format!("[{line}-]")
    }
}

// ------------------------------------------------------------------ разбор

pub fn text<'a>(node: Node<'_>, src: &'a str) -> &'a str {
    &src[node.byte_range()]
}

/// Первая строка объявления без тела: до `{` (или до `:` у python), иначе
/// первая строка целиком. Пробельные серии сжимаются в один пробел, чтобы
/// многострочная сигнатура уложилась в одну строку скелета.
pub fn head(node: Node<'_>, src: &str) -> String {
    let raw = text(node, src);
    let cut = raw.find('{').unwrap_or(raw.len());
    collapse(&raw[..boundary(raw, cut)])
}

/// Строка до первого из разделителей — для языков без фигурных скобок.
pub fn head_until(node: Node<'_>, src: &str, sep: char) -> String {
    let raw = text(node, src);
    let cut = match raw.find(sep) {
        Some(i) => i,
        None => match raw.find('\n') {
            Some(i) => i,
            None => raw.len(),
        },
    };
    collapse(&raw[..boundary(raw, cut)])
}

fn boundary(s: &str, mut at: usize) -> usize {
    while at > 0 && !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

pub fn collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.trim().chars() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    cap(&out)
}

/// Потолок одной строки скелета. Многострочный HELP или JSON-схема внутри кода
/// иначе съедают весь бюджет скелета целиком одним объявлением.
const LINE_CAP: usize = 160;

/// Обрезка по границе символа с пометкой: модель должна знать, что сигнатура
/// длиннее, а не думать, что она такая и есть.
fn cap(s: &str) -> String {
    let n = s.chars().count();
    if n <= LINE_CAP {
        return s.to_string();
    }
    let head: String = s.chars().take(LINE_CAP).collect();
    format!("{head}… (+{} симв.)", n - LINE_CAP)
}

/// Текст объявления до открывающей скобки тела, но без обрезки по первой
/// строке: для `struct X { … }` заголовок — `struct X`.
pub fn name_of<'a>(node: Node<'_>, src: &'a str) -> &'a str {
    node.child_by_field_name("name")
        .map(|n| text(n, src))
        .unwrap_or("?")
}

/// Все потомки вида `kind` на любой глубине (поля структуры лежат внутри
/// `field_declaration_list` / `class_body` / `block`).
pub fn deep<'a>(node: Node<'a>, kind: &str) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    collect(node, kind, &mut out);
    out
}

fn collect<'a>(node: Node<'a>, kind: &str, out: &mut Vec<Node<'a>>) {
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        if child.kind() == kind {
            out.push(child);
        }
        collect(child, kind, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_squeezes_whitespace_runs() {
        assert_eq!(
            collapse("  pub   fn  x(\n  a: u8,\n) -> u8"),
            "pub fn x( a: u8, ) -> u8"
        );
    }

    #[test]
    fn render_groups_by_section_and_counts_hidden() {
        let mut layout = Layout::new();
        let mut item = Item::new("pub struct A".into(), 3);
        item.children = (0..10).map(|i| Item::new(format!("f{i}: u8"), 0)).collect();
        item.hidden = 5;
        push(&mut layout, "types", item);
        let out = render(&layout);
        assert!(out.starts_with("types:"), "{out}");
        assert!(out.contains("pub struct A [3-]"), "{out}");
        assert!(out.contains("[+7 скрыто]"), "{out}");
    }
}
