// edit — заменить точный фрагмент файла (exact match, как Edit/str_replace).
// Требует ровно одного совпадения old_string: молчаливая замена не того места
// дороже отказа. Пишет атомарно, потому что обрыв на середине съедает файл.
//
// Правок может быть несколько: одна логическая правка часто разнесена по
// нескольким местам файла, и гонять её по одному фрагменту — рутина. Все правки
// пачки матчатся против ИСХОДНОГО текста (как в pi), не должны пересекаться и
// применяются разом: порядок в массиве на результат не влияет, промах любой
// правки оставляет файл нетронутым.
//
// Матчинг двухпроходный: сперва точное совпадение (быстро и без сюрпризов), и
// только если его нет — второй проход по нормализованному тексту (хвостовые
// пробелы, CRLF, NFKC, типографские кавычки/тире). Правка при этом накладывается
// на ОРИГИНАЛ по карте смещений: иначе запись нормализованной копии переписала
// бы весь файл — чужой diff на весь файл вместо одной строки.
use serde_json::Value;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::exit;
use unicode_normalization::char::canonical_combining_class;
use unicode_normalization::UnicodeNormalization;

const HELP: &str = "\
edit — заменить точный фрагмент файла (exact match, как Edit/str_replace).
edit <путь>   — либо old_string + new_string, либо edits[...] (JSON на stdin у самого тула)

Требует ровно одного совпадения old_string, иначе ошибка со списком строк.
Совпадение: точно, затем с точностью до хвостовых пробелов/CRLF/юникода.
Правки edits матчатся против ИСХОДНОГО файла и не должны пересекаться.
Пишет атомарно (temp + rename). --dry-run показывает затронутый участок и не пишет.
В stdout — одна строка JSON-события.

Многострочные значения можно подавать как есть, с реальными переводами строк
внутри кавычек: сырые управляющие символы (перевод строки, таб) чинятся сами,
экранировать их бэкслешем не нужно.";

const DRY: &str = "--dry-run";
const HINT_CUTOFF: f32 = 0.5;
const FUZZY_NOTE: &str = "совпадение найдено с точностью до хвостовых пробелов/CRLF/юникода";

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

