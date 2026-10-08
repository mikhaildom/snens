// Сессия = append-only JSONL. Лог неизменяем, seq = номер строки.
// Промпт — проекция лога (project.rs), а не сам лог.
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};

use fs2::FileExt;
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
        /// Поверхность, создавшая сессию. Legacy-логи остаются без метки.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        surface: Option<String>,
    },
    /// Системный промпт сессии. Собирается один раз в начале и дальше читается
    /// из лога: файлы инстанса (soul/user/memory, скиллы) правятся по ходу, и
    /// пересборка промахивалась бы мимо префикс-кеша на каждой правке.
    System { ts: u64, text: String },
    User {
        ts: u64,
        text: String,
        source: String,
        #[serde(default)]
        parts: Vec<Part>,
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
        /// Токены рассуждения по счётчику провайдера. У моделей с зашифрованным
        /// reasoning это весь видимый след настоящей цены хода: текста рассуждения
        /// у нас нет, а `out_tok` показывает только короткий ответ. Старые логи
        /// читаются как есть.
        #[serde(default)]
        reason_tok: u64,
        /// Провайдер закрыл рассуждение: прислал блоб вместо текста. Старые логи
        /// читаются как есть.
        #[serde(default)]
        encrypted_reasoning: bool,
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
    Recall { ts: u64, text: String },
    /// Accepted working-context projection. Original events remain immutable.
    ContextNotice { ts: u64, text: String },
    ContextRevision {
        ts: u64,
        anchor: usize,
        digest: String,
        messages: Vec<serde_json::Value>,
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
    Done { ts: u64, status: String, turns: u32 },
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
    file: Mutex<File>,
    lines: AtomicU64,
    /// Сессия-родитель, если это подагент; читается из `Meta` при открытии.
    parent: String,
}

impl Session {
    pub fn open(dir: &Path, id: &str) -> R<Session> {
        Session::open_surface(dir, id, None)
    }

    /// Открыть новую сессию с меткой поверхности. На существующем логе метка
    /// не меняется: это метаданные создания, а не resume.
    pub fn open_surface(dir: &Path, id: &str, surface: Option<&str>) -> R<Session> {
        Session::open_child_surface(dir, id, "", surface)
    }

    /// Открыть сессию с указанием родителя (для подагента). Родитель пишется в
    /// `Meta` один раз при создании лога и больше не меняется.
    pub fn open_child(dir: &Path, id: &str, parent: &str) -> R<Session> {
        Session::open_child_surface(dir, id, parent, None)
    }

    pub fn open_child_surface(
        dir: &Path,
        id: &str,
        parent: &str,
        surface: Option<&str>,
    ) -> R<Session> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{id}.jsonl"));
        let exists = path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)?;
        file.try_lock_exclusive().map_err(|e| {
            format!("session {id} is already open for writing (possibly by surface API); stop that writer before resuming: {e}")
        })?;
        recover_tail(&mut file, &path)?;
        let lines = AtomicU64::new(count_lines(&path)?.max(1));
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
                file: Mutex::new(file),
                lines,
                parent: parent.clone(),
            };
            s.append(Rec::Meta {
                ts: now(),
                model: String::new(),
                parent,
                surface: surface.map(str::to_owned),
            })?;
            return Ok(s);
        }
        let s = Session {
            id: id.to_string(),
            path,
            file: Mutex::new(file),
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
        let mut f = self.file.lock().map_err(|_| "session log lock poisoned")?;
        writeln!(f, "{line}")?;
        f.sync_data()?;
        let n = self.lines.fetch_add(1, Ordering::Relaxed);
        Ok(n)
    }

    pub fn read(&self) -> R<Vec<Rec>> {
        read_file(&self.path)
    }
}

/// With the writer lock held, validate complete records and repair only an
/// incomplete final line. A valid unterminated JSON record is preserved.
fn recover_tail(file: &mut File, path: &Path) -> R<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let complete_end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);

    for (i, line) in bytes[..complete_end].split(|b| *b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        serde_json::from_slice::<Rec>(line).map_err(|e| {
            format!(
                "corrupt session log {} at complete line {}: {e}",
                path.display(),
                i + 1
            )
        })?;
    }

    if complete_end < bytes.len() {
        let tail = &bytes[complete_end..];
        if serde_json::from_slice::<Rec>(tail).is_ok() {
            file.seek(SeekFrom::End(0))?;
            file.write_all(b"\n")?;
        } else {
            file.set_len(complete_end as u64)?;
        }
        file.sync_data()?;
    }
    Ok(())
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

