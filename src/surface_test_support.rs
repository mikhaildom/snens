// Общие хелперы тестов поверхности: Runtime с настоящим store во временном
// каталоге и запись событий так, как это делает `emit` (store + рассылка).
//
// Отдельным файлом, а не копией в двух наборах тестов: хелпер, разъехавшийся
// между копиями, даёт «тест проходит у одного и падает у другого».

#![allow(dead_code)]

use crate::cfg::Config;
use crate::surface_runtime::{self as rt, Event, Runtime, Session};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

/// Временный каталог, который убирается сам.
///
/// Раньше каждый тест звал `remove_dir_all` последней строкой, и любое упавшее
/// `assert_eq!` оставляло каталог (с `store.sqlite3` и `.env` с ключом) в
/// `%TEMP%` навсегда. `Drop` убирает и при панике.
pub(super) struct TempDir(pub(super) std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl AsRef<std::path::Path> for TempDir {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

impl std::ops::Deref for TempDir {
    type Target = std::path::Path;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub(super) fn event(session_id: &str, seq: u64) -> Event {
    Event {
        seq,
        session_id: session_id.into(),
        run_id: None,
        kind: "test".into(),
        fields: serde_json::json!({"seq":seq}),
    }
}

/// Ёмкость broadcast-канала, общая с продакшеном (см. `surface_server`).
/// Тест lag-replay завязан на неё числом событий — пусть число берётся отсюда.
pub(super) const BROADCAST_CAPACITY: usize = 1024;

pub(super) async fn runtime() -> (Arc<Runtime>, TempDir) {
    let dir = TempDir(std::env::temp_dir().join(format!("surface-sse-{}", uuid::Uuid::new_v4())));
    let (mut store, db) = rt::open_store(&dir).unwrap();
    for id in ["a", "b"] {
        store.sessions.insert(id.into(), Session::new(id, "test"));
        store.events.insert(id.into(), Vec::new());
    }
    rt::persist_store(&db, &store).unwrap();
    let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
    let (tx_updates, _) = broadcast::channel(256);
    (
        Arc::new(Runtime {
            data: Mutex::new(store),
            db: std::sync::Mutex::new(db),
            tx,
            cfg: Config::default(),
            // Отдельный конструктор для «ключа нет»: он и документирует
            // инвариант, и не сломается, если `new` начнёт проверять ключ.
            llm: Arc::new(crate::llm::OpenAi::without_key().unwrap()),
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
    data.events
        .get_mut(id)
        .unwrap_or_else(|| panic!("сессия {id} не заведена в store теста"))
        .push(event.clone());
    runtime.save_event(&event).unwrap();
    // Живых подписчиков может не быть: канал стартует пустым, а событие до
    // первого `subscribe` никому не адресовано — это нормально, но не молча.
    assert!(
        runtime.tx.send(event).is_ok() || runtime.tx.receiver_count() == 0,
        "событие {id}/{seq} не ушло ни одному подписчику"
    );
}

pub(super) async fn add_events(runtime: &Runtime, events: Vec<Event>) {
    let mut data = runtime.data.lock().await;
    for event in &events {
        data.events
            .get_mut(&event.session_id)
            .unwrap_or_else(|| panic!("сессия {} не заведена в store теста", event.session_id))
            .push(event.clone());
    }
    for event in &events {
        runtime.save_event(event).unwrap();
    }
    drop(data);
    for event in events {
        // Один проход по владению: раньше вектор обходился трижды и клонировался
        // только ради того, чтобы сохранить исходный для отправки.
        assert!(
            runtime.tx.send(event).is_ok() || runtime.tx.receiver_count() == 0,
            "событие не ушло ни одному подписчику"
        );
    }
}
