// Ответвление сессии: новая сессия с журналом источника до выбранного ответа.
//
// Копия, а не ссылка `Replay`: ответвление живёт само по себе, и удаление
// источника не должно стирать ему историю. Байты записей те же, что в
// источнике (кроме путей вложений), поэтому проекция совпадает и префикс-кеш
// провайдера остаётся тёплым.

use crate::log::Rec;
use crate::surface_runtime::{self as rt, Runtime, Session};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::Path as FsPath;
use std::sync::Arc;

use super::discovery;
use super::http::err;

#[derive(Deserialize)]
pub(super) struct Fork {
    pub(super) command_id: String,
    /// Физический номер строки журнала (1-based), он же `seq` в истории:
    /// запись `assistant`, до которой включительно копируется журнал.
    pub(super) seq: usize,
}

/// Сколько первых строк журнала войдёт в ответвление.
///
/// Строка `seq` должна быть ответом модели. За ней подбираются записи того же
/// шага, без которых журнал битый: расход хода и результаты вызовов, которые
/// этот ответ сделал. Если результатов ещё нет (ход идёт), ответвлять рано:
/// провайдер не примет вызов без ответа.
pub(super) fn cut(recs: &[Rec], seq: usize) -> Result<usize, &'static str> {
    if seq < 2 || seq > recs.len() {
        return Err("seq is out of range");
    }
    let Rec::Assistant { tool_calls, .. } = &recs[seq - 1] else {
        return Err("seq is not an assistant record");
    };
    let mut pending: BTreeSet<&str> = tool_calls.iter().map(|c| c.id.as_str()).collect();
    let mut end = seq;
    for rec in &recs[seq..] {
        match rec {
            Rec::Usage { .. } => {}
            Rec::ToolResult { call_id, .. } if pending.remove(call_id.as_str()) => {}
            _ => break,
        }
        end += 1;
    }
    if !pending.is_empty() {
        return Err("turn is not finished yet");
    }
    Ok(end)
}

/// Скопировать каталог вложений целиком. Нет каталога — нечего копировать.
fn copy_dir(from: &FsPath, to: &FsPath) -> std::io::Result<()> {
    if !from.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dst = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dst)?;
        } else {
            std::fs::copy(entry.path(), dst)?;
        }
    }
    Ok(())
}

/// Собрать журнал ответвления и положить его на диск атомарно.
/// Возвращает `(поверхность, заголовок)` для записи в store.
fn write_fork(
    root: &FsPath,
    src: &str,
    dst: &str,
    seq: usize,
) -> Result<(String, Option<String>), (StatusCode, &'static str, String)> {
    let storage = |e: String| (StatusCode::INTERNAL_SERVER_ERROR, "storage", e);
    let path = root.join(format!("{src}.jsonl"));
    let text = std::fs::read_to_string(&path).map_err(|e| storage(e.to_string()))?;
    // Строки и записи идут парами: номер строки — это `seq`. Недописанный хвост
    // (обрыв при записи) не разбирается и в ответвление не попадает.
    let mut lines: Vec<&str> = Vec::new();
    let mut recs: Vec<Rec> = Vec::new();
    for line in text.lines() {
        let Ok(rec) = serde_json::from_str::<Rec>(line) else { break };
        lines.push(line);
        recs.push(rec);
    }
    let end = cut(&recs, seq).map_err(|e| (StatusCode::CONFLICT, "invalid", e.to_string()))?;
    let Some(Rec::Meta { model, surface, .. }) = recs.first() else {
        return Err(storage("journal has no meta".into()));
    };
    let title = discovery::title(&path);
    let fork_title = Some(match &title {
        Some(t) => format!("{t} (ответвление)"),
        None => "Ответвление".to_string(),
    });
    let meta = Rec::Meta {
        ts: crate::log::now(),
        model: model.clone(),
        parent: String::new(),
        surface: surface.clone(),
        title: fork_title.clone(),
    };
    // Вложения лежат в каталогах сессии, и файловый маршрут отдаёт их только
    // своей сессии: копируем каталоги и переписываем пути в записях.
    let root_s = root.to_string_lossy();
    let swaps = [".out", ".attachments"].map(|suffix| {
        (
            format!("{root_s}/{src}{suffix}/"),
            format!("{root_s}/{dst}{suffix}/"),
        )
    });
    for suffix in [".out", ".attachments"] {
        copy_dir(
            &root.join(format!("{src}{suffix}")),
            &root.join(format!("{dst}{suffix}")),
        )
        .map_err(|e| storage(e.to_string()))?;
    }
    let mut out = serde_json::to_string(&meta).map_err(|e| storage(e.to_string()))?;
    out.push('\n');
    for line in &lines[1..end] {
        let mut line = line.to_string();
        for (from, to) in &swaps {
            if line.contains(from.as_str()) {
                line = line.replace(from.as_str(), to);
            }
        }
        out.push_str(&line);
        out.push('\n');
    }
    let tmp = root.join(format!(".{dst}.jsonl.tmp"));
    std::fs::write(&tmp, out).map_err(|e| storage(e.to_string()))?;
    std::fs::rename(&tmp, root.join(format!("{dst}.jsonl"))).map_err(|e| storage(e.to_string()))?;
    Ok((surface.clone().unwrap_or_else(|| "local".into()), fork_title))
}

