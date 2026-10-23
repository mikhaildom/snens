// Тикеты инстанса: экономно доставать и искать накопившиеся заметки о среде.
// Данные принадлежат инстансу, как sessions/agents, поэтому живут в ядре, а не в туле.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{cfg::Config, R};

/// Один тикет: front-matter плюс тело. Хранится целиком, чтобы `grep` не
/// перечитывал файл на каждую проверку.
pub struct Ticket {
    pub name: String,
    pub path: PathBuf,
    pub status: String,
    pub kind: String,
    pub target: String,
    pub family: String,
    pub weight: String,
    pub hits: String,
    pub body: String,
}

impl Ticket {
    pub fn hit_count(&self) -> u64 {
        self.hits.trim().parse().unwrap_or(0)
    }

    /// Первая непустая строка секции «Симптом» — то же, что видит список в CLI.
    pub fn symptom(&self) -> String {
        symptom(&self.body)
    }

    /// Когда файл трогали последний раз (unix, сек). `0` — метаданных нет.
    pub fn mtime(&self) -> u64 {
        mtime(&self.path)
    }
}

/// Строка списка для поверхности: только то, что видно в дашборде, без тела.
#[derive(Clone, Debug, Serialize)]
pub struct Summary {
    pub name: String,
    pub status: String,
    pub kind: String,
    pub target: String,
    pub family: String,
    pub weight: String,
    pub hits: u64,
    pub symptom: String,
    /// Время правки файла, unix-секунды. По нему и сортируется список.
    pub mtime: u64,
}

impl From<&Ticket> for Summary {
    fn from(t: &Ticket) -> Self {
        Summary {
            name: t.name.clone(),
            status: t.status.clone(),
            kind: t.kind.clone(),
            target: t.target.clone(),
            family: t.family.clone(),
            weight: t.weight.clone(),
            hits: t.hit_count(),
            symptom: clip(&t.symptom(), 200),
            mtime: t.mtime(),
        }
    }
}

/// Время правки файла в unix-секундах; `0`, если метаданные не читаются.
fn mtime(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Тикеты для дашборда: свежие сверху. Сортировка по mtime, а не по имени:
/// имя — дата создания, а дописанная «История» старый тикет делает свежим.
/// При равном mtime имя решает, чтобы порядок был устойчив между запросами.
pub fn summaries(dir: &Path) -> Vec<Summary> {
    let mut out: Vec<Summary> = load_all(dir).iter().map(Summary::from).collect();
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime).then(a.name.cmp(&b.name)));
    out
}

/// Допустимое имя тикета: имя файла без `.md`. Приходит из URL, поэтому `../`,
/// слэши и точки в начале отсекаются здесь, а не в каждом хендлере.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Один тикет по имени. Имя — ровно имя файла без `.md`: префиксы и поиск
/// остаются в CLI, поверхность отдаёт то, что попросили.
pub fn find(dir: &Path, name: &str) -> Option<Ticket> {
    if !valid_name(name) {
        return None;
    }
    load_all(dir).into_iter().find(|t| t.name == name)
}

/// Поле front-matter: первая строка `key:`, значение триммится.
fn fm(text: &str, key: &str) -> String {
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}:")))
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

/// Строка секции «Симптом»: первая непустая строка после заголовка, до следующего
/// `## `. Экономный признак, по которому тикет узнаётся в списке.
fn symptom(text: &str) -> String {
    text.lines()
        .skip_while(|l| !l.trim_start().starts_with("## Симптом"))
        .skip(1)
        .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with("## "))
        .unwrap_or("")
        .trim()
        .to_string()
}

fn clip(s: &str, n: usize) -> String {
    let mut t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        t.push('…');
    }
    t
}

/// Разбор текста тикета в структуру. Отдельно от чтения с диска — тестируется без fs.
pub fn parse(name: &str, path: PathBuf, text: &str) -> Ticket {
    Ticket {
        name: name.to_string(),
        path,
        status: fm(text, "status"),
        kind: fm(text, "kind"),
        target: fm(text, "target"),
        family: fm(text, "family"),
        weight: fm(text, "weight"),
        hits: fm(text, "hits"),
        body: text.to_string(),
    }
}

