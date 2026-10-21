use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{Arc, Mutex as StdMutex},
};
use tokio::sync::{Mutex, broadcast, mpsc, watch};

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Session {
    pub id: String,
    pub surface: String,
    /// Человекочитаемый заголовок. Сгенерированный (моделью или ядром) лежит в
    /// `Meta.title`; пока его нет, discovery подставляет первые слова первого
    /// сообщения пользователя. Пусто — ни того, ни другого.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Время последней активности (unix, сек): `mtime` лога сессии. Список
    /// сортируется по нему — поверхность и порядок id тут не при чём.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
    pub state: String,
    pub run_id: Option<String>,
    /// Экономический бюджет уборки контекста для этой сессии; пусто — общий
    /// из конфига. Нужен поверхностям с иной ценой контекста: групповой чат
    /// живёт долго и просит 100k, тогда как TUI-сессии держат свои 500k.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_budget: Option<u64>,
    /// Цель после уборки для этой сессии; пусто — общая из конфига.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_target: Option<u64>,
}
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Event {
    pub seq: u64,
    pub session_id: String,
    pub run_id: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub fields: serde_json::Value,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub command_id: String,
    pub text: String,
    #[serde(default)]
    pub attachment_ids: Vec<String>,
    pub state: String,
    pub run_id: Option<String>,
}
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
pub struct Attachment {
    pub attachment_id: String,
    pub command_id: String,
    pub filename: String,
    pub name: String,
    pub part: crate::log::Part,
    pub sha256: String,
}
#[derive(Default, Serialize, Deserialize)]
pub struct Store {
    pub sessions: BTreeMap<String, Session>,
    pub events: BTreeMap<String, Vec<Event>>,
    pub messages: BTreeMap<String, Vec<Message>>,
    #[serde(default)]
    pub attachments: BTreeMap<String, BTreeMap<String, Attachment>>,
    pub commands: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub surface_tools: BTreeMap<String, Vec<serde_json::Value>>,
    #[serde(default)]
    pub tool_results: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub tool_requests: BTreeMap<String, serde_json::Value>,
    /// Сколько ходов было в сессии на момент последнего запуска авторевью.
    /// Нужно, чтобы ревью не повторялось, пока не наберётся ещё `review_min_turns`.
    #[serde(default)]
    pub reviews: BTreeMap<String, u64>,
}
impl Session {
    /// Новая сессия поверхности без per-session бюджета: пороги берутся из конфига.
    pub fn new(id: impl Into<String>, surface: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            surface: surface.into(),
            title: None,
            updated_at: None,
            state: "idle".into(),
            run_id: None,
            context_budget: None,
            context_target: None,
        }
    }
}