pub(super) async fn fork(
    State(s): State<Arc<Runtime>>,
    Path(id): Path<String>,
    Json(b): Json<Fork>,
) -> impl IntoResponse {
    if !rt::valid_id(&id) || !rt::valid_id(&b.command_id) {
        return err("invalid", "invalid session id or command_id", StatusCode::BAD_REQUEST)
            .into_response();
    }
    let Some(path) = discovery::path(&s.cfg, &id) else {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    };
    if !path.is_file() {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    }
    let source = {
        let d = s.data.lock().await;
        // Повтор той же команды: ответ прежний, второго ответвления нет.
        if let Some(v) = d.commands.get(&b.command_id) {
            return (StatusCode::OK, Json(v.clone())).into_response();
        }
        d.sessions.get(&id).cloned()
    };
    let new_id = uuid::Uuid::new_v4().to_string();
    let root = s.cfg.sessions_path();
    let (src, dst, seq) = (id.clone(), new_id.clone(), b.seq);
    let written = tokio::task::spawn_blocking(move || write_fork(&root, &src, &dst, seq)).await;
    let (surface, title) = match written {
        Ok(Ok(v)) => v,
        Ok(Err((status, code, msg))) => return err(code, &msg, status).into_response(),
        Err(e) => {
            return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    };
    let mut session = Session::new(new_id.clone(), surface);
    session.title = title;
    session.updated_at = Some(crate::log::now());
    if let Some(src) = source {
        session.context_budget = src.context_budget;
        session.context_target = src.context_target;
        session.model = src.model;
    }
    let Ok(record) = serde_json::to_value(&session) else {
        return err("storage", "session is not serializable", StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
    };
    {
        let mut d = s.data.lock().await;
        d.sessions.insert(new_id.clone(), session.clone());
        d.events.insert(new_id.clone(), vec![]);
        d.messages.insert(new_id.clone(), vec![]);
        d.commands.insert(b.command_id.clone(), record.clone());
    }
    if let Err(e) = s
        .save_session(&session)
        .and_then(|_| s.save_command(&b.command_id, &record))
    {
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    s.event_seq.lock().await.insert(new_id, 0);
    (StatusCode::CREATED, Json(session)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(v: serde_json::Value) -> Rec {
        serde_json::from_value(v).unwrap()
    }

    fn journal() -> Vec<Rec> {
        vec![
            rec(serde_json::json!({"kind":"meta","ts":1,"model":""})),
            rec(serde_json::json!({"kind":"user","ts":2,"text":"привет","source":"web"})),
            rec(serde_json::json!({"kind":"assistant","ts":3,"text":"смотрю","tool_calls":[{"id":"c1","name":"sh","args":"{}"}]})),
            rec(serde_json::json!({"kind":"usage","ts":3,"in_tok":1,"cached_tok":0,"out_tok":1,"cost_micro":1})),
            rec(serde_json::json!({"kind":"tool_result","ts":4,"call_id":"c1","tool":"sh","argv":[],"exit":0,"bytes":0,"ms":1,"body":{"t":"inline","text":""}})),
            rec(serde_json::json!({"kind":"assistant","ts":5,"text":"готово","tool_calls":[]})),
            rec(serde_json::json!({"kind":"usage","ts":5,"in_tok":1,"cached_tok":0,"out_tok":1,"cost_micro":1})),
            rec(serde_json::json!({"kind":"user","ts":6,"text":"ещё","source":"web"})),
        ]
    }

    #[test]
    fn cut_takes_usage_and_results_of_the_step() {
        let j = journal();
        assert_eq!(cut(&j, 3), Ok(5));
        assert_eq!(cut(&j, 6), Ok(7));
    }

    #[test]
    fn cut_refuses_non_assistant_and_unfinished_steps() {
        let j = journal();
        assert!(cut(&j, 2).is_err());
        assert!(cut(&j, 1).is_err());
        assert!(cut(&j, 99).is_err());
        assert!(cut(&j[..4], 3).is_err());
    }
}
