// index — скелет файла: импорты, типы, сигнатуры с номерами строк. Дешевле
// полного чтения в разы, потому что модель читает карту, а потом добирает
// нужные диапазоны через read --offset/--limit.
mod lang;
mod render;
mod skeleton;

use std::fs;
use std::process::exit;

use lang::EXTENSIONS;
use skeleton::skeleton_of;

const USAGE: &str = "index <путь> [--max-chars N]";
const HELP: &str = "\
index — скелет файла: импорты, типы, сигнатуры с номерами строк в [скобках].
index <путь> [--max-chars N]

Отвечает картой файла вместо всего текста: у объявления печатается заголовок и
номер строки, у структуры/класса/impl — вложенные поля и методы. Дальше нужное
дочитывается через read --offset/--limit, а не чтением файла целиком.

Поддержано: rs, py/pyi, ts/tsx/js/jsx/mjs/cjs, go, sh/bash/zsh.
Не поддержано (или файл с ошибками разбора) — отказ с советом читать read.";

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        println!("{HELP}");
        exit(0);
    }
    let (path, max_chars) = match parse(&argv) {
        Ok(v) => v,
        // Usage в отказе: модель, промахнувшаяся аргументом, видит правильную
        // форму сразу, а не после второго вызова с --help.
        Err(e) => die(format!("{e}\n{USAGE}"), 1),
    };
    match run(&path, max_chars) {
        Ok(out) => {
            if out.trim().is_empty() {
                // Пустой скелет — не «файл пуст», а «описывать нечего»: пусть
                // модель прочитает файл, а не решит, что он пустой.
                die(
                    format!("index: {path} — в файле нет ни одного объявления, читай через read"),
                    1,
                );
            }
            println!("{out}");
        }
        Err(e) => die(e, 1),
    }
}

fn die(msg: String, code: i32) -> ! {
    eprintln!("{msg}");
    exit(code);
}

fn parse(argv: &[String]) -> Result<(String, usize), String> {
    let argv = expand(argv);
    let mut path: Option<String> = None;
    let mut max_chars = 0usize;
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        match a {
            "--max-chars" => {
                let v = argv.get(i + 1).ok_or("index: --max-chars без значения")?;
                max_chars = v
                    .parse()
                    .map_err(|_| format!("index: --max-chars ждёт число, получено {v}"))?;
                i += 2;
            }
            _ if a.starts_with('-') && a != "-" => {
                return Err(format!("index: неизвестный аргумент {a}"))
            }
            _ => {
                if path.is_some() {
                    return Err("index: путь указан дважды".into());
                }
                path = Some(tilde(a));
                i += 1;
            }
        }
    }
    Ok((path.ok_or("index: не указан путь")?, max_chars))
}

/// `--max-chars=8000` → `["--max-chars", "8000"]`.
fn expand(argv: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for a in argv {
        match a.strip_prefix("--").and_then(|r| r.split_once('=')) {
            Some((k, v)) => {
                out.push(format!("--{k}"));
                out.push(v.to_string());
            }
            None => out.push(a.clone()),
        }
    }
    out
}

/// От какой базы искали относительный путь: без этого отказ «файл не найден»
/// не отличить от «файла правда нет» (`read-path-cwd-mismatch`).
fn where_looked(path: &str) -> String {
    if path.starts_with('/') || path.starts_with('~') {
        return String::new();
    }
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".into());
    format!(
        "\n[искал от {cwd}: относительный путь разрешается от рабочего каталога сессии.\n \
         Сменить его — `cd <каталог>` в run или поле cwd у вызова; абсолютный путь — всегда верно]"
    )
}

/// `~/x` → `$HOME/x`: тул исполняется без оболочки, `~` доезжает буквально.
fn tilde(path: &str) -> String {
    let rest = match path.strip_prefix("~") {
        Some(r) if r.is_empty() || r.starts_with('/') => r,
        _ => return path.to_string(),
    };
    match std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        Ok(home) if !home.is_empty() => format!("{home}{rest}"),
        _ => path.to_string(),
    }
}

fn run(path: &str, max_chars: usize) -> Result<String, String> {
    let meta = fs::metadata(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            format!("index: файл не найден: {path}{}", where_looked(path))
        }
        std::io::ErrorKind::PermissionDenied => format!("index: нет доступа: {path}"),
        _ => format!("index: {path} — {e}"),
    })?;
    if meta.is_dir() {
        return Err(format!(
            "index: {path} — это каталог, для списка используй `run ls`"
        ));
    }
    let ext = path
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .ok_or_else(|| format!("index: у {path} нет расширения — читай через read"))?;
    if lang::language(&ext).is_none() {
        return Err(format!(
            "index: .{ext} не поддержан (есть: {}) — читай через read с offset/limit",
            EXTENSIONS.join(", ")
        ));
    }
    let src = fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::InvalidData => {
            format!("index: {path} — не текст (другая кодировка или бинарник), читай через read")
        }
        _ => format!("index: {path} — {e}"),
    })?;
    let mut out = skeleton_of(&src, &ext)
        .ok_or_else(|| format!("index: .{ext} не поддержан — читай через read"))?;
    if max_chars > 0 && out.chars().count() > max_chars {
        out = format!(
            "{}\n[скелет обрезан: {} символов из {} — сузь через --max-chars или читай read]",
            cut_chars(&out, max_chars),
            max_chars,
            out.chars().count()
        );
    }
    Ok(out)
}

/// Обрезка по границе символа: скелет на кириллице режется не в середине UTF-8.
fn cut_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equals_form_is_expanded() {
        assert_eq!(expand(&["--max-chars=5".into()]), vec!["--max-chars", "5"]);
    }

    #[test]
    fn directory_is_refused() {
        let e = run("/tmp", 0).unwrap_err();
        assert!(e.contains("каталог"), "{e}");
    }

    #[test]
    fn unsupported_extension_names_the_supported_ones() {
        let p = std::env::temp_dir().join("snens-index-test.txt");
        fs::write(&p, "x").unwrap();
        let e = run(p.to_str().unwrap(), 0).unwrap_err();
        assert!(e.contains("не поддержан"), "{e}");
        assert!(e.contains("rs"), "{e}");
        let _ = fs::remove_file(&p);
    }
}