pub struct Control {
    pub cancel: watch::Sender<bool>,
    pub queued: mpsc::UnboundedSender<crate::agent::QueuedInput>,
}
pub struct Runtime {
    pub data: Mutex<Store>,
    pub db: StdMutex<Connection>,
    pub tx: broadcast::Sender<Event>,
    /// Снимок конфига. Модель здесь — только стартовая: `agent::run_loop`
    /// перечитывает её из файла на каждом ходу, поэтому правка конфига во время
    /// жизни сервера видна новым ходам без перезапуска.
    pub cfg: crate::cfg::Config,
    pub llm: Arc<crate::llm::OpenAi>,
    pub locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    pub controls: Mutex<BTreeMap<String, Control>>,
    pub pending_tools:
        Mutex<BTreeMap<String, tokio::sync::oneshot::Sender<crate::agent::SurfaceToolResult>>>,
    /// Exclusive JSONL writer handles. The log lock is held for each Session lifetime.
    pub writers: Mutex<BTreeMap<String, Arc<crate::log::Session>>>,
    pub event_seq: Mutex<BTreeMap<String, u64>>,
    /// Поколение работы сессии: растёт на каждом старте хода. Отложенный таймер
    /// авторевью запоминает поколение и отменяется, если оно изменилось — тогда
    /// за время простоя началась новая работа и ревью ждёт следующей паузы.
    pub review_gen: Mutex<BTreeMap<String, u64>>,
    /// Ожидающие решения заявки на привилегированное действие: id → канал ответа.
    /// Живут только в памяти: заявка одноразовая и короткоживущая, рестарт её
    /// отменяет (шим получит отказ).
    pub approvals: Mutex<BTreeMap<String, tokio::sync::oneshot::Sender<bool>>>,
}
impl Runtime {
    pub fn persist(&self, data: &Store) -> crate::R<()> {
        persist(&self.db.lock().unwrap(), data)
    }
    pub async fn emit(
        &self,
        id: &str,
        run: Option<String>,
        kind: &str,
        fields: serde_json::Value,
    ) -> crate::R<Event> {
        let mut d = self.data.lock().await;
        let seq = {
            let mut sequences = self.event_seq.lock().await;
            let next = sequences.get(id).copied().unwrap_or_else(|| {
                d.events
                    .get(id)
                    .and_then(|events| events.iter().map(|event| event.seq).max())
                    .unwrap_or(0)
            }) + 1;
            sequences.insert(id.to_string(), next);
            let db = self.db.lock().unwrap();
            db.execute("INSERT INTO event_sequences(session_id,seq) VALUES(?1,?2) ON CONFLICT(session_id) DO UPDATE SET seq=excluded.seq", params![id, next as i64])?;
            next
        };
        let e = Event {
            seq,
            session_id: id.into(),
            run_id: run,
            kind: kind.into(),
            fields,
        };
        if kind != "assistant.delta" {
            d.events.entry(id.into()).or_default().push(e.clone());
            self.persist(&d)?;
        }
        let _ = self.tx.send(e.clone());
        Ok(e)
    }
    pub async fn session_lock(&self, id: &str) -> Arc<Mutex<()>> {
        let mut m = self.locks.lock().await;
        m.entry(id.into())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

fn schema(c: &Connection) -> crate::R<()> {
    c.execute_batch("PRAGMA foreign_keys=ON;
      CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY,surface TEXT NOT NULL,state TEXT NOT NULL,run_id TEXT,context_budget INTEGER,context_target INTEGER);
      CREATE TABLE IF NOT EXISTS messages(session_id TEXT NOT NULL,id TEXT NOT NULL,command_id TEXT NOT NULL UNIQUE,seq INTEGER NOT NULL,text TEXT NOT NULL,state TEXT NOT NULL,run_id TEXT,PRIMARY KEY(session_id,id),FOREIGN KEY(session_id) REFERENCES sessions(id));
      CREATE TABLE IF NOT EXISTS attachments(session_id TEXT NOT NULL,id TEXT NOT NULL,command_id TEXT NOT NULL,filename TEXT NOT NULL,name TEXT NOT NULL,part TEXT NOT NULL,sha256 TEXT NOT NULL,PRIMARY KEY(session_id,id),FOREIGN KEY(session_id) REFERENCES sessions(id));
      CREATE UNIQUE INDEX IF NOT EXISTS attachments_command ON attachments(session_id,command_id);
      CREATE TABLE IF NOT EXISTS message_attachments(session_id TEXT NOT NULL,message_id TEXT NOT NULL,attachment_id TEXT NOT NULL,seq INTEGER NOT NULL,PRIMARY KEY(session_id,message_id,attachment_id),FOREIGN KEY(session_id,message_id) REFERENCES messages(session_id,id),FOREIGN KEY(session_id,attachment_id) REFERENCES attachments(session_id,id));
      CREATE TABLE IF NOT EXISTS commands(command_id TEXT PRIMARY KEY,payload TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS surface_tools(session_id TEXT PRIMARY KEY,definitions TEXT NOT NULL,FOREIGN KEY(session_id) REFERENCES sessions(id));
      CREATE TABLE IF NOT EXISTS tool_results(action_id TEXT PRIMARY KEY,payload TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS tool_requests(action_id TEXT PRIMARY KEY,payload TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS reviews(session_id TEXT PRIMARY KEY,turns INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS events(session_id TEXT NOT NULL,seq INTEGER NOT NULL,run_id TEXT,kind TEXT NOT NULL,fields TEXT NOT NULL,PRIMARY KEY(session_id,seq),FOREIGN KEY(session_id) REFERENCES sessions(id));
      CREATE TABLE IF NOT EXISTS event_sequences(session_id TEXT PRIMARY KEY,seq INTEGER NOT NULL);")?;
    // Старые store.sqlite3 созданы до per-session бюджета: колонки добавляем
    // отдельно, чтобы обновление бинарника не требовало пересоздания store.
    for column in ["context_budget INTEGER", "context_target INTEGER"] {
        if let Err(error) = c.execute(&format!("ALTER TABLE sessions ADD COLUMN {column}"), []) {
            if !error.to_string().to_lowercase().contains("duplicate column") {
                return Err(error.into());
            }
        }
    }
    Ok(())
}
pub fn persist_store(c: &Connection, d: &Store) -> crate::R<()> {
    persist(c, d)
}
fn persist(c: &Connection, d: &Store) -> crate::R<()> {
    let tx = c.unchecked_transaction()?;
    tx.execute_batch(
        "DELETE FROM events; DELETE FROM tool_results; DELETE FROM tool_requests; DELETE FROM surface_tools; DELETE FROM reviews; DELETE FROM message_attachments; DELETE FROM attachments; DELETE FROM messages; DELETE FROM commands; DELETE FROM sessions;",
    )?;
    for s in d.sessions.values() {
        tx.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                s.id,
                s.surface,
                s.state,
                s.run_id,
                s.context_budget,
                s.context_target
            ],
        )?;
    }
    for (sid, ms) in &d.messages {
        for (seq, m) in ms.iter().enumerate() {
            tx.execute(
                "INSERT INTO messages VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    sid,
                    m.id,
                    m.command_id,
                    seq as i64,
                    m.text,
                    m.state,
                    m.run_id
                ],
            )?;
        }
    }
    for (sid, by_id) in &d.attachments {
        for a in by_id.values() {
            tx.execute(
                "INSERT INTO attachments VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    sid,
                    a.attachment_id,
                    a.command_id,
                    a.filename,
                    a.name,
                    serde_json::to_string(&a.part)?,
                    a.sha256
                ],
            )?;
        }
    }
    for (sid, ms) in &d.messages {
        for m in ms {
            for (seq, attachment_id) in m.attachment_ids.iter().enumerate() {
                tx.execute(
                    "INSERT INTO message_attachments VALUES(?1,?2,?3,?4)",
                    params![sid, m.id, attachment_id, seq as i64],
                )?;
            }
        }
    }
    for (sid, es) in &d.events {
        for e in es {
            tx.execute(
                "INSERT INTO events VALUES(?1,?2,?3,?4,?5)",
                params![
                    sid,
                    e.seq as i64,
                    e.run_id,
                    e.kind,
                    serde_json::to_string(&e.fields)?
                ],
            )?;
        }
    }
    for (id, payload) in &d.commands {
        tx.execute(
            "INSERT INTO commands VALUES(?1,?2)",
            params![id, serde_json::to_string(payload)?],
        )?;
    }
    for (sid, defs) in &d.surface_tools {
        tx.execute(
            "INSERT INTO surface_tools VALUES(?1,?2)",
            params![sid, serde_json::to_string(defs)?],
        )?;
    }
    for (id, payload) in &d.tool_results {
        tx.execute(
            "INSERT INTO tool_results VALUES(?1,?2)",
            params![id, serde_json::to_string(payload)?],
        )?;
    }
    for (id, payload) in &d.tool_requests {
        tx.execute(
            "INSERT INTO tool_requests VALUES(?1,?2)",
            params![id, serde_json::to_string(payload)?],
        )?;
    }
    for (id, turns) in &d.reviews {
        tx.execute(
            "INSERT INTO reviews VALUES(?1,?2)",
            params![id, *turns as i64],
        )?;
    }
    tx.commit()?;
    Ok(())
}
pub fn event_sequences(c: &Connection) -> crate::R<BTreeMap<String, u64>> {
    let mut sequences = BTreeMap::new();
    let mut query = c.prepare("SELECT session_id,seq FROM event_sequences")?;
    for row in query.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
    })? {
        let (id, seq) = row?;
        sequences.insert(id, seq);
    }
    Ok(sequences)
}
fn read_store(c: &Connection) -> crate::R<Store> {
    let mut d = Store::default();
    {
        let mut q = c
            .prepare("SELECT id,surface,state,run_id,context_budget,context_target FROM sessions ORDER BY id")?;
        for row in q.query_map([], |r| {
            Ok(Session {
                id: r.get(0)?,
                surface: r.get(1)?,
                // Заголовок не хранится в store: он выводится из лога при
                // discovery, чтобы правка Meta.title не требовала миграции.
                title: None,
                updated_at: None,
                state: r.get(2)?,
                run_id: r.get(3)?,
                context_budget: r.get(4)?,
                context_target: r.get(5)?,
            })
        })? {
            let s = row?;
            d.sessions.insert(s.id.clone(), s);
        }
    }
    {
        let mut q=c.prepare("SELECT session_id,id,command_id,text,state,run_id FROM messages ORDER BY session_id,seq")?;
        for row in q.query_map([], |r| {
            let sid: String = r.get(0)?;
            let mid: String = r.get(1)?;
            Ok((
                sid,
                Message {
                    id: mid,
                    command_id: r.get(2)?,
                    text: r.get(3)?,
                    state: r.get(4)?,
                    run_id: r.get(5)?,
                    attachment_ids: Vec::new(),
                },
            ))
        })? {
            let (sid, m) = row?;
            d.messages.entry(sid).or_default().push(m);
        }
    }
    {
        let mut q = c.prepare("SELECT session_id,message_id,attachment_id FROM message_attachments ORDER BY session_id,message_id,seq")?;
        for row in q.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })? {
            let (sid, mid, aid) = row?;
            if let Some(m) = d
                .messages
                .get_mut(&sid)
                .and_then(|ms| ms.iter_mut().find(|m| m.id == mid))
            {
                m.attachment_ids.push(aid);
            }
        }
    }
    {
        let mut q = c.prepare("SELECT session_id,id,command_id,filename,name,part,sha256 FROM attachments ORDER BY session_id,id")?;
        for row in q.query_map([], |r| {
            let part: String = r.get(5)?;
            Ok((
                r.get::<_, String>(0)?,
                Attachment {
                    attachment_id: r.get(1)?,
                    command_id: r.get(2)?,
                    filename: r.get(3)?,
                    name: r.get(4)?,
                    part: serde_json::from_str(&part).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            5,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?,
                    sha256: r.get(6)?,
                },
            ))
        })? {
            let (sid, attachment) = row?;
            d.attachments
                .entry(sid)
                .or_default()
                .insert(attachment.attachment_id.clone(), attachment);
        }
    }
    {
        let mut q = c.prepare(
            "SELECT session_id,seq,run_id,kind,fields FROM events ORDER BY session_id,seq",
        )?;
        for row in q.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Event {
                    session_id: r.get(0)?,
                    seq: r.get::<_, i64>(1)? as u64,
                    run_id: r.get(2)?,
                    kind: r.get(3)?,
                    fields: serde_json::from_str(&r.get::<_, String>(4)?)
                        .unwrap_or(serde_json::Value::Null),
                },
            ))
        })? {
            let (sid, e) = row?;
            d.events.entry(sid).or_default().push(e);
        }
    }
    {
        let mut q = c.prepare("SELECT session_id,definitions FROM surface_tools")?;
        for row in q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (sid, p) = row?;
            d.surface_tools.insert(sid, serde_json::from_str(&p)?);
        }
    }
    {
        let mut q = c.prepare("SELECT action_id,payload FROM tool_requests")?;
        for row in q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (id, p) = row?;
            d.tool_requests.insert(id, serde_json::from_str(&p)?);
        }
    }
    {
        let mut q = c.prepare("SELECT action_id,payload FROM tool_results")?;
        for row in q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (id, p) = row?;
            d.tool_results.insert(id, serde_json::from_str(&p)?);
        }
    }
    {
        let mut q = c.prepare("SELECT command_id,payload FROM commands")?;
        for row in q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (id, p) = row?;
            d.commands.insert(id, serde_json::from_str(&p)?);
        }
    }
    {
        let mut q = c.prepare("SELECT session_id,turns FROM reviews")?;
        for row in q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))? {
            let (id, turns) = row?;
            d.reviews.insert(id, turns);
        }
    }
    Ok(d)
}
pub fn open_store(dir: &Path) -> crate::R<(Store, Connection)> {
    fs::create_dir_all(dir)?;
    let db = Connection::open(dir.join("store.sqlite3"))?;
    schema(&db)?;
    let count: i64 = db.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
    if count == 0 {
        let legacy = dir.join("store.json");
        if legacy.exists() {
            let old: Store = serde_json::from_slice(&fs::read(&legacy)?)?;
            persist(&db, &old)?;
            fs::rename(&legacy, dir.join("store.json.imported"))?;
        }
    }
    Ok((read_store(&db)?, db))
}
#[allow(dead_code)]
pub fn load(dir: &Path) -> crate::R<Store> {
    Ok(open_store(dir)?.0)
}
pub fn interrupt_running(store: &mut Store, sequences: &mut BTreeMap<String, u64>) {
    let running: Vec<String> = store
        .sessions
        .values()
        .filter(|s| s.state == "running" || s.state == "stopping")
        .map(|s| s.id.clone())
        .collect();
    for id in running {
        let run_id = store.sessions.get(&id).and_then(|s| s.run_id.clone());
        for (action_id, action) in store.tool_requests.iter_mut() {
            if action_id.starts_with(&format!(
                "{}/{}/",
                id,
                run_id.as_deref().unwrap_or_default()
            )) && action.get("status").and_then(|v| v.as_str()) == Some("pending")
            {
                *action = serde_json::json!({"status":"cancelled"});
            }
        }
        if let Some(session) = store.sessions.get_mut(&id) {
            session.state = "interrupted".into();
            session.run_id = None;
        }
        let events = store.events.entry(id.clone()).or_default();
        let seq = sequences
            .get(&id)
            .copied()
            .unwrap_or_else(|| events.iter().map(|event| event.seq).max().unwrap_or(0))
            + 1;
        sequences.insert(id.clone(), seq);
        events.push(Event {
            seq,
            session_id: id.clone(),
            run_id,
            kind: "run.finished".into(),
            fields: serde_json::json!({"status":"interrupted"}),
        });
        if let Some(messages) = store.messages.get_mut(&id) {
            for message in messages.iter_mut().filter(|m| m.state == "accepted") {
                message.state = "cancelled".into();
            }
        }
    }
}

