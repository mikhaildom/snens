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
use futures_util::{stream, StreamExt};
use serde::Deserialize;
use std::{collections::VecDeque, net::SocketAddr, sync::Arc, time::Duration};

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
            &observe,
            Some(cancel_rx),
            Some(queue_rx.clone()),
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
    use tokio::sync::{broadcast, oneshot, Mutex};

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
                Session {
                    id: id.into(),
                    surface: "test".into(),
                    state: "idle".into(),
                    run_id: None,
                },
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
        assert!(got
            .iter()
            .enumerate()
            .all(|(index, (id, seq))| id == "a" && *seq == index as u64 + 4));

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
