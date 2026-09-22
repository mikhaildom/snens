// edit — заменить точный фрагмент файла (exact match, как Edit/str_replace).
// Требует ровно одного совпадения old_string: молчаливая замена не того места
// дороже отказа. Пишет атомарно, потому что обрыв на середине съедает файл.
//
// Правок может быть несколько: одна логическая правка часто разнесена по
// нескольким местам файла, и гонять её по одному фрагменту — рутина. Правки
// применяются последовательно (текст под следующую — результат предыдущей) и
// атомарно: не совпал хоть один фрагмент — файл не переписывается вовсе.
use serde_json::Value;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::exit;

const HELP: &str = "\
edit — заменить точный фрагмент файла (exact match, как Edit/str_replace).
edit <путь>   — JSON на stdin: {\"old_string\": \"...\", \"new_string\": \"...\", \"replace_all\": false}
edit <путь>   — либо {\"edits\": [{\"old_string\", \"new_string\", \"replace_all\"}, ...]}

Требует ровно одного совпадения old_string, иначе ошибка со списком строк.
Несколько правок одного файла — одним вызовом (поле edits): применяются по
порядку, и если хоть одна не совпала, файл не трогается, а в ошибке — её номер.
Пишет атомарно (temp + rename). --dry-run показывает затронутый участок и не пишет.
В stdout — одна строка JSON-события.";

const DRY: &str = "--dry-run";
const HINT_CUTOFF: f32 = 0.5;

fn die(msg: String) -> ! {
    eprintln!("{msg}");
    exit(1);
}

fn line_of(text: &str, byte_idx: usize) -> usize {
    text[..byte_idx].matches('\n').count() + 1
}

fn short(s: &str) -> String {
    let mut t: String = s.chars().take(120).collect();
    if s.chars().count() > 120 {
        t.push('…');
    }
    t
}

/// Подобие строк по парам символов (Dice). Нужно только для подсказки, поэтому
/// приблизительности хватает — важно не молчать, а показать, где искали.
fn ratio(a: &str, b: &str) -> f32 {
    if a == b {
        return 1.0;
    }
    let bigrams = |s: &str| -> Vec<(char, char)> {
        let c: Vec<char> = s.chars().collect();
        c.windows(2).map(|w| (w[0], w[1])).collect()
    };
    let (x, y) = (bigrams(a), bigrams(b));
    if x.is_empty() || y.is_empty() {
        return 0.0;
    }
    let mut left = y.clone();
    let mut common = 0usize;
    for g in &x {
        if let Some(pos) = left.iter().position(|h| h == g) {
            left.remove(pos);
            common += 1;
        }
    }
    (2.0 * common as f32) / ((x.len() + y.len()) as f32)
}

fn hint(text: &str, old: &str) -> String {
    let first = old
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_else(|| old.trim());
    let lines: Vec<&str> = text.lines().collect();
    if first.is_empty() {
        return "похожего фрагмента нет — прочитай файл заново (read)".into();
    }
    for (i, l) in lines.iter().enumerate() {
        if l.trim() == first {
            return format!(
                "похоже на строку {}: {:?} — отличается отступ или перевод строки",
                i + 1,
                short(l.trim())
            );
        }
    }
    let mut best: Option<(f32, usize, &str)> = None;
    for (i, l) in lines.iter().enumerate() {
        let r = ratio(&first.to_lowercase(), &l.trim().to_lowercase());
        if r >= HINT_CUTOFF && best.map(|(br, _, _)| r > br).unwrap_or(true) {
            best = Some((r, i + 1, l.trim()));
        }
    }
    match best {
        Some((_, n, l)) => format!("ближайшее — строка {n}: {:?}", short(l)),
        None => "похожего фрагмента нет — прочитай файл заново (read)".into(),
    }
}

fn abs(p: &str) -> String {
    let path = std::path::Path::new(p);
    if path.is_absolute() {
        return p.to_string();
    }
    std::env::current_dir()
        .map(|c| c.join(path).to_string_lossy().into_owned())
        .unwrap_or_else(|_| p.to_string())
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> Option<u32> {
    Some(meta.permissions().mode())
}

/// Windows прав файла не знает — сохранять нечего.
#[cfg(not(unix))]
fn mode_of(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn set_mode(p: &std::path::Path, mode: Option<u32>) {
    if let Some(m) = mode {
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(m));
    }
}

#[cfg(not(unix))]
fn set_mode(_p: &std::path::Path, _mode: Option<u32>) {}

fn atomic_write(path: &str, data: &[u8], mode: Option<u32>) {
    let p = std::path::Path::new(path);
    let dir = p
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".snens-edit-{}-{nanos}", std::process::id()));
    std::fs::write(&tmp, data).unwrap_or_else(|e| die(format!("edit: {}: {e}", tmp.display())));
    set_mode(&tmp, mode);
    if let Err(e) = std::fs::rename(&tmp, p) {
        let _ = std::fs::remove_file(&tmp);
        die(format!("edit: {path}: {e}"));
    }
}

/// Один фрагмент правки. `n` — номер в пачке для сообщений об отказе (0 —
/// одиночная правка, её и без номера видно).
struct Edit {
    n: usize,
    old: String,
    new: String,
    all: bool,
}

impl Edit {
    fn parse(j: &Value, n: usize) -> Edit {
        let need = |k: &str| -> String {
            j.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| {
                    die(format!(
                        "edit: нужны строковые \"old_string\" и \"new_string\"{}",
                        where_n(n)
                    ))
                })
                .to_string()
        };
        let old = need("old_string");
        let new = need("new_string");
        let all = j.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
        if old.is_empty() {
            die(format!(
                "edit: пустой old_string{} — для полной перезаписи есть write",
                where_n(n)
            ));
        }
        if old == new {
            die(format!(
                "edit: old_string и new_string совпадают{},
                менять нечего",
                where_n(n)
            ));
        }
        Edit { n, old, new, all }
    }
}