/// Все тикеты каталога, отсортированные по имени (в имени — дата, так что по времени).
pub fn load_all(dir: &Path) -> Vec<Ticket> {
    let mut out: Vec<Ticket> = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let name = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        out.push(parse(&name, p, &text));
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Открытые тикеты одной строкой каждый — то, что уезжает ревью, чтобы оно не
/// придумывало ту же проблему заново.
pub fn open_list(dir: &Path) -> String {
    let rows: Vec<String> = load_all(dir)
        .iter()
        .filter(|t| t.status == "open")
        .map(|t| {
            format!(
                "- {} | {} | {} | hits {} | {}",
                t.name,
                t.kind,
                t.target,
                t.hits,
                clip(&symptom(&t.body), 140)
            )
        })
        .collect();
    if rows.is_empty() {
        "(открытых тикетов нет)".into()
    } else {
        rows.join("\n")
    }
}

/// Полные тела открытых тикетов. Используется только тестами как мера экономии
/// [`open_brief`]; в рабочий промпт полные тела не уезжают.
#[cfg(test)]
pub fn open_full(dir: &Path) -> String {
    let mut out = String::new();
    for t in load_all(dir).iter().filter(|t| t.status == "open") {
        out.push_str(&format!("=== {} ===\n{}\n", t.name, t.body));
    }
    if out.is_empty() {
        "(открытых тикетов нет)".into()
    } else {
        out
    }
}

/// Экономная сводка открытых: front-matter и симптом, без полного тела. Хвост
/// заметок растёт линейно, поэтому в промпт уезжает только оглавление.
pub fn open_brief(dir: &Path) -> String {
    let rows: Vec<String> = load_all(dir)
        .iter()
        .filter(|t| t.status == "open")
        .map(|t| {
            let mut head = format!(
                "=== {} ===\nstatus: {} | kind: {} | target: {} | hits: {}\n",
                t.name, t.status, t.kind, t.target, t.hits
            );
            let s = symptom(&t.body);
            if !s.is_empty() {
                head.push_str(&format!("симптом: {s}\n"));
            }
            head
        })
        .collect();
    if rows.is_empty() {
        "(открытых тикетов нет)".into()
    } else {
        rows.join("\n")
    }
}

fn matches(t: &Ticket, needle: &str) -> bool {
    let n = needle.to_lowercase();
    t.name.to_lowercase().contains(&n) || t.body.to_lowercase().contains(&n)
}

fn field_ok(got: &str, want: &str) -> bool {
    got.to_lowercase().contains(&want.to_lowercase())
}

pub fn cmd(flags: &BTreeMap<String, String>, positional: Vec<String>) -> R<()> {
    let cfg = Config::load(flags.get("config").map(|s| s.as_str()))?;
    let dir = cfg.tickets_path();
    let sub = positional.first().map(|s| s.as_str()).unwrap_or("");
    let all = flags.contains_key("all");

    match sub {
        "show" => {
            let q = positional
                .get(1)
                .ok_or("нужно имя тикета (можно префикс)")?;
            let mut found: Vec<Ticket> = load_all(&dir)
                .into_iter()
                .filter(|t| t.name.starts_with(q.as_str()))
                .collect();
            if found.is_empty() {
                found = load_all(&dir)
                    .into_iter()
                    .filter(|t| matches(t, q))
                    .collect();
            }
            match found.len() {
                0 => return Err(format!("тикет не найден: {q}").into()),
                1 => {
                    if flags.contains_key("full") {
                        println!("# {}", found[0].path.display());
                    }
                    print!("{}", found[0].body);
                }
                _ => {
                    for t in &found {
                        println!("{} | {} | {}", t.name, t.status, t.target);
                    }
                    return Err(
                        format!("под «{q}» подходит {} тикетов — уточни", found.len()).into(),
                    );
                }
            }
            Ok(())
        }
        "grep" => {
            let q = positional.get(1).ok_or("нужно что искать")?;
            let all_t = load_all(&dir);
            let mut rows: Vec<&Ticket> = all_t
                .iter()
                .filter(|t| all || t.status == "open")
                .filter(|t| matches(t, q))
                .collect();
            rows.sort_by_key(|t| std::cmp::Reverse(t.hit_count()));
            if rows.is_empty() {
                println!("(нет совпадений)");
            }
            for t in rows {
                println!(
                    "{} | {} {} | hits {} | {}",
                    t.name,
                    t.status,
                    t.kind,
                    t.hits,
                    clip(&symptom(&t.body), 100)
                );
            }
            Ok(())
        }
        "stats" => {
            let all_t = load_all(&dir);
            let mut open = 0;
            let mut by_target: BTreeMap<&str, u32> = BTreeMap::new();
            for t in &all_t {
                if t.status == "open" {
                    open += 1;
                    *by_target.entry(t.target.as_str()).or_default() += 1;
                }
            }
            println!(
                "всего {} | открытых {open} | закрытых {}",
                all_t.len(),
                all_t.len() - open
            );
            println!("\nоткрытые по target:");
            let mut pairs: Vec<(&str, u32)> = by_target.into_iter().collect();
            pairs.sort_by_key(|p| std::cmp::Reverse(p.1));
            for (k, v) in pairs {
                println!("  {v:>3}  {k}");
            }
            Ok(())
        }
        "" => {
            let t = load_all(&dir);
            // По умолчанию сортируем по hits: чаще всего больно там, где симптом
            // возвращался. `--weight` — когда важнее заявленная тяжесть.
            let by_weight = flags.contains_key("weight");
            let mut rows: Vec<&Ticket> = t
                .iter()
                .filter(|t| all || t.status == "open")
                .filter(|t| flags.get("target").is_none_or(|v| field_ok(&t.target, v)))
                .filter(|t| flags.get("kind").is_none_or(|v| field_ok(&t.kind, v)))
                .filter(|t| flags.get("status").is_none_or(|v| field_ok(&t.status, v)))
                .filter(|t| flags.get("family").is_none_or(|v| field_ok(&t.family, v)))
                .collect();
            rows.sort_by(|a, b| {
                if by_weight {
                    b.weight
                        .cmp(&a.weight)
                        .then(b.hit_count().cmp(&a.hit_count()))
                } else {
                    b.hit_count().cmp(&a.hit_count()).then(a.name.cmp(&b.name))
                }
            });
            if rows.is_empty() {
                println!("(нет тикетов)");
            }
            for t in rows {
                println!(
                    "{} | {} {} | {} | hits {} | {}",
                    t.name,
                    t.status,
                    t.kind,
                    t.target,
                    t.hits,
                    clip(&symptom(&t.body), 90)
                );
            }
            Ok(())
        }
        other => Err(format!("неизвестная подкоманда tickets: {other}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: &str = "---\nstatus: open\nkind: defect\ntarget: read\nhits: 2\n---\n\
                        ## Симптом / что повторяется\nОтказ без подсказки\n## История\n- что-то\n";
    const DONE: &str = "---\nstatus: done\nkind: defect\ntarget: read\nhits: 1\n---\n\
                        ## Симптом\nУже починено\n";

    fn tmp() -> PathBuf {
        // Уникальный на КАЖДЫЙ тест, а не на процесс: тесты идут параллельно
        // и каждый зовёт `remove_dir_all` в конце — общий каталог они сносили
        // друг у друга, и `write` падал с NotFound (плавающий красный тест).
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "snens-tickets-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("2026-09-22-read.md"), OPEN).unwrap();
        std::fs::write(d.join("2026-09-22-old.md"), DONE).unwrap();
        d
    }

    #[test]
    fn parse_reads_front_matter() {
        let t = parse("x", PathBuf::from("x"), OPEN);
        assert_eq!(t.status, "open");
        assert_eq!(t.kind, "defect");
        assert_eq!(t.target, "read");
        assert_eq!(t.hit_count(), 2);
        assert_eq!(symptom(&t.body), "Отказ без подсказки");
    }

    #[test]
    fn open_list_skips_closed_and_keeps_symptom() {
        let d = tmp();
        let list = open_list(&d);
        assert!(list.contains("2026-09-22-read"), "{list}");
        assert!(list.contains("Отказ без подсказки"), "{list}");
        assert!(
            !list.contains("2026-09-22-old"),
            "закрытый в списке: {list}"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn brief_is_smaller_than_full() {
        let d = tmp();
        let brief = open_brief(&d);
        let full = open_full(&d);
        assert!(brief.contains("симптом: Отказ без подсказки"), "{brief}");
        assert!(!brief.contains("## История"), "brief утёк телом: {brief}");
        assert!(brief.len() < full.len());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn missing_dir_is_not_an_error() {
        assert!(open_list(Path::new("/нет/такого/каталога")).contains("тикетов нет"));
    }

    #[test]
    fn summaries_carry_fields_symptom_and_mtime() {
        let d = tmp();
        let rows = summaries(&d);
        assert_eq!(rows.len(), 2, "{rows:?}");
        let row = rows.iter().find(|r| r.name == "2026-09-22-read").unwrap();
        assert_eq!(row.status, "open");
        assert_eq!(row.kind, "defect");
        assert_eq!(row.target, "read");
        assert_eq!(row.hits, 2, "hits приходят числом, а не строкой");
        assert_eq!(row.symptom, "Отказ без подсказки");
        assert!(row.mtime > 0, "mtime не доехал: {row:?}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn summaries_are_newest_first() {
        let d = tmp();
        // Портим порядок имён: файл с «младшим» именем должен всплыть наверх
        // именно из-за mtime, а не по алфавиту. Пауза — чтобы mtime различались
        // даже на ФС с секундной точностью.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(d.join("2026-01-01-fresh.md"), DONE).unwrap();
        let rows = summaries(&d);
        assert_eq!(rows[0].name, "2026-01-01-fresh", "{rows:?}");
        assert!(rows.windows(2).all(|w| w[0].mtime >= w[1].mtime), "{rows:?}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn symptom_is_clipped_for_dashboard() {
        let long = format!("---\nstatus: open\n---\n## Симптом\n{}\n", "я".repeat(500));
        let t = parse("x", PathBuf::from("x"), &long);
        let s = clip(&t.symptom(), 200);
        assert!(s.chars().count() <= 201, "не обрезано: {}", s.chars().count());
        assert!(s.ends_with('…'));
    }

    #[test]
    fn find_takes_exact_name_and_refuses_traversal() {
        let d = tmp();
        assert!(find(&d, "2026-09-22-read").is_some());
        assert!(find(&d, "2026-09-22-r").is_none(), "префикс — дело CLI");
        assert!(find(&d, "../2026-09-22-read").is_none());
        assert!(find(&d, "/etc/passwd").is_none());
        assert!(find(&d, "").is_none());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn empty_dir_gives_empty_list() {
        let d = std::env::temp_dir().join(format!("snens-tickets-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert!(summaries(&d).is_empty());
        assert!(summaries(Path::new("/нет/такого/каталога")).is_empty());
        std::fs::remove_dir_all(&d).ok();
    }
}
