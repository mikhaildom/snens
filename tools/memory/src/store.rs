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

/// Живой факт для промпта дистилляции: модель должна видеть тип и subject,
/// чтобы «перекрывает» решалось в тех же координатах, а не по одному тексту.
pub struct PromptFact {
    pub uid: String,
    pub text: String,
    pub kind: String,
    pub subject: String,
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
    /// Когда факт возник в исходной системе. `None` — сейчас. Миграция обязана
    /// сохранять даты: таймлайн — половина ценности истории.
    pub created: Option<i64>,
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
            created: None,
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
  state text not null default 'new',
  -- сколько раз ход уже пытались разобрать: транзиентный сбой провайдера не
  -- должен съедать ход навсегда.
  attempts integer not null default 0
);
";

impl Store {
    pub fn open(path: &Path) -> Result<Store, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        // Расширение регистрируется ровно один раз на процесс и до первого
        // соединения: повторная регистрация добавляла дубль в глобальный список,
        // а проигнорированный код возврата превращался в «no such module: vec0».
        static REG: std::sync::OnceLock<std::os::raw::c_int> = std::sync::OnceLock::new();
        let rc = *REG.get_or_init(|| unsafe {
            // SAFETY: transmute сигнатуры sqlite3_vec_init в рукописный тип — то,
            // ради чего крейт sqlite-vec и написан; вызов идёт до первого соединения.
            let init: unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut std::os::raw::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> std::os::raw::c_int =
                std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
            sqlite3_auto_extension(Some(init))
        });
        if rc != 0 {
            return Err("не удалось зарегистрировать расширение sqlite-vec".into());
        }
        let db = Connection::open(path).map_err(|e| format!("{path:?}: {e}"))?;
        db.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
        db.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| e.to_string())?;
        db.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
        // Старые базы колонки не имеют — добавляем на месте (ошибка «duplicate
        // column name» при повторном открытии ожидаема и не фатальна).
        let _ = db.execute(
            "alter table turns add column attempts integer not null default 0",
            [],
        );
        // Размерность вектора — параметр таблицы, поэтому создаётся лениво.
        Ok(Store { db })
    }

    fn ensure_vec(&self, dim: usize) -> Result<(), String> {
        // Таблица уже есть с другой размерностью — `if not exists` молча это
        // проглотит, и ошибка вылезет низкоуровневым сбоем `vec_int8`. Проверяем.
        if self.has_vec_table()? {
            let sql: String = self
                .db
                .query_row(
                    "select sql from sqlite_master where type = 'table' and name = 'facts_vec'",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            if !sql.contains(&format!("int8[{dim}]")) {
                return Err(format!(
                    "таблица векторов создана с другой размерностью ({sql}), а в конфиге {dim}:                      удали память или верни embed_dim"
                ));
            }
            return Ok(());
        }
        let sql =
            format!("create virtual table if not exists facts_vec using vec0(emb int8[{dim}]);");
        self.db.execute_batch(&sql).map_err(|e| e.to_string())
    }

    /// Есть ли уже таблица векторов: она создаётся лениво, при первом факте с
    /// эмбеддингом, и запрос к ней на свежей базе падает.
    fn has_vec_table(&self) -> Result<bool, String> {
        self.db
            .query_row(
                "select count(*) from sqlite_master where type = 'table' and name = 'facts_vec'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .map_err(|e| e.to_string())
    }

    /// Ближайшие живые факты по вектору. `k` — кандидатов в фузию.
    pub fn vector_search(
        &self,
        emb: &[u8],
        dim: usize,
        k: usize,
    ) -> Result<Vec<(i64, f64)>, String> {
        self.ensure_vec(dim)?;
        // k у KNN ограничивает выборку ДО фильтра по valid_to, а устаревшие факты
        // свои векторы сохраняют. Берём с запасом, чтобы после фильтра осталось
        // не меньше k живых.
        let sql = "select f.id, v.distance from facts_vec v join facts f on f.id = v.rowid \
                   where v.emb match vec_int8(?1) and v.k = ?2 and f.valid_to is null";
        let mut st = self.db.prepare(sql).map_err(|e| e.to_string())?;
        let rows = st
            .query_map(
                params![emb, (k.saturating_mul(4) as i64).max(k as i64)],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)),
            )
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            // Ошибка ряда — не «нет попадания»: неполная выдача выдаёт себя за полную.
            out.push(row.map_err(|e| e.to_string())?);
            if out.len() >= k {
                break;
            }
        }
        Ok(out)
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
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn load(&self, ids: &[i64]) -> Result<Vec<Hit>, String> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // Один запрос на всю пачку: точечные выборки по одному id давали до 60
        // обращений к таблице за один recall.
        let sql = format!(
            "select id, uid, text from facts where valid_to is null and id in ({})",
            ids.iter().map(|_| "?").collect::<Vec<_>>().join(",")
        );
        let mut st = self.db.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = st
            .query_map(rusqlite::params_from_iter(ids.iter()), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    Hit {
                        uid: r.get::<_, String>(1)?,
                        text: r.get::<_, String>(2)?,
                        bm25: None,
                        vec: None,
                        rrf: 0.0,
                        rerank: None,
                    },
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut found: std::collections::HashMap<i64, Hit> = std::collections::HashMap::new();
        for row in rows {
            let (id, hit) = row.map_err(|e| e.to_string())?;
            found.insert(id, hit);
        }
        // Порядок — как просили: ids приходят из поиска по убыванию релевантности.
        Ok(ids.iter().filter_map(|id| found.remove(id)).collect())
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
    /// `facts`, `facts_fts` и `facts_vec` пишутся одной транзакцией: иначе сбой
    /// на FTS/векторе оставил бы факт, невидимый для поиска.
    /// Одиночных вставок в рабочем коде нет (ход и импорт идут пачками) — метод
    /// остаётся точкой входа для тестов и будущих вызывающих.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn insert(&self, f: &NewFact) -> Result<String, String> {
        // Виртуальная таблица векторов — до транзакции: DDL внутри неё только
        // добавляет риск, а `ensure_vec` идемпотентна.
        if f.emb.is_some() {
            self.ensure_vec(f.dim)?;
        }
        let tx = self.db.unchecked_transaction().map_err(|e| e.to_string())?;
        let uid = insert_row(&tx, f)?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(uid)
    }

    /// Записать всё, что дал один ход, одной транзакцией: новые факты, закрытие
    /// перекрытых старых и отметку `done`. Сбой на любом шаге не оставляет
    /// полузаписанного хода, который следующий `flush` переработал бы в дубли.
    pub fn finish_turn(
        &self,
        turn: i64,
        items: &[(NewFact, Option<String>)],
    ) -> Result<(usize, usize), String> {
        if let Some(dim) = items
            .iter()
            .find_map(|(f, _)| f.emb.as_ref().map(|_| f.dim))
        {
            self.ensure_vec(dim)?;
        }
        let tx = self.db.unchecked_transaction().map_err(|e| e.to_string())?;
        let (mut added, mut superseded) = (0usize, 0usize);
        for (f, over) in items {
            let uid = insert_row(&tx, f)?;
            if let Some(old) = over {
                if supersede_row(&tx, old, &uid)? {
                    superseded += 1;
                }
            }
            added += 1;
        }
        tx.execute(
            "update turns set state = 'done' where id = ?1",
            params![turn],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok((added, superseded))
    }

    /// Вставить пачку фактов одной транзакцией. Возвращает число добавленных.
    /// Повтор того же текста (без учёта регистра и крайних пробелов) пропускается:
    /// в старом банке дублей хватает, и второй экземпляр только портит выдачу.
    pub fn insert_batch(&mut self, facts: &[NewFact]) -> Result<usize, String> {
        // Таблица векторов создаётся до транзакции: `ensure_vec` работает через
        // &self, а транзакция держит &mut.
        if let Some(dim) = facts.iter().find_map(|f| f.emb.as_ref().map(|_| f.dim)) {
            self.ensure_vec(dim)?;
        }
        // Дедуп — в Rust, а не SQL: `lower()` и `COLLATE NOCASE` в SQLite
        // работают только с ASCII, и кириллица в них не складывается.
        // Только живые факты: закрытые прошлым импортом должны вставиться заново,
        // иначе `import --replace` вычистил бы банк и не вернул ничего.
        let mut seen: std::collections::HashSet<String> = self
            .db
            .prepare("select text from facts where valid_to is null")
            .map_err(|e| e.to_string())?
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .map(|t| norm(&t))
            .collect();
        let tx = self.db.transaction().map_err(|e| e.to_string())?;
        let mut added = 0;
        for f in facts {
            if !seen.insert(norm(&f.text)) {
                continue;
            }
            insert_row(&tx, f)?;
            added += 1;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(added)
    }

    /// Сколько живых фактов пришло из конкретного источника: повторный `import`
    /// не должен удваивать банк. Закрытые не считаем — иначе `--replace`
    /// не смог бы пройти второй раз.
    pub fn count_source(&self, source: &str) -> Result<i64, String> {
        self.db
            .query_row(
                "select count(*) from facts where source = ?1 and valid_to is null",
                params![source],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())
    }

    /// Закрыть всё, что пришло из источника: перед повторным импортом.
    pub fn close_source(&self, source: &str) -> Result<usize, String> {
        let now = crate::now();
        self.db
            .execute(
                "update facts set valid_to = ?1 where source = ?2 and valid_to is null",
                params![now, source],
            )
            .map_err(|e| e.to_string())
    }

    /// Закрыть факт: `valid_to = now`. Запись остаётся в истории (таймлайн),
    /// в выдачу больше не попадает. Рабочие пути (`flush`) гасят старый факт той
    /// же транзакцией через `finish_turn`; публичная форма нужна тестам и ручным
    /// вызовам.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn supersede(&self, old_uid: &str, new_uid: &str) -> Result<bool, String> {
        // Две записи — одной транзакцией: иначе смерть процесса между ними
        // оставляла закрытый факт без указателя на замену.
        let tx = self.db.unchecked_transaction().map_err(|e| e.to_string())?;
        let ok = supersede_row(&tx, old_uid, new_uid)?;
        if ok {
            // Ссылка потеряется, если нового факта нет: это ошибка, а не «ок».
            let n = tx
                .execute(
                    "update facts set supersedes = ?1 where uid = ?2",
                    params![old_uid, new_uid],
                )
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err(format!(
                    "supersede: факта {new_uid} нет — ссылка не поставлена"
                ));
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(ok)
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
    ) -> Result<Vec<PromptFact>, String> {
        let Some(emb) = emb else {
            return Ok(Vec::new());
        };
        let mut st = self
            .db
            .prepare(
                "select uid, text, kind, subject from facts where id = ?1 and valid_to is null",
            )
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for (id, _) in self.vector_search(emb, dim, k)? {
            if let Ok(p) = st.query_row(params![id], |r| {
                Ok(PromptFact {
                    uid: r.get::<_, String>(0)?,
                    text: r.get::<_, String>(1)?,
                    kind: r.get::<_, String>(2)?,
                    subject: r.get::<_, String>(3)?,
                })
            }) {
                out.push(p);
            }
        }
        Ok(out)
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
            // 'retry' — транзиентный сбой: ход ждёт следующего flush, но не
            // бесконечно (attempts ограничивает).
            .prepare(
                "select id, user, assistant, context from turns \
                 where state = 'new' or (state = 'retry' and attempts < 5) order by id limit ?1",
            )
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

    /// Отметить неудачную попытку: ход вернётся в очередь, пока попыток меньше
    /// пяти. Так транзиентный сбой провайдера не теряет ход навсегда.
    pub fn retry(&self, id: i64) -> Result<(), String> {
        self.db
            .execute(
                "update turns set state = 'retry', attempts = attempts + 1 where id = ?1",
                params![id],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn stats(&self) -> Result<Stats, String> {
        // Ошибку базы не превращаем в ноль: «пусто» и «сломалось» — разные ответы.
        let count = |sql: &str| -> Result<i64, String> {
            self.db
                .query_row(sql, [], |r| r.get(0))
                .map_err(|e| e.to_string())
        };
        let facts = count("select count(*) from facts where valid_to is null")?;
        let stale = count("select count(*) from facts where valid_to is not null")?;
        let pending = count(
            "select count(*) from turns where state = 'new' or (state = 'retry' and attempts < 5)",
        )?;
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
    /// `facts_vec` создаётся лениво, поэтому на свежей базе её может не быть —
    /// тогда векторов нет ни у кого, а не «ошибка запроса».
    pub fn missing_vectors(&self, limit: usize) -> Result<Vec<(i64, String)>, String> {
        if !self.has_vec_table()? {
            let mut st = self
                .db
                .prepare("select id, text from facts where valid_to is null limit ?1")
                .map_err(|e| e.to_string())?;
            let rows = st
                .query_map(params![limit as i64], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(|e| e.to_string())?;
            return Ok(rows.filter_map(|r| r.ok()).collect());
        }
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

/// Тело `insert`/`insert_batch`/`finish_turn`: три таблицы одной транзакцией.
/// Отдельная функция, чтобы вызывающие не повторяли один и тот же SQL.
fn insert_row(tx: &rusqlite::Transaction<'_>, f: &NewFact) -> Result<String, String> {
    let now = f.created.unwrap_or_else(crate::now);
    let uid = crate::uid();
    tx.execute(
        "insert into facts(uid, text, kind, subject, created, valid_from, source, session) \
         values (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7)",
        params![uid, f.text, f.kind, f.subject, now, f.source, f.session],
    )
    .map_err(|e| e.to_string())?;
    let id = tx.last_insert_rowid();
    tx.execute(
        "insert into facts_fts(rowid, text, uid) values (?1, ?2, ?3)",
        params![id, f.text, uid],
    )
    .map_err(|e| e.to_string())?;
    if let Some(emb) = &f.emb {
        tx.execute(
            "insert into facts_vec(rowid, emb) values (?1, vec_int8(?2))",
            params![id, emb.clone()],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(uid)
}

/// Закрыть старый факт в пользу нового в уже открытой транзакции.
fn supersede_row(
    tx: &rusqlite::Transaction<'_>,
    old_uid: &str,
    new_uid: &str,
) -> Result<bool, String> {
    let now = crate::now();
    let n = tx
        .execute(
            "update facts set valid_to = ?1, supersedes = null where uid = ?2 and valid_to is null",
            params![now, old_uid],
        )
        .map_err(|e| e.to_string())?;
    if n > 0 {
        tx.execute(
            "update facts set supersedes = ?1 where uid = ?2",
            params![old_uid, new_uid],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(n > 0)
}

/// Ключ дедупа: без регистра (в том числе кириллицы) и крайних пробелов.
pub fn norm(text: &str) -> String {
    text.trim().to_lowercase()
}
/// FTS5-выражение из свободного запроса: слова длиной ≥ 3 через OR.
/// Стемминга нет — русский unicode61 режет по буквам, дальше решает фузия.
pub fn fts_query(query: &str) -> String {
    let mut terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(|w| w.to_lowercase())
        .collect();
    // `dedup()` снимает только соседние дубли, а `vpn x vpn` оставлял оба
    // «vpn» и съедал бюджет truncate(8) впустую.
    let mut seen = std::collections::HashSet::new();
    terms.retain(|t| seen.insert(t.clone()));
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
            created: None,
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
            .insert(&fact(
                "пользователь живёт в Нидерландах",
                "fact",
                "user",
                &a,
                dim,
            ))
            .unwrap();
        s.insert(&fact(
            "сервер VPN стоит в Финляндии",
            "fact",
            "world",
            &b,
            dim,
        ))
        .unwrap();

        let by_text = s.bm25_search("Нидерландах", 5).unwrap();
        assert_eq!(by_text.len(), 1, "BM25 не нашёл факт по слову");
        let by_vec = s.vector_search(&a, dim, 2).unwrap();
        assert_eq!(by_vec.len(), 2);
        assert_eq!(by_vec[0].0, 1, "ближайший вектор должен быть первым");

        // Противоречащий факт закрывает старый, но остаётся в таймлайне.
        let uid_a2 = s
            .insert(&fact(
                "пользователь переехал в Германию",
                "fact",
                "user",
                &a,
                dim,
            ))
            .unwrap();
        assert!(s.supersede(&uid_a, &uid_a2).unwrap());
        let live = s.bm25_search("Нидерландах", 5).unwrap();
        assert!(live.is_empty(), "устаревший факт всё ещё выдаётся");
        assert_eq!(s.stats().unwrap().stale, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn batch_import_skips_duplicates_and_keeps_dates() {
        let path = tmp("batch");
        let mut s = Store::open(&path).unwrap();
        let a = crate::api::quantize(&[1.0, 0.0, 0.0, 0.0]);
        let mut first = fact("пользователь живёт в Нидерландах", "fact", "user", &a, 4);
        first.created = Some(1_700_000_000);
        let dup = fact("  Пользователь живёт в Нидерландах ", "fact", "user", &a, 4);
        assert_eq!(s.insert_batch(&[first, dup]).unwrap(), 1, "дубль не отсеян");
        let created: i64 =
            s.db.query_row("select created from facts", [], |r| r.get(0))
                .unwrap();
        assert_eq!(created, 1_700_000_000, "дата исходной записи потеряна");
        assert_eq!(s.count_source("import").unwrap(), 0, "источник не auto");
        assert_eq!(s.close_source("test").unwrap(), 1);
        assert_eq!(s.stats().unwrap().facts, 0);
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
