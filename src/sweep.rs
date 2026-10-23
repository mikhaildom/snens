// Уборка мёртвого в каталоге сессий. Логи и их формат не трогаем: убираем то, что
// нужно только закрытым сессиям, — вложения, дампы обрезанных выводов и зеркало
// рабочего контекста.
//
// «Живая» ссылка — ссылка из лога без `Done`: сессия может продолжиться (resume),
// и её вложение или дамп ещё откроют. Ссылки из закрытых сессий живыми не считаем:
// ровно про них сказано «больше не понадобится никогда».
//
// Порог по возрасту (дефолт 30 дней) — вторая страховка: свежий файл не трогаем,
// даже если ссылок на него не нашлось.
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use crate::{cfg::Config, log, R};


/// Что удалять: путь и размер.
type Victims = Vec<(PathBuf, u64)>;

/// Ссылки, живые для будущих резюмов.
#[derive(Default)]
struct Alive {
    attach: HashSet<String>,
    dumps: HashSet<String>,
    /// Сессии без `Done`: их зеркало и дампы нужны.
    open: HashSet<String>,
}

pub fn cmd(flags: &BTreeMap<String, String>, _pos: Vec<String>) -> R<()> {
    let cfg = Config::load(flags.get("config").map(|s| s.as_str()))?;
    let days: u64 = flags
        .get("older-than")
        .map(|s| s.parse())
        .transpose()
        .map_err(|_| "--older-than ждёт число дней")?
        .unwrap_or(30);
    let older = Duration::from_secs(days.saturating_mul(86_400));
    run(
        &cfg,
        older,
        flags.contains_key("dry-run"),
        flags.contains_key("attachments"),
    )
}

fn run(cfg: &Config, older: Duration, dry: bool, attachments: bool) -> R<()> {
    let dir = cfg.sessions_path();
    let cutoff = SystemTime::now()
        .checked_sub(older)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let alive = scan(&dir)?;

    // Категория -> что удалить. Считаем списком, а не счётчиком: удалять надо все
    // файлы, а не первые три из отчёта.
    let mut victims: BTreeMap<&str, Victims> = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let out = entry.path();
        if out.extension().and_then(|e| e.to_str()) != Some("out") || !out.is_dir() {
            continue;
        }
        let Some(sid) = out.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
            continue;
        };
        let is_open = alive.open.contains(&sid);
        for sub in std::fs::read_dir(&out).into_iter().flatten().flatten() {
            let path = sub.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let old = old_enough(&path, cutoff);
            let category = if path.is_dir() && name == "attach" {
                // Вложения: каталог на вызов, внутри — файлы (иногда в подкаталогах).
                // Удаляем поштучно: часть вызовов приложила файл, часть нет.
                if old && attachments {
                    collect_attach(&path, &alive.attach, &mut victims);
                }
                continue;
            } else if name.ends_with(".txt") {
                // Дамп: лог хранит путь строкой в теле результата.
                let key = format!("{sid}.out/{name}");
                (old && !alive.dumps.contains(&key)).then_some("дампы")
            } else if name == "LIVE_CONTEXT.json" {
                // Зеркало закрытой сессии: resume соберёт его заново, а черновик
                // правок в закрытой сессии некому применять.
                (old && !is_open).then_some("копия контекста")
            } else {
                None
            };
            if let Some(category) = category {
                push(&mut victims, category, path);
            }
        }
    }

    let mut total = (0u64, 0u64);
    for (category, list) in &victims {
        let bytes: u64 = list.iter().map(|(_, b)| b).sum();
        println!("{category:12} {:5} файлов {:8.1} МБ", list.len(), bytes as f64 / 1e6);
        total.0 += list.len() as u64;
        total.1 += bytes;
        if !dry {
            for (path, _) in list {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    if !attachments {
        println!("вложения не трогали: они данные человека (PDF, скриншоты). Нужно — добавь --attachments");
    }
    println!(
        "{}: {} файлов, {:.1} МБ (порог {} дней)",
        if dry { "было бы убрано" } else { "убрано" },
        total.0,
        total.1 as f64 / 1e6,
        older.as_secs() / 86_400
    );
    Ok(())
}

/// Обойти `attach` вглубь и забрать всё, на что не ссылается живой лог.
fn collect_attach(dir: &Path, referenced: &HashSet<String>, victims: &mut BTreeMap<&'static str, Victims>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_attach(&path, referenced, victims);
        } else if !referenced.contains(&path.to_string_lossy().to_string()) {
            push(victims, "вложения", path);
        }
    }
}

fn push(victims: &mut BTreeMap<&'static str, Victims>, category: &'static str, path: PathBuf) {
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    victims.entry(category).or_default().push((path, bytes));
}

fn old_enough(path: &Path, cutoff: SystemTime) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| t < cutoff)
        .unwrap_or(false)
}

