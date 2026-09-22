// Сессия = append-only JSONL. Лог неизменяем, seq = номер строки.
// Промпт — проекция лога (project.rs), а не сам лог.
use std::{
    cell::Cell,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::R;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Rec {
    Meta {
        ts: u64,
        model: String,
    },
    User {
        ts: u64,
        text: String,
        source: String,
    },
    Assistant {
        ts: u64,
        text: Option<String>,
        tool_calls: Vec<Call>,
    },
    ToolResult {
        ts: u64,
        call_id: String,
        tool: String,
        argv: Vec<String>,
        exit: i32,
        bytes: usize,
        ms: u64,
        body: Body,
    },
    Usage {
        ts: u64,
        in_tok: u64,
        cached_tok: u64,
        out_tok: u64,
        cost_micro: u64,
    },
    /// Что проекция сделала с контекстом. Лог остаётся неизменным: это журнал
    /// действий над ним, а не новая версия истории.
    Ctx {
        ts: u64,
        action: String,
        elided: usize,
        freed_bytes: usize,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Call {
    pub id: String,
    pub name: String,
    /// Аргументы как есть, строкой JSON — так их и отдаёт модель.
    pub args: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Body {
    Inline { text: String },
}

pub struct Session {
    path: PathBuf,
    file: std::cell::RefCell<File>,
    lines: Cell<u64>,
}

impl Session {
    pub fn open(dir: &Path, id: &str) -> R<Session> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{id}.jsonl"));
        let exists = path.exists();
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let lines = Cell::new(count_lines(&path)?.max(1));
        let s = Session {
            path,
            file: std::cell::RefCell::new(file),
            lines,
        };
        if !exists {
            s.append(Rec::Meta {
                ts: now(),
                model: String::new(),
            })?;
        }
        Ok(s)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Дописать запись и сбросить на диск. Append + fsync на границе записи —
    /// вся история восстановления; недописанный хвост отбрасывается при чтении.
    pub fn append(&self, rec: Rec) -> R<u64> {
        let line = serde_json::to_string(&rec)?;
        let mut f = self.file.borrow_mut();
        writeln!(f, "{line}")?;
        f.sync_data()?;
        let n = self.lines.get();
        self.lines.set(n + 1);
        Ok(n)
    }

    pub fn read(&self) -> R<Vec<Rec>> {
        read_file(&self.path)
    }
}

pub fn read_file(path: &Path) -> R<Vec<Rec>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let f = File::open(path)?;
    let mut out = Vec::new();
    for (i, line) in BufReader::new(f).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Rec>(&line) {
            Ok(r) => out.push(r),
            // Битая последняя строка — оборванная запись, а не порча данных.
            Err(e) => {
                if i as u64 + 1 == count_lines(path)? {
                    continue;
                }
                eprintln!("[log] строка {} пропущена: {e}", i + 1);
            }
        }
    }
    Ok(out)
}

fn count_lines(path: &Path) -> R<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let f = File::open(path)?;
    Ok(BufReader::new(f).lines().count() as u64)
}

pub fn latest(dir: &Path) -> R<String> {
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let mt = entry.metadata().and_then(|m| m.modified())?;
        let id = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
        if best.as_ref().map(|(t, _)| mt > *t).unwrap_or(true) {
            best = Some((mt, id));
        }
    }
    best.map(|(_, id)| id)
        .ok_or_else(|| format!("сессий нет в {}", dir.display()).into())
}

pub fn list(dir: &Path) -> R<Vec<String>> {
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            if let Some(s) = p.file_stem().and_then(|s| s.to_str()) {
                ids.push(s.to_string());
            }
        }
    }
    ids.sort();
    Ok(ids)
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Короткий id сессии: время + pid, читаемо и без коллизий на практике.
pub fn new_id() -> String {
    format!("{}-{}", now(), std::process::id() % 100000)
}