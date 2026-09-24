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
        /// Сессия-родитель, если это подагент. Пусто у корневой сессии.
        /// Старые логи читаются как есть (serde default).
        #[serde(default)]
        parent: String,
    },
    /// Системный промпт сессии. Собирается один раз в начале и дальше читается
    /// из лога: файлы инстанса (soul/user/memory, скиллы) правятся по ходу, и
    /// пересборка промахивалась бы мимо префикс-кеша на каждой правке.
    System {
        ts: u64,
        text: String,
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
        /// Рассуждение модели, если провайдер его отдал. В логе хранится целиком
        /// (это файл сессии, не только контекст), но в проекцию не уезжает —
        /// провайдеры reasoning обратно не принимают.
        #[serde(default)]
        reasoning: Option<String>,
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
        /// Вложения, которые тул положил в канал (`SNENS_ATTACH_DIR`). Пусто у
        /// текстовых тулов; старые логи читаются как есть (serde default).
        #[serde(default)]
        parts: Vec<Part>,
    },
    Usage {
        ts: u64,
        in_tok: u64,
        cached_tok: u64,
        out_tok: u64,
        cost_micro: u64,
        /// Идентификатор ответа провайдера (`gen-…`) и отпечаток бэкенда, если
        /// были. Старые логи читаются как есть (serde default). Нужны, чтобы
        /// отличить наш промах кеша от переключения апстрима роутером.
        #[serde(default)]
        upstream_id: String,
        #[serde(default)]
        fingerprint: String,
        /// `finish_reason` этого хода (`stop`/`tool_calls`/`length`/`content_filter`).
        /// Старые логи читаются как есть. Нужен, чтобы «модель закончила» отличалось
        /// от «ответ оборван лимитом токенов» при разборе сессии.
        #[serde(default)]
        finish_reason: String,
    },
    /// Что проекция сделала с контекстом. Лог остаётся неизменным: это журнал
    /// действий над ним, а не новая версия истории.
    Ctx {
        ts: u64,
        action: String,
        elided: usize,
        freed_bytes: usize,
    },
    /// Справка из долгой памяти, впрыснутая перед ходом. В логе, а не только на
    /// проводе: проекция обязана собираться байт-в-байт одинаково и после
    /// рестарта, а resume не должен впрыскивать её второй раз.
    Recall {
        ts: u64,
        text: String,
    },
    /// Сообщение от другой сессии (родитель, ребёнок, человек через steer),
    /// впрыснутое на границе хода. В проекции едет ролью `user` с пометкой
    /// отправителя: это чужое слово, а не ввод этой сессии.
    Steer {
        ts: u64,
        from: String,
        /// `steer` | `ask` | `report` — что это за слово. Имя поля не `kind`:
        /// `kind` занято тегом самой записи.
        via: String,
        text: String,
    },
    /// Терминатор сессии: пишет харнесс, а не модель, на всех путях выхода цикла.
    /// По нему родитель узнаёт, что ребёнок закончил, и с каким исходом —
    /// даже если модель «забыла» отчитаться.
    Done {
        ts: u64,
        status: String,
        turns: u32,
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

/// Вложение сообщения: файл в хранилище инстанса плюс то, что нужно, чтобы
/// собрать часть контента для провайдера. Вид (`kind`) — нейтральный, а не
/// провайдерский: мапинг в конкретный формат живёт в проекции.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Part {
    /// `image` | `audio` | `video` | `file` — расширяется без ломки старых логов.
    pub kind: String,
    /// Абсолютный путь в каталоге сессии: файл неизменяем, проекция детерминирована.
    pub path: String,
    pub mime: String,
    pub bytes: usize,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

pub struct Session {
    id: String,
    path: PathBuf,
    file: std::cell::RefCell<File>,
    lines: Cell<u64>,
    /// Сессия-родитель, если это подагент; читается из `Meta` при открытии.
    parent: String,
}

impl Session {
    pub fn open(dir: &Path, id: &str) -> R<Session> {
        Session::open_child(dir, id, "")
    }

    /// Открыть сессию с указанием родителя (для подагента). Родитель пишется в
    /// `Meta` один раз при создании лога и больше не меняется.
    pub fn open_child(dir: &Path, id: &str, parent: &str) -> R<Session> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{id}.jsonl"));
        let exists = path.exists();
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let lines = Cell::new(count_lines(&path)?.max(1));
        // Родитель неизменяем и живёт в `Meta`: при открытии существующего лога
        // берём его оттуда, а не из аргумента (resume подагента не должен терять связь).
        let parent = if exists {
            read_file(&path)
                .ok()
                .and_then(|rs| {
                    rs.into_iter().find_map(|r| match r {
                        Rec::Meta { parent, .. } => Some(parent),
                        _ => None,
                    })
                })
                .unwrap_or_default()
        } else {
            parent.to_string()
        };
        if !exists {
            let s = Session {
                id: id.to_string(),
                path,
                file: std::cell::RefCell::new(file),
                lines,
                parent: parent.clone(),
            };
            s.append(Rec::Meta {
                ts: now(),
                model: String::new(),
                parent,
            })?;
            return Ok(s);
        }
        let s = Session {
            id: id.to_string(),
            path,
            file: std::cell::RefCell::new(file),
            lines,
            parent,
        };
        Ok(s)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn parent(&self) -> &str {
        &self.parent
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
        let id = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
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
