// Команды хода: сообщение человека, остановка, результат инструмента.
//
// Общая дисциплина — идемпотентность по `command_id`: повтор запроса обязан
// вернуть тот же ответ, а не второй ход. Ради неё у каждой команды есть запись
// в `store.commands`, и сравнение payload'а идёт по сохранённому значению.

use crate::surface_runtime::{self as rt, Runtime, Session};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use std::sync::Arc;

use super::discovery;
use super::files::resolve_attachments;
use super::http::err;
use super::launch::launch;

#[derive(Deserialize)]
pub(super) struct ToolResultBody {
    pub(super) text: String,
    pub(super) exit: i32,
}
pub(super) async fn tool_result(
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
    let result_payload = serde_json::json!({"text":body.text,"exit":body.exit});
    let request_payload = serde_json::json!({"status":"completed"});
    d.tool_results.insert(key.clone(), result_payload.clone());
    d.tool_requests.insert(key.clone(), request_payload.clone());
    if let Err(e) = s
        .save_tool_result(&key, &result_payload)
        .and_then(|_| s.save_tool_request(&key, &request_payload))
    {
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    // Не `expect`: между проверкой `contains_key` выше и этим `remove` стоит
    // `await`, поэтому два одновременных result'а на один call_id могли оба
    // пройти проверку, и второй упал бы паникой в обработчике (500 без тела).
    // Гонка кончается тем же, чем истёкший waiter: 409, инструмент уже закрыт.
    let Some(tx) = s.pending_tools.lock().await.remove(&key) else {
        return err(
            "tool_expired",
            "tool call already completed by another request",
            StatusCode::CONFLICT,
        )
        .into_response();
    };
    if tx.send(result).is_err() {
        let expired = serde_json::json!({"status":"expired"});
        d.tool_results.remove(&key);
        d.tool_requests.insert(key.clone(), expired.clone());
        if let Err(e) = s.save_tool_request(&key, &expired) {
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
pub(super) struct Send {
    command_id: String,
    text: String,
    #[serde(default)]
    attachment_ids: Vec<String>,
    /// Ответ человека на вопрос артефакта (`snens.ask`): ссылка на артефакт и
    /// вопрос. Есть — ядро проверяет, что вопрос ещё не отвечен, и пишет
    /// ответ записью `Rec::User` с этим же полем.
    #[serde(default)]
    ask: Option<SendAsk>,
}

/// Ссылка на вопрос артефакта, как её присылает клиент. `seq` — номер записи
/// `assistant` с артефактом (он же `seq` в истории), `artifact` и `id` — имена
/// из заголовка фенса и из `snens.ask`.
#[derive(Deserialize)]
pub(super) struct SendAsk {
    seq: u64,
    #[serde(default)]
    artifact: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    question: String,
}
pub(super) fn same_message_payload(
    existing: &rt::Message,
    text: &str,
    attachment_ids: &[String],
    ask: &Option<crate::log::AskRef>,
) -> bool {
    existing.text == text && existing.attachment_ids == attachment_ids && &existing.ask == ask
}

pub(super) async fn ensure_writer(s: &Arc<Runtime>, id: &str) -> crate::R<Arc<crate::log::Session>> {
    if let Some(writer) = s.writers.lock().await.get(id).cloned() {
        return Ok(writer);
    }
    let path = discovery::path(&s.cfg, id).ok_or("invalid session id")?;
    // Метка поверхности при открытии существующего лога не переписывается:
    // отсутствие метки означает локальную сессию, и так её и надо открыть.
    let surface = discovery::load_meta_surface(&path);
    let writer = Arc::new(crate::log::Session::open_surface(
        &s.cfg.sessions_path(),
        id,
        surface.as_deref(),
    )?);
    let mut writers = s.writers.lock().await;
    Ok(writers
        .entry(id.to_string())
        .or_insert_with(|| writer.clone())
        .clone())
}

pub(super) async fn message(
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
            discovery::surface(&path),
        );
        let mut data = s.data.lock().await;
        let inserted = data.sessions.entry(id.clone()).or_insert(session).clone();
        data.events.entry(id.clone()).or_default();
        if let Err(error) = s.save_session(&inserted) {
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
    // Ответ на вопрос артефакта: проверка до любых записей. `artifact` клиент
    // может не прислать — тогда имя берём из самого журнала, а не из его слов:
    // иначе опечатка в клиенте делала бы вопрос «другим» и разрешала повтор.
    let ask = match b.ask.as_ref() {
        None => None,
        Some(a) => {
            let path = discovery::path(&s.cfg, &id);
            let log = path
                .as_deref()
                .map(crate::log::read_file)
                .transpose()
                .unwrap_or_else(|_| Some(Vec::new()))
                .unwrap_or_default();
            let Some(found) = crate::surface_artifact::find(&log, a.seq) else {
                return err(
                    "artifact_not_found",
                    "no artifact in that record",
                    StatusCode::CONFLICT,
                )
                .into_response();
            };
            let artifact = if a.artifact.is_empty() {
                found.id.clone()
            } else {
                a.artifact.clone()
            };
            let id_question = if a.id.is_empty() { "q".to_string() } else { a.id.clone() };
            let key = format!("{}:{}:{}", found.seq, artifact, id_question);
            if crate::surface_artifact::answered(&log).contains(&key) {
                return err(
                    "ask_answered",
                    "this question is already answered",
                    StatusCode::CONFLICT,
                )
                .into_response();
            }
            Some(crate::log::AskRef {
                seq: found.seq,
                artifact,
                id: id_question,
                question: a.question.clone(),
            })
        }
    };
    let mut d = s.data.lock().await;
    if let Some(m) = d
        .messages
        .get(&id)
        .and_then(|v| v.iter().find(|m| m.command_id == b.command_id))
    {
        if !same_message_payload(m, &b.text, &b.attachment_ids, &ask) {
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
        ask: ask.clone(),
    });
    let result = serde_json::json!({"message_id":mid,"run_id":run_id,"state":state});
    // Сессия обязана быть в store: выше её либо нашли (`known`), либо вставили,
    // а блокировка сессии держится на весь обработчик — `delete_session` её не
    // снимет. Но падать на `unwrap` в обработчике нельзя даже при нарушении
    // инварианта: клиент получил бы разрыв соединения вместо ответа.
    let command_id = b.command_id.clone();
    let Some(session) = d.sessions.get_mut(&id) else {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    };
    let surface = session.surface.clone();
    if !busy {
        session.state = "running".into();
        session.run_id = run_id.clone()
    }
    d.commands.insert(b.command_id.clone(), result.clone());
    if let Err(e) = s
        .save_session_messages(&id, &d)
        .and_then(|_| s.save_command(&command_id, &result))
    {
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    if let Some(live) = d.sessions.get(&id).cloned() {
        s.publish_session(live);
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
        // `!busy` значит, что run_id выше выдан этим же обработчиком.
        let Some(run_id) = run_id else {
            return err("conflict", "run_id is not current", StatusCode::CONFLICT).into_response();
        };
        launch(s.clone(), id, run_id, b.text, surface, parts, ask).await
    } else if let Some(c) = s.controls.lock().await.get(&id) {
        let _ = c.queued.send(crate::agent::QueuedInput {
            text: b.text.clone(),
            parts,
            ask,
        });
    }
    (StatusCode::ACCEPTED, Json(result)).into_response()
}
#[derive(Deserialize)]
pub(super) struct Stop {
    command_id: String,
    run_id: String,
}
pub(super) async fn stop(
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
    let _ = s.save_session_messages(&id, &d);
    drop(d);
    s.publish_session(out.clone());
    if let Some(c) = s.controls.lock().await.get(&id) {
        let _ = c.cancel.send(true);
    }
    Json(out).into_response()
}