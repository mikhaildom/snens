use crate::cfg::Config;
use crate::surface_runtime::{self as rt, Event, Runtime, Session};
#[path = "surface_discovery.rs"]
mod discovery;
#[path = "surface_history.rs"]
mod history;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{
        IntoResponse,
        sse::{Event as SseEvent, KeepAlive, Sse},
    },
    routing::{get, post},
};
use fs2::FileExt;
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::VecDeque, net::SocketAddr, sync::Arc, time::Duration};
use tokio::io::AsyncWriteExt;

pub async fn serve(cfg: Config, addr: SocketAddr, token: String) -> crate::R<()> {
    if !addr.ip().is_loopback() {
        return Err("surface server must bind to loopback".into());
    }
    let key = cfg.api_key()?;
    let llm = Arc::new(crate::llm::OpenAi::new(key)?);
    let dir = cfg.data_path().join("surface-api");
    std::fs::create_dir_all(&dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("server.lock"))?;
    lock.try_lock_exclusive()
        .map_err(|e| format!("surface store already locked: {e}"))?;
    let (tx, _) = tokio::sync::broadcast::channel(1024);
    let (mut store, db) = rt::open_store(&dir)?;
    for discovered in discovery::sessions(&cfg.data_path(), &store)? {
        store
            .sessions
            .entry(discovered.id.clone())
            .or_insert(discovered);
    }
    let mut discovered_ids = rt::event_sequences(&db)?;
    for (id, events) in &store.events {
        let high = events.iter().map(|event| event.seq).max().unwrap_or(0);
        discovered_ids
            .entry(id.clone())
            .and_modify(|seq| *seq = (*seq).max(high))
            .or_insert(high);
    }
    let had_running = store
        .sessions
        .values()
        .any(|s| s.state == "running" || s.state == "stopping");
    rt::interrupt_running(&mut store, &mut discovered_ids);
    if had_running {
        // Persist restart recovery before accepting requests.
        rt::persist_store(&db, &store)?;
        for (id, seq) in &discovered_ids {
            db.execute("INSERT INTO event_sequences(session_id,seq) VALUES(?1,?2) ON CONFLICT(session_id) DO UPDATE SET seq=excluded.seq", rusqlite::params![id, *seq as i64])?;
        }
    }
    let state = Arc::new(Runtime {
        data: tokio::sync::Mutex::new(store),
        db: std::sync::Mutex::new(db),
        tx,
        cfg,
        llm,
        locks: Default::default(),
        controls: Default::default(),
        pending_tools: Default::default(),
        writers: Default::default(),
        event_seq: tokio::sync::Mutex::new(discovered_ids),
        review_gen: Default::default(),
        approvals: Default::default(),
    });
    // Канал одобрений привилегированных действий: headless-сессия не может спросить
    // пароль, поэтому `sudo`-шим спрашивает согласие владельца кнопкой в чате.
    spawn_approval_server(state.clone());
    let app = Router::new()
        .route("/v1/sessions", get(list).post(create))
        .route("/v1/sessions/{id}", get(get_session))
        .route("/v1/sessions/{id}/messages", post(message))
        .route(
            "/v1/sessions/{id}/tools",
            axum::routing::put(register_tools),
        )
        .route(
            "/v1/sessions/{sid}/runs/{rid}/tools/{call_id}/result",
            post(tool_result),
        )
        .route("/v1/sessions/{id}/attachments", post(upload_attachment))
        .route("/v1/sessions/{id}/stop", post(stop))
        .route("/v1/sessions/{id}/approvals/{aid}", post(approve))
        .route("/v1/sessions/{id}/events", get(events))
        .route("/v1/sessions/{id}/history", get(history))
        .route("/v1/model", get(get_model).put(put_model))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(token, auth));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tokio::select! {
        r=axum::serve(listener,app)=>{r?;},
        _=async {
            #[cfg(unix)] {
                let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("install SIGTERM handler");
                tokio::select! { _ = tokio::signal::ctrl_c()=>{}, _ = term.recv()=>{} }
            }
            #[cfg(not(unix))] { let _ = tokio::signal::ctrl_c().await; }
        } => {}
    }
    shutdown(&state).await?;
    Ok(())
}
async fn shutdown(s: &Arc<Runtime>) -> crate::R<()> {
    {
        let controls = s.controls.lock().await;
        for control in controls.values() {
            let _ = control.cancel.send(true);
        }
    }
    let mut d = s.data.lock().await;
    let mut sequences = s.event_seq.lock().await;
    rt::interrupt_running(&mut d, &mut sequences);
    let db = s.db.lock().unwrap();
    for (id, seq) in sequences.iter() {
        db.execute("INSERT INTO event_sequences(session_id,seq) VALUES(?1,?2) ON CONFLICT(session_id) DO UPDATE SET seq=excluded.seq", rusqlite::params![id, *seq as i64])?;
    }
    rt::persist_store(&db, &d)
}

async fn auth(
    axum::extract::State(token): axum::extract::State<String>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> impl IntoResponse {
    let expected = format!("Bearer {token}");
    if req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|x| x.to_str().ok())
        != Some(&expected)
    {
        return (StatusCode::UNAUTHORIZED,Json(serde_json::json!({"error":{"code":"unauthorized","message":"Bearer token required"}}))).into_response();
    }
    next.run(req).await
}
fn err(code: &str, msg: &str, status: StatusCode) -> impl IntoResponse {
    (
        status,
        Json(serde_json::json!({"error":{"code":code,"message":msg}})),
    )
}
/// Текущая модель: перечитанная из конфига, а не снимок старта сервера.
/// Тем самым `snens model` и `/model` на любой поверхности видят одно и то же.
async fn get_model(State(s): State<Arc<Runtime>>) -> impl IntoResponse {
    Json(serde_json::json!({"model": s.cfg.model_now()})).into_response()
}

