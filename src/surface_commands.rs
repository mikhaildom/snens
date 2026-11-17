// Команды хода: сообщение человека, остановка, результат инструмента.
//
// Общая дисциплина — идемпотентность по `command_id`: повтор запроса обязан
// вернуть тот же ответ, а не второй ход. Ради неё у каждой команды есть запись
// в `store.commands`, и сравнение payload'а идёт по сохранённому значению.

use crate::surface_runtime::{self as rt, Runtime, Session};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
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
    let payload = serde_json::json!({"text":body.text,"exit":body.exit});
    // Проверки — одним коротким замком store: идемпотентность и вставка
    // результата должны идти под одним guard'ом, иначе два разных payload'а на
    // один key оба проходят `tool_results.get` и проигравший перезаписывает
    // принятый победителем результат (и в памяти, и в базе).
    {
        let d = s.data.lock().await;
        if let Some(prev) = d.tool_results.get(&key) {
            return if prev == &payload {
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
    }
    // Захват waiter'а — единственная атомарная точка владения результатом: кто
    // вынул канал из `pending_tools`, тот его и доставляет. Один захват вместо
    // трёх (`is_closed`/`contains_key`/`remove` в разных замках давали окно для
    // второго запроса).
    let Some(tx) = s.pending_tools.lock().await.remove(&key) else {
        let d = s.data.lock().await;
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
    };
    // Сначала доставка, потом запись: если waiter уже умер (`send` не прошёл),
    // ни в store, ни в SQLite следа принятого результата не остаётся — иначе
    // рестарт поднимал бы строку `tool_results` и повтор получал `accepted:true`
    // на результат, который никто не получил.
    if tx.send(result).is_err() {
        return err(
            "tool_expired",
            "tool waiter expired before result delivery",
            StatusCode::CONFLICT,
        )
        .into_response();
    }
    let request_payload = serde_json::json!({"status":"completed"});
    {
        let mut d = s.data.lock().await;
        d.tool_results.insert(key.clone(), payload.clone());
        d.tool_requests.insert(key.clone(), request_payload.clone());
    }
    // Запись — уже без store-замка: блокирующий SQLite не должен держать всех,
    // кому нужен `data`. Ошибка откатывает память, чтобы ретрай не коротко
    // замыкался на `accepted:true`.
    if let Err(e) = s
        .save_tool_result(&key, &payload)
        .and_then(|_| s.save_tool_request(&key, &request_payload))
    {
        let mut d = s.data.lock().await;
        d.tool_results.remove(&key);
        d.tool_requests.remove(&key);
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
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
    pub(super) seq: u64,
    #[serde(default)]
    pub(super) artifact: String,
    #[serde(default)]
    pub(super) id: String,
    #[serde(default)]
    pub(super) question: String,
}
pub(super) async fn ensure_writer(
    s: &Arc<Runtime>,
    id: &str,
) -> crate::R<Arc<crate::log::Session>> {
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

/// Совпадает ли сохранённый `ask` с тем, что клиент прислал в повторе.
///
/// Сравнивать нечего «в лоб»: в store лежит уже разрешённая ссылка (имя
/// артефакта из журнала, `id` по умолчанию `q`), а клиент вправе прислать пустые
/// поля — они означают «возьми из журнала». Поэтому пустое поле запроса
/// совпадает с любым сохранённым, а заполненное — обязано совпасть точно: иначе
/// повтор с чужим артефактом прошёл бы как тот же самый.
pub(super) fn ask_matches(
    existing: &Option<crate::log::AskRef>,
    request: Option<&SendAsk>,
) -> bool {
    match (existing, request) {
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
        (Some(have), Some(want)) => {
            have.seq == want.seq
                && (want.artifact.is_empty() || want.artifact == have.artifact)
                && (want.id.is_empty() || want.id == have.id)
        }
    }
}

/// Ссылка на вопрос артефакта, разрешённая по журналу: то, что реально уедет
/// в `Rec::User.ask`, плюс ключ уже отвеченного вопроса.
pub(super) fn resolve_ask(
    log: &[crate::log::Rec],
    a: &SendAsk,
) -> Result<crate::log::AskRef, Box<axum::response::Response>> {
    let Some(found) = crate::surface_artifact::find(log, a.seq) else {
        return Err(Box::new(
            err(
                "artifact_not_found",
                "no artifact in that record",
                StatusCode::CONFLICT,
            )
            .into_response(),
        ));
    };
    // Имя артефакта берём из журнала, если клиент его не прислал: иначе опечатка
    // в клиенте делала бы вопрос «другим» и разрешала повторный ответ.
    let artifact = if a.artifact.is_empty() {
        found.id.clone()
    } else {
        a.artifact.clone()
    };
    let id_question = if a.id.is_empty() {
        "q".to_string()
    } else {
        a.id.clone()
    };
    if crate::surface_artifact::answered(log).contains(&(
        found.seq,
        artifact.clone(),
        id_question.clone(),
    )) {
        return Err(Box::new(
            err(
                "ask_answered",
                "this question is already answered",
                StatusCode::CONFLICT,
            )
            .into_response(),
        ));
    }
    Ok(crate::log::AskRef {
        seq: found.seq,
        artifact,
        id: id_question,
        question: a.question.clone(),
    })
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
    // Повтор команды проверяем ДО любой валидации ответа на вопрос: клиент,
    // повторивший тот же POST (таймаут сети), обязан получить сохранённый ответ
    // `{message_id, run_id, state}`, а не 409 `ask_answered` — иначе идемпотентность
    // ломается ровно на том запросе, который ответил на вопрос.
    let existing = {
        let d = s.data.lock().await;
        d.messages
            .get(&id)
            .and_then(|v| v.iter().find(|m| m.command_id == b.command_id))
            .cloned()
    };
    if let Some(m) = existing {
        // Сравниваем с разрешённым `ask` нельзя: для этого нужен журнал. Сравнение
        // по сохранённому значению — оно и есть контракт идемпотентности.
        let same = m.text == b.text
            && m.attachment_ids == b.attachment_ids
            && ask_matches(&m.ask, b.ask.as_ref());
        if !same {
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
    let known = s.data.lock().await.sessions.contains_key(&id);
    if !known {
        let Some(path) = discovery::path(&s.cfg, &id).filter(|path| path.is_file()) else {
            return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
        };
        let surface = {
            // `discovery::surface` разбирает журнал — блокирующее чтение, не на
            // async-воркере.
            let read = path.clone();
            match tokio::task::spawn_blocking(move || discovery::surface(&read)).await {
                Ok(surface) => surface,
                Err(e) => {
                    return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
                        .into_response();
                }
            }
        };
        let session = Session::new(id.clone(), surface);
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
    // Ответ на вопрос артефакта: разбор журнала только для новой команды и
    // только в `spawn_blocking` — журнал бывает на мегабайты, а ошибку чтения
    // нельзя выдавать за «журнала нет» (это была бы ложная 409).
    let ask = match b.ask.as_ref() {
        None => None,
        Some(a) => {
            let Some(path) = discovery::path(&s.cfg, &id) else {
                return err("not_found", "session not found", StatusCode::NOT_FOUND)
                    .into_response();
            };
            let seq = a.seq;
            let artifact = a.artifact.clone();
            let qid = a.id.clone();
            let question = a.question.clone();
            let read = tokio::task::spawn_blocking(move || crate::log::read_file(&path)).await;
            let log = match read {
                Ok(Ok(log)) => log,
                Ok(Err(e)) => {
                    return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
                        .into_response();
                }
                Err(e) => {
                    return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
                        .into_response();
                }
            };
            match resolve_ask(
                &log,
                &SendAsk {
                    seq,
                    artifact,
                    id: qid,
                    question,
                },
            ) {
                Ok(ask) => Some(ask),
                Err(response) => return *response,
            }
        }
    };
    let mut d = s.data.lock().await;
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
    let prev_state = session.state.clone();
    let prev_run = session.run_id.clone();
    if !busy {
        session.state = "running".into();
        session.run_id = run_id.clone()
    }
    d.commands.insert(b.command_id.clone(), result.clone());
    // Снимок для записи — под замком, сама запись (блокирующий SQLite) — уже без
    // него: иначе fsync держал бы всех, кому нужен store.
    let snapshot = snapshot_for(&d, &id);
    drop(d);
    if let Err(e) = s
        .save_session_and_messages(&id, snapshot.session.as_ref(), &snapshot.messages)
        .and_then(|_| s.save_command(&command_id, &result))
    {
        // Откат: иначе 500 оставлял бы сообщение в очереди и сессию `running`,
        // а ретрай с тем же `command_id` шёл бы по ветке идемпотентности и
        // отвечал `applied`, не запустив ход.
        let mut d = s.data.lock().await;
        if let Some(ms) = d.messages.get_mut(&id) {
            ms.retain(|m| m.id != mid);
        }
        d.commands.remove(&command_id);
        if let Some(session) = d.sessions.get_mut(&id) {
            session.state = prev_state;
            session.run_id = prev_run;
        }
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    if let Some(live) = s.data.lock().await.sessions.get(&id).cloned() {
        s.publish_session(live);
    }
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
        if c.queued
            .send(crate::agent::QueuedInput {
                text: b.text.clone(),
                parts,
                ask,
            })
            .is_err()
        {
            // Канал закрыт: ход уже закончился, и очередь не заберёт вход.
            // Сообщение нельзя оставлять `accepted` — клиент ждал бы вечно.
            cancel_message(&s, &id, &mid).await;
            return err(
                "run_finished",
                "run finished before the input was queued",
                StatusCode::CONFLICT,
            )
            .into_response();
        }
    } else {
        // Сессия была `busy` по снимку, но контрол уже снят: вход терять некуда.
        cancel_message(&s, &id, &mid).await;
        return err(
            "run_finished",
            "run finished before the input was queued",
            StatusCode::CONFLICT,
        )
        .into_response();
    }
    (StatusCode::ACCEPTED, Json(result)).into_response()
}

/// Пометить сообщение отменённым: вход в закрытую очередь не доставлен, и
/// оставлять `accepted` нельзя — клиент ждал бы `applied`, которого не будет.
pub(super) async fn cancel_message(s: &Arc<Runtime>, id: &str, mid: &str) {
    let snapshot = {
        let mut d = s.data.lock().await;
        if let Some(ms) = d.messages.get_mut(id) {
            for m in ms.iter_mut().filter(|m| m.id == mid) {
                m.state = "cancelled".into();
            }
        }
        snapshot_for(&d, id)
    };
    let _ = s.save_session_and_messages(id, snapshot.session.as_ref(), &snapshot.messages);
    if let Some(live) = s.data.lock().await.sessions.get(id).cloned() {
        s.publish_session(live);
    }
}

/// Записать состояние сессии и её сообщений без async-замка: `save_*` берут
/// синхронный `db`, и держать над ними `data` незачем.
pub(super) fn snapshot_for(d: &rt::Store, id: &str) -> PersistSnapshot {
    PersistSnapshot {
        session: d.sessions.get(id).cloned(),
        messages: d.messages.get(id).cloned().unwrap_or_default(),
    }
}

/// Снимок одной сессии для записи вне замка store.
pub(super) struct PersistSnapshot {
    session: Option<rt::Session>,
    messages: Vec<rt::Message>,
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
    // Та же дисциплина, что у `message`: замок сессии на весь обработчик и
    // запись `command_id`. Без замка параллельный `message` мог создать новый
    // ход или долить вход, пока `stop` переводит состояние в `stopping`.
    let _guard = s.session_lock(&id).await.lock_owned().await;
    let mut d = s.data.lock().await;
    // Повтор стопа идемпотентен: тот же `command_id` с тем же `run_id` —
    // сохранённый ответ, а не второй проход и не 409.
    if let Some(prev) = d.commands.get(&b.command_id) {
        let same = prev.get("run_id").and_then(|v| v.as_str()) == Some(b.run_id.as_str());
        if !same {
            return err(
                "idempotency_conflict",
                "command_id reused with different payload",
                StatusCode::CONFLICT,
            )
            .into_response();
        }
        return (StatusCode::OK, Json(prev.clone())).into_response();
    }
    let Some(x) = d.sessions.get_mut(&id) else {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    };
    if x.run_id.as_deref() != Some(&b.run_id) {
        return err("conflict", "run_id is not current", StatusCode::CONFLICT).into_response();
    }
    let prev_state = x.state.clone();
    x.state = "stopping".into();
    let out = x.clone();
    let mut cancelled: Vec<String> = Vec::new();
    if let Some(ms) = d.messages.get_mut(&id) {
        for m in ms
            .iter_mut()
            .filter(|m| m.run_id.as_deref() == Some(&b.run_id) && m.state == "accepted")
        {
            m.state = "cancelled".into();
            cancelled.push(m.id.clone());
        }
    }
    let result =
        serde_json::to_value(&out).unwrap_or_else(|_| serde_json::json!({"state":"stopping"}));
    d.commands.insert(b.command_id.clone(), result.clone());
    // Снимок — под замком, запись — без него: блокирующий SQLite не должен
    // держать store, пока пишет.
    let snapshot = snapshot_for(&d, &id);
    drop(d);
    if let Err(e) = s
        .save_session_and_messages(&id, snapshot.session.as_ref(), &snapshot.messages)
        .and_then(|_| s.save_command(&b.command_id, &result))
    {
        // Откат: иначе 500 оставлял бы сессию `stopping` и сообщения
        // отменёнными только в памяти, а после рестарта они воскресли бы как
        // `accepted`.
        let mut d = s.data.lock().await;
        if let Some(x) = d.sessions.get_mut(&id) {
            x.state = prev_state;
        }
        if let Some(ms) = d.messages.get_mut(&id) {
            for m in ms.iter_mut().filter(|m| cancelled.contains(&m.id)) {
                m.state = "accepted".into();
            }
        }
        d.commands.remove(&b.command_id);
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    s.publish_session(out.clone());
    if let Some(c) = s.controls.lock().await.get(&id) {
        let _ = c.cancel.send(true);
    }
    Json(out).into_response()
}
