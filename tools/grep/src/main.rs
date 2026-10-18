// grep — поиск по файлам: путь, строка, текст совпадения. Обход и фильтры свои,
// потому что `grep -rn` из оболочки тащит в контекст служебный мусор (.git, target,
// node_modules) и не умеет ни лимита с пометкой, ни «нет совпадений — это ответ».
use std::fs;
use std::path::{Path, PathBuf};
use std::process::exit;

const DEFAULT_LIMIT: usize = 200;
const LINE_MAX: usize = 400;
/// Файлы больше этого не читаем: в контексте модели они всё равно не помещаются.
const SKIP_BYTES: u64 = 1024 * 1024;
/// Первые килобайты файла: NUL в них — достаточный признак бинарника.
const SNIFF: usize = 8192;
const SKIP_DIRS: [&str; 10] = [
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".cargo",
    "dist",
    "build",
    "bin",
];

const HELP: &str = "\
grep — поиск по файлам: путь, строка, текст совпадения.
grep <шаблон> [<путь>...] [--glob G] [--context N] [--limit N] [--ignore-case] [--word] [--files-only] [--no-ignore]

Поиск литеральный (регулярок нет): --word — только целым словом, --ignore-case — без
учёта регистра. Обход рекурсивный; по умолчанию пропускаются каталоги
.git/target/node_modules/…, файлы > 1 МБ и бинарные (--no-ignore снимает пропуск
каталогов). Печатает `путь:строка: текст`, при --context N соседние строки —
`путь:строка- текст`. Нет совпадений — код 0 и строка «нет совпадений».";

fn die(msg: String) -> ! {
    eprintln!("{msg}");
    exit(1);
}

#[derive(Clone)]
struct Args {
    pattern: String,
    paths: Vec<String>,
    globs: Vec<String>,
    context: usize,
    limit: usize,
    ignore_case: bool,
    word: bool,
    files_only: bool,
    no_ignore: bool,
}

/// `--limit=5` → `["--limit", "5"]`: один цикл разбора вместо двух (как в read).
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
    let mut pattern: Option<String> = None;
    let mut paths: Vec<String> = Vec::new();
    let mut globs: Vec<String> = Vec::new();
    let (mut context, mut limit) = (0usize, DEFAULT_LIMIT);
    let (mut ignore_case, mut word, mut files_only, mut no_ignore) = (false, false, false, false);
    // После `--` идёт путь даже если он начинается с дефиса.
    let mut only_paths = false;
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        if only_paths {
            paths.push(a.to_string());
            i += 1;
            continue;
        }
        match a {
            "--" => only_paths = true,
            "--ignore-case" => ignore_case = true,
            "--word" => word = true,
            "--files-only" => files_only = true,
            "--no-ignore" => no_ignore = true,
            "--glob" | "--context" | "--limit" => {
                let v = argv
                    .get(i + 1)
                    .unwrap_or_else(|| die(format!("grep: {a} без значения")));
                if a == "--glob" {
                    globs.push(v.clone());
                } else {
                    let n: usize = v.parse().unwrap_or_else(|_| {
                        die(format!("grep: {a} ждёт неотрицательное число, получено {v}"))
                    });
                    if a == "--context" {
                        context = n;
                    } else {
                        limit = n;
                    }
                }
                i += 2;
                continue;
            }
            _ if a.starts_with('-') && a != "-" => die(format!("grep: неизвестный аргумент {a}")),
            _ => match pattern {
                None => pattern = Some(a.to_string()),
                // Пустой шаблон — не опечатка разбора: `grep ""` совпадал бы со всем.
                Some(_) => paths.push(a.to_string()),
            },
        }
        i += 1;
    }
    if limit == 0 {
        die("grep: --limit 0 — показывать нечего".into());
    }
    Args {
        pattern: pattern.unwrap_or_else(|| die("grep: не указан шаблон".into())),
        paths: if paths.is_empty() {
            vec![".".to_string()]
        } else {
            paths.iter().map(|p| tilde(p)).collect()
        },
        globs,
        context,
        limit,
        ignore_case,
        word,
        files_only,
        no_ignore,
    }
}

/// `~/x` → `$HOME/x`: тул исполняется без оболочки, поэтому `~` доходит до него
/// буквально. Разворачиваем только ведущий `~` (как read).
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

