// Одноразовый инструмент: перевод старых логов ревью (`<id>-review.jsonl` — полная
// копия лога сессии плюс ход ревью) на ссылку `Replay`. Ядро читает оба формата,
// так что нужен он ровно один раз: вернуть место на диске.
//
// Снаружи workspace намеренно: в `snens build` и в реестре тулов ему делать нечего.
//   cargo run --manifest-path oneoff/session-compact/Cargo.toml -- \
//       ~/.local/share/snens/sessions --dry-run
use std::{collections::BTreeMap, path::{Path, PathBuf}};

const USAGE: &str = "session-compact — старые логи ревью на ссылку Replay
usage: session-compact <каталог сессий> [--dry-run] [--verbose]
  --dry-run   только посчитать, файлы не трогать
  --verbose   по строке на файл";

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(dir) = argv.iter().find(|a| !a.starts_with("--")).cloned() else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let dry = argv.iter().any(|a| a == "--dry-run");
    let verbose = argv.iter().any(|a| a == "--verbose");
    match run(Path::new(&dir), dry, verbose) {
        Ok(report) => print!("{report}"),
        Err(e) => {
            eprintln!("session-compact: {e}");
            std::process::exit(1);
        }
    }
}

fn run(dir: &Path, dry: bool, verbose: bool) -> Result<String, String> {
    let (mut files, mut saved, mut skipped, mut failed) = (0u64, 0u64, 0u64, 0u64);
    let mut out = String::new();
    for (id, path) in reviews(dir)? {
        let base = id.trim_end_matches("-review");
        let src = dir.join(format!("{base}.jsonl"));
        let source = lines(&src).map_err(|e| format!("{}: {e}", src.display()))?;
        if source.is_empty() {
            // Источника нет: копию не к чему привязать, а молча терять транскрипт
            // нельзя. Такие файлы человек удаляет сам.
            skipped += 1;
            out.push_str(&format!("{id}: нет источника {base}, пропуск\n"));
            continue;
        }
        let raw = lines(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if is_replay(&raw) {
            skipped += 1;
            if verbose {
                out.push_str(&format!("{id}: уже со ссылкой, пропуск\n"));
            }
            continue;
        }
        // Общий префикс — ровно те записи, что совпали: их и адресует ссылка.
        // Расхождение дальше означает лишь, что сессия продолжалась после
        // копирования; копия от этого хуже не становится.
        let shared = prefix_len(&raw, &source);
        if shared < 2 {
            // Префикс не совпал с сессией: сжимать без потерь нечем.
            skipped += 1;
            out.push_str(&format!(
                "{id}: префикс не совпадает (общих {} из {}), пропуск\n",
                shared,
                source.len()
            ));
            continue;
        }
        let replay = format!(
            "{{\"kind\":\"replay\",\"session\":{},\"lines\":{shared}}}",
            quoted(base)
        );
        let tail = &raw[shared..];
        let body = [raw[0].as_str(), replay.as_str()]
            .into_iter()
            .chain(tail.iter().map(String::as_str))
            .map(|l| format!("{l}\n"))
            .collect::<String>();
        let before = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
        if dry {
            saved += before.saturating_sub(body.len() as u64);
            files += 1;
            if verbose {
                out.push_str(&format!(
                    "{id}: {before} → {} байт, ссылка на {shared}, хвост {} записей\n",
                    body.len(),
                    tail.len()
                ));
            }
            continue;
        }
        match write_atomic(&path, &body) {
            Ok(()) => {
                saved += before.saturating_sub(body.len() as u64);
                files += 1;
                if verbose {
                    out.push_str(&format!("{id}: {before} → {} байт\n", body.len()));
                }
            }
            Err(e) => {
                failed += 1;
                out.push_str(&format!("{id}: {e}\n"));
            }
        }
    }
    out.push_str(&format!(
        "{} {}: сжато {files}, пропущено {skipped}, ошибок {failed}; {} {:.1} МБ\n",
        if dry { "проверка" } else { "итого" },
        dir.display(),
        if dry { "сэкономило бы" } else { "освобождено" },
        saved as f64 / 1e6
    ));
    Ok(out)
}

/// Логи ревью в каталоге, по имени.
fn reviews(dir: &Path) -> Result<BTreeMap<String, PathBuf>, String> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.ends_with("-review.jsonl") {
            out.insert(name.trim_end_matches(".jsonl").to_string(), path);
        }
    }
    Ok(out)
}

fn lines(path: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect())
}

fn is_replay(raw: &[String]) -> bool {
    raw.get(1)
        .is_some_and(|l| l.contains("\"kind\":\"replay\""))
}

/// Длина общего префикса строк. Шапка сравнивается по `kind`: у копии она байт в
/// байт та же, но опираться на это не нужно.
fn prefix_len(a: &[String], b: &[String]) -> usize {
    let mut n = 0;
    while n < a.len() && n < b.len() {
        let same = if n == 0 {
            a[0].contains("\"kind\":\"meta\"") && b[0].contains("\"kind\":\"meta\"")
        } else {
            a[n] == b[n]
        };
        if !same {
            break;
        }
        n += 1;
    }
    n
}

fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Замена файла через временный рядом: читатель видит либо старый файл, либо новый.
fn write_atomic(path: &Path, body: &str) -> Result<(), String> {
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}
