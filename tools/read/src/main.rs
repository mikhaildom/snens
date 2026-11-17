// read — вывести файл с номерами строк (формат cat -n), чтобы на эти номера
// можно было ссылаться при правке. Ничего больше: листинг каталога — это `run ls`.
use std::fs;
use std::io::BufReader;
use std::process::exit;

/// До этого размера файла считаем общее число строк целиком: иначе футер
/// «показаны 1-2 из N» невозможен без чтения всего файла.
const COUNT_TOTAL_UPTO: u64 = 8 * 1024 * 1024;
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
                    // Ноль строк — не «всё»: пустое окно потом сваливается на
                    // диагностику про --max-chars, хотя причина в --limit.
                    "--limit" => limit = n.max(1),
                    _ => max_chars = n,
                }
                i += 2;
            }
            // Хелп — в любом месте аргументов, а не только первым.
            "-h" | "--help" => {
                println!("{HELP}");
                exit(0);
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
    match std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        Ok(home) if !home.is_empty() => format!("{home}{rest}"),
        _ => path.to_string(),
    }
}

/// Открыть файл, превратив типовые ошибки в человеческие сообщения.
fn open(path: &str) -> fs::File {
    // Каталог на Linux открывается успешно и падает уже на чтении: проверяем до.
    if fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false) {
        die(format!(
            "read: {path} — это каталог, для списка используй `run ls`"
        ));
    }
    match fs::File::open(path) {
        Ok(f) => f,
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
    let size = fs::metadata(&a.path).map(|m| m.len()).unwrap_or(0);
    let mut rd = BufReader::new(open(&a.path));

    // Бинарник отсекаем по голове файла: NUL в первых килобайтах — достаточный
    // признак, а мусор в контексте дороже отказа.
    let mut head = [0u8; 8192];
    let n = std::io::Read::read(&mut rd, &mut head).unwrap_or(0);
    if head[..n].contains(&0) {
        die(format!(
            "read: {} — бинарный файл ({size} Б), не читаю",
            a.path
        ));
    }
    use std::io::Seek;
    if let Err(e) = rd.seek(std::io::SeekFrom::Start(0)) {
        die(format!("read: {}: {e}", a.path));
    }

    // Окно читаем потоком. Общее число строк считаем до конца только у файла,
    // который влезает в COUNT_TOTAL_UPTO: ради футера «из N» читать 2 ГБ лога —
    // неверная цена, и тогда честнее сказать «файл длиннее».
    let count_all = size <= COUNT_TOTAL_UPTO;
    let mut total = 0usize;
    let mut at_end = true;
    let mut chunk: Vec<String> = Vec::new();
    let mut buf = Vec::new();
    loop {
        if !count_all && chunk.len() >= a.limit {
            // Хвост не считаем; проверяем только «есть ли ещё строки».
            let mut one = Vec::new();
            match std::io::BufRead::read_until(&mut rd, b'\n', &mut one) {
                Ok(0) => {}
                Ok(_) => {
                    at_end = false;
                    total += 1;
                }
                Err(e) => die(format!("read: {}: {e}", a.path)),
            }
            break;
        }
        buf.clear();
        match std::io::BufRead::read_until(&mut rd, b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => die(format!("read: {}: {e}", a.path)),
        }
        total += 1;
        if total >= a.offset && chunk.len() < a.limit {
            let line = String::from_utf8_lossy(&buf);
            chunk.push(line.trim_end_matches(['\n', '\r']).to_string());
        }
    }
    if total == 0 && chunk.is_empty() {
        println!("[{}: пустой файл]", a.path);
        return;
    }
    if a.offset > total {
        die(format!(
            "read: offset {} за концом файла (всего {total} строк)",
            a.offset
        ));
    }
    let width = std::cmp::max(6, (a.offset + chunk.len().saturating_sub(1)).to_string().len());

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

    // Тело печатаем только если оно есть: иначе перед диагностикой пустая строка.
    let shown = out.len();
    if shown > 0 {
        println!("{}", out.join("\n"));
    }
    if shown == 0 {
        println!(
            "[строки не поместились при --max-chars {}, увеличь его или сузь --limit]",
            a.max_chars
        );
        return;
    }
    if a.offset > 1 || !at_end || shown < total {
        if at_end {
            println!(
                "[показаны строки {}-{} из {}]",
                a.offset,
                a.offset + shown - 1,
                total
            );
        } else {
            // Хвост не считали: честно говорим «и дальше есть», а не выдумываем число.
            println!(
                "[показаны строки {}-{}; файл длиннее — total не считался]",
                a.offset,
                a.offset + shown - 1
            );
        }
    }
}