/// Где искали — адресно, а не «не найдено». Две ближайшие строки с номерами:
/// по одной модели часто не понять, попала ли подсказка в цель.
fn hint(text: &str, old: &str) -> String {
    let first = old
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_else(|| old.trim());
    let lines: Vec<&str> = text.lines().collect();
    // Регистр снимаем один раз: иначе на большом файле это O(строк) аллокаций
    // только чтобы построить подсказку об ошибке.
    let first_lower = first.to_lowercase();
    let nothing = || "похожего фрагмента нет — прочитай файл заново (read)".to_string();
    if first.is_empty() {
        return nothing();
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
    let mut scored: Vec<(f32, usize, &str)> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            (
                ratio(&first_lower, &l.trim().to_lowercase()),
                i + 1,
                l.trim(),
            )
        })
        .filter(|(r, _, _)| *r >= HINT_CUTOFF)
        .collect();
    if scored.is_empty() {
        return nothing();
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored
        .iter()
        .take(2)
        .enumerate()
        .map(|(k, (_, n, l))| {
            let tag = if k == 0 {
                "ближайшее"
            } else {
                "ещё похоже"
            };
            format!("{tag} — строка {n}: {:?}", short(l))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── нормализация ────────────────────────────────────────────────────────────

/// Типографские знаки, которых не трогает NFKC: модель «печатает» ими, файл
/// лежит с ASCII. Замена 1:1, поэтому карта смещений остаётся однозначной.
fn fold_char(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' | '\u{2032}' | '\u{ff07}' => '\'',
        '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' | '\u{2033}' | '\u{ab}' | '\u{bb}'
        | '\u{ff02}' => '"',
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' | '\u{fe58}' | '\u{fe63}' => '-',
        _ => c,
    }
}

/// Нормализованный текст вместе с картой обратно в оригинал: для каждого
/// нормализованного символа — диапазон байт исходника, который он изображает.
///
/// Что нормализуется: хвостовые пробелы/табы строки отбрасываются, CRLF
/// считается переводом строки, текст прогоняется через NFKC (неразрывный
/// пробел → пробел, лигатуры, полноширинные формы), типографские кавычки и тире
/// сводятся к ASCII.
///
/// Что НЕ нормализуется: отступы в начале строки. Таб против четырёх пробелов —
/// это разное содержимое (в питоне и makefile вообще другой смысл), и «угадать»
/// ширину таба значит молча переписать блок с другим отступом.
struct Norm {
    text: String,
    /// для каждого символа нормализованного текста — границы изображаемого им
    /// куска оригинала (у разложенной лигатуры оба символа делят один кусок)
    from: Vec<usize>,
    to: Vec<usize>,
    /// байтовые смещения символов `text`: по ним байтовый матч переводится в индексы
    at: Vec<usize>,
}

fn push_line(
    line: &str,
    off: usize,
    text: &mut String,
    from: &mut Vec<usize>,
    to: &mut Vec<usize>,
) {
    let trimmed = line.trim_end();
    let mut it = trimmed.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let start = off + i;
        let mut end = start + c.len_utf8();
        // Кластер «базовый символ + комбинирующие» нормализуем целиком: так
        // «e + U+0301» и «é» сходятся, а карта остаётся однозначной.
        let mut cluster = String::from(c);
        while let Some(&(j, next)) = it.peek() {
            if canonical_combining_class(next) == 0 {
                break;
            }
            cluster.push(next);
            end = off + j + next.len_utf8();
            it.next();
        }
        for n in cluster.as_str().nfkc() {
            text.push(fold_char(n));
            from.push(start);
            to.push(end);
        }
    }
}

fn normalize(src: &str) -> Norm {
    let mut text = String::with_capacity(src.len());
    let mut from: Vec<usize> = Vec::new();
    let mut to: Vec<usize> = Vec::new();
    let mut pos = 0usize;
    while pos < src.len() {
        let rest = &src[pos..];
        match rest.find('\n') {
            Some(k) => {
                let body = &rest[..k];
                let cr = usize::from(body.ends_with('\r'));
                let line = &body[..body.len() - cr];
                push_line(line, pos, &mut text, &mut from, &mut to);
                text.push('\n');
                from.push(pos + line.len());
                to.push(pos + line.len() + cr + 1);
                pos += k + 1;
            }
            None => {
                push_line(rest, pos, &mut text, &mut from, &mut to);
                pos = src.len();
            }
        }
    }
    let at: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
    Norm { text, from, to, at }
}

/// Диапазон нормализованного текста → диапазон байт оригинала.
/// Диапазон нормализованного текста → диапазон байт оригинала, если границы
/// матча приходятся на границы символов. Матч, начинающийся внутри разложенной
/// лигатуры (`fi` из `ﬁ`), вернёт None: кусок такого символа отдельно не
/// правится — иначе диапазон захватил бы половину чужого символа.
fn range_of(n: &Norm, byte_start: usize, byte_end: usize) -> Option<(usize, usize)> {
    let ci = n.at.binary_search(&byte_start).ok()?;
    // Конец — либо граница символа, либо конец текста.
    let cj = match n.at.binary_search(&byte_end) {
        Ok(i) => i,
        Err(_) if byte_end == n.text.len() => n.from.len(),
        Err(_) => return None,
    };
    if cj == 0 || cj > n.from.len() {
        return None;
    }
    if ci > 0 && n.from[ci] == n.from[ci - 1] {
        return None;
    }
    if cj < n.from.len() && n.to[cj] == n.to[cj - 1] {
        return None;
    }
    Some((n.from[ci], n.to[cj - 1]))
}

/// Файл с CRLF: новые строки вставляемого фрагмента пишем его же концом строки,
/// иначе посреди файла появляется смесь CRLF и LF.
fn is_crlf(text: &str) -> bool {
    let b = text.as_bytes();
    let (mut crlf, mut lf) = (0usize, 0usize);
    for (i, c) in b.iter().enumerate() {
        if *c == b'\n' {
            if i > 0 && b[i - 1] == b'\r' {
                crlf += 1;
            } else {
                lf += 1;
            }
        }
    }
    crlf > lf
}

// ── JSON ────────────────────────────────────────────────────────────────────

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

/// Конечная цель симлинка, иначе путь как есть. Битая ссылка — остаётся как есть:
/// дальше её отсечёт `metadata`.
fn resolve_symlink(path: &str) -> String {
    let p = std::path::Path::new(path);
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => std::fs::canonicalize(p)
            .map(|c| c.display().to_string())
            .unwrap_or_else(|_| path.to_string()),
        _ => path.to_string(),
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

/// Права — до данных и без проглатывания ошибки: файл с секретами не должен
/// ни секунды лежать с дефолтными 0644. `m & 0o7777` отсекает биты типа файла.
#[cfg(unix)]
fn set_mode(p: &std::path::Path, mode: Option<u32>) {
    if let Some(m) = mode {
        let m = m & 0o7777;
        if let Err(e) = std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)) {
            die(format!(
                "edit: {}: не удалось сохранить права {:o}: {e}",
                p.display(),
                m
            ));
        }
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
    // create_new: имя предсказуемо, а `fs::write` (create+truncate) по симлинку,
    // заранее положенному в каталог, писал бы в чужой файл.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(m) = mode {
        // Права при создании: окно «данные уже на диске, а режим ещё 0644» исчезает.
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, m & 0o7777);
    }
    let mut f = opts
        .open(&tmp)
        .unwrap_or_else(|e| die(format!("edit: {}: {e}", tmp.display())));
    if let Err(e) = f.write_all(data).and_then(|_| f.sync_all()) {
        let _ = std::fs::remove_file(&tmp);
        die(format!("edit: {}: {e}", tmp.display()));
    }
    drop(f);
    set_mode(&tmp, mode);
    if let Err(e) = std::fs::rename(&tmp, p) {
        let _ = std::fs::remove_file(&tmp);
        die(format!("edit: {path}: {e}"));
    }
    // fsync каталога: без него после сбоя rename может быть виден без данных.
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

