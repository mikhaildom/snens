// Сеть: клиент, заголовки, свой GET и фолбэк r.jina.ai.
use std::io::Read;
use std::time::Duration;

use crate::cli::{Args, Fallback};
use crate::html;
use crate::{save_binary, MAX_BODY};

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36 snens/0.1";
/// jina отбивает браузерный UA (403), простой — нет: у фолбэка свой.
const UA_JINA: &str = "snens-fetch/0.1";
const ACCEPT: &str =
    "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8";
const ACCEPT_LANG: &str = "en,nl;q=0.8,ru;q=0.6";
const JINA: &str = "https://r.jina.ai/";

pub fn client(timeout: u64) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| format!("не удалось создать HTTP-клиент: {e}"))
}

/// Заголовки: без честного User-Agent и Accept сайты отдают заглушки и 403.
fn with_headers(rb: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
    rb.header(reqwest::header::USER_AGENT, UA)
        .header(reqwest::header::ACCEPT, ACCEPT)
        .header(reqwest::header::ACCEPT_LANGUAGE, ACCEPT_LANG)
}

/// Страница-заглушка антибота: тело есть, содержимого нет. Свой путь её отдаёт
/// кодом 200 — это повод сходить в jina и в любом случае предупредить в stderr.
pub fn looks_blocked(md: &str) -> bool {
    let low = md.to_ascii_lowercase();
    low.contains("access denied") && (low.contains("automation tools") || low.contains("security"))
        || low.contains("enable javascript and cookies")
        || low.contains("unusual traffic")
}

pub enum Own {
    Text(String),
    Binary(String),
    /// HTTP-ошибка/сеть: код и сообщение для stderr.
    Unavailable(u16, String),
}

/// Свой путь: GET и разбор по content-type.
pub fn own(a: &Args) -> Own {
    let cl = match client(a.timeout) {
        Ok(c) => c,
        Err(e) => return Own::Unavailable(0, format!("{}: {e}", a.url)),
    };
    let resp = match with_headers(cl.get(&a.url)).send() {
        Ok(r) => r,
        Err(e) => return Own::Unavailable(0, format!("{}: {e}", a.url)),
    };
    let status = resp.status();
    if !status.is_success() {
        return Own::Unavailable(
            status.as_u16(),
            format!("{} → HTTP {}", a.url, status.as_u16()),
        );
    }

    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let disposition = resp
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let is_html =
        ctype.is_empty() || ctype.contains("text/html") || ctype.contains("application/xhtml");
    if is_html {
        // Лимит — только на текст в контекст: больше в модель не влезет.
        let mut bytes = Vec::new();
        let _ = resp.take(MAX_BODY).read_to_end(&mut bytes);
        if bytes.len() as u64 == MAX_BODY {
            eprintln!("fetch: тело обрезано по потолку {MAX_BODY} Б ({})", a.url);
        }
        return Own::Text(html::to_markdown(&String::from_utf8_lossy(&bytes), a.raw));
    }

    // Бинарь не держим в памяти и не режем потолком: обрезанный на 2 МБ PDF —
    // битый файл, а не «текст подлиннее». Пишем потоком прямо в файл.
    let name = filename(a, &ctype, disposition.as_deref());
    match save_binary(a, resp, name.as_deref(), &ctype) {
        Ok(line) => Own::Binary(line),
        Err(e) => Own::Unavailable(0, format!("{}: {e}", a.url)),
    }
}

/// Ответ jina на успешную страницу всегда в обёртке с `Markdown Content:`.
/// Заглушка блокировщика («Access Denied») приходит тем же кодом 200, а на цель,
/// ответившую 4xx/5xx, jina пишет `Warning: Target URL returned error` в шапке:
/// без этих проверок фолбэк отдавал пустоту с кодом 0, то есть врал об успехе.
fn jina_check(text: &str) -> Result<(), String> {
    if !text.contains("Markdown Content:") {
        return Err("не похоже на ответ jina".into());
    }
    if let Some(p) = text.find("Target URL returned error") {
        let tail = text[p..].lines().next().unwrap_or("").trim().to_string();
        return Err(format!("jina: {tail}"));
    }
    // Заглушка антибота: тело есть, толку нет. Лучше честный отказ, чем «Access Denied»
    // в контексте как содержимое страницы.
    if text.contains("Access Denied") && text.contains("automation tools") {
        return Err("jina отдала страницу-заглушку блокировщика".into());
    }
    Ok(())
}

/// Время публикации из шапки jina — в своём выводе не нужно, отрезаем шапку.
fn jina_body(text: &str) -> String {
    match text.split_once("Markdown Content:") {
        Some((_, body)) => body.trim().to_string(),
        None => text.trim().to_string(),
    }
}