/// `*.rs` / `main.r?`: только `*` и `?`, без классов символов — фильтр по имени
/// файла, а не глоб-движок.
fn glob_match(pat: &str, name: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    // dp[j] — совпадает ли разобранный префикс шаблона с первыми j символами имени.
    let mut dp = vec![false; n.len() + 1];
    dp[0] = true;
    for pc in &p {
        let mut next = vec![false; n.len() + 1];
        if *pc == '*' {
            // `*` съедает ноль символов или продолжает серию.
            next[0] = dp[0];
            for j in 1..=n.len() {
                next[j] = dp[j] || next[j - 1];
            }
        } else {
            for j in 1..=n.len() {
                next[j] = dp[j - 1] && (*pc == '?' || *pc == n[j - 1]);
            }
        }
        dp = next;
    }
    dp[n.len()]
}

fn ignored_dir(name: &str) -> bool {
    SKIP_DIRS.contains(&name)
}

/// Обход: симлинки не разворачиваем (циклы), каталоги-исключения режем целиком.
fn walk(dir: &Path, rel: &str, args: &Args, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let Some(name) = p.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        let child = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let Ok(md) = p.symlink_metadata() else {
            continue;
        };
        if md.file_type().is_symlink() {
            continue;
        }
        if md.is_dir() {
            if !args.no_ignore && ignored_dir(&name) {
                continue;
            }
            walk(&p, &child, args, out);
            continue;
        }
        if !md.is_file() || md.len() > SKIP_BYTES {
            continue;
        }
        if !args.globs.is_empty() && !args.globs.iter().any(|g| glob_match(g, &name)) {
            continue;
        }
        out.push((child, p));
    }
}

/// Текст файла или None, если он бинарный — пропуск молча, как и просит контракт.
fn read_text(path: &Path) -> Option<String> {
    let data = fs::read(path).ok()?;
    if data.iter().take(SNIFF).any(|b| *b == 0) {
        return None;
    }
    Some(String::from_utf8_lossy(&data).into_owned())
}

/// Строка, обрезанная до LINE_MAX символов — с многоточием, чтобы обрезка была видна.
fn clip(line: &str) -> String {
    if line.chars().count() <= LINE_MAX {
        return line.to_string();
    }
    let head: String = line.chars().take(LINE_MAX).collect();
    format!("{head}…")
}

/// Совпадение шаблона в строке: поиск литеральный, --word и --ignore-case — надстройки.
fn hits(line: &str, pattern: &str, ignore_case: bool, word: bool) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let (hay, needle) = if ignore_case {
        (line.to_lowercase(), pattern.to_lowercase())
    } else {
        (line.to_string(), pattern.to_string())
    };
    let mut from = 0;
    while let Some(pos) = hay[from..].find(&needle) {
        let start = from + pos;
        let end = start + needle.len();
        if !word {
            return true;
        }
        // Граница слова — не буквенно-цифровой сосед. `_` и `-` границей не считаем:
        // иначе `dispatch` в `dispatch_tool` выдавало бы себя за целое слово.
        let edge = |c: char| !(c.is_alphanumeric() || c == '_' || c == '-');
        let before_ok = hay[..start].chars().next_back().map(edge).unwrap_or(true);
        let after_ok = hay[end..].chars().next().map(edge).unwrap_or(true);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
        if from >= hay.len() {
            return false;
        }
    }
    false
}

struct Row {
    /// 0 — разделитель блоков, не строка файла.
    num: usize,
    text: String,
    is_match: bool,
}

struct Found {
    file: String,
    /// Всего совпадений в файле — даже тех, что не поместились в лимит.
    total: usize,
    rows: Vec<Row>,
    shown: usize,
}

