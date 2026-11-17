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
    /// Значения, которые не прошли проверку формы и были отброшены: иначе
    /// опечатка модели молча превращается в дефолт и `check` о ней не скажет.
    pub rejected: Vec<(String, String)>,
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

/// Хост из `net=`: то же плюс порт и обрамление IPv6-литерала. Раньше `:` не
/// проходил, и `net=localhost:8080` просто выпадал из списка — запрет не
/// срабатывал, потому что проверять было нечего.
fn valid_net_host(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '.' | '_' | '-' | ':' | '[' | ']' | '%')
        })
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
            "id" => h.rejected.push((key.to_string(), value.to_string())),
            // `src` — путь внутри каталога сессии (`<id>.out/…`): заголовок пишет
            // модель, поэтому `../..`, абсолютный путь и схема отсекаются сразу.
            "src" if valid_src(value) => h.src = value.to_string(),
            "src" => h.rejected.push((key.to_string(), value.to_string())),
            "net" => {
                h.net.clear();
                for item in value.split(',').map(str::trim) {
                    if item.is_empty() {
                        continue;
                    }
                    if valid_net_host(item) {
                        h.net.push(item.to_string());
                    } else {
                        h.rejected.push((key.to_string(), item.to_string()));
                    }
                }
            }
            "height" => {
                let n: u32 = value.parse().unwrap_or(0);
                if (80..=4000).contains(&n) {
                    h.height = n;
                } else {
                    h.rejected.push((key.to_string(), value.to_string()));
                }
            }
            _ => {}
        }
    }
    h
}

/// Путь из `src`: относительный, без `..`, без схемы и ведущего слэша.
fn valid_src(v: &str) -> bool {
    !v.is_empty()
        && !v.starts_with('/')
        && !v.starts_with('\\')
        && !v.contains("..")
        && !v.contains(':')
        && !v.chars().any(|c| c.is_control())
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
    // `lines()` срезает `\r` у CRLF: иначе `\`\`\`artifact\r` не опознаётся как фенс
    // и тело тащит `\r` в каждой строке.
    let lines: Vec<&str> = text.lines().collect();
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
    // Порт в `net=` не должен влиять на проверку: `localhost:8080` — тот же
    // localhost, и одним двоеточием документированный запрет не обойти.
    let h = strip_port(host.trim()).to_ascii_lowercase();
    // Финальная точка — каноничное DNS-имя: `localhost.` — тот же localhost.
    let h = h.trim_end_matches('.');
    if h.is_empty() {
        return true;
    }
    if h == "localhost" || h.ends_with(".local") || h.ends_with(".localhost") {
        return true;
    }
    // Браузер разворачивает сокращённые/целочисленные IPv4 (`127.1`,
    // `2130706433`, `0x7f.0.0.1`) в обычный адрес — значит, и мы обязаны.
    if let Some(n) = ipv4_loose(h) {
        return private_v4(n);
    }
    // IPv6-литерал: `::1`, ULA `fc00::/7`, link-local `fe80::/10`,
    // IPv4-mapped `::ffff:10.0.0.1` — всё это локальная сеть того же браузера.
    if let Some(v6) = ipv6_literal(h) {
        return private_v6(v6);
    }
    false
}

/// Снять порт и обрамляющие скобки у IPv6-литерала.
fn strip_port(host: &str) -> String {
    if let Some(rest) = host.strip_prefix('[') {
        // `[::1]:8080` и `[::1]`
        return match rest.split_once(']') {
            Some((addr, _)) => format!("[{addr}]"),
            None => host.to_string(),
        };
    }
    match host.rsplit_once(':') {
        // Больше одного двоеточия — это IPv6 без скобок, порта там нет.
        Some((head, port)) if !head.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => {
            head.to_string()
        }
        _ => host.to_string(),
    }
}

