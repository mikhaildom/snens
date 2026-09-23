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
    /// Usage-строка хелпа несёт признак «нужен stdin»: тогда определение тула
    /// помечает stdin обязательным, и модель узнаёт контракт заранее, а не
    /// тратит ход на exit=1 при первом вызове.
    pub uses_stdin: bool,
}

pub async fn discover(cfg: &Config) -> Vec<Tool> {
    let dir = cfg.tools_path();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return out;
    };
    let mut dirs: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    for d in dirs {
        let Some(name) = d.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        let bin = d.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        if !bin.is_file() {
            eprintln!(
                "[snens] тул {name}: нет исполняемого файла {}",
                bin.display()
            );
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
        let uses_stdin = stdin_required(&summary, &usage);
        out.push(Tool {
            name,
            bin,
            summary,
            usage,
            uses_stdin,
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

/// Нужен ли тулу stdin — видно из сути и usage-строки хелпа (`<путь>   — JSON на
/// stdin`, `содержимое берётся со stdin`). Признак ставит сам тул, и он же в
/// `--help`: отдельной схемы на тип тула не заводим.
fn stdin_required(summary: &str, usage: &str) -> bool {
    summary.contains("stdin") || usage.contains("stdin")
}

/// Одна generic-схема на все тулы: аргументы — argv, документация — хелп.
pub fn definitions(tools: &[Tool]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            let mut desc = format!("{}\n{}", t.summary, t.usage);
            // Требование stdin доезжает до модели из описания, а не из exit=1
            // на первом вызове: это часть контракта тула, не сюрприз.
            let required: Vec<&str> = if t.uses_stdin {
                desc.push_str("\nstdin обязателен: JSON правки / содержимое файла.");
                vec!["args", "stdin"]
            } else {
                vec!["args"]
            };
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": desc,
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
                        "required": required
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
    // Каталог наш; чистим только то, что сами и создали — на unix симлинки,
    // на Windows копии (симлинк там требует прав).
    let names: Vec<String> = tools
        .iter()
        .map(|t| format!("{}{}", t.name, std::env::consts::EXE_SUFFIX))
        .collect();
    for e in std::fs::read_dir(&bin)?.flatten() {
        let p = e.path();
        let own_link = p
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        let own_copy = !own_link
            && p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| !names.iter().any(|x| x == n))
                .unwrap_or(false);
        if own_link || own_copy {
            let _ = std::fs::remove_file(&p);
        }
    }
    for (t, name) in tools.iter().zip(&names) {
        let to = bin.join(name);
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(&t.bin, &to);
        #[cfg(not(unix))]
        std::fs::copy(&t.bin, &to)?;
    }
    Ok(bin)
}

/// PATH детей: разделитель путей свой у каждой платформы, склеиваем им. Каталог
/// самого харнесса тоже в PATH — иначе изнутри сессии не позвать `snens sessions`.
pub fn child_path(bin: &Path) -> String {
    let mut parts = vec![bin.to_path_buf()];
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        parts.push(dir);
    }
    if let Some(cur) = std::env::var_os("PATH") {
        parts.extend(std::env::split_paths(&cur));
    }
    std::env::join_paths(parts)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}
