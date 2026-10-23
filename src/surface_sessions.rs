// Жизненный цикл сессии: список, создание, чтение, удаление и набор
// инструментов, который поверхность разрешает сессии.
//
// Список — самый дорогой маршрут поверхности (разбор журналов всех сессий),
// поэтому блокирующая работа здесь уходит в `spawn_blocking`, а мьютекс store
// на время разбора не держится: иначе `emit` любой сессии стоял бы в очереди.

use crate::cfg::Config;
use crate::surface_runtime::{self as rt, Runtime, Session};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
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
                    // Состояние процесса — правда store (его ставит launch/stop),
                    // остальное точнее в журнале.
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
    let mut d = s.data.lock().await;
    let Some(session) = d.sessions.get(&id) else {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    };
    let old = d.surface_tools.get(&id);
    if old == Some(&defs) {
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
    d.surface_tools.insert(id.clone(), defs.clone());
    if let Err(e) = s.save_surface_tools(&id, &defs) {
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
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
    let core: BTreeSet<String> = crate::dispatch::builtin_defs()
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
    let same = v.get("_surface_request").and_then(|x| x.as_str()) == Some(b.surface.as_str())
        && v.get("context_budget").and_then(Value::as_u64) == b.context_budget
        && v.get("context_target").and_then(Value::as_u64) == b.context_target;
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

pub(super) async fn create(State(s): State<Arc<Runtime>>, Json(b): Json<Create>) -> impl IntoResponse {
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
        return err("storage", "session is not serializable", StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
    };
    record["_surface_request"] = serde_json::Value::String(b.surface.clone());
    let command_payload = record.clone();
    let command_id = b.command_id.clone();
    let mut d = s.data.lock().await;
    // Второй заход: пока мы открывали журнал, параллельный запрос с тем же
    // command_id мог уже создать сессию. Тогда он и есть ответ, а свой пустой
    // журнал (реплик в нём нет — discovery его отбросит) убираем.
    match replay_create(&d, &b) {
        Ok(Some(response)) => {
            drop(d);
            drop(writer);
            let _ = std::fs::remove_file(s.cfg.sessions_path().join(format!("{id}.jsonl")));
            return (StatusCode::OK, Json(response)).into_response();
        }
        Ok(None) => {}
        Err(response) => {
            drop(d);
            drop(writer);
            let _ = std::fs::remove_file(s.cfg.sessions_path().join(format!("{id}.jsonl")));
            return *response;
        }
    }
    d.sessions.insert(id.clone(), session.clone());
    d.events.insert(id.clone(), vec![]);
    d.messages.insert(id.clone(), vec![]);
    d.commands.insert(b.command_id, record);
    s.writers.lock().await.insert(id.clone(), writer);
    if let Err(e) = s
        .save_session(&session)
        .and_then(|_| s.save_command(&command_id, &command_payload))
    {
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    s.event_seq.lock().await.insert(id, 0);
    (StatusCode::CREATED, Json(session)).into_response()
}
pub(super) async fn get_session(State(s): State<Arc<Runtime>>, Path(id): Path<String>) -> impl IntoResponse {
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
    let mut session = Session::new(id.clone(), discovery::surface(&path));
    session.title = discovery::title(&path);
    drop(d);
    s.data
        .lock()
        .await
        .sessions
        .insert(id.clone(), session.clone());
    s.data.lock().await.events.entry(id).or_default();
    Json(session).into_response()
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
    let mut removed = false;
    for path in [
        stem.with_extension("jsonl"),
        stem.with_extension("jsonl.tmp"),
        root.join(format!("{id}.out")),
        root.join(format!("{id}.attachments")),
        root.join(format!("{id}.attach")),
    ] {
        let exists = path.exists();
        if !exists {
            continue;
        }
        let result = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => removed = true,
            Err(e) => {
                return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
                    .into_response();
            }
        }
    }
    if !removed {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    }
    // Блокировка сессии на время удаления: без неё `message` мог увидеть её в
    // проверке «known» и упасть на `unwrap()` после того, как её уже стёрли.
    let guard = s.session_lock(&id).await;
    let _guard = guard.lock().await;
    {
        let mut d = s.data.lock().await;
        d.sessions.remove(&id);
        d.events.remove(&id);
        d.messages.remove(&id);
        d.attachments.remove(&id);
    }
    if let Err(e) = s.drop_session(&id) {
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    Json(serde_json::json!({"deleted": id})).into_response()
}
