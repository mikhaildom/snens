use crate::surface_client::{Client, Event, Session, TranscriptMessage};
use crate::{
    input::{self, In},
    render::{Stream, Ui},
    R,
};
use std::{
    io::IsTerminal,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

const HELP: &str = "/new · /sessions · /resume ID · /stop · /reasoning · /memory [on|off] · /compact [инструкция] · /model [slug] · /help · /quit (Ctrl+D); Ctrl+C — stop текущего run · sudo/секреты пока не поддерживаются";
type Error = Box<dyn std::error::Error + Send + Sync>;

enum Notice {
    Event(Event, u64),
    Error(String),
}

pub async fn run(
    endpoint: String,
    token: String,
    session: Option<String>,
    history_path: PathBuf,
) -> R<()> {
    let client = Client::new(endpoint, token).map_err(other)?;
    if std::io::stdin().is_terminal() {
        if let Ok(raw) = input::raw_mode() {
            return live_loop(client, session, history_path, raw).await;
        }
    }
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines() {
            match line {
                Ok(line) => {
                    if tx.send(In::Line(line)).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(In::Eof);
    });
    app_loop(client, session, rx, Ui::plain()).await
}

fn other(s: String) -> Error {
    s.into()
}

fn choose_default_session(sessions: &[Session]) -> Option<&Session> {
    sessions.iter().find(|session| session.surface == "tui")
}

async fn live_loop(
    client: Client,
    initial: Option<String>,
    history_path: PathBuf,
    _raw: input::RawGuard,
) -> R<()> {
    let term = Arc::new(input::Term::new("> "));
    let (tx, rx) = mpsc::unbounded_channel();
    input::spawn(Arc::clone(&term), tx, history_path);
    let ui = Ui::with_sink(Box::new(input::TermSink(term)));
    app_loop(client, initial, rx, ui).await
}

async fn app_loop(
    client: Client,
    initial: Option<String>,
    mut input: UnboundedReceiver<In>,
    ui: Ui,
) -> R<()> {
    let (event_tx, mut events) = mpsc::unbounded_channel();
    let mut subscription: Option<tokio::task::JoinHandle<()>> = None;
    let mut subscription_session_id: Option<String>;
    let mut session = None;
    let mut cursor = 0;
    let mut selected_run = None;
    let mut stream: Option<Stream> = None;
    let mut received_text = String::new();
    if let Some(id) = initial {
        match open_session(&client, &id, &ui).await {
            Ok((s, c)) => {
                selected_run = if s.state == "running" {
                    s.run_id.clone()
                } else {
                    None
                };
                session = Some(s);
                cursor = c;
            }
            Err(e) => ui.line(&format!("ошибка открытия: {e}")),
        }
    } else if let Ok(sessions) = client.sessions().await {
        if let Some(s) = choose_default_session(&sessions) {
            match open_session(&client, &s.id, &ui).await {
                Ok((opened, c)) => {
                    selected_run = if opened.state == "running" {
                        opened.run_id.clone()
                    } else {
                        None
                    };
                    session = Some(opened);
                    cursor = c;
                }
                Err(e) => ui.line(&format!("ошибка открытия: {e}")),
            }
        } else {
            ui.line(
                "нет TUI-сессии; /new или /sessions (чужие поверхности не выбраны автоматически)",
            );
        }
    }
    subscription_session_id = session.as_ref().map(|s| s.id.clone());
    subscribe(
        &client,
        session.as_ref(),
        cursor,
        &event_tx,
        &mut subscription,
    );
    ui.line(HELP);
    loop {
        tokio::select! {
            incoming = input.recv() => {
                let Some(incoming) = incoming else { break };
                match incoming {
                    In::Eof => break,
                    In::Interrupt => stop_current(&client, &session, &selected_run, &ui).await,
                    In::Line(line) => {
                        let line = line.trim();
                        if line.is_empty() { continue; }
                        let result = command(&client, line, &mut session, &mut selected_run, &mut cursor, &ui).await;
                        let should_quit = match result {
                            Ok(quit) => quit,
                            Err(error) => { ui.line(&format!("ошибка: {error}")); false },
                        };
                        if should_quit { break; }
                        // A selected-session transition must close old event stream.
                        if current_session_id(session.as_ref()) != subscription_session_id.as_deref() {
                            stream = None;
                            received_text.clear();
                            if let Some(task) = subscription.take() { task.abort(); }
                            subscription_session_id = session.as_ref().map(|s| s.id.clone());
                            if session.is_none() { cursor = 0; }
                            subscribe(&client, session.as_ref(), cursor, &event_tx, &mut subscription);
                        }
                    }
                }
            }
            message = events.recv() => if let Some(message) = message {
                match message {
                    Notice::Event(event, seq) => {
                        if accept_event(&event, seq, session.as_ref().map(|s| s.id.as_str()), &mut cursor) {
                            if event.kind == "run.started" {
                                stream = None;
                                received_text.clear();
                                selected_run = event.run_id.clone();
                            }
                            if event.kind == "assistant.delta" {
                                if event.fields["kind"] != "reasoning" {
                                    received_text.push_str(event.fields["text"].as_str().unwrap_or(""));
                                }
                                display(&event, &ui, Some(stream.get_or_insert_with(|| Stream::new(ui.color, ui.sink()))));
                            } else if event.kind == "assistant.completed" {
                                let full_text = event.fields["text"].as_str().unwrap_or("");
                                let action = completion_action(&received_text, full_text);
                                let current = stream.get_or_insert_with(|| Stream::new(ui.color, ui.sink()));
                                match action {
                                    CompletionAction::Append(suffix) => current.content(suffix),
                                    CompletionAction::Full(text) => current.content(text),
                                    CompletionAction::Finish => {},
                                    CompletionAction::Recover(text) => {
                                        current.finish();
                                        ui.line(&format!("Ответ восстановлен целиком:\n{text}"));
                                    }
                                }
                                current.finish();
                                stream = None;
                                received_text.clear();
                            } else {
                                display(&event, &ui, stream.as_mut());
                            }
                            if event.kind == "run.finished" {
                                if let Some(mut current) = stream.take() { current.finish(); }
                                received_text.clear();
                                selected_run = None;
                            }
                        }
                    }
                    Notice::Error(e) => { ui.line(&format!("события недоступны: {e}; переподключаюсь")); }
                }
            }
        }
    }
    if let Some(task) = subscription {
        task.abort();
    }
    Ok(())
}

enum CompletionAction<'a> {
    Append(&'a str),
    Full(&'a str),
    Finish,
    Recover(&'a str),
}

fn completion_action<'a>(received: &str, full: &'a str) -> CompletionAction<'a> {
    if received.is_empty() {
        CompletionAction::Full(full)
    } else if received == full {
        CompletionAction::Finish
    } else if let Some(suffix) = full.strip_prefix(received) {
        CompletionAction::Append(suffix)
    } else {
        CompletionAction::Recover(full)
    }
}

fn accept_event(event: &Event, seq: u64, selected: Option<&str>, cursor: &mut u64) -> bool {
    if selected == Some(event.session_id.as_str()) && seq == event.seq && seq > *cursor {
        *cursor = seq;
        true
    } else {
        false
    }
}

fn current_session_id(session: Option<&Session>) -> Option<&str> {
    session.map(|s| s.id.as_str())
}

fn subscribe(
    client: &Client,
    session: Option<&Session>,
    after: u64,
    tx: &UnboundedSender<Notice>,
    handle: &mut Option<tokio::task::JoinHandle<()>>,
) {
    let Some(session) = session else {
        return;
    };
    let client = client.clone();
    let id = session.id.clone();
    let tx = tx.clone();
    *handle = Some(tokio::spawn(async move {
        let delivered = Arc::new(AtomicU64::new(after));
        loop {
            let tx_events = tx.clone();
            let last = Arc::clone(&delivered);
            let result = client
                .events(&id, delivered.load(Ordering::Relaxed), |event, seq| {
                    if event.session_id == id {
                        last.fetch_max(seq, Ordering::Relaxed);
                        let _ = tx_events.send(Notice::Event(event, seq));
                    }
                })
                .await;
            match result {
                Ok(()) => tokio::time::sleep(Duration::from_millis(300)).await,
                Err(e) => {
                    let _ = tx.send(Notice::Error(e));
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }));
}

async fn open_session(client: &Client, id: &str, ui: &Ui) -> Result<(Session, u64), String> {
    let session = client.get_session(id).await?;
    let history = client.history(id).await?;
    if let Some(messages) = history.messages.as_deref() {
        for message in messages {
            display_message(message, ui);
        }
    } else if !history.events.is_empty() {
        // Compatibility with older API servers that predate the JSONL transcript field.
        for event in &history.events {
            display(event, ui, None);
        }
    }
    ui.line(&format!(
        "сессия {} · {} · cursor {}",
        session.id, session.state, history.cursor
    ));
    Ok((session, history.cursor))
}

async fn command(
    client: &Client,
    line: &str,
    session: &mut Option<Session>,
    selected_run: &mut Option<String>,
    cursor: &mut u64,
    ui: &Ui,
) -> Result<bool, String> {
    if line == "/quit" || line == "/q" {
        return Ok(true);
    }
    if line == "/help" || line == "/h" {
        ui.line(HELP);
        return Ok(false);
    }
    if line == "/reasoning" {
        let on = ui.toggle_reasoning();
        ui.line(if on {
            "рассуждение: показано"
        } else {
            "рассуждение: скрыто"
        });
        return Ok(false);
    }
    if line == "/sessions" {
        for s in client.sessions().await? {
            ui.line(&format!("{} · {} · {}", s.id, s.surface, s.state));
        }
        return Ok(false);
    }
    if line == "/new" {
        let s = client.create(&command_id()).await?;
        ui.line(&format!("сессия {}", s.id));
        *session = Some(s);
        *selected_run = None;
        *cursor = 0;
        return Ok(false);
    }
    if let Some(id) = line.strip_prefix("/resume ") {
        let (s, new_cursor) = open_session(client, id.trim(), ui).await?;
        *selected_run = if s.state == "running" {
            s.run_id.clone()
        } else {
            None
        };
        *session = Some(s);
        *cursor = new_cursor;
        return Ok(false);
    }
    if line == "/stop" {
        stop_current(client, session, selected_run, ui).await;
        return Ok(false);
    }
    if line == "/model" {
        match client.model().await {
            Ok(model) => ui.line(&format!("модель: {model}")),
            Err(e) => ui.line(&format!("ошибка модели: {e}")),
        }
        return Ok(false);
    }
    if let Some(slug) = line.strip_prefix("/model ") {
        let slug = slug.trim();
        if slug.is_empty() {
            ui.line("использование: /model <slug>");
            return Ok(false);
        }
        match client.set_model(slug).await {
            Ok(model) => ui.line(&format!("модель: {model} (применится к следующему ходу)")),
            Err(e) => ui.line(&format!("ошибка модели: {e}")),
        }
        return Ok(false);
    }
    if line == "/memory" {
        match client.memory().await {
            Ok(v) => ui.line(&format!(
                "память: {} · справка {} · запись хода {} · cmd {}",
                if v["enabled"] == true { "вкл" } else { "выкл" },
                v["recall"].as_str().unwrap_or("?"),
                if v["retain"] == true { "да" } else { "нет" },
                v["cmd"].as_str().unwrap_or("(нет)"),
            )),
            Err(e) => ui.line(&format!("ошибка памяти: {e}")),
        }
        ui.line("сменить: /memory on|off (действует со следующего хода)");
        return Ok(false);
    }
    if line == "/memory on" || line == "/memory off" {
        let on = line.ends_with("on");
        match client
            .set_memory(serde_json::json!({"enabled": on}))
            .await
        {
            Ok(_) => ui.line(&format!("память: {}", if on { "вкл" } else { "выкл" })),
            Err(e) => ui.line(&format!("ошибка памяти: {e}")),
        }
        return Ok(false);
    }
    if let Some(instruction) = line.strip_prefix("/compact") {
        let text = if instruction.trim().is_empty() {
            "Please compact the conversation so far, preserving key decisions and open tasks."
                .to_owned()
        } else {
            format!(
                "Please compact the conversation so far: {}",
                instruction.trim()
            )
        };
        send_message(client, session, selected_run, &text, ui).await?;
        return Ok(false);
    }
    if line.starts_with('/') {
        ui.line("неизвестная команда; /help · sudo и секреты API пока не поддержаны");
        return Ok(false);
    }
    send_message(client, session, selected_run, line, ui).await?;
    Ok(false)
}

async fn send_message(
    client: &Client,
    session: &Option<Session>,
    run: &mut Option<String>,
    text: &str,
    ui: &Ui,
) -> Result<(), String> {
    let Some(s) = session else {
        ui.line("сессии нет; вызови /new или /resume ID");
        return Ok(());
    };
    let accepted = client.message(&s.id, &command_id(), text).await?;
    *run = Some(accepted.run_id.clone());
    ui.line(&format!(
        "сообщение {} · {}",
        accepted.message_id, accepted.state
    ));
    Ok(())
}

async fn stop_current(client: &Client, session: &Option<Session>, run: &Option<String>, ui: &Ui) {
    let (Some(s), Some(run_id)) = (session, run) else {
        ui.line("нет выбранного run");
        return;
    };
    match client.stop(&s.id, &command_id(), run_id).await {
        Ok(s) => ui.line(&format!("остановка: {}", s.state)),
        Err(e) => ui.line(&format!("ошибка stop: {e}")),
    }
}

fn command_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn display_message(message: &TranscriptMessage, ui: &Ui) {
    match message.role.as_str() {
        "user" => {
            if let Some(text) = &message.content {
                ui.line(&format!("ты: {text}"));
            }
        }
        "assistant" => {
            if let Some(text) = &message.content {
                ui.line(text);
            }
            if ui.show_reasoning() {
                if let Some(text) = &message.reasoning {
                    ui.line(&format!("[рассуждение] {text}"));
                }
            }
            if let Some(calls) = &message.tool_calls {
                for call in calls {
                    let function = &call["function"];
                    ui.line(&format!(
                        "[{}] {}",
                        function["name"].as_str().unwrap_or("tool"),
                        function["arguments"]
                    ));
                }
            }
        }
        "tool" => ui.line(&format!(
            "[{}] exit={} {}",
            message.name.as_deref().unwrap_or("tool"),
            message.exit.unwrap_or(-1),
            message.content.as_deref().unwrap_or("")
        )),
        _ => {}
    }
}

fn display(event: &Event, ui: &Ui, stream: Option<&mut Stream>) {
    let fields = &event.fields;
    match event.kind.as_str() {
        "assistant.delta" => {
            if let Some(stream) = stream {
                let text = fields["text"].as_str().unwrap_or("");
                if fields["kind"] == "reasoning" {
                    if ui.show_reasoning() {
                        stream.reasoning(true);
                        stream.reasoning_chunk(text);
                    }
                } else {
                    stream.content(text);
                }
            }
        }
        "assistant.completed" => {
            if let Some(stream) = stream {
                stream.finish();
            } else if let Some(text) = fields["text"].as_str() {
                ui.line(text);
            }
        }
        "tool.started" => ui.line(&format!(
            "[{}] {}",
            fields["name"].as_str().unwrap_or("tool"),
            fields["args"]
        )),
        "tool.completed" => ui.line(&format!(
            "[{}] exit={} {}",
            fields["name"].as_str().unwrap_or("tool"),
            fields["exit"],
            fields["preview"].as_str().unwrap_or("")
        )),
        "notice" => ui.notice(fields["text"].as_str().unwrap_or("")),
        "run.finished" => {
            if let Some(stream) = stream {
                stream.finish();
            }
            ui.line(&format!(
                "run завершён: {}{}",
                fields["status"].as_str().unwrap_or("?"),
                fields["error"]
                    .as_str()
                    .map(|e| format!(" — {e}"))
                    .unwrap_or_default()
            ));
        }
        "message.accepted" | "message.applied" | "message.cancelled" => ui.line(&format!(
            "{}: {}",
            event.kind,
            fields["text"].as_str().unwrap_or("")
        )),
        _ => {}
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session(id: &str, surface: &str) -> Session {
        Session {
            id: id.into(),
            surface: surface.into(),
            state: "idle".into(),
            run_id: None,
            context_budget: None,
            context_target: None,
        }
    }

    #[test]
    fn default_session_never_crosses_surfaces() {
        let telegram_only = vec![session("tg", "telegram")];
        assert!(choose_default_session(&telegram_only).is_none());

        let mixed = vec![
            session("tg", "telegram"),
            session("old", "legacy"),
            session("tui", "tui"),
        ];
        assert_eq!(
            choose_default_session(&mixed).map(|s| s.id.as_str()),
            Some("tui")
        );
    }

    fn event(session_id: &str, seq: u64) -> Event {
        Event {
            seq,
            session_id: session_id.into(),
            run_id: None,
            kind: "test".into(),
            fields: json!({}),
        }
    }

    #[test]
    fn completion_fallback_handles_missing_partial_full_and_mismatched_deltas() {
        assert!(matches!(
            completion_action("", "answer"),
            CompletionAction::Full("answer")
        ));
        assert!(matches!(
            completion_action("ans", "answer"),
            CompletionAction::Append("wer")
        ));
        assert!(matches!(
            completion_action("answer", "answer"),
            CompletionAction::Finish
        ));
        assert!(matches!(
            completion_action("answer!", "answer"),
            CompletionAction::Recover("answer")
        ));
        assert!(matches!(
            completion_action("sw", "answer"),
            CompletionAction::Recover("answer")
        ));
    }

    #[test]
    fn delivered_cursor_ignores_foreign_and_duplicate_events() {
        let mut cursor = 2;
        assert!(!accept_event(&event("b", 20), 20, Some("a"), &mut cursor));
        assert_eq!(cursor, 2);
        assert!(!accept_event(&event("a", 2), 2, Some("a"), &mut cursor));
        assert!(!accept_event(&event("a", 4), 3, Some("a"), &mut cursor));
        assert_eq!(cursor, 2);
        assert!(accept_event(&event("a", 3), 3, Some("a"), &mut cursor));
        assert_eq!(cursor, 3);
        assert!(!accept_event(&event("a", 3), 3, Some("a"), &mut cursor));
        assert_eq!(cursor, 3);
    }
}
