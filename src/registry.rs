// Реестр тулов: каталог = тул, файл с тем же именем внутри — исполняемый.
// Определение тула для модели — его собственный `--help`, поэтому контракт жёсткий:
// строка 1 — суть, строка 2 — usage, дальше подробности.
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::{cfg::Config, exec, R};

pub struct Tool {
    pub name: String,
    pub bin: PathBuf,
    pub summary: String,
    pub usage: String,
}

pub async fn discover(cfg: &Config) -> Vec<Tool> {
    let dir = cfg.tools_path();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return out;
    };
    let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();

    for d in dirs {
        let Some(name) = d.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        let bin = d.join(&name);
        if !bin.is_file() {
            eprintln!("[snens] тул {name}: нет исполняемого файла {}", bin.display());
            continue;
        }
        let bin_s = bin.to_string_lossy().into_owned();
        let mut sp = exec::Spawn::argv(&bin_s, vec!["--help".into()]);
        sp.timeout_s = 5;
        sp.cap = 4000;
        let (summary, usage) = match exec::spawn(sp).await {
            Ok(o) if o.exit == 0 => split_help(&o.text),
            Ok(o) => {
                eprintln!("[snens] тул {name}: --help вернул код {}", o.exit);
                continue;
            }
            Err(e) => {
                eprintln!("[snens] тул {name}: {e}");
                continue;
            }
        };
        if summary.is_empty() {
            eprintln!("[snens] тул {name}: пустой --help");
            continue;
        }
        out.push(Tool {
            name,
            bin,
            summary,
            usage,
        });
    }
    out
}

/// Первая непустая строка — суть, вторая — usage.
fn split_help(text: &str) -> (String, String) {
    let mut it = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let summary = it.next().unwrap_or_default().to_string();
    let usage = it.next().unwrap_or_default().to_string();
    (summary, usage)
}

/// Одна generic-схема на все тулы: аргументы — argv, документация — хелп.
pub fn definitions(tools: &[Tool]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": format!("{}\n{}", t.summary, t.usage),
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "args": {
                                "type": ["array", "string"],
                                "items": {"type": "string"},
                                "description": "Аргументы argv. Предпочитай массив: строка разбивается по пробелам и пробел внутри аргумента так не передать"
                            },
                            "stdin": {"type": "string", "description": "Что подать на stdin"},
                            "timeout_seconds": {"type": "integer"}
                        },
                        "required": ["args"]
                    }
                }
            })
        })
        .collect()
}

/// Симлинки в один каталог: так тулы попадают в PATH и харнесса, и человека.
/// Каталог наш, но чужие файлы не трогаем — удаляем только симлинки.
pub fn sync_bin_dir(cfg: &Config, tools: &[Tool]) -> R<PathBuf> {
    let bin = cfg.bin_path();
    std::fs::create_dir_all(&bin)?;
    for e in std::fs::read_dir(&bin)?.flatten() {
        let p = e.path();
        let is_link = p
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if is_link {
            let _ = std::fs::remove_file(&p);
        }
    }
    for t in tools {
        std::os::unix::fs::symlink(&t.bin, bin.join(&t.name))?;
    }
    Ok(bin)
}

pub fn child_path(bin: &Path) -> String {
    let cur = std::env::var("PATH").unwrap_or_default();
    format!("{}:{}", bin.display(), cur)
}
