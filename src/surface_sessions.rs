// Жизненный цикл сессии: список, создание, чтение, удаление и набор
// инструментов, который поверхность разрешает сессии.
//
// Список — самый дорогой маршрут поверхности (разбор журналов всех сессий),
// поэтому блокирующая работа здесь уходит в `spawn_blocking`, а мьютекс store
// на время разбора не держится: иначе `emit` любой сессии стоял бы в очереди.

use crate::cfg::Config;
use crate::surface_runtime::{self as rt, Runtime, Session};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

use super::discovery;
use super::http::err;

pub(super) async fn list(State(s): State<Arc<Runtime>>) -> impl IntoResponse {
    // Разбор журналов — блокирующая работа (read_dir + чтение файлов), и на
    // 500 сессиях она занимает секунды. В async-обработчике она останавливала
    // воркер tokio целиком: на двух ядрах это половина рантайма. Плюс мьютекс
    // store не держится во время чтения — иначе `emit` любой другой сессии
    // стоял бы в очереди за списком.
    let cfg = s.cfg.clone();
    let snapshot = s.data.lock().await.sessions.clone();
    let discovered = tokio::task::spawn_blocking(move || {
        let store = rt::Store {
            sessions: snapshot,
            ..Default::default()
        };
        discovery::sessions(&cfg, &store)
    })
    .await;
    let sessions = match discovered {
        Ok(Ok(sessions)) => sessions,
        Ok(Err(e)) => {
            return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
        Err(e) => {
            return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    };
    // Записи, которые список принёс впервые, дописываем в store точечно.
    {
        let mut state = s.data.lock().await;
        for session in &sessions {
            match state.sessions.get_mut(&session.id) {
                Some(slot) => {
                    // Состояние процесса, `run_id` и бюджеты — правда store (её
                    // ставит launch/stop/API), остальное точнее в журнале.
                    // Перезапись этих полей снимком журнала откатывала только
                    // что завершённый ход или сменённый бюджет: discovery читал
                    // журналы секундами, и снимок успевал устареть.
                    slot.title = session.title.clone().or(slot.title.take());
                    slot.surface = session.surface.clone();
                    slot.parent = session.parent.clone();
                    slot.model = session.model.clone();
                    slot.updated_at = session.updated_at;
                    slot.turns = session.turns;
                    slot.done = session.done.clone();
                    slot.started_at = session.started_at;
                    slot.done_at = session.done_at;
                }
                None => {
                    // Сессию могли удалить, пока шёл разбор журналов: журнала
                    // уже нет — не воскрешаем её в store зомби.
                    if !s
                        .cfg
                        .sessions_path()
                        .join(format!("{}.jsonl", session.id))
                        .is_file()
                    {
                        continue;
                    }
                    state.sessions.insert(session.id.clone(), session.clone());
                    state.events.entry(session.id.clone()).or_default();
                }
            }
        }
    }
    Json(serde_json::json!({"sessions":sessions})).into_response()
}

#[derive(Deserialize)]
pub(super) struct ToolDefs {
    tools: Vec<serde_json::Value>,
}
pub(super) async fn register_tools(
    State(s): State<Arc<Runtime>>,
    Path(id): Path<String>,
    Json(body): Json<ToolDefs>,
) -> impl IntoResponse {
    if !rt::valid_id(&id) {
        return err("invalid", "invalid session id", StatusCode::BAD_REQUEST).into_response();
    }
    let defs = match validate_tools(body.tools, &s.cfg).await {
        Ok(x) => x,
        Err(e) => return err("invalid_tools", &e, StatusCode::BAD_REQUEST).into_response(),
    };
    let d = s.data.lock().await;
    let Some(session) = d.sessions.get(&id) else {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    };
    let old = d.surface_tools.get(&id).cloned();
    if old.as_ref() == Some(&defs) {
        return (StatusCode::OK, Json(serde_json::json!({"tools":defs}))).into_response();
    }
    if session.state == "running" || session.state == "stopping" {
        return err(
            "run_active",
            "cannot change tools while run is active",
            StatusCode::CONFLICT,
        )
        .into_response();
    }
    drop(d);
    // Сначала база, потом память: при ошибке записи map остаётся прежним, и
    // повтор с теми же `defs` не отвечает 200 «уже так и есть» на то, чего в
    // базе нет (иначе инструменты молча пропадали бы после рестарта).
    if let Err(e) = s.save_surface_tools(&id, &defs) {
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    s.data.lock().await.surface_tools.insert(id, defs.clone());
    (StatusCode::OK, Json(serde_json::json!({"tools":defs}))).into_response()
}
pub(super) async fn validate_tools(
    tools: Vec<serde_json::Value>,
    cfg: &Config,
) -> Result<Vec<serde_json::Value>, String> {
    use std::collections::BTreeSet;
    if tools.len() > 8 {
        return Err("at most 8 surface tools".into());
    }
    if serde_json::to_vec(&tools).map_err(|e| e.to_string())?.len() > 32768 {
        return Err("tool definitions exceed 32 KiB".into());
    }
    let dynamic = crate::registry::discover(cfg).await;
    let core: BTreeSet<String> = crate::dispatch::builtin_defs(&cfg.shell_line())
        .iter()
        .filter_map(|d| {
            d.pointer("/function/name")
                .and_then(|x| x.as_str())
                .map(str::to_string)
        })
        .chain(
            crate::registry::definitions(&dynamic)
                .iter()
                .filter_map(|d| {
                    d.pointer("/function/name")
                        .and_then(|x| x.as_str())
                        .map(str::to_string)
                }),
        )
        .collect();
    let mut names = BTreeSet::new();
    for t in &tools {
        if t.get("type").and_then(|x| x.as_str()) != Some("function") {
            return Err("only OpenAI function tools are supported".into());
        }
        let f = &t["function"];
        let name = f
            .get("name")
            .and_then(|x| x.as_str())
            .ok_or("function.name is required")?;
        if name.is_empty()
            || name.len() > 64
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err("invalid function name".into());
        }
        if core.contains(name) || !names.insert(name.to_string()) {
            return Err("duplicate or core tool name".into());
        }
        if f.pointer("/parameters/type").and_then(|x| x.as_str()) != Some("object") {
            return Err("function.parameters.type must be object".into());
        }
    }
    Ok(tools)
}
#[derive(Deserialize)]
pub(super) struct Create {
    pub(super) command_id: String,
    pub(super) surface: String,
    /// Необязательный per-session бюджет уборки: поверхность с иной ценой
    /// контекста не должна диктовать порог остальным сессиям. Ноль — общий
    /// порог из конфига.
    #[serde(default)]
    pub(super) context_budget: Option<u64>,
    #[serde(default)]
    pub(super) context_target: Option<u64>,
}
/// Ответ на повтор `POST /v1/sessions` с тем же `command_id`.
///
/// `Ok(None)` — команда новая. `Err` — ответ клиенту целиком (409 с чужим
/// payload): так развилка идемпотентности живёт в одном месте, а не в двух
/// копиях «первый заход / второй заход».
fn replay_create(
    d: &rt::Store,
    b: &Create,
) -> Result<Option<Value>, Box<axum::response::Response>> {
    let Some(v) = d.commands.get(&b.command_id) else {
        return Ok(None);
    };
    // Повтор команды обязан совпасть и по бюджету: иначе тихий replay вернул бы
    // сессию с другим порогом уборки.
    //
    // Сравниваем с НОРМАЛИЗОВАННЫМ payload'ом: `create` пишет `context_budget`
    // только если он больше нуля, а `Session` сериализуется с
    // `skip_serializing_if`, поэтому первый запрос с `context_budget: 0`
    // сохранялся без ключа, и тот же повтор сравнивал `None` с `Some(0)` и
    // получал 409 `idempotency_conflict` вместо идемпотентного ответа.
    let same = v.get("_surface_request").and_then(|x| x.as_str()) == Some(b.surface.as_str())
        && v.get("context_budget").and_then(Value::as_u64) == b.context_budget.filter(|v| *v > 0)
        && v.get("context_target").and_then(Value::as_u64) == b.context_target.filter(|v| *v > 0);
    if !same {
        // Ответ в `Box`, как у `resolve_attachments`: `Response` велик, и
        // `Result` с ним на стеке каждого вызова — лишние 128 байт.
        return Err(Box::new(
            err(
                "idempotency_conflict",
                "command_id reused with different payload",
                StatusCode::CONFLICT,
            )
            .into_response(),
        ));
    }
    let mut response = v.clone();
    if let Some(obj) = response.as_object_mut() {
        obj.remove("_surface_request");
    }
    Ok(Some(response))
}

pub(super) async fn create(
    State(s): State<Arc<Runtime>>,
    Json(b): Json<Create>,
) -> impl IntoResponse {
    if !rt::valid_id(&b.command_id) || b.surface.is_empty() || b.surface.len() > 80 {
        return err(
            "invalid",
            "invalid command_id or surface",
            StatusCode::BAD_REQUEST,
        )
        .into_response();
    }
    // Повтор команды проверяем ДО работы с файлом: `open_surface` делает
    // create + `flock`, а держать под этим мьютекс store нельзя — один
    // `POST /v1/sessions` останавливал бы все обработчики, которым нужен store.
    {
        let d = s.data.lock().await;
        match replay_create(&d, &b) {
            Ok(Some(response)) => return (StatusCode::OK, Json(response)).into_response(),
            Ok(None) => {}
            Err(response) => return *response,
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    let writer =
        match crate::log::Session::open_surface(&s.cfg.sessions_path(), &id, Some(&b.surface)) {
            Ok(writer) => Arc::new(writer),
            Err(e) => {
                return err("session_owned", &e.to_string(), StatusCode::CONFLICT).into_response();
            }
        };
    let mut session = Session::new(id.clone(), b.surface.clone());
    session.context_budget = b.context_budget.filter(|v| *v > 0);
    session.context_target = b.context_target.filter(|v| *v > 0);
    // `Session` сериализуется всегда (все поля простые), но паника в создании
    // сессии — это 500 без ответа вместо ошибки; отвечаем штатно.
    let Ok(mut record) = serde_json::to_value(&session) else {
        drop(writer);
        let _ = std::fs::remove_file(s.cfg.sessions_path().join(format!("{id}.jsonl")));
        return err(
            "storage",
            "session is not serializable",
            StatusCode::INTERNAL_SERVER_ERROR,
        )
        .into_response();
    };
    record["_surface_request"] = serde_json::Value::String(b.surface.clone());
    let command_payload = record.clone();
    let command_id = b.command_id.clone();
    let journal = s.cfg.sessions_path().join(format!("{id}.jsonl"));
    // Второй заход и вставка в store — одним замком: проверка и резервирование
    // `command_id` не должны расходиться по времени.
    {
        let mut d = s.data.lock().await;
        match replay_create(&d, &b) {
            Ok(Some(response)) => {
                drop(d);
                drop(writer);
                let _ = std::fs::remove_file(&journal);
                return (StatusCode::OK, Json(response)).into_response();
            }
            Ok(None) => {}
            Err(response) => {
                drop(d);
                drop(writer);
                let _ = std::fs::remove_file(&journal);
                return *response;
            }
        }
        d.sessions.insert(id.clone(), session.clone());
        d.events.insert(id.clone(), vec![]);
        d.messages.insert(id.clone(), vec![]);
        d.commands.insert(b.command_id.clone(), record);
    }
    s.writers.lock().await.insert(id.clone(), writer.clone());
    // Запись — уже без store-замка (блокирующий SQLite), но с откатом всего,
    // что уже сделано: полузарегистрированная сессия с живым `.lock`-flock и
    // осиротевшим журналом хуже, чем честная 500.
    if let Err(e) = s
        .save_session(&session)
        .and_then(|_| s.save_command(&command_id, &command_payload))
    {
        s.writers.lock().await.remove(&id);
        drop(writer);
        let _ = std::fs::remove_file(&journal);
        let mut d = s.data.lock().await;
        d.sessions.remove(&id);
        d.events.remove(&id);
        d.messages.remove(&id);
        d.commands.remove(&command_id);
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    s.event_seq.lock().await.insert(id, 0);
    (StatusCode::CREATED, Json(session)).into_response()
}
pub(super) async fn get_session(
    State(s): State<Arc<Runtime>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if !rt::valid_id(&id) {
        return err("invalid", "invalid session id", StatusCode::BAD_REQUEST).into_response();
    }
    let Some(path) = discovery::path(&s.cfg, &id) else {
        // `valid_id` выше уже проверен, так что сюда путь не приводит; но
        // `unreachable!` в обработчике — это паника на кривом запросе.
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    };
    if !path.is_file() {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    }
    let d = s.data.lock().await;
    if let Some(session) = d.sessions.get(&id).cloned() {
        return Json(session).into_response();
    }
    drop(d);
    // Разбор журнала — блокирующее чтение, и `surface` с `title` — два разных
    // прохода по тому же файлу; `facts` берёт всё одним проходом (и кэшем).
    let read_path = path.clone();
    let mut session = match tokio::task::spawn_blocking(move || {
        let facts = crate::surface_facts::facts(&read_path, false);
        let surface = facts.surface.unwrap_or_else(|| "local".into());
        (surface, facts.title)
    })
    .await
    {
        Ok((surface, title)) => {
            let mut session = Session::new(id.clone(), surface);
            session.title = title;
            session
        }
        Err(e) => {
            return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
    };
    session.updated_at = discovery::mtime(&path);
    // Вставка — с повторной проверкой: за время разбора сессию мог создать
    // `message` или `ingest`, и их слот (с `state`/`run_id`/бюджетами) нельзя
    // затирать пустой заготовкой.
    let out = {
        let mut d = s.data.lock().await;
        d.events.entry(id.clone()).or_default();
        let entry = d.sessions.entry(id).or_insert(session);
        entry.clone()
    };
    Json(out).into_response()
}
/// Удаление сессии: журнал, каталоги вложений и запись в store. Только когда
/// ход не идёт — иначе удалили бы из-под работающего агента.
pub(super) async fn delete_session(
    State(s): State<Arc<Runtime>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if !rt::valid_id(&id) {
        return err("invalid", "invalid session id", StatusCode::BAD_REQUEST).into_response();
    }
    // Замок сессии берётся ДО проверки состояния и держится через удаление
    // файлов: иначе `launch` успевал стартовать в окне между проверкой и
    // `remove_file` и его журнал удаляли из-под работающего агента.
    let guard = s.session_lock(&id).await;
    let _guard = guard.lock().await;
    {
        let d = s.data.lock().await;
        if let Some(session) = d.sessions.get(&id) {
            if session.state == "running" || session.state == "stopping" {
                return err("busy", "session is running", StatusCode::CONFLICT).into_response();
            }
        }
    }
    let root = s.cfg.sessions_path();
    let stem = root.join(&id);
    let paths = vec![
        stem.with_extension("jsonl"),
        stem.with_extension("jsonl.tmp"),
        root.join(format!("{id}.out")),
        root.join(format!("{id}.attachments")),
        root.join(format!("{id}.attach")),
        root.join(format!("{id}.lock")),
    ];
    // Файловые удаления — блокирующие, каталог вложений бывает большим: не на
    // воркере.
    let removed = tokio::task::spawn_blocking(move || {
        let mut removed = false;
        for path in paths {
            if !path.exists() {
                continue;
            }
            let result = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match result {
                Ok(()) => removed = true,
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok::<_, String>(removed)
    })
    .await;
    match removed {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => {
            return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
        }
        Ok(Err(e)) => {
            return err("storage", &e, StatusCode::INTERNAL_SERVER_ERROR).into_response();
        }
        Err(e) => {
            return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
    }
    {
        let mut d = s.data.lock().await;
        d.sessions.remove(&id);
        d.events.remove(&id);
        d.messages.remove(&id);
        d.attachments.remove(&id);
        d.surface_tools.remove(&id);
        d.reviews.remove(&id);
    }
    // Всё, что держит состояние сессии: без этого `ensure_writer` для того же id
    // упрётся в `.lock`-flock от живого `Arc<log::Session>`, а `event_seq`
    // продолжит нумерацию удалённой сессии.
    s.writers.lock().await.remove(&id);
    s.event_seq.lock().await.remove(&id);
    s.controls.lock().await.remove(&id);
    s.review_gen.lock().await.remove(&id);
    s.locks.lock().await.remove(&id);
    if let Err(e) = s.drop_session(&id) {
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    Json(serde_json::json!({"deleted": id})).into_response()
}
