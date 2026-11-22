//! Встроенные скиллы: что поставка даёт инстансу из коробки.
//!
//! Скилл — данные, а не код: каталог с `SKILL.md` в `skills_dir`, чьё оглавление
//! (`name` + `description`) уезжает в промпт. Но до сих пор поставка не содержала
//! ни одного скилла: на чистой машине каталог был пуст, и знание вроде «как
//! разбирать тикеты» приходилось заводить руками в каждом инстансе.
//!
//! Здесь скиллы вкомпилированы (`include_str!`) и раскладываются на первом
//! запуске. Правило одно и главное: **чужое не перезаписываем**. Если в
//! `skills_dir` уже есть хоть один `SKILL.md`, установка не трогает ничего —
//! у инстанса могут быть свои версии, и молча затереть их правки нельзя.
//! Явно обновить — `snens skills install --force <имя>`.

use crate::{cfg::Config, R};
use std::path::Path;

/// Скилл поставки: имя каталога и тело `SKILL.md`.
pub struct Bundled {
    pub name: &'static str,
    pub body: &'static str,
}

/// Скиллы, которые получает каждый инстанс.
///
/// Держим список коротким и по делу: оглавление едет в префикс на каждом ходу,
/// поэтому лишний скилл — это налог на все сессии, а не «на всякий случай».
pub const BUNDLED: &[Bundled] = &[
    Bundled {
        name: "tickets",
        body: include_str!("../skills/tickets/SKILL.md"),
    },
    // Ревью кода по методике open-code-review: с `ocr`, если он есть, иначе git.
    Bundled {
        name: "code-review",
        body: include_str!("../skills/code-review/SKILL.md"),
    },
];

/// `name` и `description` из YAML front-matter — для справки и для теста,
/// что у встроенного скилла они есть.
pub fn frontmatter(text: &str) -> (String, String) {
    let (mut name, mut desc) = (String::new(), String::new());
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (name, desc);
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        if let Some(v) = line.strip_prefix("name:") {
            name = v.trim().trim_matches('"').to_string();
        } else if let Some(v) = line.strip_prefix("description:") {
            desc = v.trim().trim_matches('"').to_string();
        }
    }
    (name, desc)
}

/// Разложить встроенные скиллы, если каталог пуст. Возвращает имена записанных.
///
/// Условие «пуст» проверяется по любому `SKILL.md` внутри `skills_dir`: один
/// установленный скилл — признак, что инстанс ведёт каталог сам.
pub fn seed_if_empty(cfg: &Config) -> R<Vec<&'static str>> {
    let dir = cfg.skills_path();
    if has_any_skill(&dir) {
        return Ok(Vec::new());
    }
    let mut written = Vec::new();
    for b in BUNDLED {
        write_skill(&dir, b, false)?;
        written.push(b.name);
    }
    Ok(written)
}

/// Есть ли в каталоге хоть один скилл.
fn has_any_skill(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    rd.flatten()
        .any(|e| e.path().join("SKILL.md").is_file())
}

/// Записать один встроенный скилл. `force` разрешает затереть существующий.
fn write_skill(dir: &Path, b: &Bundled, force: bool) -> R<()> {
    let path = dir.join(b.name).join("SKILL.md");
    if path.exists() && !force {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, b.body)?;
    Ok(())
}

/// `snens skills` — что есть в поставке и что уже лежит у инстанса.
pub fn cmd(flags: &std::collections::BTreeMap<String, String>, positional: Vec<String>) -> R<()> {
    let cfg = Config::load(flags.get("config").map(String::as_str))?;
    let dir = cfg.skills_path();
    let sub = positional.first().map(String::as_str).unwrap_or("");
    match sub {
        "install" => {
            // Имена позиционно после подкоманды; без имён — все встроенные.
            let names: Vec<&String> = positional.iter().skip(1).collect();
            let force = flags.contains_key("force");
            let mut installed = Vec::new();
            for b in BUNDLED {
                let wanted = names.is_empty()
                    || names.iter().any(|n| n.as_str() == b.name);
                if !wanted {
                    continue;
                }
                let path = dir.join(b.name).join("SKILL.md");
                let existed = path.exists();
                write_skill(&dir, b, force)?;
                installed.push(match (existed, force) {
                    (true, true) => format!("{} (перезаписан)", b.name),
                    (true, false) => format!("{} (уже есть, оставлен как есть)", b.name),
                    _ => b.name.to_string(),
                });
            }
            if installed.is_empty() {
                return Err(format!(
                    "нет такого встроенного скилла: {}; есть: {}",
                    names.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
                    BUNDLED.iter().map(|b| b.name).collect::<Vec<_>>().join(", ")
                )
                .into());
            }
            for row in installed {
                println!("{row}");
            }
            println!("\nкаталог: {}", dir.display());
            Ok(())
        }
        "" => {
            for b in BUNDLED {
                let (_, desc) = frontmatter(b.body);
                let path = dir.join(b.name).join("SKILL.md");
                let state = if !path.exists() {
                    "не установлен".to_string()
                } else {
                    match std::fs::read_to_string(&path) {
                        // Тот же файл — сравнение по телу: правка инстанса видна.
                        Ok(text) if text == b.body => "установлен".to_string(),
                        Ok(_) => "установлен, изменён локально".to_string(),
                        Err(_) => "не читается".to_string(),
                    }
                };
                println!("{} | {state} | {desc}", b.name);
            }
            println!("\nкаталог: {}", dir.display());
            println!("поставить: snens skills install [--force] [<имя>…]");
            Ok(())
        }
        other => Err(format!("неизвестная подкоманда skills: {other}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(dir: &Path) -> Config {
        let mut cfg = Config::default();
        cfg.skills_dir = dir.display().to_string();
        cfg
    }

    #[test]
    fn bundled_skills_have_frontmatter() {
        for b in BUNDLED {
            let (name, desc) = frontmatter(b.body);
            assert_eq!(name, b.name, "name в front-matter не совпал с каталогом");
            assert!(desc.len() > 20, "{}: пустое description", b.name);
        }
    }

    #[test]
    fn seed_writes_into_empty_dir_and_never_overwrites() {
        let root = std::env::temp_dir().join(format!("snens-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cfg = cfg_with(&root);
        let written = seed_if_empty(&cfg).unwrap();
        assert_eq!(written.len(), BUNDLED.len());
        let path = root.join("tickets/SKILL.md");
        assert!(path.is_file());

        // Инстанс правит скилл — повторная установка не имеет права затереть.
        std::fs::write(&path, "моя версия").unwrap();
        assert!(seed_if_empty(&cfg).unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "моя версия");
        let _ = std::fs::remove_dir_all(&root);
    }
}