// ── правки ──────────────────────────────────────────────────────────────────

/// Один фрагмент правки. `n` — номер в пачке для сообщений об отказе (0 —
/// одиночная правка, её и без номера видно).
struct Edit {
    n: usize,
    old: String,
    new: String,
    all: bool,
}

/// Отказ на вызове без самой правки. По схеме такой вызов валиден (обязателен
/// только `path`), поэтому модель присылала `{path, replace_all}` и по короткому
/// «нужны строковые old_string и new_string» не понимала, что не доехало:
/// в отказе не было ни второй формы (`edits`), ни списка пришедших полей.
fn missing_fields(j: &Value, missing: &str, n: usize) -> String {
    let came: Vec<&str> = j
        .as_object()
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default();
    if n > 0 {
        return format!(
            "edit: в правке {n} нет поля \"{missing}\" — у каждой правки пачки нужны old_string и new_string. Пришло: {}",
            came.join(", ")
        );
    }
    format!(
        "edit: нет поля \"{missing}\" — правка пустая, менять нечего.\n\
         Допустимые формы (одна из двух):\n  \
         {{\"path\": \"…\", \"old_string\": \"…\", \"new_string\": \"…\"}}\n  \
         {{\"path\": \"…\", \"edits\": [{{\"old_string\": \"…\", \"new_string\": \"…\"}}]}}\n\
         Пришло в вызове: {}",
        if came.is_empty() {
            "ничего".to_string()
        } else {
            came.join(", ")
        }
    )
}