fn scan_file(file: &str, path: &Path, args: &Args, budget: usize) -> Option<Found> {
    let text = read_text(path)?;
    let lines: Vec<&str> = text.lines().collect();
    let matched: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| hits(l, &args.pattern, args.ignore_case, args.word))
        .map(|(i, _)| i)
        .collect();
    if matched.is_empty() {
        return None;
    }
    // В вывод идут только первые `budget` совпадений, но общее число считаем по всем:
    // пометка об обрезке должна называть настоящий M.
    let take = matched.len().min(budget);
    let mut rows: Vec<Row> = Vec::new();
    let mut last: Option<usize> = None;
    for &m in matched.iter().take(take) {
        let lo = m.saturating_sub(args.context);
        let hi = (m + args.context).min(lines.len().saturating_sub(1));
        for (i, line) in lines.iter().enumerate().take(hi + 1).skip(lo) {
            if let Some(prev) = last {
                if i <= prev {
                    // Строка уже выведена; остаётся уточнить, что она совпадение.
                    if i == m {
                        if let Some(r) = rows.iter_mut().find(|r| r.num == m + 1) {
                            r.is_match = true;
                        }
                    }
                    continue;
                }
                // Разрыв между блоками контекста помечаем `--`, как grep: без него
                // несмежные строки читаются как одна пачка. Без контекста пачки нет.
                if i > prev + 1 && args.context > 0 {
                    rows.push(Row {
                        num: 0,
                        text: String::new(),
                        is_match: false,
                    });
                }
            }
            rows.push(Row {
                num: i + 1,
                text: line.to_string(),
                is_match: i == m,
            });
            last = Some(i);
        }
    }
    Some(Found {
        file: file.to_string(),
        total: matched.len(),
        rows,
        shown: take,
    })
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "-h" || argv[0] == "--help" {
        println!("{HELP}");
        exit(if argv.is_empty() { 1 } else { 0 });
    }
    let args = parse(&argv);

    // Корни проверяем до обхода: молчаливое «нет совпадений» на опечатке в пути
    // выглядело бы честным ответом.
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for root in &args.paths {
        let p = PathBuf::from(root);
        let Ok(md) = fs::metadata(&p) else {
            die(format!("grep: нет такого пути: {root}"));
        };
        if md.is_dir() {
            walk(&p, "", &args, &mut files);
        } else if md.is_file() {
            // Названный файл — выбор человека, --glob и размер тут не указ.
            files.push((root.clone(), p));
        } else {
            die(format!("grep: не файл и не каталог: {root}"));
        }
    }

    let mut total = 0usize;
    let mut shown = 0usize;
    let mut out: Vec<String> = Vec::new();
    let mut listed: Vec<String> = Vec::new();
    for (file, path) in &files {
        let budget = args.limit.saturating_sub(shown);
        let Some(found) = scan_file(file, path, &args, budget.max(1)) else {
            continue;
        };
        total += found.total;
        if args.files_only {
            listed.push(found.file);
            continue;
        }
        if budget == 0 {
            continue;
        }
        for r in &found.rows {
            if r.num == 0 {
                out.push("--".to_string());
                continue;
            }
            let sep = if r.is_match { ':' } else { '-' };
            out.push(format!("{}:{}{} {}", found.file, r.num, sep, clip(&r.text)));
        }
        shown += found.shown;
    }

    if args.files_only && !listed.is_empty() {
        println!("{}", listed.join("\n"));
        return;
    }
    if out.is_empty() {
        println!(
            "grep: нет совпадений: {} в {}",
            args.pattern,
            args.paths.join(" ")
        );
        return;
    }
    println!("{}", out.join("\n"));
    if shown < total {
        println!("обрезано: показано {shown} из {total}, сузь шаблон или путь");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!("grep-test-{tag}-{}-{nanos}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("каталог теста");
        d
    }

    fn args(pattern: &str) -> Args {
        Args {
            pattern: pattern.to_string(),
            paths: vec![".".to_string()],
            globs: Vec::new(),
            context: 0,
            limit: DEFAULT_LIMIT,
            ignore_case: false,
            word: false,
            files_only: false,
            no_ignore: false,
        }
    }

    fn put(dir: &Path, rel: &str, body: &[u8]) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("каталог");
        }
        fs::write(p, body).expect("файл");
    }

    #[test]
    fn glob_star_and_question() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "main.rss"));
        assert!(glob_match("main.r?", "main.rs"));
        assert!(!glob_match("main.r?", "main.rss"));
        assert!(glob_match("*", "что-угодно"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
        assert!(glob_match("*.tar.*", "x.tar.gz"));
    }

    #[test]
    fn literals_ignore_case_and_word() {
        assert!(hits("fn dispatch_tool", "dispatch", false, false));
        assert!(!hits("fn dispatch_tool", "dispatch", false, true));
        assert!(hits("fn dispatch(x)", "dispatch", false, true));
        assert!(hits("Fn Dispatch", "dispatch", true, true));
        assert!(!hits("Fn Dispatch", "dispatch", false, false));
        assert!(hits("a.b", "a.b", false, false));
        assert!(!hits("axb", "a.b", false, false));
        assert!(!hits("что угодно", "", false, false));
    }

    #[test]
    fn walk_skips_service_dirs_big_and_binary_files() {
        let d = scratch("walk");
        put(&d, "src/a.rs", b"fn target_line() {}\n");
        put(&d, "target/junk.rs", b"fn target_line() {}\n");
        put(&d, "node_modules/m/i.js", b"fn target_line() {}\n");
        put(&d, ".git/HEAD", b"fn target_line() {}\n");
        put(&d, "big.rs", &vec![b'x'; (SKIP_BYTES + 1) as usize]);
        put(&d, "bin.dat", b"ab\0cd fn target_line() {}\n");

        let a = args("target_line");
        let mut files = Vec::new();
        walk(&d, "", &a, &mut files);
        let names: Vec<String> = files.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(names, vec!["bin.dat".to_string(), "src/a.rs".to_string()]);

        // Бинарник прошёл отбор по имени и размеру — его отсеивает чтение, молча.
        let mut hits_files = Vec::new();
        for (rel, p) in &files {
            if let Some(f) = scan_file(rel, p, &a, DEFAULT_LIMIT) {
                hits_files.push(f.file);
            }
        }
        assert_eq!(hits_files, vec!["src/a.rs".to_string()]);

        // --no-ignore снимает пропуск каталогов.
        let mut b = a.clone();
        b.no_ignore = true;
        let mut files = Vec::new();
        walk(&d, "", &b, &mut files);
        assert!(files.iter().any(|(r, _)| r == "target/junk.rs"));
        assert!(!files.iter().any(|(r, _)| r == "big.rs"), "большой файл не читаем");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn glob_filters_names() {
        let d = scratch("glob");
        put(&d, "a.rs", b"needle\n");
        put(&d, "b.txt", b"needle\n");
        let mut a = args("needle");
        a.globs = vec!["*.rs".to_string()];
        let mut files = Vec::new();
        walk(&d, "", &a, &mut files);
        let names: Vec<String> = files.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(names, vec!["a.rs".to_string()]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn context_rows_carry_dash_prefix() {
        let d = scratch("ctx");
        put(&d, "a.txt", b"one\ntwo\nneedle\nfour\nfive\n");
        let mut a = args("needle");
        a.context = 1;
        let f = scan_file("a.txt", &d.join("a.txt"), &a, DEFAULT_LIMIT).expect("совпадение");
        let rows: Vec<(usize, bool)> = f.rows.iter().map(|r| (r.num, r.is_match)).collect();
        assert_eq!(rows, vec![(2, false), (3, true), (4, false)]);
        assert_eq!(f.total, 1);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn no_separator_without_context() {
        let d = scratch("nosep");
        put(&d, "a.txt", b"needle 1\nother\nneedle 2\n");
        let a = args("needle");
        let f = scan_file("a.txt", &d.join("a.txt"), &a, DEFAULT_LIMIT).expect("совпадение");
        assert_eq!(f.total, 2);
        assert!(
            f.rows.iter().all(|r| r.num > 0 && r.is_match),
            "без --context в выводе только совпадения, без разделителей"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn limit_keeps_total_for_truncation_notice() {
        let d = scratch("limit");
        let body: String = (0..10).map(|i| format!("needle {i}\n")).collect();
        put(&d, "a.txt", body.as_bytes());
        let mut a = args("needle");
        a.limit = 3;
        let f = scan_file("a.txt", &d.join("a.txt"), &a, a.limit).expect("совпадение");
        assert_eq!(f.shown, 3);
        assert_eq!(f.total, 10, "M — все совпадения, а не показанные");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn long_line_is_clipped_with_ellipsis() {
        let long = "x".repeat(LINE_MAX + 50);
        let got = clip(&long);
        assert_eq!(got.chars().count(), LINE_MAX + 1);
        assert!(got.ends_with('…'));
    }

    #[test]
    fn tilde_expands_only_leading() {
        std::env::set_var("HOME", "/home/test");
        assert_eq!(tilde("~/x"), "/home/test/x");
        assert_eq!(tilde("~"), "/home/test");
        assert_eq!(tilde("a/~/x"), "a/~/x");
        assert_eq!(tilde("~user/x"), "~user/x");
    }
}
