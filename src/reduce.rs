// Сокращение вывода до попадания в контекст: усечение, ANSI, длинные строки,
// повторы. Изолировано от спавна: это чистая функция над текстом.

/// Строку такой длины модель всё равно не читает, а контекст она съедает.
const MAX_LINE: usize = 400;

/// Превентивное сокращение вывода shell-команд до попадания в контекст:
/// ANSI-мусор, нечеловечески длинные строки, повторы, общий размер.
pub fn reduce(s: &str, cap: usize) -> String {
    let stripped = strip_ansi(s);
    let mut lines: Vec<String> = Vec::new();
    let mut prev: Option<String> = None;
    let mut run = 0usize;
    for raw_line in stripped.lines() {
        let line = cap_line(raw_line.trim_end());
        if Some(&line) == prev.as_ref() {
            run += 1;
            continue;
        }
        if run > 0 {
            lines.push(format!("  [×{}]", run + 1));
            run = 0;
        }
        prev = Some(line.clone());
        lines.push(line);
    }
    if run > 0 {
        lines.push(format!("  [×{}]", run + 1));
    }
    truncate(&lines.join("\n"), cap)
}

fn cap_line(l: &str) -> String {
    let n = l.chars().count();
    if n <= MAX_LINE {
        return l.to_string();
    }
    let head: String = l.chars().take(MAX_LINE).collect();
    format!("{head}… (+{} симв.)", n - MAX_LINE)
}

/// Управляющие последовательности: цвет, прогресс-бары, курсор — токены без смысла.
fn strip_ansi(s: &str) -> String {
    if !s.contains('\u{1b}') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match it.peek() {
            // CSI: ESC [ ... финальный байт в диапазоне @..~
            Some('[') => {
                it.next();
                while let Some(&n) = it.peek() {
                    it.next();
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            // OSC: ESC ] ... BEL или ESC \
            Some(']') => {
                it.next();
                while let Some(n) = it.next() {
                    if n == '\u{7}' {
                        break;
                    }
                    if n == '\u{1b}' && it.peek() == Some(&'\\') {
                        it.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Усечение с явной пометкой: модель должна знать, что видит часть.
pub fn truncate(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let head: String = s.chars().take(cap * 3 / 4).collect();
    let tail: String = s
        .chars()
        .rev()
        .take(cap / 4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!(
        "{head}\n[... обрезано {} символов из {} ...]\n{tail}",
        s.len() - cap,
        s.len()
    )
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_and_repeats_are_squeezed() {
        let s = "\u{1b}[32mok\u{1b}[0m\nшум\nшум\nшум\nконец\n";
        let r = reduce(s, 10_000);
        assert!(r.contains("ok"), "текст потерян: {r}");
        assert!(!r.contains('\u{1b}'), "ANSI остался: {r:?}");
        assert!(r.contains("[×3]"), "повторы не схлопнуты: {r}");
        assert!(r.contains("конец"));
        let long = "x".repeat(1000);
        let r2 = reduce(&long, 10_000);
        assert!(
            r2.contains("симв."),
            "длинная строка без пометки: {}",
            r2.len()
        );
    }
}
