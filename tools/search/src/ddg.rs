//! Запасной поиск без своего сервера: HTML-выдача DuckDuckGo.
//!
//! SearXNG — основной путь, но он должен где-то работать. На машине без него
//! (ноутбук с Windows 7, чистая установка) поиск иначе просто отсутствовал бы.
//! `html.duckduckgo.com` отдаёт простую страницу без JS: ссылки результатов в
//! `a.result__a` (адрес спрятан в параметре `uddg` редиректа), сниппет — в
//! `a.result__snippet`. Разбираем строками: новых зависимостей ради этого не тянем.
use serde_json::{json, Value};

use crate::{clip, Args, SearchResults, SNIPPET_MAX};

const ENDPOINT: &str = "https://html.duckduckgo.com/html/";
/// Обычный браузерный UA: на «пустой» DuckDuckGo чаще отвечает капчей.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

pub fn fetch(a: &Args) -> Result<SearchResults, String> {
    let mut q = a.query.clone();
    if let Some(site) = &a.site {
        q.push_str(&format!(" site:{site}"));
    }
    let mut query: Vec<(&str, String)> = vec![("q", q), ("kl", region(&a.lang))];
    if let Some(f) = &a.fresh {
        // day|week|month|year → d|w|m|y
        query.push(("df", f[..1].to_string()));
    }
    if a.page > 1 {
        // Страница выдачи — 30 результатов (без рекламы чуть меньше).
        query.push(("s", ((a.page - 1) * 30).to_string()));
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(UA)
        .build()
        .map_err(|e| format!("search: не собрать HTTP-клиент: {e}"))?;
    let resp = client
        .get(ENDPOINT)
        .query(&query)
        .send()
        .map_err(|e| format!("search: DuckDuckGo недоступен: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("search: DuckDuckGo ответил HTTP {}", status.as_u16()));
    }
    let body = resp
        .text()
        .map_err(|e| format!("search: не прочитать ответ DuckDuckGo: {e}"))?;
    let hits = parse(&body);
    if hits.is_empty() && is_challenge(&body) {
        return Err("search: DuckDuckGo попросил капчу (слишком часто) — повторить позже".into());
    }
    let total = hits.len();
    let results: Vec<Value> = hits
        .into_iter()
        .take(a.n)
        .map(|h| {
            json!({
                "title": h.title,
                "url": h.url,
                "snippet": clip(&h.snippet, SNIPPET_MAX),
                "engine": "duckduckgo",
                "published": Value::Null,
            })
        })
        .collect();
    Ok((total, results, Vec::new()))
}

/// `--lang` в регион DuckDuckGo: `ru` → `ru-ru`, `all` → без региона.
fn region(lang: &str) -> String {
    match lang.to_ascii_lowercase().as_str() {
        "" | "all" | "auto" => "wt-wt".into(),
        "en" => "us-en".into(),
        l if l.contains('-') => {
            // ru-RU → ru-ru; DuckDuckGo пишет регион как «страна-язык», но и так понимает.
            l.to_string()
        }
        l => format!("{l}-{l}"),
    }
}

fn is_challenge(body: &str) -> bool {
    body.contains("anomaly-modal") || body.contains("challenge-form")
}

#[derive(Debug, PartialEq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

pub fn parse(html: &str) -> Vec<Hit> {
    let mut out = Vec::new();
    let marks: Vec<usize> = html.match_indices("class=\"result__a\"").map(|(i, _)| i).collect();
    for (k, &at) in marks.iter().enumerate() {
        let end = marks.get(k + 1).copied().unwrap_or(html.len());
        let block = &html[at..end];
        let Some(href) = attr(block, "href") else { continue };
        let Some(url) = real_url(&href) else { continue };
        let title = block
            .find('>')
            .and_then(|s| block[s + 1..].find("</a>").map(|e| &block[s + 1..s + 1 + e]))
            .map(text)
            .unwrap_or_default();
        let snippet = block
            .find("class=\"result__snippet\"")
            .and_then(|s| {
                let rest = &block[s..];
                let open = rest.find('>')?;
                let close = rest[open + 1..].find("</a>")?;
                Some(text(&rest[open + 1..open + 1 + close]))
            })
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        out.push(Hit { title, url, snippet });
    }
    out
}

/// Значение атрибута `name="…"` в начале блока (до первого `>`).
fn attr(block: &str, name: &str) -> Option<String> {
    let head = &block[..block.find('>').unwrap_or(block.len())];
    let key = format!("{name}=\"");
    let s = head.find(&key)? + key.len();
    let e = head[s..].find('"')?;
    Some(entities(&head[s..s + e]))
}

/// Ссылка выдачи — редирект `//duckduckgo.com/l/?uddg=<адрес>&rut=…`.
/// Реклама идёт через `duckduckgo.com/y.js` — её пропускаем.
fn real_url(href: &str) -> Option<String> {
    if href.contains("/y.js") {
        return None;
    }
    if let Some(i) = href.find("uddg=") {
        let v = &href[i + 5..];
        let v = &v[..v.find('&').unwrap_or(v.len())];
        let url = percent_decode(v);
        return url.starts_with("http").then_some(url);
    }
    href.starts_with("http").then(|| href.to_string())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Текст из куска HTML: теги вон, сущности раскрыть, пробелы схлопнуть.
fn text(html: &str) -> String {
    let mut s = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => s.push(c),
            _ => {}
        }
    }
    entities(&s).split_whitespace().collect::<Vec<_>>().join(" ")
}

fn entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let Some(semi) = tail[..tail.len().min(12)].find(';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let name = &tail[1..semi];
        let ch = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            n if n.starts_with("#x") || n.starts_with("#X") => {
                u32::from_str_radix(&n[2..], 16).ok().and_then(char::from_u32)
            }
            n if n.starts_with('#') => n[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"
<div class="result results_links results_links_deep result--ad">
 <a rel="nofollow" class="result__a" href="https://duckduckgo.com/y.js?ad_domain=x&amp;u3=1">Реклама</a>
</div>
<div class="result results_links results_links_deep web-result">
 <h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Ftokio.rs%2F&amp;rut=ee">Tokio &#x27;async&#x27; - <b>Rust</b></a></h2>
 <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Ftokio.rs%2F&amp;rut=ee"><b>Tokio</b> is a library   for &amp; writing</a>
</div>
<div class="result results_links results_links_deep web-result">
 <h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fru.wikipedia.org%2Fwiki%2F%D0%A0%D0%B6%D0%B0%D0%B2%D1%87%D0%B8%D0%BD%D0%B0&amp;rut=ff">Ржавчина</a></h2>
</div>"#;

    #[test]
    fn parses_results_skips_ads_decodes_urls_and_entities() {
        let hits = parse(PAGE);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].url, "https://tokio.rs/");
        assert_eq!(hits[0].title, "Tokio 'async' - Rust");
        assert_eq!(hits[0].snippet, "Tokio is a library for & writing");
        assert_eq!(hits[1].url, "https://ru.wikipedia.org/wiki/Ржавчина");
        assert_eq!(hits[1].snippet, "");
    }

    #[test]
    fn challenge_page_is_recognised() {
        assert!(is_challenge(r#"<div class="anomaly-modal__title">"#));
        assert!(parse(r#"<div class="anomaly-modal__title">"#).is_empty());
    }

    #[test]
    fn region_from_lang() {
        assert_eq!(region("all"), "wt-wt");
        assert_eq!(region("ru"), "ru-ru");
        assert_eq!(region("en"), "us-en");
    }

    #[test]
    fn percent_decode_keeps_bad_escapes() {
        assert_eq!(percent_decode("a%2Fb%zz%"), "a/b%zz%");
    }
}