/// IPv4 в любой из форм, которые понимает `inet_aton`: 1–4 части, десятичные,
/// восьмеричные (`0177`) и шестнадцатеричные (`0x7f`) числа.
fn ipv4_loose(s: &str) -> Option<[u8; 4]> {
    if let Ok(v4) = s.parse::<std::net::Ipv4Addr>() {
        return Some(v4.octets());
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut nums = Vec::with_capacity(parts.len());
    for p in &parts {
        if p.is_empty() {
            return None;
        }
        nums.push(parse_int_part(p)?);
    }
    let last = *nums.last()?;
    let lead = &nums[..nums.len() - 1];
    for n in lead {
        if *n > 255 {
            return None;
        }
    }
    // Хвост занимает все оставшиеся байты: `127.1` = 127.0.0.1.
    let rest = 4 - lead.len() as u32;
    if last >= 1u64 << (8 * rest) {
        return None;
    }
    let mut out = [0u8; 4];
    for (i, n) in lead.iter().enumerate() {
        out[i] = *n as u8;
    }
    for i in 0..rest {
        out[lead.len() + i as usize] = ((last >> (8 * (rest - 1 - i))) & 0xff) as u8;
    }
    Some(out)
}

/// Число в форме inet_aton: `0x..` — hex, ведущий `0` — восьмеричное, иначе десятичное.
fn parse_int_part(p: &str) -> Option<u64> {
    let (radix, digits) = match p.strip_prefix("0x").or_else(|| p.strip_prefix("0X")) {
        Some(d) => (16, d),
        None if p.len() > 1 && p.starts_with('0') => (8, &p[1..]),
        None => (10, p),
    };
    if digits.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(digits, radix).ok()
}

fn private_v4(n: [u8; 4]) -> bool {
    let (a, b) = (n[0], n[1]);
    a == 127
        || a == 10
        || a == 0
        || (a == 192 && b == 168)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 169 && b == 254)
        // CGNAT 100.64.0.0/10 — тоже «внутренняя» сеть, браузер в неё попадает.
        || (a == 100 && (64..=127).contains(&b))
}

fn ipv6_literal(h: &str) -> Option<std::net::Ipv6Addr> {
    let s = h.strip_prefix('[').unwrap_or(h).strip_suffix(']').unwrap_or(h);
    s.parse::<std::net::Ipv6Addr>().ok()
}

fn private_v6(v6: std::net::Ipv6Addr) -> bool {
    let seg = v6.segments();
    if v6.is_loopback() || v6.is_unspecified() {
        return true;
    }
    // IPv4-mapped `::ffff:a.b.c.d` — это адрес той же машины в другой нотации.
    if let Some(v4) = v6.to_ipv4_mapped() {
        return private_v4(v4.octets());
    }
    // ULA fc00::/7, link-local fe80::/10, site-local (устаревший) fec0::/10.
    (seg[0] & 0xfe00) == 0xfc00
        || (seg[0] & 0xffc0) == 0xfe80
        || (seg[0] & 0xffc0) == 0xfec0
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
    fn private_host_with_port_is_still_private() {
        for h in [
            "localhost:8080",
            "127.0.0.1:80",
            "10.0.0.1:443",
            "[::1]:8080",
            "192.168.1.1:3000",
        ] {
            assert!(is_private_host(h), "{h}");
        }
        assert!(!is_private_host("api.example.com:443"));
    }

    #[test]
    fn non_canonical_ipv4_forms_are_private() {
        for h in [
            "127.1",
            "2130706433",
            "0x7f.0.0.1",
            "0177.0.0.1",
            "127.0.0.1.",
            "localhost.",
            "0.0.0.0",
            "100.64.1.1",
        ] {
            assert!(is_private_host(h), "{h}");
        }
        assert!(!is_private_host("8.8.8.8"));
        assert!(!is_private_host("example.com."));
    }

    #[test]
    fn ipv6_local_ranges_are_private() {
        for h in [
            "::1",
            "[::1]",
            "fd00::1",
            "fc00::1",
            "fe80::1",
            "fec0::1",
            "::ffff:127.0.0.1",
            "[::ffff:10.0.0.1]",
            "::",
        ] {
            assert!(is_private_host(h), "{h}");
        }
        assert!(!is_private_host("2001:4860:4860::8888"));
    }

    #[test]
    fn crlf_fence_is_recognised() {
        // `\r` в конце строки не должен мешать опознать фенс и попадать в тело.
        let arts = parse("```artifact id=a\r\n<p>1</p>\r\n```\r\n");
        assert_eq!(arts.len(), 1, "фенс с CRLF не опознан");
        assert_eq!(arts[0].head.id, "a");
        assert!(!arts[0].body.contains('\r'), "тело тащит \r");
    }

    #[test]
    fn src_outside_the_session_dir_is_ignored() {
        for bad in ["../../etc/passwd", "/etc/passwd", "https://evil.example/x"] {
            let h = parse_head(&format!("id=a src={bad}"));
            assert!(h.src.is_empty(), "{bad} принят как src");
        }
        assert_eq!(parse_head("id=a src=out/chart.svg").src, "out/chart.svg");
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
