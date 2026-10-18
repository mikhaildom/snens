//! Одно хранилище: SQLite-файл. BM25 — FTS5, вектора — sqlite-vec (int8).
//! Никакого демона: процесс живёт один вызов, ~10 мс на ход.

use rusqlite::{ffi::sqlite3_auto_extension, params, Connection};
use std::path::Path;

pub struct Store {
    pub db: Connection,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub uid: String,
    pub text: String,
    pub bm25: Option<f64>,
    pub vec: Option<f64>,
    pub rrf: f64,
    pub rerank: Option<f32>,
}

/// Что положить в базу: факт с метаданными и, если есть, вектором.
pub struct NewFact {
    pub text: String,
    pub kind: String,
    pub subject: String,
    pub source: String,
    pub session: String,
    pub emb: Option<Vec<u8>>,
    pub dim: usize,
}

impl NewFact {
    /// Автозапись из `flush`: источник и сессия известны коду, не вызывающему.
    pub fn auto(text: &str, kind: &str, subject: &str, emb: Option<Vec<u8>>, dim: usize) -> Self {
        Self {
            text: text.to_string(),
            kind: kind.to_string(),
            subject: subject.to_string(),
            source: "auto".into(),
            session: String::new(),
            emb,
            dim,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Stats {
    pub facts: i64,
    pub stale: i64,
    pub pending: i64,
    pub kinds: Vec<(String, i64)>,
}

const SCHEMA: &str = "
create table if not exists facts(
  id integer primary key autoincrement,
  uid text not null unique,
  text text not null,
  kind text not null,
  subject text not null,
  created integer not null,
  valid_from integer not null,
  valid_to integer,
  supersedes text,
  source text not null default '',
  session text not null default ''
);
create index if not exists facts_live on facts(valid_to);
create virtual table if not exists facts_fts using fts5(text, uid unindexed, tokenize='unicode61');
create table if not exists turns(
  id integer primary key autoincrement,
  ts integer not null,
  user text not null,
  assistant text not null,
  context text not null default '',
  state text not null default 'new'
);
";

impl Store {
    pub fn open(path: &Path) -> Result<Store, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        // Расширение регистрируется один раз на процесс — до первого соединения,
        // иначе `vec0` не появится в схеме.
        unsafe {
            // Регистрация расширения — до первого соединения: иначе vec0 не будет.
            let init: unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut std::os::raw::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> std::os::raw::c_int = std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
            sqlite3_auto_extension(Some(init));
        }
        let db = Connection::open(path).map_err(|e| format!("{path:?}: {e}"))?;
        db.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
        db.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| e.to_string())?;
        db.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
        // Размерность вектора — параметр таблицы, поэтому создаётся лениво.
        Ok(Store { db })
    }

    fn ensure_vec(&self, dim: usize) -> Result<(), String> {
        let sql = format!("create virtual table if not exists facts_vec using vec0(emb int8[{dim}]);");
        self.db.execute_batch(&sql).map_err(|e| e.to_string())
    }

    /// Ближайшие живые факты по вектору. `k` — кандидатов в фузию.
    pub fn vector_search(
        &self,
        emb: &[u8],
        dim: usize,
        k: usize,
    ) -> Result<Vec<(i64, f64)>, String> {
        self.ensure_vec(dim)?;
        let sql = "select f.id, v.distance from facts_vec v join facts f on f.id = v.rowid \
                   where v.emb match vec_int8(?1) and v.k = ?2 and f.valid_to is null";
        let mut st = self.db.prepare(sql).map_err(|e| e.to_string())?;
        let rows = st
            .query_map(params![emb, k as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
            })
            .map_err(|e| e.to_string())?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// BM25 по FTS5. Возвращает `(id, score)`; score FTS5 отрицательный, меньше —
    /// значит лучше, поэтому сортировка по возрастанию.
    pub fn bm25_search(&self, query: &str, k: usize) -> Result<Vec<(i64, f64)>, String> {
        let expr = fts_query(query);
        if expr.is_empty() {
            return Ok(Vec::new());
        }
        // Устаревший факт остаётся в FTS5 (это таймлайн, а не удаление),
        // поэтому фильтр по `valid_to` обязателен в самой выборке.
        let sql = "select f.id, bm25(facts_fts) as score from facts_fts \
                   join facts f on f.id = facts_fts.rowid \
                   where facts_fts match ?1 and f.valid_to is null \
                   order by score limit ?2";
        let mut st = self.db.prepare(sql).map_err(|e| e.to_string())?;
        let rows = st
            .query_map(params![expr, k as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
            })
            .map_err(|e| e.to_string())?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn load(&self, ids: &[i64]) -> Result<Vec<Hit>, String> {
        let mut out = Vec::new();
        let mut st = self
            .db
            .prepare("select uid, text from facts where id = ?1 and valid_to is null")
            .map_err(|e| e.to_string())?;
        for id in ids {
            if let Ok(row) = st.query_row(params![id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            }) {
                out.push(Hit {
                    uid: row.0,
                    text: row.1,
                    bm25: None,
                    vec: None,
                    rrf: 0.0,
                    rerank: None,
                });
            }
        }
        Ok(out)
    }

    /// uid по id, в том же порядке: `load` отдаёт только живые факты, поэтому
    /// соответствие позиций надо восстановить до фильтрации.
    pub fn uids(&self, ids: &[i64]) -> Result<Vec<String>, String> {
        let mut st = self
            .db
            .prepare("select uid from facts where id = ?1")
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for id in ids {
            if let Ok(uid) = st.query_row(params![id], |r| r.get::<_, String>(0)) {
                out.push(uid);
            }
        }
        Ok(out)
    }

    /// Вставить факт. Возвращает uid записи.
    pub fn insert(&self, f: &NewFact) -> Result<String, String> {
        let (text, kind, subject, source, session, emb) = (
            f.text.as_str(),
            f.kind.as_str(),
            f.subject.as_str(),
            f.source.as_str(),
            f.session.as_str(),
            f.emb.as_deref(),
        );
        let now = crate::now();
        let uid = crate::uid();
        self.db
            .execute(
                "insert into facts(uid, text, kind, subject, created, valid_from, source, session) \
                 values (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7)",
                params![uid, text, kind, subject, now, source, session],
            )
            .map_err(|e| e.to_string())?;
        let id = self.db.last_insert_rowid();
        self.db
            .execute(
                "insert into facts_fts(rowid, text, uid) values (?1, ?2, ?3)",
                params![id, text, uid],
            )
            .map_err(|e| e.to_string())?;
        if let Some(emb) = emb {
            self.ensure_vec(f.dim)?;
            self.db
                .execute(
                    "insert into facts_vec(rowid, emb) values (?1, vec_int8(?2))",
                    params![id, emb.to_vec()],
                )
                .map_err(|e| e.to_string())?;
        }
        Ok(uid)
    }

    /// Закрыть факт: `valid_to = now`. Запись остаётся в истории (таймлайн),
    /// в выдачу больше не попадает.
    pub fn supersede(&self, old_uid: &str, new_uid: &str) -> Result<bool, String> {
        let now = crate::now();
        let n = self
            .db
            .execute(
                "update facts set valid_to = ?1, supersedes = null where uid = ?2 and valid_to is null",
                params![now, old_uid],
            )
            .map_err(|e| e.to_string())?;
        if n > 0 {
            self.db
                .execute(
                    "update facts set supersedes = ?1 where uid = ?2",
                    params![old_uid, new_uid],
                )
                .map_err(|e| e.to_string())?;
        }
        Ok(n > 0)
    }

    pub fn forget(&self, uid: &str) -> Result<bool, String> {
        let now = crate::now();
        let n = self
            .db
            .execute(
                "update facts set valid_to = ?1 where uid = ?2 and valid_to is null",
                params![now, uid],
            )
            .map_err(|e| e.to_string())?;
        Ok(n > 0)
    }

    /// Кандидаты для промпта дистилляции: несколько ближайших живых фактов,
    /// чтобы модель могла сказать «это противоречит вот этому».
    pub fn similar_for_prompt(
        &self,
        emb: Option<&[u8]>,
        dim: usize,
        k: usize,
    ) -> Result<Vec<(String, String)>, String> {
        let Some(emb) = emb else {
            return Ok(Vec::new());
        };
        let ids: Vec<i64> = self
            .vector_search(emb, dim, k)?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        Ok(self
            .load(&ids)?
            .into_iter()
            .map(|h| (h.uid, h.text))
            .collect())
    }

    pub fn enqueue(&self, user: &str, assistant: &str, context: &str) -> Result<i64, String> {
        self.db
            .execute(
                "insert into turns(ts, user, assistant, context) values (?1, ?2, ?3, ?4)",
                params![crate::now(), user, assistant, context],
            )
            .map_err(|e| e.to_string())?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn pending(&self, limit: usize) -> Result<Vec<(i64, String, String, String)>, String> {
        let mut st = self
            .db
            .prepare("select id, user, assistant, context from turns where state = 'new' order by id limit ?1")
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map(params![limit as i64], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn finish(&self, id: i64, state: &str) -> Result<(), String> {
        self.db
            .execute("update turns set state = ?1 where id = ?2", params![state, id])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn stats(&self) -> Result<Stats, String> {
        let count = |sql: &str| -> i64 {
            self.db.query_row(sql, [], |r| r.get(0)).unwrap_or(0)
        };
        let facts = count("select count(*) from facts where valid_to is null");
        let stale = count("select count(*) from facts where valid_to is not null");
        let pending = count("select count(*) from turns where state = 'new'");
        let mut kinds = Vec::new();
        let mut st = self
            .db
            .prepare("select kind, count(*) from facts where valid_to is null group by kind order by 2 desc")
            .map_err(|e| e.to_string())?;
        if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) {
            kinds.extend(rows.filter_map(|r| r.ok()));
        }
        Ok(Stats {
            facts,
            stale,
            pending,
            kinds,
        })
    }

    /// Факты без вектора (записаны, когда провайдер эмбеддингов лежал).
    pub fn missing_vectors(&self, limit: usize) -> Result<Vec<(i64, String)>, String> {
        let mut st = self
            .db
            .prepare(
                "select f.id, f.text from facts f left join facts_vec v on v.rowid = f.id \
                 where v.rowid is null and f.valid_to is null limit ?1",
            )
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map(params![limit as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| e.to_string())?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn store_vector(&self, id: i64, emb: &[u8], dim: usize) -> Result<(), String> {
        self.ensure_vec(dim)?;
        self.db
            .execute(
                "insert or replace into facts_vec(rowid, emb) values (?1, vec_int8(?2))",
                params![id, emb.to_vec()],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// FTS5-выражение из свободного запроса: слова длиной ≥ 3 через OR.
/// Стемминга нет — русский unicode61 режет по буквам, дальше решает фузия.
pub fn fts_query(query: &str) -> String {
    let mut terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(|w| w.to_lowercase())
        .collect();
    terms.dedup();
    terms.truncate(8);
    terms
        .into_iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Reciprocal Rank Fusion: склейка двух ранжирований по рангам.
/// k=20: при ~1 нужном факте на запрос оптимум ниже, чем пилотные 60
/// (Cormack, SIGIR'09 — «choice not critical»).
pub fn rrf(ranks: &[usize], k: f64) -> f64 {
    ranks.iter().map(|r| 1.0 / (k + *r as f64)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("snens-memory-{}-{name}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn fact(text: &str, kind: &str, subject: &str, emb: &[u8], dim: usize) -> NewFact {
        NewFact {
            text: text.into(),
            kind: kind.into(),
            subject: subject.into(),
            source: "test".into(),
            session: String::new(),
            emb: Some(emb.to_vec()),
            dim,
        }
    }

    #[test]
    fn fts_and_vectors_fuse_and_supersede() {
        let path = tmp("fuse");
        let s = Store::open(&path).unwrap();
        let dim = 4;
        let a = crate::api::quantize(&[1.0, 0.0, 0.0, 0.0]);
        let b = crate::api::quantize(&[0.0, 1.0, 0.0, 0.0]);
        let uid_a = s
            .insert(&fact("пользователь живёт в Нидерландах", "fact", "user", &a, dim))
            .unwrap();
        s.insert(&fact("сервер VPN стоит в Финляндии", "fact", "world", &b, dim))
            .unwrap();

        let by_text = s.bm25_search("Нидерландах", 5).unwrap();
        assert_eq!(by_text.len(), 1, "BM25 не нашёл факт по слову");
        let by_vec = s.vector_search(&a, dim, 2).unwrap();
        assert_eq!(by_vec.len(), 2);
        assert_eq!(by_vec[0].0, 1, "ближайший вектор должен быть первым");

        // Противоречащий факт закрывает старый, но остаётся в таймлайне.
        let uid_a2 = s
            .insert(&fact("пользователь переехал в Германию", "fact", "user", &a, dim))
            .unwrap();
        assert!(s.supersede(&uid_a, &uid_a2).unwrap());
        let live = s.bm25_search("Нидерландах", 5).unwrap();
        assert!(live.is_empty(), "устаревший факт всё ещё выдаётся");
        assert_eq!(s.stats().unwrap().stale, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rrf_prefers_items_found_by_both() {
        let both = rrf(&[0, 2], 20.0);
        let only_one = rrf(&[0], 20.0);
        assert!(both > only_one);
    }

    #[test]
    fn fts_query_drops_short_words_and_ors() {
        assert_eq!(fts_query("я про VPN"), "\"про\" OR \"vpn\"");
    }
}