#[cfg(test)]
mod writer_safety_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("snens-log-test-{}-{nonce}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn user(text: &str) -> Rec {
        Rec::User {
            ts: 1,
            text: text.to_string(),
            source: "test".to_string(),
            parts: vec![],
        }
    }

    #[test]
    fn subprocess_writer_observes_lifetime_lock() {
        let Some(dir) = std::env::var_os("SNENS_LOG_LOCK_TEST_DIR") else {
            return;
        };
        let error = Session::open(Path::new(&dir), "lock").err().unwrap();
        assert!(error.to_string().contains("already open for writing"));
    }

    #[test]
    fn writer_lock_is_exclusive_read_only_access_works_and_drop_releases() {
        let dir = TempDir::new();
        let session = Session::open(&dir.0, "lock").unwrap();
        let path = session.path().to_path_buf();
        let second = Session::open(&dir.0, "lock");
        assert!(second.err().unwrap().to_string().contains("surface API"));
        assert_eq!(read_file(&path).unwrap().len(), 1);
        assert_eq!(list(&dir.0).unwrap(), vec!["lock"]);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("log::writer_safety_tests::subprocess_writer_observes_lifetime_lock")
            .arg("--nocapture")
            .env("SNENS_LOG_LOCK_TEST_DIR", &dir.0)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "subprocess failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        drop(session);
        let reopened = Session::open(&dir.0, "lock").unwrap();
        reopened.append(user("after release")).unwrap();
        assert_eq!(reopened.read().unwrap().len(), 2);
    }

    #[test]
    fn valid_unterminated_tail_is_preserved_and_separated_before_append() {
        let dir = TempDir::new();
        let path = dir.0.join("tail.jsonl");
        let rec = serde_json::to_vec(&user("preserve me")).unwrap();
        std::fs::write(&path, &rec).unwrap();
        let session = Session::open(&dir.0, "tail").unwrap();
        session.append(user("new record")).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.windows(2).any(|w| w == b"}\n"));
        let records = read_file(&path).unwrap();
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn malformed_unterminated_tail_is_truncated_before_append() {
        let dir = TempDir::new();
        let path = dir.0.join("tail.jsonl");
        let valid = serde_json::to_vec(&user("keep")).unwrap();
        let mut contents = valid;
        contents.extend_from_slice(b"\n{\"kind\":");
        std::fs::write(&path, contents).unwrap();
        let session = Session::open(&dir.0, "tail").unwrap();
        session.append(user("next")).unwrap();
        let records = read_file(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert!(matches!(&records[0], Rec::User { text, .. } if text == "keep"));
    }

    #[test]
    fn surface_marker_is_written_only_for_new_logs_and_legacy_meta_defaults() {
        let dir = TempDir::new();
        let session = Session::open_surface(&dir.0, "api", Some("telegram")).unwrap();
        let records = session.read().unwrap();
        assert!(
            matches!(&records[0], Rec::Meta { surface: Some(surface), .. } if surface == "telegram")
        );
        drop(session);

        let legacy = dir.0.join("legacy.jsonl");
        std::fs::write(
            &legacy,
            b"{\"kind\":\"meta\",\"ts\":1,\"model\":\"\",\"parent\":\"\"}\n",
        )
        .unwrap();
        let session = Session::open_surface(&dir.0, "legacy", Some("telegram")).unwrap();
        assert!(matches!(
            &session.read().unwrap()[0],
            Rec::Meta { surface: None, .. }
        ));
    }

    #[test]
    fn corruption_in_complete_middle_record_rejects_writer_open() {
        let dir = TempDir::new();
        let path = dir.0.join("bad.jsonl");
        std::fs::write(&path, b"{}\nnot-json\n{}\n").unwrap();
        assert!(Session::open(&dir.0, "bad").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{}\nnot-json\n{}\n");
    }
}