/// « (правка 2)» — часть сообщения, которой у одиночной правки нет.
fn where_n(n: usize) -> String {
    if n == 0 {
        String::new()
    } else {
        format!(" (правка {n})")
    }
}

fn apply_one(text: &str, e: &Edit, path: &str) -> (String, usize) {
    let count = text.matches(&e.old).count();
    if count == 0 {
        die(format!(
            "edit: old_string не найден в {path}{}\n{}",
            where_n(e.n),
            hint(text, &e.old)
        ));
    }
    if count > 1 && !e.all {
        let spots: Vec<String> = text
            .match_indices(&e.old)
            .take(20)
            .map(|(i, _)| line_of(text, i).to_string())
            .collect();
        die(format!(
            "edit: old_string встречается {count} раз (строки {}){} — уточни контекст или поставь replace_all: true",
            spots.join(", "),
            where_n(e.n)
        ));
    }
    if e.all {
        (text.replace(&e.old, &e.new), count)
    } else {
        (text.replacen(&e.old, &e.new, 1), 1)
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "-h" || argv[0] == "--help" {
        println!("{HELP}");
        exit(if argv.is_empty() { 1 } else { 0 });
    }
    let dry = argv.iter().any(|a| a == DRY);
    let rest: Vec<&String> = argv.iter().filter(|a| *a != DRY).collect();
    if rest.len() > 1 {
        die("edit: ожидается один путь".into());
    }

    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .unwrap_or_else(|e| die(format!("edit: stdin: {e}")));
    if raw.trim().is_empty() {
        die("edit: пустой stdin — нужен JSON {\"old_string\", \"new_string\"}".into());
    }
    let j: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| die(format!("edit: stdin не JSON: {e}")));

    let path = rest
        .first()
        .map(|s| (*s).clone())
        .or_else(|| j.get("path").and_then(|p| p.as_str()).map(str::to_string))
        .unwrap_or_else(|| die("edit: не указан путь (аргументом или полем path)".into()));
    // Один фрагмент или пачка: разбор обеих форм приводит к общему списку.
    // Пачка — это одна логическая правка, разнесённая по месту файла, поэтому
    // её части носят номер: по нему видно, какая не совпала.
    let edits: Vec<Edit> = match j.get("edits") {
        None => vec![Edit::parse(&j, 0)],
        Some(Value::Array(items)) => {
            if items.is_empty() {
                die("edit: пустой edits — нечего менять".into());
            }
            items
                .iter()
                .enumerate()
                .map(|(i, item)| match item {
                    Value::Object(o) => Edit::parse(&Value::Object(o.clone()), i + 1),
                    other => die(format!(
                        "edit: edits[{i}] — ожидался объект, получено {other}"
                    )),
                })
                .collect()
        }
        Some(other) => die(format!(
            "edit: edits — ожидался массив, получено {other}"
        )),
    };

    let meta = std::fs::metadata(&path).unwrap_or_else(|e| match e.kind() {
        std::io::ErrorKind::NotFound => die(format!("edit: файл не найден: {path}")),
        _ => die(format!("edit: {path}: {e}")),
    });
    if meta.is_dir() {
        die(format!("edit: {path} — это каталог"));
    }
    let data = std::fs::read(&path).unwrap_or_else(|e| die(format!("edit: {path}: {e}")));
    if data.iter().take(8192).any(|b| *b == 0) {
        die(format!(
            "edit: {path} — бинарный файл ({} Б), не редактирую",
            data.len()
        ));
    }
    let text = String::from_utf8_lossy(&data).into_owned();

    // Все правки считаются по промежуточному тексту: вторая видит результат
    // первой. Файл пишется один раз — только если сошлись все фрагменты.
    let mut out_text = text.clone();
    let mut replaced = 0usize;
    for e in &edits {
        match apply_one(&out_text, e, &path) {
            (t, n) => {
                out_text = t;
                replaced += n;
            }
        }
    }

    if dry {
        // Полноценный unified diff не нужен: git даёт его точнее, а здесь важно
        // показать затронутый участок до записи. Для пачки — каждый фрагмент.
        for e in &edits {
            let at = text.find(&e.old).unwrap_or(0);
            eprintln!("--- {path}{} (строка {})", where_n(e.n), line_of(&text, at));
            for l in e.old.lines() {
                eprintln!("- {l}");
            }
            for l in e.new.lines() {
                eprintln!("+ {l}");
            }
        }
    } else {
        atomic_write(&path, out_text.as_bytes(), mode_of(&meta));
    }

    println!(
        "{}",
        serde_json::json!({
            "path": abs(&path),
            "edits": edits.len(),
            "replaced": replaced,
            "bytes_before": data.len(),
            "bytes_after": out_text.len(),
            "dry_run": dry,
        })
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_finds_line_with_different_indent() {
        let text = "alpha\n    gamma\ndelta\n";
        let h = hint(text, "  gamma");
        assert!(h.contains("строку 2"), "подсказка без номера строки: {h}");
    }

    #[test]
    fn hint_reports_close_line_when_nothing_matches() {
        let text = "let value = compute(alpha);\n";
        let h = hint(text, "let value = compute(beta);");
        assert!(h.contains("ближайшее"), "нет подсказки о близком: {h}");
    }

    fn e(n: usize, old: &str, new: &str) -> Edit {
        Edit {
            n,
            old: old.into(),
            new: new.into(),
            all: false,
        }
    }

    #[test]
    fn second_hunk_sees_result_of_first() {
        let (a, n1) = apply_one("let a = 1;\nlet b = 2;\n", &e(1, "let a = 1;", "let a = 10;"), "f");
        let (b, n2) = apply_one(&a, &e(2, "let b = 2;", "let b = a + 1;"), "f");
        assert_eq!(n1 + n2, 2);
        assert_eq!(b, "let a = 10;\nlet b = a + 1;\n");
    }

    #[test]
    fn hunk_number_lands_in_messages() {
        assert_eq!(where_n(0), "");
        assert!(where_n(2).contains("2"));
    }
}