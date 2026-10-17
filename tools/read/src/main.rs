// read — вывести файл с номерами строк (формат cat -n), чтобы на эти номера
// можно было ссылаться при правке. Ничего больше: листинг каталога — это `run ls`.
use std::fs;
use std::process::exit;

const DEFAULT_LIMIT: usize = 400;
const DEFAULT_MAX_CHARS: usize = 40_000;

const HELP: &str = "\
read — прочитать текстовый файл с номерами строк (формат cat -n).
read <путь> [--offset N] [--limit M] [--max-chars N]

Номера строк нужны, чтобы ссылаться на место правки. По умолчанию 400 строк.
Бинарные файлы отклоняются. Для каталога — `run ls`.";

fn die(msg: String) -> ! {
    eprintln!("{msg}");
    exit(1);
}

struct Args {
    path: String,
    offset: usize,
    limit: usize,
    max_chars: usize,
}

/// `--limit=5` → `["--limit", "5"]`: один цикл разбора вместо двух.
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

fn parse(argv: &[String]) -> Args {
    let argv = expand(argv);
    let mut path: Option<String> = None;
    let (mut offset, mut limit, mut max_chars) = (1usize, DEFAULT_LIMIT, DEFAULT_MAX_CHARS);
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        match a {
            "--offset" | "--limit" | "--max-chars" => {
                let v = argv
                    .get(i + 1)
                    .unwrap_or_else(|| die(format!("read: {a} без значения")));
                let n: usize = v
                    .parse()
                    .unwrap_or_else(|_| die(format!("read: {a} ждёт число, получено {v}")));
                match a {
                    "--offset" => offset = n.max(1),
                    "--limit" => limit = n,
                    _ => max_chars = n,
                }
                i += 2;
            }
            _ if a.starts_with('-') && a != "-" => die(format!("read: неизвестный аргумент {a}")),
            _ => {
                if path.is_some() {
                    die("read: путь указан дважды".into());
                }
                path = Some(a.to_string());
                i += 1;
            }
        }
    }
    Args {
        path: tilde(&path.unwrap_or_else(|| die("read: не указан путь".into()))),
        offset,
        limit,
        max_chars,
    }
}

/// `~/x` → `$HOME/x`: тул исполняется без оболочки, поэтому `~` до него
/// доходит буквально. Разворачиваем только ведущий `~` — `~user` и `~` в середине
/// оставляем как есть (нужен passwd-лукап, а выгода нулевая).
fn tilde(path: &str) -> String {
    let rest = match path.strip_prefix("~") {
        Some(r) if r.is_empty() || r.starts_with('/') => r,
        _ => return path.to_string(),
    };
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => format!("{home}{rest}"),
        _ => path.to_string(),
    }
}

fn slurp(path: &str) -> Vec<u8> {
    match fs::read(path) {
        Ok(d) => d,
        Err(e) => match e.kind() {
            std::io::ErrorKind::NotFound => die(format!("read: файл не найден: {path}")),
            std::io::ErrorKind::PermissionDenied => die(format!("read: нет доступа: {path}")),
            std::io::ErrorKind::IsADirectory => die(format!(
                "read: {path} — это каталог, для списка используй `run ls`"
            )),
            _ => die(format!("read: {path}: {e}")),
        },
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "-h" || argv[0] == "--help" {
        println!("{HELP}");
        exit(if argv.is_empty() { 1 } else { 0 });
    }
    let a = parse(&argv);
    let data = slurp(&a.path);
    // NUL в первых килобайтах — достаточный признак бинарника; мусор в контексте
    // дороже, чем отказ.
    if data.iter().take(8192).any(|b| *b == 0) {
        die(format!(
            "read: {} — бинарный файл ({} Б), не читаю",
            a.path,
            data.len()
        ));
    }

    let text = String::from_utf8_lossy(&data);
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if total == 0 {
        println!("[{}: пустой файл]", a.path);
        return;
    }
    if a.offset > total {
        die(format!(
            "read: offset {} за концом файла (всего {total} строк)",
            a.offset
        ));
    }

    let end = a
        .offset
        .saturating_sub(1)
        .saturating_add(a.limit)
        .min(total);
    let chunk = &lines[a.offset - 1..end];
    let width = std::cmp::max(6, (a.offset + chunk.len() - 1).to_string().len());

    let mut out: Vec<String> = Vec::new();
    let mut used = 0usize;
    for (k, line) in chunk.iter().enumerate() {
        let row = format!("{:>width$}\t{}", a.offset + k, line);
        if used + row.chars().count() + 1 > a.max_chars {
            break;
        }
        used += row.chars().count() + 1;
        out.push(row);
    }

    println!("{}", out.join("\n"));
    let shown = out.len();
    if shown == 0 {
        println!(
            "[строки не поместились при --max-chars {}, увеличь его или сузь --limit]",
            a.max_chars
        );
        return;
    }
    if a.offset > 1 || shown < total {
        println!(
            "[показаны строки {}-{} из {}]",
            a.offset,
            a.offset + shown - 1,
            total
        );
    }
}