/// Собрать ссылки из логов: какие из них живые и какие сессии не закрыты.
fn scan(dir: &Path) -> R<Alive> {
    let mut alive = Alive::default();
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    names.sort();
    for path in names {
        let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
            continue;
        };
        // Читаем файлом, без `Session::open`: тот берёт эксклюзивный лок, а его
        // держит поверхность — и живая сессия выглядела бы нечитаемой.
        let Ok(records) = log::read_file(&path) else {
            // Лог не читается (порча) — считаем сессию живой: лучше не удалить,
            // чем удалить нужное.
            alive.open.insert(id);
            continue;
        };
        let records = log::expand(&records, dir);
        let closed = records.iter().any(|r| matches!(r, log::Rec::Done { .. }));
        if !closed {
            alive.open.insert(id.clone());
        }
        for rec in &records {
            if let log::Rec::ToolResult { parts, body, .. } = rec {
                if closed {
                    continue;
                }
                for p in parts {
                    alive.attach.insert(p.path.clone());
                }
                // Тело пока одно (`Inline`): расширение вида — правка одной ветки.
                let log::Body::Inline { text } = body;
                collect_dumps(text, &mut alive.dumps);
            }
        }
    }
    Ok(alive)
}

/// Пути дампов внутри текста результата: `<sid>.out/<hash>.txt`. Режем по
/// символам, а не по байтам: текст русский, а `.out/` в нём может стоять после
/// любой кириллицы.
fn collect_dumps(text: &str, out: &mut HashSet<String>) {
    const MARK: &str = ".out/";
    for (i, _) in text.match_indices(MARK) {
        // Слева — имя сессии, справа — имя файла дампа.
        let sid: String = text[..i]
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let file: String = text[i + MARK.len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
            .collect();
        // Точка после имени — конец предложения, а не часть файла.
        let file = file.trim_end_matches('.');
        if sid.is_empty() || !file.ends_with(".txt") {
            continue;
        }
        out.insert(format!("{sid}{MARK}{file}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_paths_are_found_in_result_text() {
        let mut out = HashSet::new();
        collect_dumps("см. /x/sessions/1-2.out/abc123.txt и всё", &mut out);
        collect_dumps("файл 1-2.out/deadbeef.txt.\n", &mut out);
        collect_dumps("/x/a.out/attach/call/img.png", &mut out);
        assert!(out.contains("1-2.out/abc123.txt"), "{out:?}");
        assert!(out.contains("1-2.out/deadbeef.txt"), "{out:?}");
        assert_eq!(out.len(), 2, "png не дамп: {out:?}");
    }

    /// Полный прогон: старое зеркало закрытой сессии уходит, свежее и зеркало
    /// живой остаются. `--dry-run` не удаляет ничего.
    #[test]
    fn sweep_removes_only_old_and_closed_and_dry_run_keeps_files() {
        let dir = std::env::temp_dir().join(format!("snens-sweep-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let old = dir.join("old.out/LIVE_CONTEXT.json");
        let open = dir.join("open.out/LIVE_CONTEXT.json");
        for path in [&old, &open] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"{}").unwrap();
        }
        std::fs::write(
            dir.join("old.jsonl"),
            "{\"kind\":\"done\",\"ts\":1,\"status\":\"ok\",\"turns\":1}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("open.jsonl"),
            "{\"kind\":\"meta\",\"ts\":1,\"model\":\"\",\"parent\":\"\"}\n",
        )
        .unwrap();
        let cfg = Config {
            sessions_dir: dir.display().to_string(),
            ..Config::default()
        };
        // Файлы только что созданы, поэтому порог «старше» нулевой, а не дефолтный.
        run(&cfg, Duration::from_secs(0), true, false).unwrap();
        assert!(old.exists() && open.exists(), "dry-run удалил файлы");
        run(&cfg, Duration::from_secs(0), false, false).unwrap();
        assert!(!old.exists(), "зеркало закрытой сессии осталось");
        assert!(open.exists(), "зеркало живой сессии удалено");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn closed_session_releases_its_attachments_but_open_keeps_them() {
        let dir = std::env::temp_dir().join(format!("snens-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let part = |sid: &str| {
            format!(
                "{{\"kind\":\"tool_result\",\"ts\":2,\"call_id\":\"c\",\"tool\":\"view\",\"argv\":[],\"exit\":0,\"bytes\":1,\"ms\":1,\"body\":{{\"t\":\"inline\",\"text\":\"ok\"}},\"parts\":[{{\"kind\":\"image\",\"path\":\"{sid}.out/attach/c/img.png\",\"mime\":\"image/png\",\"bytes\":3}}]}}\n"
            )
        };
        let done = "{\"kind\":\"done\",\"ts\":3,\"status\":\"ok\",\"turns\":1}\n";
        std::fs::write(dir.join("closed.jsonl"), [part("closed"), done.into()].concat()).unwrap();
        std::fs::write(dir.join("open.jsonl"), part("open")).unwrap();
        let alive = scan(&dir).unwrap();
        assert!(alive.open.contains("open"));
        assert!(!alive.open.contains("closed"));
        assert!(alive.attach.contains("open.out/attach/c/img.png"));
        assert!(
            !alive.attach.contains("closed.out/attach/c/img.png"),
            "вложение закрытой сессии осталось живым"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
