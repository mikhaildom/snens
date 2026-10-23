// Общие хелперы тестов поверхности: Runtime с настоящим store во временном
// каталоге и запись событий так, как это делает `emit` (store + рассылка).
//
// Отдельным файлом, а не копией в двух наборах тестов: хелпер, разъехавшийся
// между копиями, даёт «тест проходит у одного и падает у другого».

#![allow(dead_code)]

use crate::surface_runtime::{self as rt, Event, Runtime, Session};
use crate::cfg::Config;
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast};

pub(super) fn event(session_id: &str, seq: u64) -> Event {
    Event {
        seq,
        session_id: session_id.into(),
        run_id: None,
        kind: "test".into(),
        fields: serde_json::json!({"seq":seq}),
    }
}

pub(super) async fn runtime() -> (Arc<Runtime>, std::path::PathBuf) {
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
    let (tx_updates, _) = broadcast::channel(256);
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
            pending: Default::default(),
            registry: Default::default(),
            tx_updates,
        }),
        dir,
    )
}

pub(super) async fn add_event(runtime: &Runtime, id: &str, seq: u64) {
    let event = event(id, seq);
    let mut data = runtime.data.lock().await;
    data.events.get_mut(id).unwrap().push(event.clone());
    runtime.save_event(&event).unwrap();
    let _ = runtime.tx.send(event);
}

pub(super) async fn add_events(runtime: &Runtime, events: Vec<Event>) {
    let mut data = runtime.data.lock().await;
    for event in &events {
        data.events
            .get_mut(&event.session_id)
            .unwrap()
            .push(event.clone());
    }
    for event in &events {
        runtime.save_event(event).unwrap();
    }
    for event in events {
        let _ = runtime.tx.send(event);
    }
}