pub fn valid_id(s: &str) -> bool {
    if uuid::Uuid::parse_str(s).is_ok() {
        return true;
    }
    let Some((stamp, pid)) = s.split_once('-') else {
        return false;
    };
    !stamp.is_empty()
        && !pid.is_empty()
        && !pid.contains('-')
        && stamp.bytes().all(|b| b.is_ascii_digit())
        && pid.bytes().all(|b| b.is_ascii_digit())
}
#[cfg(test)]
mod legacy_id_tests {
    use super::valid_id;
    #[test]
    fn accepts_production_legacy_ids_but_rejects_paths_and_suffixes() {
        assert!(valid_id("1791498455-70558"));
        assert!(valid_id("1791499422-79425"));
        assert!(valid_id("550e8400-e29b-41d4-a716-446655440000"));
        for invalid in [
            "../1791498455-70558",
            "1791498455-70558.jsonl",
            "1791498455-70558-review",
            "-70558",
            "1791498455-",
        ] {
            assert!(!valid_id(invalid), "{invalid}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!("surface-{}", uuid::Uuid::new_v4()))
    }
    #[test]
    fn per_session_cleanup_limits_survive_persist_roundtrip() {
        let dir = temp();
        let (mut d, db) = open_store(&dir).unwrap();
        let mut s = Session::new("s", "telegram");
        s.context_budget = Some(100_000);
        s.context_target = Some(70_000);
        d.sessions.insert("s".into(), s);
        persist(&db, &d).unwrap();
        let got = read_store(&db).unwrap();
        assert_eq!(got.sessions["s"].context_budget, Some(100_000));
        assert_eq!(got.sessions["s"].context_target, Some(70_000));
        // Сессия без своего порога читается как None, а не как ноль.
        let mut plain = Session::new("p", "tui");
        plain.context_budget = None;
        d.sessions.insert("p".into(), plain);
        persist(&db, &d).unwrap();
        let got = read_store(&db).unwrap();
        assert_eq!(got.sessions["p"].context_budget, None);
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_store_without_cleanup_columns_is_migrated() {
        let dir = temp();
        fs::create_dir_all(&dir).unwrap();
        // Store старой сборки: колонок бюджета в sessions ещё нет.
        {
            let db = Connection::open(dir.join("store.sqlite3")).unwrap();
            db.execute_batch(
                "CREATE TABLE sessions(id TEXT PRIMARY KEY,surface TEXT NOT NULL,state TEXT NOT NULL,run_id TEXT);\
                 INSERT INTO sessions VALUES('s','telegram','idle',NULL);",
            )
            .unwrap();
        }
        let (store, db) = open_store(&dir).unwrap();
        assert_eq!(store.sessions["s"].context_budget, None);
        assert!(store.sessions["s"].context_target.is_none());
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn emit_load_roundtrip_and_dedup_persist() {
        let dir = temp();
        let (mut d, db) = open_store(&dir).unwrap();
        let id = "s".to_string();
        d.sessions.insert(id.clone(), Session::new(id.clone(), "test"));
        d.surface_tools.insert(id.clone(),vec![serde_json::json!({"type":"function","function":{"name":"send_file","parameters":{"type":"object"}}})]);
        d.tool_requests
            .insert("s/r/c".into(), serde_json::json!({"status":"pending"}));
        d.commands
            .insert("cmd".into(), serde_json::json!({"ok":true}));
        d.messages.insert(
            id.clone(),
            vec![Message {
                id: "m".into(),
                command_id: "cmd".into(),
                text: "hello".into(),
                attachment_ids: Vec::new(),
                state: "applied".into(),
                run_id: None,
            }],
        );
        d.events.insert(
            id.clone(),
            vec![Event {
                seq: 1,
                session_id: id,
                run_id: None,
                kind: "test".into(),
                fields: serde_json::json!({"x":1}),
            }],
        );
        persist(&db, &d).unwrap();
        let got = read_store(&db).unwrap();
        assert_eq!(got.events["s"][0].kind, "test");
        db.execute(
            "INSERT INTO event_sequences(session_id,seq) VALUES('s',9)",
            [],
        )
        .unwrap();
        assert_eq!(event_sequences(&db).unwrap()["s"], 9);
        assert_eq!(got.surface_tools["s"][0]["function"]["name"], "send_file");
        assert_eq!(got.tool_requests["s/r/c"]["status"], "pending");
        assert_eq!(got.messages["s"][0].command_id, "cmd");
        assert_eq!(got.commands["cmd"]["ok"], true);
        assert!(
            db.execute(
                "INSERT INTO messages VALUES('s','m2','cmd',1,'dup','accepted',NULL)",
                []
            )
            .is_err()
        );
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn restart_marks_running_interrupted_and_queued_cancelled() {
        let mut d = Store::default();
        d.sessions.insert(
            "s".into(),
            Session {
                run_id: Some("r".into()),
                state: "running".into(),
                ..Session::new("s", "test")
            },
        );
        d.tool_requests
            .insert("s/r/call".into(), serde_json::json!({"status":"pending"}));
        d.messages.insert(
            "s".into(),
            vec![Message {
                id: "m".into(),
                command_id: "c".into(),
                text: "queued".into(),
                attachment_ids: Vec::new(),
                state: "accepted".into(),
                run_id: Some("r".into()),
            }],
        );
        let mut sequences = BTreeMap::from([("s".to_string(), 9)]);
        d.events.insert(
            "s".into(),
            vec![Event {
                seq: 4,
                session_id: "s".into(),
                run_id: None,
                kind: "assistant.delta".into(),
                fields: serde_json::Value::Null,
            }],
        );
        interrupt_running(&mut d, &mut sequences);
        assert_eq!(sequences["s"], 10);
        assert_eq!(d.sessions["s"].state, "interrupted");
        assert_eq!(d.tool_requests["s/r/call"]["status"], "cancelled");
        assert_eq!(d.messages["s"][0].state, "cancelled");
        assert_eq!(d.events["s"].last().unwrap().seq, 10);
        assert_eq!(d.events["s"][1].fields["status"], "interrupted");
    }
    #[test]
    fn attachment_metadata_and_message_references_roundtrip_in_order() {
        let dir = temp();
        let (mut store, db) = open_store(&dir).unwrap();
        store.sessions.insert(
            "s".into(),
            Session::new("s", "test"),
        );
        let part = |path: &str, bytes| crate::log::Part {
            kind: "image".into(),
            path: path.into(),
            mime: "image/png".into(),
            bytes,
            width: 12,
            height: 34,
        };
        let first = Attachment {
            attachment_id: "a1".into(),
            command_id: "cmd".into(),
            filename: "first.png".into(),
            name: "upload-1".into(),
            part: part("/session/first.png", 123),
            sha256: "hash-1".into(),
        };
        let second = Attachment {
            attachment_id: "a2".into(),
            command_id: "cmd-upload-2".into(),
            filename: "second.png".into(),
            name: "upload-2".into(),
            part: part("/session/second.png", 456),
            sha256: "hash-2".into(),
        };
        store.attachments.insert(
            "s".into(),
            [("a1".into(), first.clone()), ("a2".into(), second.clone())]
                .into_iter()
                .collect(),
        );
        store.messages.insert(
            "s".into(),
            vec![Message {
                id: "m".into(),
                command_id: "cmd".into(),
                text: "with files".into(),
                attachment_ids: vec!["a2".into(), "a1".into()],
                state: "applied".into(),
                run_id: None,
            }],
        );

        persist(&db, &store).unwrap();
        let loaded = read_store(&db).unwrap();
        assert_eq!(loaded.attachments["s"]["a1"], first);
        assert_eq!(loaded.attachments["s"]["a2"], second);
        assert_eq!(loaded.messages["s"][0].attachment_ids, ["a2", "a1"]);
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn user_record_parts_default_for_legacy_and_roundtrip() {
        use crate::log::Rec;

        let legacy = r#"{"kind":"user","ts":1,"text":"old","source":"surface"}"#;
        match serde_json::from_str::<Rec>(legacy).unwrap() {
            Rec::User { parts, .. } => assert!(parts.is_empty()),
            other => panic!("expected user record, got {other:?}"),
        }

        let record = Rec::User {
            ts: 2,
            text: "new".into(),
            source: "surface".into(),
            parts: vec![crate::log::Part {
                kind: "image".into(),
                path: "/session/image.png".into(),
                mime: "image/png".into(),
                bytes: 42,
                width: 640,
                height: 480,
            }],
        };
        let decoded: Rec = serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
        match decoded {
            Rec::User { parts, .. } => {
                assert_eq!(parts.len(), 1);
                assert_eq!(parts[0].path, "/session/image.png");
                assert_eq!(parts[0].bytes, 42);
                assert_eq!((parts[0].width, parts[0].height), (640, 480));
            }
            other => panic!("expected user record, got {other:?}"),
        }
    }

    #[test]
    fn migration_imports_json_and_renames_it() {
        let dir = temp();
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("store.json"),
            serde_json::to_vec(&Store::default()).unwrap(),
        )
        .unwrap();
        let _ = open_store(&dir).unwrap();
        assert!(dir.join("store.json.imported").exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