impl Edit {
    fn new(j: &Value, n: usize) -> Result<Edit, String> {
        let need = |k: &str| -> Result<String, String> {
            j.get(k)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .ok_or_else(|| missing_fields(j, k, n))
        };
        let old = need("old_string")?;
        let new = need("new_string")?;
        let all = j
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if old.is_empty() {
            return Err(format!(
                "edit: пустой old_string{} — для полной перезаписи есть write",
                where_n(n)
            ));
        }
        // `old == new` — не ошибка, а no-op: файл ровно в том состоянии, которое
        // просили. Возврат ошибки красит нормальный исход в «неудачу» в разборе.
        Ok(Edit { n, old, new, all })
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

/// «правка 2» / «правка» — для мест, где номер стоит в начале фразы.
fn label(n: usize) -> String {
    if n == 0 {
        "правка".into()
    } else {
        format!("правка {n}")
    }
}

/// Разбор поля `edits`: массив правок, либо одиночный объект (модели иногда
/// присылают вторую форму — отказ стоил бы повторного вызова на ровном месте).
fn parse_edits(j: &Value) -> Result<Vec<Edit>, String> {
    match j.get("edits") {
        None => Ok(vec![Edit::new(j, 0)?]),
        Some(Value::Array(items)) => {
            // Пустой `edits` рядом с заполненными old_string/new_string — не
            // отказ, а шум вызывающего: модель заполнила все поля схемы сразу.
            // Значение есть — берём его, отказ стоил бы лишнего хода.
            if items.is_empty() {
                if j.get("old_string").and_then(Value::as_str).is_some() {
                    return Ok(vec![Edit::new(j, 0)?]);
                }
                return Err("edit: пустой edits — нечего менять".into());
            }
            items
                .iter()
                .enumerate()
                .map(|(i, item)| match item {
                    Value::Object(_) => Edit::new(item, i + 1),
                    other => Err(format!(
                        "edit: edits[{i}] — ожидался объект, получено {other}"
                    )),
                })
                .collect()
        }
        Some(obj @ Value::Object(_)) => Ok(vec![Edit::new(obj, 0)?]),
        Some(other) => Err(format!(
            "edit: edits — ожидался массив или объект, получено {other}"
        )),
    }
}

/// Найденное место правки в байтах ОРИГИНАЛА.
#[derive(Clone, Copy)]
struct Spot {
    start: usize,
    end: usize,
    fuzzy: bool,
}

/// Применяемая правка: диапазон оригинала и то, что встаёт на его место.
#[derive(Debug)]
struct Applied {
    n: usize,
    start: usize,
    end: usize,
    new: String,
    fuzzy: bool,
}

fn not_found(orig: &str, e: &Edit, path: &str) -> String {
    format!(
        "edit: old_string не найден в {path}{} — ни точно, ни с точностью до пробелов/юникода\n{}",
        where_n(e.n),
        hint(orig, &e.old)
    )
}

/// Отказ в пачке: первая настоящая причина (её текст) + построчно, какие правки
/// сошлись бы и какие нет. Модели нужно видеть, что дело не в её тексте вообще,
/// а в одной конкретной правке — иначе следующий вызов повторяет ту же пачку.
fn batch_refusal(
    orig: &str,
    edits: &[Edit],
    per: &[Result<Vec<Spot>, String>],
    first: String,
) -> String {
    let mut msg = first;
    let mut lines: Vec<String> = Vec::new();
    for (e, r) in edits.iter().zip(per) {
        match r {
            Ok(spots) if spots.is_empty() => {
                lines.push(format!("{} — без изменений (old == new)", label(e.n)))
            }
            Ok(spots) => lines.push(format!(
                "{} сошлась бы (строка {})",
                label(e.n),
                line_of(orig, spots[0].start)
            )),
            Err(_) => lines.push(format!("{} не сошлась", label(e.n))),
        }
    }
    msg.push_str(&format!(
        "\nпачка из {} правок: {}",
        edits.len(),
        lines.join("; ")
    ));
    msg.push_str(
        "\nни одна правка не применена — файл не тронут (все матчатся против исходного файла)",
    );
    msg
}

/// Найти места правки: сначала точное совпадение, иначе — нормализованное.
/// `norm` строится лениво: точный матч (обычный случай) не платит за NFKC всего
/// файла, а файлы бывают большими.
fn locate(orig: &str, norm: &mut Option<Norm>, e: &Edit, path: &str) -> Result<Vec<Spot>, String> {
    if e.old == e.new {
        return Ok(Vec::new());
    }
    let exact: Vec<usize> = orig.match_indices(&e.old).map(|(i, _)| i).collect();
    let spots: Vec<Spot> = if !exact.is_empty() {
        exact
            .iter()
            .map(|&i| Spot {
                start: i,
                end: i + e.old.len(),
                fuzzy: false,
            })
            .collect()
    } else {
        let norm = norm.get_or_insert_with(|| normalize(orig));
        let old_norm = normalize(&e.old).text;
        if old_norm.is_empty() {
            return Err(not_found(orig, e, path));
        }
        let hits: Vec<usize> = norm.text.match_indices(&old_norm).map(|(i, _)| i).collect();
        if hits.is_empty() {
            return Err(not_found(orig, e, path));
        }
        let spots: Vec<Spot> = hits
            .iter()
            .filter_map(|&i| range_of(norm, i, i + old_norm.len()))
            .map(|(start, end)| Spot {
                start,
                end,
                fuzzy: true,
            })
            .collect();
        if spots.is_empty() {
            return Err(not_found(orig, e, path));
        }
        // Совпадение в нормализованном тексте, которое не отображается назад
        // в оригинал (граница внутри лигатуры), — не «единственное место»,
        // а неоднозначность: молча править одно из двух нельзя.
        if spots.len() != hits.len() {
            return Err(format!(
                "edit: в нормализованном тексте {} совпадений, а в исходном отображается только {} — \
                 место неоднозначно, уточни old_string{}",
                hits.len(),
                spots.len(),
                where_n(e.n)
            ));
        }
        spots
    };
    if spots.len() > 1 && !e.all {
        let at: Vec<String> = spots
            .iter()
            .take(20)
            .map(|s| line_of(orig, s.start).to_string())
            .collect();
        return Err(format!(
            "edit: old_string встречается {} раз (строки {}){} — уточни контекст или поставь replace_all: true",
            spots.len(),
            at.join(", "),
            where_n(e.n)
        ));
    }
    Ok(if e.all {
        spots
    } else {
        spots.into_iter().take(1).collect()
    })
}

/// Применить все правки к оригиналу разом. Возвращает новый текст и список
/// применённых правок (для --dry-run и события). Файл не пишется: решение о
/// записи — в main.
///
/// Собираем результат склейкой оригинала: всё, что не попало в диапазоны,
/// переносится байт в байт — концы строк, хвостовые пробелы и кодировка
/// остальных строк не меняются даже при fuzzy-совпадении.
fn apply_all(orig: &str, edits: &[Edit], path: &str) -> Result<(String, Vec<Applied>), String> {
    let crlf = is_crlf(orig);
    // Один проход поиска: найденные места идут и в проверку атомарности, и в план.
    // Нормализация файла — только если хоть одна правка не нашлась точно.
    let mut norm: Option<Norm> = None;
    let per: Vec<Result<Vec<Spot>, String>> = edits
        .iter()
        .map(|e| locate(orig, &mut norm, e, path))
        .collect();
    if let Some(first) = per.iter().find_map(|r| r.as_ref().err()).cloned() {
        return Err(if edits.len() > 1 {
            batch_refusal(orig, edits, &per, first)
        } else {
            first
        });
    }
    let mut plan: Vec<Applied> = Vec::new();
    for (e, spots) in edits.iter().zip(&per) {
        for s in spots.as_ref().expect("ошибки уже отсеяны выше") {
            let mut new = e.new.clone();
            if crlf && !new.contains("\r\n") && new.contains('\n') {
                new = new.replace('\n', "\r\n");
            }
            plan.push(Applied {
                n: e.n,
                start: s.start,
                end: s.end,
                new,
                fuzzy: s.fuzzy,
            });
        }
    }
    // Правки независимы (все матчатся против оригинала), значит их диапазоны не
    // должны делить байты: иначе результат зависит от порядка применения.
    plan.sort_by_key(|a| (a.start, a.end));
    for w in plan.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        if a.end > b.start {
            return Err(format!(
                "edit: {} и {} задевают один и тот же участок (байты {}–{} и {}–{}) — правка пачки не должна менять то, что меняет другая\nни одна правка не применена — файл не тронут",
                label(a.n),
                label(b.n),
                a.start,
                a.end,
                b.start,
                b.end
            ));
        }
    }
    let mut out = String::with_capacity(orig.len());
    let mut cursor = 0usize;
    for a in &plan {
        out.push_str(&orig[cursor..a.start]);
        out.push_str(&a.new);
        cursor = a.end;
    }
    out.push_str(&orig[cursor..]);
    Ok((out, plan))
}

fn main() {
    // args_os: путь на Linux не обязан быть UTF-8, а args() на таком паникует
    // вместо понятного отказа.
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| {
            a.into_string()
                .unwrap_or_else(|bad| die(format!("edit: аргумент не UTF-8: {bad:?}")))
        })
        .collect();
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
        die("edit: пустой stdin — нечего менять. Передай {\"old_string\", \"new_string\"} либо {\"edits\": [{…}]}".into());
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
    let edits = parse_edits(&j).unwrap_or_else(|e| die(e));

