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
В stdout — одна строка JSON-события.

Многострочные значения можно подавать как есть, с реальными переводами строк
внутри кавычек: сырые управляющие символы (перевод строки, таб) чинятся сами,
экранировать их бэкслешем не нужно.";

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

/// Невалидный JSON здесь — почти всегда одно и то же: модель пишет значения
/// `old_string`/`new_string` с реальными переводами строк внутри кавычек, а JSON
/// такие управляющие символы в строке запрещает. Требовать `\n`-экранирования —
/// терять вызов на каждой многострочной правке. Поэтому: если ошибка парсера —
/// «control character while parsing a string», чиним вход, экранируя сырые
/// управляющие символы ВНУТРИ строк, и пробуем ещё раз. Пробуем один раз: если
/// вход и после этого не разобрался, показываем свою ошибку парсера.
fn parse_patch(raw: &str) -> Result<Value, serde_json::Error> {
    let first = match serde_json::from_str(raw) {
        Ok(v) => return Ok(v),
        Err(e) => e,
    };
    // Вторая попытка: сырые управляющие символы внутри строк.
    let escaped = escape_raw_controls(raw);
    if let Ok(v) = serde_json::from_str(&escaped) {
        return Ok(v);
    }
    // Третья попытка: незаэкранированные кавычки внутри значений — типичный
    // случай, когда в правке встречается код с `"` (Go/Rust). Ремонт только
    // при неудаче парсера: корректный JSON не переписываем никогда.
    serde_json::from_str(&escape_inner_quotes(&escaped)).map_err(|_| first)
}

/// Экранировать кавычки, которые открывают/закрывают не строку JSON, а текст
/// внутри неё. Строка кончается только перед `,` `:` `}` `]` — всё остальное
/// `"` внутри считаем литеральной кавычкой значения.
fn escape_inner_quotes(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len() + 16);
    let mut in_string = false;
    let mut escape = false;
    for (i, c) in chars.iter().enumerate() {
        if !in_string {
            out.push(*c);
            if *c == '"' {
                in_string = true;
            }
            continue;
        }
        if escape {
            out.push(*c);
            escape = false;
            continue;
        }
        match c {
            '\\' => {
                out.push(*c);
                escape = true;
            }
            '"' => {
                // Следующий непробельный символ решает: терминатор строки или текст.
                let next = chars[i + 1..].iter().find(|x| !x.is_whitespace()).copied();
                match next {
                    Some(',') | Some(':') | Some('}') | Some(']') => {
                        out.push('"');
                        in_string = false;
                    }
                    _ => out.push_str("\\\""),
                }
            }
            _ => out.push(*c),
        }
    }
    out
}

