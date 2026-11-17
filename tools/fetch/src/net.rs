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
    // «security» — слишком частое слово: страница про права доступа на сайте про
    // безопасность не должна уезжать в jina. Признак стаба — «automation tools»/капча.
    (low.contains("access denied")
        && (low.contains("automation tools") || low.contains("captcha")))
        || low.contains("enable javascript and cookies")
        || low.contains("unusual traffic")
}

pub enum Own {
    Text(String),
    Binary(String),
    /// Ответ пришёл, но не тот: HTTP-код и сообщение для stderr.
    Http(u16, String),
    /// Ответа нет вовсе: сеть, таймаут, локальная запись файла. Ноль как код
    /// HTTP смешивал бы это с «сайт ответил 0» и врал в решении о фолбэке.
    Offline(String),
}

/// Байты тела → строка по charset из content-type. Неизвестная кодировка — UTF-8
/// с заменой невалидных байтов: это лучше, чем пустой документ.
fn decode(bytes: &[u8], ctype: &str) -> String {
    let label = ctype
        .split(';')
        .filter_map(|p| p.trim().strip_prefix("charset="))
        .map(|v| v.trim().trim_matches('"').to_ascii_lowercase())
        .next();
    match label
        .as_deref()
        .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
    {
        Some(enc) => enc.decode(bytes).0.into_owned(),
        None => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Свой путь: GET и разбор по content-type.
pub fn own(a: &Args) -> Own {
    let cl = match client(a.timeout) {
        Ok(c) => c,
        Err(e) => return Own::Offline(format!("{}: {e}", a.url)),
    };
    let resp = match with_headers(cl.get(&a.url)).send() {
        Ok(r) => r,
        Err(e) => return Own::Offline(format!("{}: {e}", a.url)),
    };
    let status = resp.status();
    if !status.is_success() {
        return Own::Http(
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
        // Читаем на байт больше потолка, чтобы отличить «ровно столько» от «упёрлось».
        let mut bytes = Vec::new();
        if let Err(e) = resp.take(MAX_BODY + 1).read_to_end(&mut bytes) {
            // Обрыв посреди тела дал бы обрезанный документ под видом целого.
            return Own::Offline(format!("{}: {e}", a.url));
        }
        let cut = bytes.len() as u64 > MAX_BODY;
        if cut {
            bytes.truncate(MAX_BODY as usize);
            eprintln!("fetch: тело обрезано по потолку {MAX_BODY} Б ({})", a.url);
        }
        // Кодировка — из charset в content-type: `from_utf8_lossy` превратил бы
        // windows-1251/koi8-r (а в ACCEPT_LANG есть ru) в мусор.
        let text = decode(&bytes, &ctype);
        return Own::Text(html::to_markdown(&text, a.raw));
    }

    // Бинарь не держим в памяти и не режем потолком: обрезанный на 2 МБ PDF —
    // битый файл, а не «текст подлиннее». Пишем потоком прямо в файл.
    let name = filename(a, &ctype, disposition.as_deref());
    match save_binary(a, resp, name.as_deref(), &ctype) {
        Ok(line) => Own::Binary(line),
        Err(e) => Own::Offline(format!("{}: {e}", a.url)),
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
    // Шапка — только до «Markdown Content:»: иначе страница, в тексте которой
    // встречается эта фраза, считалась бы ошибкой доставки.
    let head = text.split_once("Markdown Content:").map_or(text, |(h, _)| h);
    if let Some(p) = head.find("Target URL returned error") {
        let tail = head[p..].lines().next().unwrap_or("").trim().to_string();
        return Err(format!("jina: {tail}"));
    }
    // Заглушка антибота: тело есть, толку нет. Проверка та же, что у своего пути, —
    // регистр и локализованные варианты разбирает `looks_blocked`.
    if looks_blocked(text) {
        return Err("jina отдала страницу-заглушку блокировщика".into());
    }
    Ok(())
}

/// Время публикации из шапки jina — в своём выводе не нужно, отрезаем шапку.
/// Маркер гарантирован `jina_check`, который вызывается раньше; запасной ветки
/// нет намеренно, иначе проверка и нарезка разошлись бы.
fn jina_body(text: &str) -> String {
    match text.split_once("Markdown Content:") {
        Some((_, body)) => body.trim().to_string(),
        None => String::new(),
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
        .map_err(|e| format!("{url}: {e}"))?;
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
        if let Some(v) = disposition_filename(cd) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(sanitize(v));
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

/// `filename`/`filename*` из Content-Disposition по RFC 6266: имена параметров
/// без учёта регистра, значение в кавычках не режется по `;`, у `*` снимается
/// ровно префикс `charset'lang'`.
fn disposition_filename(cd: &str) -> Option<String> {
    let mut rest = cd;
    let mut plain: Option<String> = None;
    let mut extended: Option<String> = None;
    while let Some(eq) = rest.find('=') {
        // Имя параметра — всё после последней `;`: `attachment; filename=x`.
        let name = rest[..eq]
            .rsplit(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let after = &rest[eq + 1..];
        let (value, tail) = if let Some(stripped) = after.trim_start().strip_prefix('"') {
            match stripped.find('"') {
                Some(end) => (&stripped[..end], &stripped[end + 1..]),
                None => (stripped, ""),
            }
        } else {
            match after.find(';') {
                Some(end) => (&after[..end], &after[end..]),
                None => (after, ""),
            }
        };
        if name == "filename" && plain.is_none() {
            plain = Some(value.trim().to_string());
        } else if name == "filename*" && extended.is_none() {
            // `utf-8''имя` → `имя`. charset/lang нужны только для декодирования,
            // а процент-кодирование разворачиваем сами: `%D0%9F` иначе попадёт
            // в имя файла буквально.
            let (charset, v) = match value.split_once("''") {
                Some((cs, v)) => (cs, v),
                None => ("utf-8", value),
            };
            extended = Some(percent_decode(v, charset).trim().to_string());
        }
        rest = tail;
    }
    extended.or(plain)
}

/// Процент-кодирование из `filename*`: байты собираются и декодируются по charset.
/// Ошибки декодирования не фатальны — берём UTF-8 с заменой.
fn percent_decode(v: &str, charset: &str) -> String {
    let mut bytes = Vec::with_capacity(v.len());
    let mut it = v.bytes();
    while let Some(b) = it.next() {
        if b == b'%' {
            let hi = it.next().and_then(hex);
            let lo = it.next().and_then(hex);
            match (hi, lo) {
                (Some(h), Some(l)) => bytes.push(h * 16 + l),
                _ => {
                    bytes.push(b'%');
                    if let Some(h) = hi {
                        bytes.push(h);
                    }
                }
            }
        } else {
            bytes.push(b);
        }
    }
    let enc = encoding_rs::Encoding::for_label(charset.trim().as_bytes())
        .unwrap_or(encoding_rs::UTF_8);
    enc.decode(&bytes).0.into_owned()
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

fn sanitize(name: &str) -> String {
    // Управляющие символы из заголовка (\n, \r, ANSI-последовательности) в имени
    // файла и в выводе пути недопустимы: это инъекция в терминал.
    let s: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == '\0' {
                '_'
            } else {
                c
            }
        })
        .filter(|c| !c.is_control())
        .collect();
    let s = s.trim_matches('.').to_string();
    // Длина: ENAMETOOLONG из File::create хуже понятного усечения.
    let s = match s.char_indices().nth(120) {
        Some((idx, _)) => s[..idx].to_string(),
        None => s,
    };
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
            Own::Offline(msg) => {
                // Ответа нет вовсе — фолбэк по режиму: сайт не отказал, до него не дошли.
                if a.fallback == Fallback::Auto {
                    match jina(a) {
                        Ok(t) => {
                            eprintln!("fetch: свой путь не дошёл до сайта — фолбэк r.jina.ai");
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
            Own::Http(code, msg) => {
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