    // Симлинк разрешаем до цели: `rename` в atomic_write заменил бы саму ссылку
    // обычным файлом. Для слинкованных дотфайлов правка иначе уходит не туда.
    let path = resolve_symlink(&path);
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
    // Не UTF-8 — отказ, а не lossy-декод: `from_utf8_lossy` заменил бы каждый
    // нелегальный байт на U+FFFD, и запись целиком переписала бы файл в CP1251
    // или Latin-1, пообещав «остальной файл не трогаю».
    let text = match std::str::from_utf8(&data) {
        Ok(t) => t.to_string(),
        Err(e) => die(format!(
            "edit: {path} — не UTF-8 (сбой на байте {}), правка текстом испортила бы файл",
            e.valid_up_to()
        )),
    };

    // Все правки считаются против ИСХОДНОГО текста и применяются разом. Файл
    // пишется один раз — только если сошлись все фрагменты и не пересеклись.
    let (out_text, applied) = apply_all(&text, &edits, &path).unwrap_or_else(|e| die(e));
    let replaced = applied.len();
    let fuzzy = applied.iter().any(|a| a.fuzzy);

    if dry {
        // Полноценный unified diff не нужен: git даёт его точнее, а здесь важно
        // показать затронутый участок до записи. Для пачки — каждый фрагмент.
        for a in &applied {
            eprintln!(
                "--- {path}{} (строка {})",
                where_n(a.n),
                line_of(&text, a.start)
            );
            if a.fuzzy {
                eprintln!("~ {FUZZY_NOTE}: правлю только этот участок, остальной файл не трогаю");
            }
            for l in text[a.start..a.end].lines() {
                eprintln!("- {l}");
            }
            for l in a.new.lines() {
                eprintln!("+ {l}");
            }
        }
    } else {
        atomic_write(&path, out_text.as_bytes(), mode_of(&meta));
    }