/// Заменить сырые управляющие символы внутри строк на их \-экранированные
/// формы, не трогая структуру JSON. Правила как в JSON: строки в `"..."`,
/// экранирование бэкслешом. `\n`, `\t`, `\r` эмитируются сразу с бэкслешем —
/// это распространённые разделители строк; любой другой управляющий символ —
/// как `\uXXXX`, лишь бы парсер принял его в строке.
fn escape_raw_controls(src: &str) -> String {
    let mut out = String::with_capacity(src.len() + 16);
    let mut in_string = false;
    let mut escape = false;
    for c in src.chars() {
        if in_string {
            if escape {
                out.push(c);
                escape = false;
                continue;
            }
            match c {
                '\\' => {
                    out.push(c);
                    escape = true;
                }
                '"' => {
                    out.push(c);
                    in_string = false;
                }
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                '\r' => out.push_str("\\r"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                _ => out.push(c),
            }
        } else {
            out.push(c);
            if c == '"' {
                in_string = true;
            }
        }
    }
    out
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
        let all = j
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if old.is_empty() {
            die(format!(
                "edit: пустой old_string{} — для полной перезаписи есть write",
                where_n(n)
            ));
        }
        // `old == new` — не ошибка, а no-op: файл ровно в том состоянии, которое
        // просили. Возврат ошибки красит нормальный исход в «неудачу» в разборе.
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
    // No-op: менять нечего. Возвращаем текст как есть — файл уже в нужном виде.
    // Проверяем ДО поиска фрагмента: иначе нечего-менять ошибочно читается как
    // «фрагмент не найден», хотя это разные исходы.
    if e.old == e.new {
        return (text.to_string(), 0);
    }
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
    let j: serde_json::Value = parse_patch(&raw).unwrap_or_else(|e| {
        let msg = e.to_string();
        let hint = if msg.contains("EOF while parsing") {
            // Незакрытая строка/объект на последнем байте = вход оборван на полуслове,
            // а не кривой JSON. Раньше модель гадала и уходила переписывать файл целиком.
            format!(
                "вход оборван: {} Б, JSON не закрыт (передай stdin одной строкой или разбей правку на куски)",
                raw.len()
            )
        } else if msg.contains("control character") {
            "сырые переводы строк/табы внутри строк тоже не починились — проверь кавычки в значениях".into()
        } else {
            "разбор не удался; кавычки внутри значений чинятся сами — если не помогло, перепиши файл через write".into()
        };
        die(format!("edit: stdin не JSON: {e}\n({hint})"))
    });

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
        Some(other) => die(format!("edit: edits — ожидался массив, получено {other}")),
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
        let (t, n) = apply_one(&out_text, e, &path);
        out_text = t;
        replaced += n;
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
            // Все правки оказались no-op: файл уже был в нужном виде. Не ошибка,
            // но и не «сделано» — разбору полезно видеть разницу.
            "unchanged": replaced == 0,
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
        let (a, n1) = apply_one(
            "let a = 1;\nlet b = 2;\n",
            &e(1, "let a = 1;", "let a = 10;"),
            "f",
        );
        let (b, n2) = apply_one(&a, &e(2, "let b = 2;", "let b = a + 1;"), "f");
        assert_eq!(n1 + n2, 2);
        assert_eq!(b, "let a = 10;\nlet b = a + 1;\n");
    }

    #[test]
    fn identical_strings_are_noop_not_error() {
        // Файл уже ровно в нужном виде: 0 замен, текст без изменений.
        let (t, n) = apply_one("alpha beta\n", &e(0, "alpha", "alpha"), "f");
        assert_eq!(n, 0);
        assert_eq!(t, "alpha beta\n");
        // Даже когда фрагмента в файле нет — при old == new это не «не найден».
        let (t, n) = apply_one("other\n", &e(0, "absent", "absent"), "f");
        assert_eq!(n, 0);
        assert_eq!(t, "other\n");
    }

    #[test]
    fn accepts_literal_newlines_inside_strings() {
        // Ровно то, что присылает модель: реальные переводы строк в кавычках.
        let raw = "{\"old_string\": \"let a = 1;\nlet b = 2;\", \"new_string\": \"let a = 10;\nlet b = 2;\"}";
        let j = parse_patch(raw).expect("сырые переводы строк должны разбираться");
        assert_eq!(j["old_string"], "let a = 1;\nlet b = 2;");
        assert_eq!(j["new_string"], "let a = 10;\nlet b = 2;");
    }

    #[test]
    fn accepts_literal_tabs_inside_strings() {
        let raw = "{\"old_string\": \"\tvalue\", \"new_string\": \"\tother\"}";
        let j = parse_patch(raw).expect("сырые табы должны разбираться");
        assert_eq!(j["old_string"], "\tvalue");
    }

    #[test]
    fn valid_json_still_works_and_backslashes_survive() {
        let raw = r#"{"old_string": "let a = \"x\";", "new_string": "let a = \"y\";"}"#;
        let j = parse_patch(raw).expect("валидный JSON не трогаем");
        assert_eq!(j["old_string"], "let a = \"x\";");
    }

    #[test]
    fn repairs_unescaped_quotes_inside_values() {
        // Реальный payload из сессии 1791834511 (вызов 254): кавычка внутри
        // значения не экранирована — парсер видел конец строки на `v != ""`.
        let raw = r#"{"old_string": "func envBool(k string, def bool) bool {", "new_string": "func envInt(k string, def int) int {\n\tif v := strings.TrimSpace(os.Getenv(k)); v != "" {\n\t\treturn 1\n\t}"}"#;
        let j = parse_patch(raw).expect("кавычки внутри значения должны чиниться");
        assert!(j["new_string"].as_str().unwrap().contains(r#"v != """#));
        // Корректный JSON с экранированными кавычками не переписывается.
        let ok = r#"{"old_string": "let a = "x";", "new_string": "let a = "y";"}"#;
        assert_eq!(parse_patch(ok).unwrap()["new_string"], r#"let a = "y";"#);
    }

    #[test]
    fn truncated_input_is_named_as_truncated() {
        // Незакрытая строка = оборванный вход, а не кривой JSON.
        let raw = r#"{"old_string": "abc"#;
        let e = parse_patch(raw).unwrap_err().to_string();
        assert!(e.contains("EOF"), "{e}");
    }

    #[test]
    fn garbage_still_rejected() {
        let raw = "{не json}";
        assert!(parse_patch(raw).is_err());
    }

    #[test]
    fn escape_leaves_structure_untouched() {
        let raw = "{\"old_string\": \"a\nb\", \"path\": \"f\"}";
        let fixed = escape_raw_controls(raw);
        assert!(serde_json::from_str::<Value>(&fixed).is_ok());
        assert!(fixed.contains("a\\nb"));
    }

    #[test]
    fn hunk_number_lands_in_messages() {
        assert_eq!(where_n(0), "");
        assert!(where_n(2).contains("2"));
    }
}
