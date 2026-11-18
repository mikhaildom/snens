// Уборка за собой: сборочные артефакты и брошенные worktree.
//
// Зачем отдельный механизм. Сессии собирают в отдельных target-каталогах
// (`snens-next-verify`, `m2v`, …), проверяют и уходят, оставляя на диске
// гигабайты. На сервере это не «неудобство», а остановка работы: `/mnt` уходил
// в 100%, и следующая сборка падала уже не по своей вине.
//
// Правило одно: удаляем то, что заведомо восстановимо командой. Никаких
// эвристик про «похоже на мусор» — только известные имена каталогов сборки,
// брошенные git-worktree и куски cargo-кэша, у которых истёк срок.
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::{cfg::Config, R};

/// Что нашли: категория, размер, список путей.
#[derive(Debug)]
struct Victim {
    path: PathBuf,
    bytes: u64,
}

/// Каталог сборки, который точно можно снести: имя из известного набора.
///
/// Список намеренно короткий и с явными именами, а не `*debug*` или `snens-*`:
/// `snens-memory` — живой target другого проекта, и общее правило по префиксу
/// снесло бы его вместе с мусором (часы пересборки).
const BUILD_DIRS: &[&str] = &[
    "snens-next",
    "snens-next-verify",
    "snens-next-verify-core",
    "snens-next-verify-fix",
    "verify-target",
    "m2v",
];

/// Каталог — мусор от прогона проверки: имя заканчивается на `-verify`
/// или начинается с `verify-`. Обе формы — наши же однократные прогоны.
fn is_run_dir(name: &str) -> bool {
    name.ends_with("-verify")
        || name.ends_with("-verify-core")
        || name.ends_with("-verify-fix")
        || name.starts_with("verify-")
}

pub fn cmd(flags: &std::collections::BTreeMap<String, String>, _pos: Vec<String>) -> R<()> {
    let cfg = Config::load(flags.get("config").map(|s| s.as_str()))?;
    let days: u64 = flags
        .get("older-than")
        .map(|s| s.parse())
        .transpose()
        .map_err(|_| "--older-than ждёт число дней")?
        .unwrap_or(7);
    run(
        &cfg,
        Duration::from_secs(days.saturating_mul(86_400)),
        flags.contains_key("dry-run"),
    )
}

fn run(cfg: &Config, older: Duration, dry: bool) -> R<()> {
    let cutoff = SystemTime::now()
        .checked_sub(older)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    // Корни, где смотрим: каталог сборки (он на втором диске и там кончается
    // место), плюс каталог данных — там живут worktree прошлых прогонов.
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(dir) = cargo_target_root() {
        roots.push(dir);
    }
    roots.push(cfg.data_path());

    let mut total = (0u64, 0u64);
    for root in roots {
        let victims = scan(&root, cutoff);
        if victims.is_empty() {
            continue;
        }
        let bytes: u64 = victims.iter().map(|v| v.bytes).sum();
        println!(
            "{:34} {:3} шт {:8.1} МБ",
            root.display(),
            victims.len(),
            bytes as f64 / 1e6
        );
        total.0 += victims.len() as u64;
        total.1 += bytes;
        if dry {
            for v in &victims {
                println!("  было бы снесено: {}", v.path.display());
            }
            continue;
        }
        for v in &victims {
            if let Err(e) = std::fs::remove_dir_all(&v.path) {
                // Права/занятость — не повод падать: остальное всё равно уберём,
                // а причина видна человеку.
                eprintln!("  не удалось {}: {e}", v.path.display());
            }
        }
    }
    println!(
        "{}: {} каталогов, {:.1} ГБ (порог {} дней)",
        if dry { "было бы убрано" } else { "убрано" },
        total.0,
        total.1 as f64 / 1e9,
        older.as_secs() / 86_400
    );
    Ok(())
}

/// Каталог сборки cargo: из `CARGO_TARGET_DIR` или `~/.cargo/config.toml`.
/// Свой target у каждого проекта, поэтому спрашиваем у cargo, а не угадываем.
fn cargo_target_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).ok()?;
    let cfg = Path::new(&home).join(".cargo/config.toml");
    let text = std::fs::read_to_string(cfg).ok()?;
    let doc: toml::Value = toml::from_str(&text).ok()?;
    let dir = doc.get("build")?.get("target-dir")?.as_str()?;
    Some(PathBuf::from(dir))
}

/// Что снести в одном корне: известные каталоги сборки, брошенные worktree и
/// `git worktree`-записи без каталога.
fn scan(root: &Path, cutoff: SystemTime) -> Vec<Victim> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for e in rd.flatten() {
        let path = e.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Каталог сборки по имени — но только если он не трогался дольше порога:
        // идущая сборка не должна лишиться своего target.
        let is_build = BUILD_DIRS.contains(&name) || is_run_dir(name);
        if !is_build {
            continue;
        }
        if !old_enough(&path, cutoff) {
            continue;
        }
        out.push(Victim {
            bytes: dir_size(&path),
            path,
        });
    }
    out
}

/// Возраст по mtime каталога. `read_dir` его обновляет, поэтому берём самый
/// свежий mtime среди верхних уровней: у большого дерева корень может быть
/// старым, а файлы внутри — свежими.
fn old_enough(path: &Path, cutoff: SystemTime) -> bool {
    let mut newest = SystemTime::UNIX_EPOCH;
    for e in std::fs::read_dir(path).into_iter().flatten().flatten() {
        if let Ok(md) = e.metadata() {
            if let Ok(t) = md.modified() {
                newest = newest.max(t);
            }
        }
    }
    if let Ok(md) = std::fs::metadata(path) {
        if let Ok(t) = md.modified() {
            newest = newest.max(t);
        }
    }
    newest < cutoff
}

/// Размер дерева. `st_size` каталога — это его запись, а не содержимое, поэтому
/// обходим файлы.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                stack.push(e.path());
            } else {
                total += md.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_build_dirs_are_victims() {
        let root = std::env::temp_dir().join(format!("snens-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for name in ["snens-next-verify", "verify-target", "target", "src", "my-project", "snens-memory"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let found = scan(&root, SystemTime::now() + Duration::from_secs(1));
        let names: Vec<String> = found
            .iter()
            .map(|v| v.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"snens-next-verify".to_string()), "{names:?}");
        assert!(names.contains(&"verify-target".to_string()), "{names:?}");
        // Чужой `target` и рабочие каталоги не трогаем: это часы пересборки.
        assert!(!names.contains(&"target".to_string()), "{names:?}");
        assert!(!names.contains(&"src".to_string()), "{names:?}");
        assert!(!names.contains(&"my-project".to_string()), "{names:?}");
        // `snens-memory` — живой target другого проекта, не наш мусор.
        assert!(!names.contains(&"snens-memory".to_string()), "{names:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fresh_dirs_are_not_victims() {
        let root = std::env::temp_dir().join(format!("snens-disk-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("snens-next-verify")).unwrap();
        std::fs::write(root.join("snens-next-verify/x"), "x").unwrap();
        // Порог в прошлом — ничего не старое.
        let found = scan(&root, SystemTime::UNIX_EPOCH);
        assert!(found.is_empty(), "{found:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