    let mut event = serde_json::json!({
        "path": abs(&path),
        "edits": edits.len(),
        "replaced": replaced,
        "bytes_before": data.len(),
        "bytes_after": out_text.len(),
        "dry_run": dry,
        "fuzzy": fuzzy,
        // Все правки оказались no-op: файл уже был в нужном виде. Не ошибка,
        // но и не «сделано» — разбору полезно видеть разницу.
        "unchanged": replaced == 0,
    });
    if fuzzy {
        event["note"] = Value::String(FUZZY_NOTE.into());
    }
    println!("{event}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_file_gets_crlf_even_on_exact_match() {
        // Точное совпадение на CRLF-файле, а new_string — с голыми \n: иначе в
        // середину файла попадали LF-строки и получались смешанные концы.
        let orig = "alpha\r\nbeta\r\n";
        let (t, _plan) = apply(orig, &[e(0, "beta", "gamma\ndelta")]);
        assert_eq!(t, "alpha\r\ngamma\r\ndelta\r\n", "концы строк смешались");
    }

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

    #[test]
    fn hint_gives_two_candidate_lines_with_numbers() {
        let text = "let a = 1;\nlet b = 2;\nlet c = 3;\n";
        let h = hint(text, "let d = 4;");
        assert!(h.contains("строка"), "нет номера строки: {h}");
        assert_eq!(
            h.lines().count(),
            2,
            "подсказка должна давать две строки, а не {h:?}"
        );
    }

    fn e(n: usize, old: &str, new: &str) -> Edit {
        Edit {
            n,
            old: old.into(),
            new: new.into(),
            all: false,
        }
    }

    fn apply(orig: &str, edits: &[Edit]) -> (String, Vec<Applied>) {
        apply_all(orig, edits, "f").expect("правки должны примениться")
    }

    #[test]
    fn identical_strings_are_noop_not_error() {
        // Файл уже ровно в нужном виде: 0 замен, текст без изменений.
        let (t, plan) = apply("alpha beta\n", &[e(0, "alpha", "alpha")]);
        assert!(plan.is_empty());
        assert_eq!(t, "alpha beta\n");
        // Даже когда фрагмента в файле нет — при old == new это не «не найден».
        let (t, plan) = apply("other\n", &[e(0, "absent", "absent")]);
        assert!(plan.is_empty());
        assert_eq!(t, "other\n");
    }

