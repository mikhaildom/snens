use crate::cfg::Config;
use crate::surface_runtime::{self as rt, Event, Runtime, Session};
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use fs2::FileExt;
use futures_util::StreamExt;
use serde::Deserialize;
use std::{net::SocketAddr, sync::Arc, time::Duration};

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
    let had_running = store
        .sessions
        .values()
        .any(|s| s.state == "running" || s.state == "stopping");
    rt::interrupt_running(&mut store);
    if had_running {
        // Persist restart recovery before accepting requests.
        rt::persist_store(&db, &store)?;
    }
    let state = Arc::new(Runtime {
        data: tokio::sync::Mutex::new(store),
        db: std::sync::Mutex::new(db),
        tx,
        cfg,
        llm,
        locks: Default::default(),
        controls: Default::default(),
    });
    let app = Router::new()
        .route("/v1/sessions", get(list).post(create))
        .route("/v1/sessions/{id}", get(get_session))
        .route("/v1/sessions/{id}/messages", post(message))
        .route("/v1/sessions/{id}/stop", post(stop))
        .route("/v1/sessions/{id}/events", get(events))
        .route("/v1/sessions/{id}/history", get(history))
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
    rt::interrupt_running(&mut d);
    s.persist(&d)
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
async fn list(State(s): State<Arc<Runtime>>) -> Json<serde_json::Value> {
    let d = s.data.lock().await;
    Json(serde_json::json!({"sessions":d.sessions.values().collect::<Vec<_>>()}))
}
#[derive(Deserialize)]
struct Create {
    command_id: String,
    surface: String,
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
        let same = v.get("_surface_request").and_then(|x| x.as_str()) == Some(b.surface.as_str());
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
    let session = Session {
        id: id.clone(),
        surface: b.surface.clone(),
        state: "idle".into(),
        run_id: None,
    };
    d.sessions.insert(id.clone(), session.clone());
    d.events.insert(id.clone(), vec![]);
    d.messages.insert(id.clone(), vec![]);
    let mut record = serde_json::to_value(&session).unwrap();
    record["_surface_request"] = serde_json::Value::String(b.surface.clone());
    d.commands.insert(b.command_id, record);
    if let Err(e) = s.persist(&d) {
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    (StatusCode::CREATED, Json(session)).into_response()
}
async fn get_session(State(s): State<Arc<Runtime>>, Path(id): Path<String>) -> impl IntoResponse {
    if !rt::valid_id(&id) {
        return err("invalid", "invalid session id", StatusCode::BAD_REQUEST).into_response();
    }
    match s.data.lock().await.sessions.get(&id).cloned() {
        Some(x) => Json(x).into_response(),
        None => err("not_found", "session not found", StatusCode::NOT_FOUND).into_response(),
    }
}
#[derive(Deserialize)]
struct Send {
    command_id: String,
    text: String,
}
async fn message(
    State(s): State<Arc<Runtime>>,
    Path(id): Path<String>,
    Json(b): Json<Send>,
) -> impl IntoResponse {
    if !rt::valid_id(&id) || !rt::valid_id(&b.command_id) || b.text.trim().is_empty() {
        return err(
            "invalid",
            "invalid id or empty text",
            StatusCode::BAD_REQUEST,
        )
        .into_response();
    }
    let _guard = s.session_lock(&id).await.lock_owned().await;
    let mut d = s.data.lock().await;
    if !d.sessions.contains_key(&id) {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    }
    if let Some(m) = d
        .messages
        .get(&id)
        .and_then(|v| v.iter().find(|m| m.command_id == b.command_id))
    {
        return (
            StatusCode::OK,
            Json(serde_json::json!({"message_id":m.id,"run_id":m.run_id,"state":m.state})),
        )
            .into_response();
    }
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
        state: state.into(),
        run_id: run_id.clone(),
    });
    let result = serde_json::json!({"message_id":mid,"run_id":run_id,"state":state});
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
        launch(s.clone(), id, run_id.unwrap(), b.text).await
    } else if let Some(c) = s.controls.lock().await.get(&id) {
        let _ = c.queued.send(b.text.clone());
    }
    (StatusCode::ACCEPTED, Json(result)).into_response()
}
async fn launch(s: Arc<Runtime>, id: String, run: String, text: String) {
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
    let cfg = s.cfg.clone();
    let llm = s.llm.clone();
    tokio::spawn(async move {
        let sess = match crate::log::Session::open(&cfg.sessions_path(), &id) {
            Ok(x) => x,
            Err(e) => {
                let _ = s
                    .emit(
                        &id,
                        Some(run.clone()),
                        "notice",
                        serde_json::json!({"text":e.to_string()}),
                    )
                    .await;
                return;
            }
        };
        let _ = sess.append(crate::log::Rec::User {
            ts: crate::log::now(),
            text,
            source: "surface".into(),
        });
        let obs_s = s.clone();
        let obs_id = id.clone();
        let obs_run = run.clone();
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
            let s = obs_s.clone();
            let id = obs_id.clone();
            let run = obs_run.clone();
            tokio::spawn(async move {
                let _ = s.emit(&id, Some(run), kind, fields).await;
            });
        };
        let _ = s
            .emit(&id, Some(run.clone()), "run.started", serde_json::json!({}))
            .await;
        let result = crate::agent::run_agent_surface(
            &sess,
            &cfg,
            &*llm,
            &crate::render::Ui::plain(),
            &observe,
            Some(cancel_rx),
            Some(queue_rx.clone()),
        )
        .await;
        let status = if result.is_ok() {
            "ok"
        } else if result
            .as_ref()
            .err()
            .is_some_and(|e| e.to_string() == "cancelled")
        {
            "cancelled"
        } else {
            "error"
        };
        let assistant = sess
            .read()
            .unwrap_or_default()
            .into_iter()
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
                serde_json::json!({"status":status,"error":result.err().map(|e|e.to_string())}),
            )
            .await;
        s.controls.lock().await.remove(&id);
        let mut d = s.data.lock().await;
        if let Some(x) = d.sessions.get_mut(&id) {
            x.state = "idle".into();
            x.run_id = None
        }
        let _ = s.persist(&d);
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
    let d = s.data.lock().await;
    if let Some(e) = d.events.get(&id) {
        Json(serde_json::json!({"cursor":e.last().map(|x|x.seq).unwrap_or(0),"events":e}))
            .into_response()
    } else {
        err("not_found", "session not found", StatusCode::NOT_FOUND).into_response()
    }
}
async fn events(
    State(s): State<Arc<Runtime>>,
    Path(id): Path<String>,
    Query(q): Query<After>,
) -> impl IntoResponse {
    let rx = s.tx.subscribe();
    let after = q.after.unwrap_or(0);
    let initial = {
        let d = s.data.lock().await;
        if !d.sessions.contains_key(&id) {
            return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
        }
        d.events.get(&id).cloned().unwrap_or_default()
    };
    let init = tokio_stream::iter(
        initial
            .into_iter()
            .filter(move |e| e.seq > after)
            .map(|e| Ok::<_, std::convert::Infallible>(sse(e))),
    );
    let live = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|msg| async move {
        match msg {
            Ok(e) => Some(Ok(sse(e))),
            Err(_) => None,
        }
    });
    Sse::new(init.chain(live))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}
fn sse(e: Event) -> SseEvent {
    SseEvent::default()
        .id(e.seq.to_string())
        .json_data(e)
        .unwrap_or_else(|_| SseEvent::default())
}
