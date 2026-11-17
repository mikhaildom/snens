// Запуск хода: контроль, брокер инструментов поверхности, шина сигналов.
//
// Ход живёт отдельной задачей, а не в обработчике запроса: клиент получает 202
// и следит за сессией потоком событий. Всё, что здесь есть, служит одной цели —
// сигналы хода доезжают до шины по порядку и ровно один раз, включая отмену.

use crate::surface_runtime::{self as rt, Runtime};
use std::sync::Arc;
use tokio::time::Duration;

use super::review::{schedule_review, session_cleanup_limits};

pub(super) async fn drain_surface_signals<F, Fut>(
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

pub(super) async fn finalize_launch_error(s: &Arc<Runtime>, id: &str, run: &str, message: &str) {
    let snapshot = {
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
        let snapshot = data.sessions.get(id).cloned();
        drop(data);
        snapshot
    };
    // Запись и публикация карточки — как в успешном пути: без `publish_session`
    // подписчики `GET /v1/events` продолжали показывать сессию `running`, хотя
    // ход уже закончился ошибкой.
    if let Some(live) = snapshot {
        let _ = s.save_session(&live);
        s.publish_session(live);
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

pub(super) async fn launch(
    s: Arc<Runtime>,
    id: String,
    run: String,
    text: String,
    surface: String,
    parts: Vec<crate::log::Part>,
    ask: Option<crate::log::AskRef>,
) {
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let (queue_tx, queue_rx) = tokio::sync::mpsc::unbounded_channel();
    let queue_rx = Arc::new(tokio::sync::Mutex::new(queue_rx));
    // Второй `launch` на ту же сессию не должен затирать живой контроль: иначе
    // прежний ход теряет канал отмены, а его завершение снимет control нового —
    // и новый ход останется неостанавливаемым. Останавливаем прежний и
    // занимаем место сами.
    {
        let mut controls = s.controls.lock().await;
        if let Some(prev) = controls.get(&id) {
            let _ = prev.cancel.send(true);
        }
        controls.insert(
            id.clone(),
            rt::Control {
                cancel: cancel_tx,
                queued: queue_tx,
            },
        );
    }
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
                let payload =
                    serde_json::json!({"status":"pending","name":req.name,"args":req.args});
                d.tool_requests.insert(key.clone(), payload.clone());
                if let Err(error) = broker_s.save_tool_request(&key, &payload) {
                    d.tool_requests.remove(&key);
                    drop(d);
                    // Раньше `return` убивал брокера до конца хода: все
                    // последующие вызовы инструментов просто не доезжали.
                    // Ошибку записи сообщаем в сессию и живём дальше.
                    let _ = broker_s
                        .emit(
                            &broker_id,
                            Some(broker_run.clone()),
                            "notice",
                            serde_json::json!({"text":format!(
                                "запрос инструмента не записан: {error}"
                            )}),
                        )
                        .await;
                    // Waiter закрываем отказом, чтобы агент не ждал вечно.
                    drop(req.result);
                    continue;
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
                let expired = serde_json::json!({"status":"expired"});
                d.tool_requests.insert(key.clone(), expired.clone());
                let _ = broker_s.save_tool_request(&key, &expired);
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
        // Ответ из артефакта помечается своим источником: в журнале видно, что
        // текст собран интерфейсом, а не набран руками. Для проекции это ничего
        // не меняет — роль всё равно `user`, различия едут полем `ask`.
        let source = if ask.is_some() { "artifact" } else { "surface" };
        if let Err(error) = sess.append(crate::log::Rec::User {
            ts: crate::log::now(),
            text,
            source: source.into(),
            parts,
            ask,
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
                    parts,
                    ms,
                } => (
                    "tool.completed",
                    serde_json::json!({"name":name,"call_id":call_id,"exit":exit,"preview":preview,"parts":parts,"ms":ms}),
                ),
                crate::agent::SurfaceSignal::Usage {
                    in_tok,
                    cached_tok,
                    out_tok,
                    reason_tok,
                    cost_micro,
                } => (
                    "usage",
                    serde_json::json!({"in_tok":in_tok,"cached_tok":cached_tok,"out_tok":out_tok,"reason_tok":reason_tok,"cost_micro":cost_micro}),
                ),
                crate::agent::SurfaceSignal::Notice(text) => {
                    ("notice", serde_json::json!({"text":text}))
                }
                crate::agent::SurfaceSignal::ContextPressure { approx, budget } => (
                    "context.pressure",
                    serde_json::json!({"approx":approx,"budget":budget}),
                ),
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
        // Страховка от паники в агенте: без неё `run.finished` не уезжает,
        // control не снимается, и сессия висит `running` до перезапуска — все
        // последующие сообщения уходят в очередь, которую никто не читает.
        let agent_fut = async {
            crate::agent::run_agent_surface(
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
            .await
        };
        // `AssertUnwindSafe`: состояние сессии живёт в файле и в store, а не в
        // этих футурах; после паники мы всё равно финализируем ход как ошибку.
        let result =
            match futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(agent_fut))
                .await
            {
                Ok(result) => result,
                Err(_) => Err("агент упал с паникой".into()),
            };
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
        // Порядок важен: сперва сессия становится `idle`, и только потом снимается
        // контроль. Наоборот было окно, в котором `message` уже видел `running`,
        // но `controls.get` возвращал `None` — вход терялся молча, а клиент ждал
        // `applied`, которого никогда не будет.
        let snapshot = {
            let mut d = s.data.lock().await;
            if let Some(x) = d.sessions.get_mut(&id) {
                x.state = "idle".into();
                x.run_id = None
            }
            let snapshot = d.sessions.get(&id).cloned();
            drop(d);
            snapshot
        };
        // Запись — без замка store: блокирующий SQLite под ним держал всё.
        if let Some(live) = snapshot {
            let _ = s.save_session(&live);
            s.publish_session(live);
        }
        s.controls.lock().await.remove(&id);
        // Ход завершён и сессия снова idle — планируем авторевью после простоя.
        // Считаем только успешные ходы: отменённый/ошибочный ход ревью не стоит.
        if status == "ok" {
            schedule_review(s.clone(), id.clone());
        }
    });
}