#[derive(Deserialize)]
struct ModelBody {
    model: String,
}
/// Сменить основную модель: правит `model = "..."` в конфиге и сразу делает её
/// текущей для новых ходов (перечитка в `run_loop`). Активный ход не трогаем.
async fn put_model(State(s): State<Arc<Runtime>>, Json(body): Json<ModelBody>) -> impl IntoResponse {
    let slug = body.model.trim();
    if slug.is_empty() {
        return err("invalid", "empty model slug", StatusCode::BAD_REQUEST).into_response();
    }
    // Пишем в тот же конфиг, что читает этот процесс (учёт `--config`).
    let mut cfg = s.cfg.clone();
    match cfg.set_model_now(slug) {
        Ok(path) => Json(serde_json::json!({"model": slug, "config": path.display().to_string()}))
            .into_response(),
        Err(e) => err("config", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response(),
    }
}

async fn list(State(s): State<Arc<Runtime>>) -> impl IntoResponse {
    let d = s.data.lock().await;
    match discovery::sessions(&s.cfg.data_path(), &d) {
        Ok(sessions) => {
            let sessions = sessions
                .into_iter()
                .filter(|session| discovery::path(&s.cfg, &session.id).is_some_and(|p| p.is_file()))
                .collect::<Vec<_>>();
            drop(d);
            let mut state = s.data.lock().await;
            for session in &sessions {
                state
                    .sessions
                    .entry(session.id.clone())
                    .or_insert_with(|| session.clone());
                state.events.entry(session.id.clone()).or_default();
            }
            Json(serde_json::json!({"sessions":sessions})).into_response()
        }
        Err(e) => err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response(),
    }
}
#[derive(Deserialize)]
struct ToolDefs {
    tools: Vec<serde_json::Value>,
}
async fn register_tools(
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
    if let Err(e) = s.persist(&d) {
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    (StatusCode::OK, Json(serde_json::json!({"tools":defs}))).into_response()
}
async fn validate_tools(
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
struct ToolResultBody {
    text: String,
    exit: i32,
}
async fn tool_result(
    State(s): State<Arc<Runtime>>,
    Path((sid, rid, call_id)): Path<(String, String, String)>,
    Json(body): Json<ToolResultBody>,
) -> impl IntoResponse {
    if body.text.len() > 65536 {
        return err(
            "invalid",
            "tool result exceeds 64 KiB",
            StatusCode::BAD_REQUEST,
        )
        .into_response();
    }
    let key = format!("{sid}/{rid}/{call_id}");
    let result = crate::agent::SurfaceToolResult {
        text: body.text.clone(),
        exit: body.exit,
    };
    let mut d = s.data.lock().await;
    if let Some(prev) = d.tool_results.get(&key) {
        return if prev == &serde_json::json!({"text":body.text,"exit":body.exit}) {
            (StatusCode::OK, Json(serde_json::json!({"accepted":true}))).into_response()
        } else {
            err(
                "result_conflict",
                "different tool result already accepted",
                StatusCode::CONFLICT,
            )
            .into_response()
        };
    }
    if !d.sessions.contains_key(&sid) {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    }
    if !d
        .sessions
        .get(&sid)
        .is_some_and(|x| x.run_id.as_deref() == Some(&rid) && x.state == "running")
    {
        return err("tool_expired", "run is not active", StatusCode::CONFLICT).into_response();
    }
    let pending_closed = s
        .pending_tools
        .lock()
        .await
        .get(&key)
        .is_some_and(|tx| tx.is_closed());
    if pending_closed {
        s.pending_tools.lock().await.remove(&key);
    }
    if !s.pending_tools.lock().await.contains_key(&key) {
        let known = d.events.get(&sid).is_some_and(|es| {
            es.iter().any(|e| {
                e.run_id.as_deref() == Some(&rid)
                    && e.kind == "tool.requested"
                    && e.fields.get("call_id").and_then(|v| v.as_str()) == Some(call_id.as_str())
            })
        });
        return if known {
            err(
                "tool_expired",
                "tool call expired or was cancelled",
                StatusCode::CONFLICT,
            )
            .into_response()
        } else {
            err("not_found", "tool call not found", StatusCode::NOT_FOUND).into_response()
        };
    }
    d.tool_results.insert(
        key.clone(),
        serde_json::json!({"text":body.text,"exit":body.exit}),
    );
    d.tool_requests
        .insert(key.clone(), serde_json::json!({"status":"completed"}));
    if let Err(e) = s.persist(&d) {
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    let tx = s
        .pending_tools
        .lock()
        .await
        .remove(&key)
        .expect("pending tool checked under serialized result handler");
    if tx.send(result).is_err() {
        d.tool_results.remove(&key);
        d.tool_requests
            .insert(key.clone(), serde_json::json!({"status":"expired"}));
        if let Err(e) = s.persist(&d) {
            return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
        return err(
            "tool_expired",
            "tool waiter expired before result delivery",
            StatusCode::CONFLICT,
        )
        .into_response();
    }
    drop(d);
    (StatusCode::OK, Json(serde_json::json!({"accepted":true}))).into_response()
}
#[derive(Deserialize)]
struct Create {
    command_id: String,
    surface: String,
    /// Необязательный per-session бюджет уборки: поверхность с иной ценой
    /// контекста не должна диктовать порог остальным сессиям. Ноль — общий
    /// порог из конфига.
    #[serde(default)]
    context_budget: Option<u64>,
    #[serde(default)]
    context_target: Option<u64>,
}
async fn create(State(s): State<Arc<Runtime>>, Json(b): Json<Create>) -> impl IntoResponse {
    if !rt::valid_id(&b.command_id) || b.surface.is_empty() || b.surface.len() > 80 {
        return err(
            "invalid",
            "invalid command_id or surface",
            StatusCode::BAD_REQUEST,
        )
        .into_response();
    }
    let mut d = s.data.lock().await;
    if let Some(v) = d.commands.get(&b.command_id) {
        // Повтор команды обязан совпасть и по бюджету: иначе тихий replay
        // вернул бы сессию с другим порогом уборки.
        let same = v.get("_surface_request").and_then(|x| x.as_str()) == Some(b.surface.as_str())
            && v.get("context_budget").and_then(Value::as_u64) == b.context_budget
            && v.get("context_target").and_then(Value::as_u64) == b.context_target;
        if !same {
            return err(
                "idempotency_conflict",
                "command_id reused with different payload",
                StatusCode::CONFLICT,
            )
            .into_response();
        }
        let mut response = v.clone();
        if let Some(obj) = response.as_object_mut() {
            obj.remove("_surface_request");
        }
        return (StatusCode::OK, Json(response)).into_response();
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
    d.sessions.insert(id.clone(), session.clone());
    d.events.insert(id.clone(), vec![]);
    d.messages.insert(id.clone(), vec![]);
    let mut record = serde_json::to_value(&session).unwrap();
    record["_surface_request"] = serde_json::Value::String(b.surface.clone());
    d.commands.insert(b.command_id, record);
    s.writers.lock().await.insert(id.clone(), writer);
    if let Err(e) = s.persist(&d) {
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    s.event_seq.lock().await.insert(id, 0);
    (StatusCode::CREATED, Json(session)).into_response()
}
async fn get_session(State(s): State<Arc<Runtime>>, Path(id): Path<String>) -> impl IntoResponse {
    if !rt::valid_id(&id) {
        return err("invalid", "invalid session id", StatusCode::BAD_REQUEST).into_response();
    }
    let Some(path) = discovery::path(&s.cfg, &id) else {
        unreachable!()
    };
    if !path.is_file() {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    }
    let d = s.data.lock().await;
    if let Some(session) = d.sessions.get(&id).cloned() {
        return Json(session).into_response();
    }
    let surface = discovery::load_meta_surface(&path).unwrap_or_else(|| "legacy".into());
    let session = Session::new(id.clone(), surface);
    drop(d);
    s.data
        .lock()
        .await
        .sessions
        .insert(id.clone(), session.clone());
    s.data.lock().await.events.entry(id).or_default();
    Json(session).into_response()
}
#[derive(Deserialize)]
struct UploadQuery {
    filename: String,
    command_id: String,
}
fn attachment_response(
    id: &str,
    name: &str,
    path: &str,
    mime: &str,
    bytes: usize,
) -> serde_json::Value {
    serde_json::json!({"attachment_id":id,"name":name,"path":path,"mime":mime,"bytes":bytes})
}
async fn upload_attachment(
    State(s): State<Arc<Runtime>>,
    Path(id): Path<String>,
    Query(q): Query<UploadQuery>,
    request: axum::extract::Request,
) -> impl IntoResponse {
    if !rt::valid_id(&id)
        || !rt::valid_id(&q.command_id)
        || q.filename.is_empty()
        || q.filename.len() > 255
        || q.filename.contains('/')
        || q.filename.contains('\\')
    {
        return err(
            "invalid",
            "invalid attachment id or filename",
            StatusCode::BAD_REQUEST,
        )
        .into_response();
    }
    let max = s.cfg.max_attachment_bytes;
    let body = match axum::body::to_bytes(request.into_body(), max.saturating_add(1)).await {
        Ok(b) if b.len() <= max => b,
        _ => {
            return err(
                "too_large",
                "attachment exceeds configured size limit",
                StatusCode::PAYLOAD_TOO_LARGE,
            )
            .into_response();
        }
    };
    let digest = format!("{:x}", Sha256::digest(&body));
    let _guard = s.session_lock(&id).await.lock_owned().await;
    let mut d = s.data.lock().await;
    if !d.sessions.contains_key(&id) {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    }
    if let Some(existing) = d
        .attachments
        .get(&id)
        .and_then(|m| m.values().find(|a| a.command_id == q.command_id))
    {
        if existing.filename == q.filename && existing.sha256 == digest {
            let p = &existing.part;
            return (
                StatusCode::OK,
                Json(attachment_response(
                    &existing.attachment_id,
                    &existing.name,
                    &p.path,
                    &p.mime,
                    p.bytes,
                )),
            )
                .into_response();
        }
        return err(
            "idempotency_conflict",
            "command_id reused with different upload",
            StatusCode::CONFLICT,
        )
        .into_response();
    }
    let original = std::path::Path::new(&q.filename);
    let candidate = crate::attach::attachment_kind(original);
    let (kind, mime) = candidate.unwrap_or_else(|| {
        let mime = match original
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "txt" | "md" | "csv" | "log" => "text/plain",
            "json" => "application/json",
            "pdf" => "application/pdf",
            _ => "application/octet-stream",
        };
        ("file", mime)
    });
    let aid = uuid::Uuid::new_v4().to_string();
    let extension = original
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| e.len() <= 16 && e.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let dir = s.cfg.sessions_path().join(format!("{id}.attachments"));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let path = dir.join(format!("{aid}{extension}"));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let write_result = async {
        let mut f = options.open(&path).await?;
        f.write_all(&body).await?;
        f.sync_all().await
    }
    .await;
    if let Err(e) = write_result {
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    let (width, height) = if kind == "image" {
        crate::attach::image_dimensions(&body, mime)
    } else {
        (0, 0)
    };
    if kind == "image" && (width == 0 || height == 0) {
        let _ = tokio::fs::remove_file(&path).await;
        return err(
            "invalid_image",
            "image extension/header mismatch",
            StatusCode::BAD_REQUEST,
        )
        .into_response();
    }
    let name = q
        .filename
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>();
    let part = crate::log::Part {
        kind: kind.into(),
        path: path.to_string_lossy().into_owned(),
        mime: mime.into(),
        bytes: body.len(),
        width,
        height,
    };
    let attachment = rt::Attachment {
        attachment_id: aid.clone(),
        command_id: q.command_id,
        filename: q.filename,
        name: name.clone(),
        part: part.clone(),
        sha256: digest,
    };
    d.attachments
        .entry(id.clone())
        .or_default()
        .insert(aid.clone(), attachment);
    if let Err(e) = s.persist(&d) {
        d.attachments.get_mut(&id).and_then(|m| m.remove(&aid));
        let _ = tokio::fs::remove_file(&path).await;
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    (
        StatusCode::CREATED,
        Json(attachment_response(
            &aid, &name, &part.path, &part.mime, part.bytes,
        )),
    )
        .into_response()
}
fn resolve_attachments(
    d: &rt::Store,
    sid: &str,
    ids: &[String],
) -> Result<Vec<crate::log::Part>, Box<axum::response::Response>> {
    let mut result = Vec::new();
    for id in ids {
        let Some(a) = d.attachments.get(sid).and_then(|m| m.get(id)) else {
            return Err(Box::new(
                err(
                    "not_found",
                    "attachment not found in session",
                    StatusCode::BAD_REQUEST,
                )
                .into_response(),
            ));
        };
        result.push(a.part.clone());
    }
    Ok(result)
}

#[derive(Deserialize)]
struct Send {
    command_id: String,
    text: String,
    #[serde(default)]
    attachment_ids: Vec<String>,
}
fn same_message_payload(existing: &rt::Message, text: &str, attachment_ids: &[String]) -> bool {
    existing.text == text && existing.attachment_ids == attachment_ids
}

async fn ensure_writer(s: &Arc<Runtime>, id: &str) -> crate::R<Arc<crate::log::Session>> {
    if let Some(writer) = s.writers.lock().await.get(id).cloned() {
        return Ok(writer);
    }
    let path = discovery::path(&s.cfg, id).ok_or("invalid session id")?;
    let marker = discovery::load_meta_surface(&path);
    let surface = marker.as_deref().filter(|value| *value != "legacy");
    let writer = Arc::new(crate::log::Session::open_surface(
        &s.cfg.sessions_path(),
        id,
        surface,
    )?);
    let mut writers = s.writers.lock().await;
    Ok(writers
        .entry(id.to_string())
        .or_insert_with(|| writer.clone())
        .clone())
}

async fn message(
    State(s): State<Arc<Runtime>>,
    Path(id): Path<String>,
    Json(b): Json<Send>,
) -> impl IntoResponse {
    if !rt::valid_id(&id)
        || !rt::valid_id(&b.command_id)
        || (b.text.trim().is_empty() && b.attachment_ids.is_empty())
    {
        return err(
            "invalid",
            "invalid id or empty message",
            StatusCode::BAD_REQUEST,
        )
        .into_response();
    }
    let _guard = s.session_lock(&id).await.lock_owned().await;
    let known = s.data.lock().await.sessions.contains_key(&id);
    if !known {
        let Some(path) = discovery::path(&s.cfg, &id).filter(|path| path.is_file()) else {
            return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
        };
        let session = Session::new(
            id.clone(),
            discovery::load_meta_surface(&path).unwrap_or_else(|| "legacy".into()),
        );
        let mut data = s.data.lock().await;
        data.sessions.entry(id.clone()).or_insert(session);
        data.events.entry(id.clone()).or_default();
        if let Err(error) = s.persist(&data) {
            return err(
                "storage",
                &error.to_string(),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
            .into_response();
        }
    }
    if let Err(e) = ensure_writer(&s, &id).await {
        return err("session_owned", &e.to_string(), StatusCode::CONFLICT).into_response();
    }
    let mut d = s.data.lock().await;
    if let Some(m) = d
        .messages
        .get(&id)
        .and_then(|v| v.iter().find(|m| m.command_id == b.command_id))
    {
        if !same_message_payload(m, &b.text, &b.attachment_ids) {
            return err(
                "idempotency_conflict",
                "command_id reused with different payload",
                StatusCode::CONFLICT,
            )
            .into_response();
        }
        return (
            StatusCode::OK,
            Json(serde_json::json!({"message_id":m.id,"run_id":m.run_id,"state":m.state})),
        )
            .into_response();
    }
    let parts = match resolve_attachments(&d, &id, &b.attachment_ids) {
        Ok(parts) => parts,
        Err(response) => return *response,
    };
    let busy = d
        .sessions
        .get(&id)
        .map(|x| x.state == "running" || x.state == "stopping")
        .unwrap_or(false);
    let run_id = if busy {
        d.sessions.get(&id).and_then(|x| x.run_id.clone())
    } else {
        Some(uuid::Uuid::new_v4().to_string())
    };
    let mid = uuid::Uuid::new_v4().to_string();
    let state = if busy { "accepted" } else { "applied" };
    d.messages.entry(id.clone()).or_default().push(rt::Message {
        id: mid.clone(),
        command_id: b.command_id.clone(),
        text: b.text.clone(),
        attachment_ids: b.attachment_ids.clone(),
        state: state.into(),
        run_id: run_id.clone(),
    });
    let result = serde_json::json!({"message_id":mid,"run_id":run_id,"state":state});
    let surface = d.sessions.get(&id).unwrap().surface.clone();
    d.commands.insert(b.command_id, result.clone());
    if !busy {
        let x = d.sessions.get_mut(&id).unwrap();
        x.state = "running".into();
        x.run_id = run_id.clone()
    }
    if let Err(e) = s.persist(&d) {
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    drop(d);
    let _ = s
        .emit(
            &id,
            run_id.clone(),
            if busy {
                "message.accepted"
            } else {
                "message.applied"
            },
            serde_json::json!({"message_id":mid,"text":b.text}),
        )
        .await;
    if !busy {
        launch(s.clone(), id, run_id.unwrap(), b.text, surface, parts).await
    } else if let Some(c) = s.controls.lock().await.get(&id) {
        let _ = c.queued.send(crate::agent::QueuedInput {
            text: b.text.clone(),
            parts,
        });
    }
    (StatusCode::ACCEPTED, Json(result)).into_response()
}
async fn drain_surface_signals<F, Fut>(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<(String, serde_json::Value)>,
    mut emit: F,
) -> crate::R<()>
where
    F: FnMut(String, serde_json::Value) -> Fut,
    Fut: std::future::Future<Output = crate::R<()>>,
{
    let mut first_error = None;
    while let Some((kind, fields)) = rx.recv().await {
        if let Err(error) = emit(kind, fields).await {
            first_error.get_or_insert_with(|| error.to_string());
        }
    }
    match first_error {
        Some(error) => Err(std::io::Error::other(error).into()),
        None => Ok(()),
    }
}

async fn finalize_launch_error(s: &Arc<Runtime>, id: &str, run: &str, message: &str) {
    {
        let mut data = s.data.lock().await;
        if let Some(session) = data.sessions.get_mut(id) {
            session.state = "error".into();
            session.run_id = None;
        }
        for queued in data
            .messages
            .entry(id.to_string())
            .or_default()
            .iter_mut()
            .filter(|message| message.run_id.as_deref() == Some(run) && message.state == "accepted")
        {
            queued.state = "cancelled".into();
        }
        let _ = s.persist(&data);
    }
    s.controls.lock().await.remove(id);
    let _ = s
        .emit(
            id,
            Some(run.to_string()),
            "notice",
            serde_json::json!({"text":message}),
        )
        .await;
    let _ = s
        .emit(
            id,
            Some(run.to_string()),
            "run.finished",
            serde_json::json!({"status":"error"}),
        )
        .await;
}

/// Поднять сокет одобрений. Шим `sudo` в headless-сессии подключается к нему,
/// шлёт заявку (session/cwd/argv) и ждёт решение. Ядро эмитит `approval.requested`
/// в сессию владельца, а решение приходит POST'ом и закрывает канал ответа.
fn spawn_approval_server(s: Arc<Runtime>) {
    let timeout = s.cfg.approval_timeout_seconds;
    if timeout == 0 {
        return;
    }
    #[cfg(not(unix))]
    {
        let _ = (s, timeout);
    }
    #[cfg(unix)]
    tokio::spawn(async move {
        let path = crate::approval::sock_path(&s.cfg.sessions_path());
        let _ = std::fs::remove_file(&path);
        let Ok(listener) = tokio::net::UnixListener::bind(&path) else {
            eprintln!("[snens] не удалось поднять сокет одобрений {}", path.display());
            return;
        };
        let _ = std::fs::set_permissions(
            &path,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        );
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let s = s.clone();
            tokio::spawn(async move {
                let _ = handle_approval_conn(s, stream, timeout).await;
            });
        }
    });
}

#[cfg(unix)]
async fn handle_approval_conn(
    s: Arc<Runtime>,
    mut stream: tokio::net::UnixStream,
    timeout: u64,
) -> crate::R<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut line = String::new();
    BufReader::new(&mut stream).read_line(&mut line).await?;
    let req: crate::approval::Request = serde_json::from_str(line.trim_end())?;
    // Заявка привязана к сессии: решение ищется по её событиям. Неизвестная
    // сессия — сразу отказ, без карточки (нельзя показать владельцу — нет чата).
    let known = s.data.lock().await.sessions.contains_key(&req.session);
    let answer = if !known {
        false
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = tokio::sync::oneshot::channel();
        s.approvals.lock().await.insert(id.clone(), tx);
        // Точное действие в карточке: argv как есть, плюс cwd и сессия.
        let detail = req
            .argv
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        let _ = s
            .emit(
                &req.session,
                None,
                "approval.requested",
                serde_json::json!({
                    "approval_id":id,
                    "command":format!("sudo {detail}"),
                    "cwd":req.cwd,
                }),
            )
            .await;
        let decided = tokio::time::timeout(Duration::from_secs(timeout), rx).await;
        s.approvals.lock().await.remove(&id);
        match decided {
            Ok(Ok(allow)) => allow,
            // Таймаут или обрыв: считаем отказом, шим получит «не одобрено».
            _ => false,
        }
    };
    let _ = stream
        .write_all(if answer { b"allow\n" } else { b"deny\n" })
        .await;
    let _ = stream.flush().await;
    Ok(())
}

/// Показать argv в карточке так, как его увидел бы командный интерпретатор:
/// элементы с пробелами оборачиваем в кавычки, чтобы владелец видел точную
/// границу аргумента, а не «одну длинную команду».
fn shell_quote(arg: &str) -> String {
    if arg.is_empty() || arg.chars().any(|c| c.is_whitespace() || c == '"' || c == '\'') {
        format!("{:?}", arg)
    } else {
        arg.to_string()
    }
}

#[derive(Deserialize)]
struct ApprovalDecision {
    allow: bool,
}

/// Решение владельца по заявке: `{allow: true|false}`.
async fn approve(
    State(s): State<Arc<Runtime>>,
    Path((id, aid)): Path<(String, String)>,
    Json(b): Json<ApprovalDecision>,
) -> impl IntoResponse {
    if !rt::valid_id(&id) || aid.is_empty() {
        return err("invalid", "invalid id", StatusCode::BAD_REQUEST).into_response();
    }
    let Some(tx) = s.approvals.lock().await.remove(&aid) else {
        // Уже решено/истекло: повтор идемпотентен как «нет» — карточку давно сняли.
        return err("gone", "approval is no longer pending", StatusCode::GONE).into_response();
    };
    let _ = s
        .emit(
            &id,
            None,
            "approval.resolved",
            serde_json::json!({"approval_id":aid,"allow":b.allow}),
        )
        .await;
    let _ = tx.send(b.allow);
    (StatusCode::OK, Json(serde_json::json!({"resolved":true}))).into_response()
}

/// Пора ли запускать авторевью: набрался порог ходов и с прошлого ревью добавлено
/// не меньше порога новых ходов. Простой (`review_idle_seconds`) проверяется
/// отдельно таймером; эта функция — только счётная часть.
fn review_due(min_turns: usize, turns: usize, last_review_turns: u64) -> bool {
    min_turns > 0 && turns >= min_turns && (turns as u64).saturating_sub(last_review_turns) >= min_turns as u64
}

/// Число ходов сессии (ответов ассистента) по её JSONL.
fn session_turns(cfg: &Config, id: &str) -> usize {
    let path = cfg.sessions_path().join(format!("{id}.jsonl"));
    crate::log::read_file(&path)
        .map(|rs| {
            rs.iter()
                .filter(|r| matches!(r, crate::log::Rec::Assistant { .. }))
                .count()
        })
        .unwrap_or(0)
}

/// Планирует авторевью после конца хода. Ревью запускается только при двух
/// условиях сразу: набралось `review_min_turns` ходов и сессия простояла
/// `review_idle_seconds` без новой работы. Таймер запоминает поколение работы:
/// если за время простоя начался новый ход, ревью отменяется — оно дождётся
/// следующей паузы. `review_idle_seconds = 0` выключает авторевью.
fn schedule_review(s: Arc<Runtime>, id: String) {
    let idle = s.cfg.review_idle_seconds;
    if idle == 0 {
        return;
    }
    let min = s.cfg.review_min_turns;
    tokio::spawn(async move {
        let gen = s.review_gen.lock().await.get(&id).copied().unwrap_or(0);
        // Планировщик стартует на конце хода, а не на его старте: поколение только
        // что установлено. Ждём простой.
        tokio::time::sleep(Duration::from_secs(idle)).await;
        // Простой нарушен — новая работа перезапустит планировщик сама.
        if s.review_gen.lock().await.get(&id).copied().unwrap_or(0) != gen {
            return;
        }
        // Сессия всё ещё занята? Планировщик хода снял бы поколение, но перестрахуемся.
        if s.data
            .lock()
            .await
            .sessions
            .get(&id)
            .map(|x| x.state != "idle")
            .unwrap_or(false)
        {
            return;
        }
        let turns = session_turns(&s.cfg, &id);
        let last = s.data.lock().await.reviews.get(&id).copied().unwrap_or(0);
        if !review_due(min, turns, last) {
            return;
        }
        // Записываем порог до запуска: даже если процесс ревью не стартует, повтор
        // немедленно не случится — только после нового роста числа ходов.
        {
            let mut d = s.data.lock().await;
            d.reviews.insert(id.clone(), turns as u64);
            let _ = s.persist(&d);
        }
        if let Err(error) = crate::spawn_review(&s.cfg, &id) {
            s.emit(
                &id,
                None,
                "notice",
                serde_json::json!({"text":format!("не удалось запустить авторевью: {error}")}),
            )
            .await
            .ok();
            return;
        }
        s.emit(
            &id,
            None,
            "notice",
            serde_json::json!({"text":"запущено саморевью сессии (фоном)"}),
        )
        .await
        .ok();
    });
}

/// Порог уборки контекста для запуска: per-session значение сессии, если оно
/// задано, иначе общее из конфига. Вынесено из `launch` ради теста: развилка
/// «сессия диктует свой бюджет или живёт на общем» должна быть проверяемой.
fn session_cleanup_limits(cfg: &Config, session: Option<&Session>) -> (u64, u64) {
    let budget = session
        .and_then(|s| s.context_budget)
        .filter(|b| *b > 0)
        .unwrap_or(cfg.context_budget);
    let target = session
        .and_then(|s| s.context_target)
        .filter(|t| *t > 0)
        .unwrap_or(cfg.context_target);
    (budget, target)
}

async fn launch(
    s: Arc<Runtime>,
    id: String,
    run: String,
    text: String,
    surface: String,
    parts: Vec<crate::log::Part>,
) {
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let (queue_tx, queue_rx) = tokio::sync::mpsc::unbounded_channel();
    let queue_rx = Arc::new(tokio::sync::Mutex::new(queue_rx));
    s.controls.lock().await.insert(
        id.clone(),
        rt::Control {
            cancel: cancel_tx,
            queued: queue_tx,
        },
    );
    // Новое поколение работы: отложенный таймер авторевью, если ждал, увидит
    // изменение и не запустится посреди этого хода.
    *s.review_gen.lock().await.entry(id.clone()).or_insert(0) += 1;
    let mut cfg = s.cfg.clone();
    // Per-session порог уборки: у поверхности своя цена контекста (групповой
    // чат живёт долго и просит 100k), у остальных сессий остаётся общий порог.
    let session = s.data.lock().await.sessions.get(&id).cloned();
    let (budget, target) = session_cleanup_limits(&cfg, session.as_ref());
    cfg.context_budget = budget;
    cfg.context_target = target;
    let llm = s.llm.clone();
    let surface_tools = s
        .data
        .lock()
        .await
        .surface_tools
        .get(&id)
        .cloned()
        .unwrap_or_default();
    let (tool_tx, mut tool_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::agent::SurfaceToolRequest>();
    let broker_s = s.clone();
    let broker_id = id.clone();
    let broker_run = run.clone();
    tokio::spawn(async move {
        while let Some(req) = tool_rx.recv().await {
            let key = format!("{broker_id}/{broker_run}/{}", req.call_id);
            {
                let mut d = broker_s.data.lock().await;
                d.tool_requests.insert(
                    key.clone(),
                    serde_json::json!({"status":"pending","name":req.name,"args":req.args}),
                );
                if broker_s.persist(&d).is_err() {
                    d.tool_requests.remove(&key);
                    return;
                }
            }
            broker_s
                .pending_tools
                .lock()
                .await
                .insert(key.clone(), req.result);
            let _ = broker_s
                .emit(
                    &broker_id,
                    Some(broker_run.clone()),
                    "tool.requested",
                    serde_json::json!({"call_id":req.call_id,"name":req.name,"args":req.args}),
                )
                .await;
            while !broker_s
                .pending_tools
                .lock()
                .await
                .get(&key)
                .is_none_or(|tx| tx.is_closed())
            {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            broker_s.pending_tools.lock().await.remove(&key);
            let mut d = broker_s.data.lock().await;
            if d.tool_requests
                .get(&key)
                .and_then(|v| v.get("status"))
                .and_then(|v| v.as_str())
                == Some("pending")
            {
                d.tool_requests
                    .insert(key.clone(), serde_json::json!({"status":"expired"}));
                let _ = broker_s.persist(&d);
            }
        }
    });
    tokio::spawn(async move {
        let sess = match s.writers.lock().await.get(&id).cloned() {
            Some(writer) => writer,
            None => {
                finalize_launch_error(&s, &id, &run, "session writer missing").await;
                return;
            }
        };
        let start_records = match sess.read() {
            Ok(records) => records.len(),
            Err(error) => {
                finalize_launch_error(&s, &id, &run, &error.to_string()).await;
                return;
            }
        };
        if let Err(error) = sess.append(crate::log::Rec::User {
            ts: crate::log::now(),
            text,
            source: "surface".into(),
            parts,
        }) {
            finalize_launch_error(&s, &id, &run, &error.to_string()).await;
            return;
        }
        let (signal_tx, signal_rx) = tokio::sync::mpsc::unbounded_channel();
        let observe = move |sig: crate::agent::SurfaceSignal| {
            let (kind, fields) = match sig {
                crate::agent::SurfaceSignal::Delta { text, kind } => (
                    "assistant.delta",
                    serde_json::json!({"text":text,"kind":kind}),
                ),
                crate::agent::SurfaceSignal::ToolStarted {
                    name,
                    call_id,
                    args,
                } => (
                    "tool.started",
                    serde_json::json!({"name":name,"call_id":call_id,"args":args}),
                ),
                crate::agent::SurfaceSignal::ToolCompleted {
                    name,
                    call_id,
                    exit,
                    preview,
                } => (
                    "tool.completed",
                    serde_json::json!({"name":name,"call_id":call_id,"exit":exit,"preview":preview}),
                ),
                crate::agent::SurfaceSignal::Notice(text) => {
                    ("notice", serde_json::json!({"text":text}))
                }
            };
            // An unbounded FIFO preserves callback order without blocking the agent.
            let _ = signal_tx.send((kind.to_owned(), fields));
        };
        let relay_s = s.clone();
        let relay_id = id.clone();
        let relay_run = run.clone();
        let relay = tokio::spawn(async move {
            drain_surface_signals(signal_rx, |kind, fields| {
                let s = relay_s.clone();
                let id = relay_id.clone();
                let run = relay_run.clone();
                async move { s.emit(&id, Some(run), &kind, fields).await.map(|_| ()) }
            })
            .await
        });
        let _ = s
            .emit(&id, Some(run.clone()), "run.started", serde_json::json!({}))
            .await;
        let result = crate::agent::run_agent_surface(
            &sess,
            &cfg,
            &*llm,
            &crate::render::Ui::plain(),
            crate::agent::SurfaceRunOptions {
                observe: &observe,
                cancel: Some(cancel_rx),
                queued: Some(queue_rx.clone()),
                surface: Some(&surface),
                surface_tools,
                tool_tx: Some(tool_tx),
            },
        )
        .await;
        // Drop the callback's sender before joining: every accepted signal is drained
        // before completion events, including on cancellation.
        drop(observe);
        let relay_error = match relay.await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(error) => Some(format!("signal relay task failed: {error}")),
        };
        let result_error = result.as_ref().err().map(|e| e.to_string());
        let status = if relay_error.is_some() {
            "error"
        } else if result.is_ok() {
            "ok"
        } else if result_error.as_deref() == Some("cancelled") {
            "cancelled"
        } else {
            "error"
        };
        let error = relay_error.or(result_error);
        let assistant = sess
            .read()
            .unwrap_or_default()
            .into_iter()
            .skip(start_records + 1)
            .rev()
            .find_map(|r| {
                if let crate::log::Rec::Assistant {
                    text: Some(text),
                    tool_calls,
                    ..
                } = r
                {
                    if tool_calls.is_empty() {
                        Some(text)
                    } else {
                        None
                    }
                } else {
                    None
                }
            });
        if let Some(text) = assistant {
            let _ = s
                .emit(
                    &id,
                    Some(run.clone()),
                    "assistant.completed",
                    serde_json::json!({"text":text}),
                )
                .await;
        }
        let _ = s
            .emit(
                &id,
                Some(run.clone()),
                "run.finished",
                serde_json::json!({"status":status,"error":error}),
            )
            .await;
        s.controls.lock().await.remove(&id);
        let mut d = s.data.lock().await;
        if let Some(x) = d.sessions.get_mut(&id) {
            x.state = "idle".into();
            x.run_id = None
        }
        let _ = s.persist(&d);
        drop(d);
        // Ход завершён и сессия снова idle — планируем авторевью после простоя.
        // Считаем только успешные ходы: отменённый/ошибочный ход ревью не стоит.
        if status == "ok" {
            schedule_review(s.clone(), id.clone());
        }
    });
}
#[derive(Deserialize)]
struct Stop {
    command_id: String,
    run_id: String,
}
async fn stop(
    State(s): State<Arc<Runtime>>,
    Path(id): Path<String>,
    Json(b): Json<Stop>,
) -> impl IntoResponse {
    if !rt::valid_id(&id) || !rt::valid_id(&b.command_id) || !rt::valid_id(&b.run_id) {
        return err("invalid", "invalid id", StatusCode::BAD_REQUEST).into_response();
    }
    let mut d = s.data.lock().await;
    let Some(x) = d.sessions.get_mut(&id) else {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    };
    if x.run_id.as_deref() != Some(&b.run_id) {
        return err("conflict", "run_id is not current", StatusCode::CONFLICT).into_response();
    }
    x.state = "stopping".into();
    let out = x.clone();
    if let Some(ms) = d.messages.get_mut(&id) {
        for m in ms
            .iter_mut()
            .filter(|m| m.run_id.as_deref() == Some(&b.run_id) && m.state == "accepted")
        {
            m.state = "cancelled".into()
        }
    }
    let _ = s.persist(&d);
    drop(d);
    if let Some(c) = s.controls.lock().await.get(&id) {
        let _ = c.cancel.send(true);
    }
    Json(out).into_response()
}
#[derive(Deserialize)]
struct After {
    after: Option<u64>,
}
async fn history(State(s): State<Arc<Runtime>>, Path(id): Path<String>) -> impl IntoResponse {
    if !rt::valid_id(&id) {
        return err("invalid", "invalid session id", StatusCode::BAD_REQUEST).into_response();
    }
    let d = s.data.lock().await;
    let session = d.sessions.get(&id);
    let Some(path) = discovery::path(&s.cfg, &id) else {
        unreachable!()
    };
    if session.is_none() && !path.is_file() {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    }
    let messages = match history::read(&path, &s.cfg) {
        Ok(messages) => messages,
        Err(e) => {
            return err(
                "history_read",
                &e.to_string(),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
            .into_response();
        }
    };
    let events = d.events.get(&id).cloned().unwrap_or_default();
    let cursor = s
        .event_seq
        .lock()
        .await
        .get(&id)
        .copied()
        .unwrap_or_else(|| events.iter().map(|event| event.seq).max().unwrap_or(0));
    Json(serde_json::json!({"cursor":cursor,"events":events,"messages":messages})).into_response()
}
async fn events(
    State(s): State<Arc<Runtime>>,
    Path(id): Path<String>,
    Query(q): Query<After>,
) -> impl IntoResponse {
    let after = q.after.unwrap_or(0);
    let Some((rx, initial)) = snapshot_and_subscribe(&s, &id, after).await else {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    };
    let (initial, cursor) = initial;
    let live = session_event_stream(s, id, rx, cursor, initial)
        .map(|event| Ok::<_, std::convert::Infallible>(sse(event)));
    Sse::new(live)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

async fn snapshot_and_subscribe(
    s: &Runtime,
    id: &str,
    after: u64,
) -> Option<(
    tokio::sync::broadcast::Receiver<Event>,
    (VecDeque<Event>, u64),
)> {
    let d = s.data.lock().await;
    if !d.sessions.contains_key(id) {
        return None;
    }
    let initial = d
        .events
        .get(id)
        .into_iter()
        .flatten()
        .filter(|event| event.seq > after)
        .cloned()
        .collect();
    // emit also needs data lock, so no publish can cross this boundary.
    let rx = s.tx.subscribe();
    Some((rx, (initial, after)))
}

fn session_event_stream(
    s: Arc<Runtime>,
    id: String,
    rx: tokio::sync::broadcast::Receiver<Event>,
    cursor: u64,
    initial: VecDeque<Event>,
) -> impl futures_util::Stream<Item = Event> {
    stream::unfold(
        (s, id, rx, cursor, initial),
        |(s, id, mut rx, mut cursor, mut pending)| async move {
            loop {
                if let Some(event) = pending.pop_front() {
                    if event.session_id == id && event.seq > cursor {
                        cursor = event.seq;
                        return Some((event, (s, id, rx, cursor, pending)));
                    }
                    continue;
                }
                match rx.recv().await {
                    Ok(event) if event.session_id == id && event.seq > cursor => {
                        cursor = event.seq;
                        return Some((event, (s, id, rx, cursor, pending)));
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Broadcast is only a wake-up/low-latency path. Replay from the
                        // durable in-memory store to close every overflow gap.
                        let d = s.data.lock().await;
                        pending.extend(
                            d.events
                                .get(&id)
                                .into_iter()
                                .flatten()
                                .filter(|event| event.seq > cursor)
                                .cloned(),
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    )
}
fn sse(e: Event) -> SseEvent {
    SseEvent::default()
        .id(e.seq.to_string())
        .json_data(e)
        .unwrap_or_else(|_| SseEvent::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use tokio::sync::{Mutex, broadcast, oneshot};

    #[test]
    fn session_cleanup_limits_prefer_own_budget_over_config() {
        let mut cfg = Config::default();
        cfg.context_budget = 500_000;
        cfg.context_target = 350_000;
        // Без своей сессии — общий порог конфига.
        assert_eq!(session_cleanup_limits(&cfg, None), (500_000, 350_000));
        // Сессия без бюджета — тоже общий порог.
        assert_eq!(
            session_cleanup_limits(&cfg, Some(&Session::new("s", "telegram"))),
            (500_000, 350_000)
        );
        // Свой бюджет перебивает конфиг: групповая поверхность не диктует его TUI.
        let mut own = Session::new("s", "telegram");
        own.context_budget = Some(100_000);
        own.context_target = Some(70_000);
        assert_eq!(session_cleanup_limits(&cfg, Some(&own)), (100_000, 70_000));
        // Ноль — это «не задано», а не «уборка на нуле».
        let mut zero = Session::new("s", "telegram");
        zero.context_budget = Some(0);
        assert_eq!(session_cleanup_limits(&cfg, Some(&zero)), (500_000, 350_000));
    }

    #[test]
    fn auto_review_needs_threshold_and_new_turns_since_last() {
        // Порог не взят — ревью нет.
        assert!(!review_due(20, 19, 0));
        // Порог взят, ревью не было — пора.
        assert!(review_due(20, 20, 0));
        // Ревью было на 20, набралось только 30 — ещё рано (нужно +20).
        assert!(!review_due(20, 30, 20));
        // Ревью было на 20, дошло до 40 — снова пора.
        assert!(review_due(20, 40, 20));
        // Нулевой порог выключает авторевью.
        assert!(!review_due(0, 1000, 0));
    }

    #[tokio::test]
    async fn failed_launch_clears_running_state_and_cancels_queued_messages() {
        let (runtime, dir) = runtime().await;
        {
            let mut data = runtime.data.lock().await;
            let session = data.sessions.get_mut("a").unwrap();
            session.state = "running".into();
            session.run_id = Some("run".into());
            data.messages.insert(
                "a".into(),
                vec![rt::Message {
                    id: "queued".into(),
                    command_id: "command".into(),
                    text: "queued text".into(),
                    attachment_ids: Vec::new(),
                    state: "accepted".into(),
                    run_id: Some("run".into()),
                }],
            );
        }
        finalize_launch_error(&runtime, "a", "run", "writer open failed").await;
        let data = runtime.data.lock().await;
        assert_eq!(data.sessions["a"].state, "error");
        assert_eq!(data.sessions["a"].run_id, None);
        assert_eq!(data.messages["a"][0].state, "cancelled");
        assert!(
            data.events["a"]
                .iter()
                .any(|event| event.kind == "run.finished")
        );
        drop(data);
        drop(runtime);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn message_idempotency_checks_text_payload() {
        let existing = rt::Message {
            id: "message-id".into(),
            command_id: "command-id".into(),
            text: "original input".into(),
            attachment_ids: vec!["attachment-id".into()],
            state: "applied".into(),
            run_id: Some("run-id".into()),
        };
        assert!(same_message_payload(
            &existing,
            "original input",
            &["attachment-id".into()]
        ));
        assert!(!same_message_payload(&existing, "original input", &[]));
        assert!(!same_message_payload(
            &existing,
            "changed input",
            &["attachment-id".into()]
        ));
    }

    fn event(session_id: &str, seq: u64) -> Event {
        Event {
            seq,
            session_id: session_id.into(),
            run_id: None,
            kind: "test".into(),
            fields: serde_json::json!({"seq":seq}),
        }
    }

    async fn runtime() -> (Arc<Runtime>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("surface-sse-{}", uuid::Uuid::new_v4()));
        let (mut store, db) = rt::open_store(&dir).unwrap();
        for id in ["a", "b"] {
            store.sessions.insert(
                id.into(),
                Session::new(id, "test"),
            );
            store.events.insert(id.into(), Vec::new());
        }
        rt::persist_store(&db, &store).unwrap();
        let (tx, _) = broadcast::channel(1024);
        (
            Arc::new(Runtime {
                data: Mutex::new(store),
                db: std::sync::Mutex::new(db),
                tx,
                cfg: Config::default(),
                llm: Arc::new(crate::llm::OpenAi::new(String::new()).unwrap()),
                locks: Default::default(),
                controls: Default::default(),
                pending_tools: Default::default(),
                writers: Default::default(),
                event_seq: Default::default(),
                review_gen: Default::default(),
                approvals: Default::default(),
            }),
            dir,
        )
    }

    async fn add_event(runtime: &Runtime, id: &str, seq: u64) {
        let event = event(id, seq);
        let mut data = runtime.data.lock().await;
        data.events.get_mut(id).unwrap().push(event.clone());
        runtime.persist(&data).unwrap();
        let _ = runtime.tx.send(event);
    }

    async fn add_events(runtime: &Runtime, events: Vec<Event>) {
        let mut data = runtime.data.lock().await;
        for event in &events {
            data.events
                .get_mut(&event.session_id)
                .unwrap()
                .push(event.clone());
        }
        // Match emit's persist-before-publish ordering, but pay for one durable
        // transaction rather than rewriting the whole store for every test event.
        runtime.persist(&data).unwrap();
        for event in events {
            let _ = runtime.tx.send(event);
        }
    }

    #[test]
    fn surface_tool_definitions_are_validated_and_bounded() {
        let dir = std::env::temp_dir().join(format!("surface-tools-{}", uuid::Uuid::new_v4()));
        let cfg = Config::default();
        let valid = serde_json::json!({"type":"function","function":{"name":"send_file","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}});
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(
            rt.block_on(validate_tools(vec![valid.clone()], &cfg))
                .is_ok()
        );
        assert!(rt.block_on(validate_tools(vec![serde_json::json!({"type":"function","function":{"name":"run","parameters":{"type":"object"}}})],&cfg)).is_err());
        assert!(
            rt.block_on(validate_tools(vec![valid.clone(), valid], &cfg))
                .is_err()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn result_after_waiter_timeout_is_rejected_even_when_run_active() {
        let (runtime, dir) = runtime().await;
        {
            let mut d = runtime.data.lock().await;
            let s = d.sessions.get_mut("a").unwrap();
            s.state = "running".into();
            s.run_id = Some("r".into());
            d.events.get_mut("a").unwrap().push(Event {
                seq: 1,
                session_id: "a".into(),
                run_id: Some("r".into()),
                kind: "tool.requested".into(),
                fields: serde_json::json!({"call_id":"c"}),
            });
            runtime.persist(&d).unwrap();
        }
        let (tx, rx) = oneshot::channel();
        drop(rx);
        runtime
            .pending_tools
            .lock()
            .await
            .insert("a/r/c".into(), tx);
        let response = tool_result(
            State(runtime.clone()),
            Path(("a".into(), "r".into(), "c".into())),
            Json(ToolResultBody {
                text: "late".into(),
                exit: 0,
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(!runtime.data.lock().await.tool_results.contains_key("a/r/c"));
        drop(runtime);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn signal_relay_preserves_fifo_and_final_waits_for_drain() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let emitted = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = emitted.clone();
        let relay = tokio::spawn(async move {
            let mut started_tx = Some(started_tx);
            let mut release_rx = Some(release_rx);
            drain_surface_signals(rx, move |kind, _| {
                let seen = seen.clone();
                let is_blocked = kind == "assistant.delta";
                let started = started_tx.take();
                let release = if is_blocked { release_rx.take() } else { None };
                async move {
                    if let Some(started) = started {
                        let _ = started.send(());
                    }
                    if let Some(release) = release {
                        let _ = release.await;
                    }
                    seen.lock().await.push(kind);
                    Ok(())
                }
            })
            .await
        });

        tx.send(("notice".to_owned(), serde_json::json!({"text":"stub-ok: "})))
            .unwrap();
        tx.send((
            "assistant.delta".to_owned(),
            serde_json::json!({"text":"surface-smoke:pty-steer"}),
        ))
        .unwrap();
        started_rx.await.unwrap();
        tx.send(("notice".to_owned(), serde_json::json!({"text":"queued"})))
            .unwrap();
        drop(tx); // producer finished; relay must drain queued signal before final.
        assert!(
            !relay.is_finished(),
            "relay must wait for the blocked first signal"
        );
        release_tx.send(()).unwrap();
        relay.await.unwrap().unwrap();
        emitted.lock().await.push("assistant.completed".into());
        emitted.lock().await.push("run.finished".into());
        assert_eq!(
            *emitted.lock().await,
            [
                "notice",
                "assistant.delta",
                "notice",
                "assistant.completed",
                "run.finished"
            ]
        );
    }

    #[tokio::test]
    async fn snapshot_boundary_filters_sessions_and_suppresses_overlap() {
        let (runtime, dir) = runtime().await;
        add_event(&runtime, "a", 1).await;
        add_event(&runtime, "b", 1).await;
        // Snapshot captures A:1; its already-broadcast copy and B:2 must not leak live.
        let (rx, (initial, cursor)) = snapshot_and_subscribe(&runtime, "a", 0).await.unwrap();
        // Inject the overlap explicitly: A:1 was included in the snapshot and
        // can also be present in the receiver queue at the boundary.
        let _ = runtime.tx.send(event("a", 1));
        add_event(&runtime, "a", 2).await;
        add_event(&runtime, "b", 2).await;
        let mut stream = Box::pin(session_event_stream(
            runtime.clone(),
            "a".into(),
            rx,
            cursor,
            initial,
        ));
        let first = stream.next().await.unwrap();
        assert_eq!((first.session_id.as_str(), first.seq), ("a", 1));
        let second = stream.next().await.unwrap();
        assert_eq!((second.session_id.as_str(), second.seq), ("a", 2));
        drop(stream);
        drop(runtime);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn after_cursor_and_lag_replay_are_session_scoped_and_gapless() {
        let (runtime, dir) = runtime().await;
        // Different sessions have independent sequences; A resumes strictly after 3.
        for n in 1..=3 {
            add_event(&runtime, "a", n).await;
        }
        for n in 1..=12 {
            add_event(&runtime, "b", n).await;
        }
        let (rx, (initial, cursor)) = snapshot_and_subscribe(&runtime, "a", 3).await.unwrap();
        let mut stream = Box::pin(session_event_stream(
            runtime.clone(),
            "a".into(),
            rx,
            cursor,
            initial,
        ));
        // New A events overflow the 1024-entry broadcast buffer; B makes global traffic
        // exceed the limit too, without sharing A's per-session cursor.
        let mut overflow = Vec::new();
        for n in 4..=1100 {
            overflow.push(event("a", n));
            overflow.push(event("b", n + 12));
        }
        add_events(&runtime, overflow).await;
        let mut got = Vec::new();
        for _ in 4..=1100 {
            let event = tokio::time::timeout(Duration::from_secs(3), stream.next())
                .await
                .unwrap()
                .unwrap();
            got.push((event.session_id, event.seq));
        }
        assert_eq!(got.len(), 1097);
        assert!(
            got.iter()
                .enumerate()
                .all(|(index, (id, seq))| id == "a" && *seq == index as u64 + 4)
        );

        // Replay drained the gap; the same subscription must continue live,
        // including while traffic from the other session keeps arriving.
        add_events(&runtime, vec![event("b", 1200), event("a", 1101)]).await;
        let next = tokio::time::timeout(Duration::from_secs(3), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((next.session_id.as_str(), next.seq), ("a", 1101));
        add_event(&runtime, "b", 1201).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(30), stream.next())
                .await
                .is_err()
        );
        drop(stream);
        drop(runtime);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
