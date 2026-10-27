//! Разбор фенса `artifact` — ровно та же грамматика, что в клиенте.
//!
//! Копия парсера сознательная цена: Rust и TS общей библиотеки не имеют, а
//! грамматика поэтому минимальна (заголовок `ключ=значение` в одну строку плюс
//! тело как есть). Фикстуры у обеих сторон одни и те же — `fixtures/`, и
//! `cargo test` / `npm test` сверяются с ними.

/// Заголовок фенса, как его понимает клиент.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Head {
    pub id: String,
    pub src: String,
    pub net: Vec<String>,
    pub height: u32,
    /// Все токены как есть: по ним ищутся опечатки в ключах.
    pub raw: Vec<(String, String)>,
}

/// Артефакт, вынутый из текста ответа.
#[derive(Debug, Clone)]
pub struct Artifact {
    pub head: Head,
    pub body: String,
    /// Фенс не закрыт: модель ещё печатает.
    pub open: bool,
    /// Позиция в тексте — для сообщений об ошибке («фенс №2»).
    pub index: usize,
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Разобрать заголовок: токены `ключ=значение` через пробелы.
pub fn parse_head(line: &str) -> Head {
    let mut h = Head::default();
    for token in line.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            h.raw.push((token.to_string(), String::new()));
            continue;
        };
        if key.is_empty() {
            continue;
        }
        h.raw.push((key.to_string(), value.to_string()));
        match key {
            "id" if valid_name(value) => h.id = value.to_string(),
            "src" => h.src = value.to_string(),
            "net" => {
                h.net = value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| valid_name(s))
                    .map(str::to_string)
                    .collect()
            }
            "height" => {
                let n: u32 = value.parse().unwrap_or(0);
                if (80..=4000).contains(&n) {
                    h.height = n;
                }
            }
            _ => {}
        }
    }
    h
}

/// Открывающая строка фенса: три и более бэктика + инфо-строка `artifact`.
fn open_line(line: &str) -> Option<(usize, &str)> {
    let t = line.trim_start();
    let ticks = t.len() - t.trim_start_matches('`').len();
    if ticks < 3 {
        return None;
    }
    let rest = &t[ticks..];
    let rest = rest.strip_prefix("artifact")?;
    // `artifactx` — не наш фенс.
    if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    Some((ticks, rest.trim()))
}

/// Закрывающая строка: только бэктики и пробелы, не короче открывающей.
fn close_line(line: &str, fence: usize) -> bool {
    let t = line.trim();
    !t.is_empty() && t.chars().all(|c| c == '`') && t.len() >= fence
}

/// Порезать текст ответа на артефакты. Текст между ними не нужен: клиент
/// рендерит его как обычно, а проверке интересны только фенсы.
pub fn parse(text: &str) -> Vec<Artifact> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some((fence, head_line)) = open_line(lines[i]) else {
            i += 1;
            continue;
        };
        let head = parse_head(head_line);
        let mut body = Vec::new();
        let mut closed = false;
        let mut j = i + 1;
        while j < lines.len() {
            if close_line(lines[j], fence) {
                closed = true;
                break;
            }
            body.push(lines[j]);
            j += 1;
        }
        out.push(Artifact {
            head,
            body: body.join("\n"),
            open: !closed,
            index: out.len() + 1,
        });
        i = j + 1;
    }
    out
}

/// Приватная сеть: запрещена всегда, независимо от `net=`. Список — часть
/// контракта (`docs/artifact.md`) и дублируется в клиенте намеренно: клиент
/// обязан отказать, даже если проверку обошли.
pub fn is_private_host(host: &str) -> bool {
    let h = host.trim().to_ascii_lowercase();
    if h.is_empty() {
        return true;
    }
    if h == "localhost" || h == "::1" || h == "[::1]" {
        return true;
    }
    if h.ends_with(".local") || h.ends_with(".localhost") {
        return true;
    }
    let parts: Vec<&str> = h.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    let mut n = [0u32; 4];
    for (i, p) in parts.iter().enumerate() {
        match p.parse::<u32>() {
            Ok(v) if v <= 255 => n[i] = v,
            _ => return false,
        }
    }
    let (a, b) = (n[0], n[1]);
    a == 127
        || a == 10
        || a == 0
        || (a == 192 && b == 168)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 169 && b == 254)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_code_fence_is_not_an_artifact() {
        assert!(parse("```html\n<b>x</b>\n```").is_empty());
    }

    #[test]
    fn artifact_is_found_and_closed() {
        let arts = parse("текст\n```artifact id=quiz net=api.example.com\n<p>1</p>\n```\nещё");
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0].head.id, "quiz");
        assert_eq!(arts[0].head.net, vec!["api.example.com".to_string()]);
        assert_eq!(arts[0].body, "<p>1</p>");
        assert!(!arts[0].open);
    }

    #[test]
    fn unterminated_fence_is_open() {
        let arts = parse("```artifact id=quiz\n<div>печатаю…");
        assert_eq!(arts.len(), 1);
        assert!(arts[0].open);
    }

    #[test]
    fn longer_fence_keeps_inner_backticks() {
        let arts = parse("````artifact id=a\n```\nx\n```\n````");
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0].body, "```\nx\n```");
        assert!(!arts[0].open);
    }

    #[test]
    fn two_artifacts_are_numbered() {
        let arts = parse("```artifact id=a\n1\n```\n```artifact id=b\n2\n```");
        assert_eq!(arts.len(), 2);
        assert_eq!(arts[1].index, 2);
        assert_eq!(arts[1].head.id, "b");
    }

    #[test]
    fn artifact_word_prefix_is_not_a_fence() {
        assert!(parse("```artifactx id=a\n").is_empty());
    }

    #[test]
    fn private_hosts_are_refused() {
        for h in [
            "localhost",
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.10",
            "172.20.0.1",
            "169.254.1.1",
            "printer.local",
            "::1",
        ] {
            assert!(is_private_host(h), "{h}");
        }
        assert!(!is_private_host("api.example.com"));
        assert!(!is_private_host("172.32.0.1"));
    }

    #[test]
    fn head_ignores_garbage_values() {
        let h = parse_head("id=<script> height=99999 net=api.example.com,localhost");
        assert_eq!(h.id, "");
        assert_eq!(h.height, 0);
        // `net` хранится как написан: приватные адреса отсекаются в CSP и lint,
        // а не в разборе — иначе человек не увидит, что модель попросила.
        assert_eq!(
            h.net,
            vec!["api.example.com".to_string(), "localhost".to_string()]
        );
    }

    #[test]
    fn unknown_key_is_visible_in_raw() {
        let h = parse_head("id=q nt=api.example.com");
        assert!(h.raw.iter().any(|(k, _)| k == "nt"));
    }
}