    #[test]
    fn hunks_match_the_original_not_the_previous_result() {
        // Правка 2 матчится против исходника: результат правки 1 для неё не текст.
        let err = apply_all(
            "let a = 1;\n",
            &[
                e(1, "let a = 1;", "let a = 10;"),
                e(2, "let a = 10;", "let a = 11;"),
            ],
            "f",
        )
        .unwrap_err();
        assert!(err.contains("правка 2"), "{err}");
        assert!(err.contains("не найден"), "{err}");
    }

    #[test]
    fn batch_applies_all_against_original() {
        let (t, plan) = apply("one\ntwo\nthree\n", &[e(1, "one", "1"), e(2, "three", "3")]);
        assert_eq!(plan.len(), 2);
        assert_eq!(t, "1\ntwo\n3\n");
    }

    #[test]
    fn overlapping_batch_is_rejected() {
        let err = apply_all("abcdef\n", &[e(1, "abc", "X"), e(2, "bcd", "Y")], "f").unwrap_err();
        assert!(err.contains("задевают один и тот же участок"), "{err}");
        assert!(
            err.contains("1") && err.contains("2"),
            "нет номеров правок: {err}"
        );
        assert!(err.contains("файл не тронут"), "{err}");
    }

    #[test]
    fn identical_hunks_in_batch_are_overlap_not_double_write() {
        let err = apply_all("x\ny = 1;\n", &[e(1, "x", "z"), e(2, "x", "w")], "f").unwrap_err();
        assert!(err.contains("задевают один и тот же участок"), "{err}");
    }

    #[test]
    fn missing_hunk_names_batch_atomicity() {
        let err = apply_all(
            "one\ntwo\n",
            &[e(1, "one", "1"), e(2, "нетакого", "2")],
            "f",
        )
        .unwrap_err();
        assert!(err.contains("правка 2"), "{err}");
        assert!(err.contains("ни одна правка не применена"), "{err}");
        // Перечислено, что сошлось бы: модель видит, какая именно правка мимо.
        assert!(err.contains("правка 1 сошлась бы"), "{err}");
        assert!(err.contains("правка 2 не сошлась"), "{err}");
    }

    #[test]
    fn exact_match_wins_over_fuzzy() {
        // В файле есть и ASCII-кавычки, и «ёлочки»: точное совпадение должно
        // взять ровно одно место, а не полезть в нормализованные дубли.
        let (t, plan) = apply("x = \"q\";\ny = «q»;\n", &[e(0, "\"q\"", "\"z\"")]);
        assert_eq!(plan.len(), 1);
        assert!(!plan[0].fuzzy);
        assert_eq!(t, "x = \"z\";\ny = «q»;\n");
    }

    // ── fuzzy ───────────────────────────────────────────────────────────────

    #[test]
    fn fuzzy_trailing_whitespace() {
        let (t, plan) = apply(
            "head\nvalue = 1;   \ntail\n",
            &[e(0, "value = 1;\ntail", "value = 2;\ntail")],
        );
        assert!(plan[0].fuzzy, "совпадение должно быть помечено fuzzy");
        assert_eq!(t, "head\nvalue = 2;\ntail\n");
    }

    #[test]
    fn fuzzy_crlf() {
        let (t, plan) = apply(
            "alpha\r\nbeta\r\ngamma\r\n",
            &[e(0, "alpha\nbeta\n", "ALPHA\nBETA\n")],
        );
        assert!(plan[0].fuzzy);
        // Изменённые строки сохраняют CRLF, хвост файла — байт в байт.
        assert_eq!(t, "ALPHA\r\nBETA\r\ngamma\r\n");
    }

    #[test]
    fn fuzzy_crlf_with_trailing_newline_does_not_double_it() {
        // Совпадение уже захватило перевод строки: добавлять ещё один CRLF
        // (переводя \n нового фрагмента) значило бы вставить пустую строку.
        let (t, plan) = apply("a\r\nb\r\nc\r\n", &[e(0, "a\nb\n", "A\nB\n")]);
        assert!(plan[0].fuzzy);
        assert_eq!(t, "A\r\nB\r\nc\r\n");
    }

    #[test]
    fn fuzzy_curly_quotes_and_dashes() {
        let (t, plan) = apply(
            "let s = “привет”; // a — b\n",
            &[e(0, "let s = \"привет\"; // a - b", "let s = \"пока\";")],
        );
        assert!(plan[0].fuzzy);
        assert_eq!(t, "let s = \"пока\";\n");
    }