pub fn jina(a: &Args) -> Result<String, String> {
    let cl = client(a.timeout)?;
    let url = format!("{JINA}{}", a.url);
    let resp = cl
        .get(&url)
        .header(reqwest::header::USER_AGENT, UA_JINA)
        .header(reqwest::header::ACCEPT, "text/plain,text/markdown,*/*")
        .send()
        .map_err(|e| format!("{url}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("{url} → HTTP {}", status.as_u16()));
    }
    let mut bytes = Vec::new();
    resp.take(MAX_BODY)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    jina_check(&text).map_err(|e| format!("{url}: {e}"))?;
    let body = html::collapse_blank_lines(&jina_body(&text));
    if html::text_len(&body) < crate::FALLBACK_MIN {
        return Err(format!("{url}: пусто после разбора"));
    }
    Ok(body)
}

/// Имя файла: из content-disposition, иначе из URL. Расширение по content-type.
fn filename(a: &Args, ctype: &str, disposition: Option<&str>) -> Option<String> {
    if let Some(cd) = disposition {
        for part in cd.split(';') {
            let part = part.trim();
            if let Some(v) = part
                .strip_prefix("filename=")
                .or_else(|| part.strip_prefix("filename*="))
            {
                let v = v.trim_matches('"').trim_start_matches("UTF-8''");
                if !v.is_empty() {
                    return Some(sanitize(v));
                }
            }
        }
    }
    let path = a.url.split(['?', '#']).next().unwrap_or(&a.url);
    let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let base = if last.is_empty() || !last.contains('.') {
        "page".to_string()
    } else {
        sanitize(last)
    };
    if base.contains('.') {
        return Some(base);
    }
    // Расширение важно: по нему `attach.rs` ядра определяет mime вложения.
    let ext = [
        ("pdf", "pdf"),
        ("json", "json"),
        ("zip", "zip"),
        ("csv", "csv"),
        ("png", "png"),
        ("jpeg", "jpg"),
        ("gif", "gif"),
        ("webp", "webp"),
        ("plain", "txt"),
    ]
    .iter()
    .find(|(m, _)| ctype.contains(m))
    .map(|(_, e)| *e)
    .unwrap_or("bin");
    Some(format!("{base}.{ext}"))
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == '\0' {
                '_'
            } else {
                c
            }
        })
        .collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() {
        "page".into()
    } else {
        s
    }
}

/// Выбор режима фолбэка: он не должен маскировать ошибку — о срабатывании пишем в stderr.
pub fn fetch(a: &Args) -> String {
    match a.fallback {
        Fallback::Always => {
            eprintln!("fetch: --fallback always — беру r.jina.ai");
            jina(a).unwrap_or_else(|e| crate::die(&format!("fetch: {e}"), 1))
        }
        _ => match own(a) {
            Own::Text(md) => {
                let empty = html::text_len(&md) < crate::FALLBACK_MIN;
                let blocked = looks_blocked(&md);
                if a.fallback == Fallback::Auto && (empty || blocked) {
                    match jina(a) {
                        Ok(t) => {
                            let why = if blocked {
                                "страница-заглушка блокировщика"
                            } else {
                                "свой путь дал пустоту"
                            };
                            eprintln!("fetch: {why} — фолбэк r.jina.ai");
                            t
                        }
                        // Заглушку блокировщика в контекст не отдаём: это не содержимое
                        // страницы, и молчаливая подмена стоила бы дороже честного отказа.
                        Err(e) if blocked => crate::die(
                            &format!(
                                "fetch: {} — страница-заглушка блокировщика; \
                                 фолбэк r.jina.ai тоже не сработал ({e})",
                                a.url
                            ),
                            1,
                        ),
                        Err(e) => {
                            eprintln!(
                                "fetch: фолбэк r.jina.ai не сработал ({e}); оставляю свой результат"
                            );
                            md
                        }
                    }
                } else {
                    md
                }
            }
            Own::Binary(path_line) => path_line,
            Own::Unavailable(code, msg) => {
                // 403/429 — повод для фолбэка; прочее (404 и т.п.) — честная ошибка.
                if a.fallback == Fallback::Auto && (code == 403 || code == 429) {
                    match jina(a) {
                        Ok(t) => {
                            eprintln!("fetch: HTTP {code} от сайта — фолбэк r.jina.ai");
                            t
                        }
                        Err(e) => crate::die(
                            &format!("fetch: {msg}; фолбэк r.jina.ai тоже не сработал ({e})"),
                            1,
                        ),
                    }
                } else {
                    crate::die(&format!("fetch: {msg}"), 1)
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Fallback;

    fn args(url: &str, save: Option<&str>) -> Args {
        Args {
            url: url.into(),
            raw: false,
            fallback: Fallback::Auto,
            save: save.map(str::to_string),
            max_chars: 40_000,
            timeout: 30,
        }
    }

    #[test]
    fn blocked_page_is_recognised() {
        assert!(looks_blocked(
            "# Access Denied\nwe believe you are using automation tools"
        ));
        assert!(looks_blocked(
            "Please enable JavaScript and cookies to continue"
        ));
        assert!(!looks_blocked(
            "# Example Domain\nThis domain is for use in examples"
        ));
    }

    #[test]
    fn jina_wrapper_is_checked_not_trusted() {
        // Успех: обёртка есть, про ошибку цели ничего.
        assert!(jina_check("URL Source: x\n\nMarkdown Content:\nпривет").is_ok());
        // Цель ответила 403 — jina всё равно отдаёт 200.
        assert!(jina_check(
            "Warning: Target URL returned error 403: Forbidden\n\nMarkdown Content:\n"
        )
        .is_err());
        // Не jina вовсе (страница-заглушка блокировщика).
        assert!(jina_check("# Access Denied\nautomation tools").is_err());
    }

    #[test]
    fn filename_from_url_and_content_type() {
        assert_eq!(
            filename(
                &args("https://x.org/a/paper.pdf?y=1", None),
                "application/pdf",
                None
            )
            .unwrap(),
            "paper.pdf"
        );
        assert_eq!(
            filename(
                &args("https://x.org/download", None),
                "application/pdf",
                None
            )
            .unwrap(),
            "page.pdf",
            "расширение берётся из content-type, когда URL без него"
        );
        assert_eq!(
            filename(
                &args("https://x.org/d", None),
                "application/octet-stream",
                Some("attachment; filename=\"otchet.pdf\"")
            )
            .unwrap(),
            "otchet.pdf"
        );
    }
}
