// fetch — прочитать веб-страницу как markdown. Свой путь: GET с честными заголовками,
// очистка DOM, конвертация в markdown. Бинарь (pdf и прочее не-html) сливается в файл.
// Фолбэк на r.jina.ai — когда свой путь упёрся в 403/429 или отдал пустоту.
mod cli;
mod html;
mod net;

use std::io::Read;
use std::process::exit;

use cli::{Args, HELP, USAGE};

/// Лимит тела для текста: страница длиннее — уже не для контекста, а бомба по памяти.
const MAX_BODY: u64 = 2 * 1024 * 1024;
/// Ниже этого порога после очистки результат считается пустым и идёт фолбэк.
/// Порог низкий намеренно: короткая страница-заглушка (example.com) — это текст,
/// и подменять его ответом jina незачем; пустой JS-шелл даёт ноль-двадцать символов.
const FALLBACK_MIN: usize = 80;

fn die(msg: &str, code: i32) -> ! {
    eprintln!("{msg}");
    exit(code);
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if !argv.is_empty() && (argv[0] == "-h" || argv[0] == "--help") {
        println!("{HELP}");
        exit(0);
    }
    if argv.is_empty() {
        eprintln!("{USAGE}");
        exit(2);
    }
    let a = cli::parse(&argv);
    if !(a.url.starts_with("http://") || a.url.starts_with("https://")) {
        die(
            &format!(
                "fetch: поддерживаются только http/https: {}\n{USAGE}",
                a.url
            ),
            2,
        );
    }
    print!("{}", trim(&net::fetch(&a), a.max_chars));
    exit(0);
}

/// Слить бинарь в файл и напечатать путь. PDF без --save и с pdftotext — напечатать текст.
/// Тело пишется потоком: бинарь бывает крупным, а обрезанный PDF — просто битый файл.
fn save_binary(
    a: &Args,
    resp: reqwest::blocking::Response,
    name: Option<&str>,
    ctype: &str,
) -> Result<String, String> {
    let explicit = a.save.is_some();
    let fname = name.unwrap_or("fetch.bin");
    let is_pdf = ctype.contains("pdf") || fname.to_ascii_lowercase().ends_with(".pdf");
    let attach = std::env::var("SNENS_ATTACH_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty());
    // Временность помечаем явно: путь под /tmp не значит «временный» (там же может
    // лежать каталог вложений человека).
    let (path, temp) = match (&a.save, attach) {
        (Some(p), _) => (std::path::PathBuf::from(p), false),
        (None, Some(d)) => (std::path::PathBuf::from(d).join(fname), false),
        // Ни --save, ни каталога вложений: PDF всё равно стоит попробовать вытащить текстом
        // (файл временный) — иначе человеку нужна подсказка, куда класть.
        (None, None) if is_pdf && has_pdftotext() => (std::env::temp_dir().join(fname), true),
        (None, None) => {
            return Err(format!(
                "не-html ответ ({ctype}); куда сохранить неясно — укажи --save путь \
                 или задай SNENS_ATTACH_DIR"
            ))
        }
    };
    // Каталог получателя должен существовать: --save /путь/в/новый/каталог.
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
    }
    let mut f = std::fs::File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    std::io::copy(&mut resp.take(u64::MAX), &mut f)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    drop(f);

    // PDF без --save: человек просил текст, а не файл — отдаём текст, временный файл убираем.
    if is_pdf && !explicit {
        if let Some(text) = pdftotext(&path) {
            if temp {
                let _ = std::fs::remove_file(&path);
            }
            return Ok(text);
        }
    }
    Ok(path.display().to_string())
}

fn has_pdftotext() -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v pdftotext")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Что вытащил системный pdftotext. Нет утилиты/ошибка — None (отдаём путь к файлу).
fn pdftotext(path: &std::path::Path) -> Option<String> {
    if !has_pdftotext() {
        return None;
    }
    let out = std::process::Command::new("pdftotext")
        .arg("-q")
        .arg(path)
        .arg("-")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(html::collapse_blank_lines(&text))
    }
}

/// Обрезка по --max-chars с пометкой, сколько отброшено (в символах).
fn trim(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return ensure_nl(s);
    }
    let cut: String = s.chars().take(max_chars).collect();
    let dropped = s.chars().count() - max_chars;
    // Хвостовую пустоту среза снимаем только переводами строк: trim_end съел бы
    // пробелы-паддинг строки таблицы, на которой упало окно, и срез схлопнулся бы вдвое.
    format!(
        "{}\n…[обрезано: {dropped} символов]\n",
        cut.trim_end_matches('\n')
    )
}

fn ensure_nl(s: &str) -> String {
    if s.is_empty() || s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_marks_truncation_and_keeps_short_text() {
        assert_eq!(trim("короткий\n", 100), "короткий\n");
        assert_eq!(trim("без перевода строки", 100), "без перевода строки\n");
        let long = "x".repeat(50);
        let out = trim(&long, 10);
        assert!(
            out.starts_with(&"x".repeat(10)),
            "срез не по границе: {out}"
        );
        assert!(out.contains("обрезано: 40 символов"), "нет пометки: {out}");
    }

    #[test]
    fn trim_does_not_eat_table_padding_at_the_cut() {
        let md = format!("| {} |", " ".repeat(100));
        let out = trim(&md, 20);
        // Срез — ровно 20 символов: «|» и 19 пробелов-паддинга, ни один не снят.
        assert!(
            out.starts_with(&format!("|{}", " ".repeat(19))),
            "пробелы-паддинг среза съедены: {out:?}"
        );
    }
}