    #[test]
    fn fuzzy_nbsp_to_space() {
        let (t, plan) = apply("a\u{a0}b\n", &[e(0, "a b", "A B")]);
        assert!(plan[0].fuzzy);
        assert_eq!(t, "A B\n");
    }

    #[test]
    fn fuzzy_nbsp_and_ligature() {
        let (t, plan) = apply("a\u{a0}b ﬁle\n", &[e(0, "a b file", "A B FILE")]);
        assert!(plan[0].fuzzy);
        assert_eq!(t, "A B FILE\n");
    }

    #[test]
    fn fuzzy_refuses_tab_versus_spaces() {
        // Отступы в начале строки не нормализуются: ширина таба не угадывается,
        // иначе блок с другим отступом молча переписывается.
        let err = apply_all(
            "if x {\n\tvalue\n}\n",
            &[e(0, "if x {\n    value\n}", "if x {\n    VALUE\n}")],
            "f",
        )
        .unwrap_err();
        assert!(err.contains("не найден"), "{err}");
    }

    #[test]
    fn fuzzy_refuses_match_inside_a_ligature() {
        // «il» попадает внутрь раскрытого NFKC «ﬁ»: диапазон захватил бы половину
        // чужого символа, поэтому такой матч честнее отклонить, чем угадывать.
        let err = apply_all("\u{fb01}le\n", &[e(0, "il", "IL")], "f").unwrap_err();
        assert!(err.contains("не найден"), "{err}");
    }

    #[test]
    fn fuzzy_keeps_untouched_lines_byte_for_byte() {
        let orig = "keep\tthis\r\nalpha   \r\nxxx\r\n";
        let (t, plan) = apply(orig, &[e(0, "alpha\nxxx\n", "ALPHA\nXXX\n")]);
        assert!(plan[0].fuzzy);
        assert_eq!(t, "keep\tthis\r\nALPHA\r\nXXX\r\n");
        // Всё, что до и после правки, — тот же срез оригинала.
        assert!(t.starts_with("keep\tthis\r\n"), "префикс переписан: {t:?}");
        assert!(t.ends_with("XXX\r\n"), "хвост переписан: {t:?}");
    }

    #[test]
    fn fuzzy_mid_line_keeps_the_rest_of_the_line() {
        let orig = "let a = “x”; let b = 2;\n";
        let (t, _) = apply(orig, &[e(0, "\"x\"", "\"y\"")]);
        assert_eq!(t, "let a = \"y\"; let b = 2;\n");
    }

    #[test]
    fn fuzzy_line_count_stays_same_for_unrelated_lines() {
        // Ни одна строка не переписана «заодно»: число строк то же, пробелы на
        // месте, меняется только затронутый участок.
        let orig = "a  \nb  \nvalue — 1\nc  \n";
        let (t, _) = apply(orig, &[e(0, "value - 1", "value - 2")]);
        assert_eq!(t, "a  \nb  \nvalue - 2\nc  \n");
    }

    // ── разбор ──────────────────────────────────────────────────────────────

    #[test]
    fn empty_edits_with_single_pair_is_accepted() {
        // Живой прогон: модель прислала {"path","old_string","new_string",
        // "replace_all","edits":[]} — все поля схемы сразу, массив пустой.
        // Отказ «пустой edits» стоил хода на ровном месте.
        let j = serde_json::json!({
            "old_string": "a", "new_string": "b", "replace_all": false, "edits": []
        });
        let list = parse_edits(&j).expect("одиночная пара при пустом edits принимается");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].old, "a");
        // Пустой edits без пары — по-прежнему отказ.
        let j = serde_json::json!({"edits": []});
        assert!(parse_edits(&j).is_err(), "менять нечего — это отказ");
    }

    #[test]
    fn edits_accepts_single_object() {
        let j = serde_json::json!({"edits": {"old_string": "a", "new_string": "b"}});
        let list = parse_edits(&j).expect("одиночный объект в edits принимается");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].old, "a");
        assert_eq!(list[0].n, 0);
    }

    #[test]
    fn edits_rejects_empty_and_garbage() {
        assert!(parse_edits(&serde_json::json!({"edits": []})).is_err());
        assert!(parse_edits(&serde_json::json!({"edits": [1]})).is_err());
        assert!(parse_edits(&serde_json::json!({"edits": "x"})).is_err());
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
