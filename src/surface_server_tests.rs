// Тесты поверхности: маршруты, потоки событий, авторевью.
//
// Лежат отдельным файлом от обработчиков: тестов здесь больше, чем кода в
// `surface_server`, и в общем файле они прятали точку сборки сервера.
//
// Тесты профилей провайдеров — рядом, в `surface_providers_tests.rs`: там свой
// предмет (что попадает на диск) и своя сборка Runtime.

use super::{
    events::{session_event_stream, snapshot_and_subscribe},
    launch::{drain_surface_signals, finalize_launch_error},
    review::{review_due, session_cleanup_limits, session_turns},
    test_support::{add_event, add_events, event, runtime},
};
use crate::{
    cfg::Config,
    surface_runtime::{self as rt, Session},
};
use futures_util::StreamExt;
use std::{sync::Arc, time::Duration};
use tokio::sync::{Mutex, oneshot};


#[test]
fn session_cleanup_limits_prefer_own_budget_over_config() {
    let cfg = Config {
        context_budget: 500_000,
        context_target: 350_000,
        ..Default::default()
    };
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

/// Маркер ревью лежит в журнале сессии, но ходом не считается: `session_turns`
/// видит только `Assistant`. Значит, отметка не сдвигает границу — ни вверх
/// (ложное «пора»), ни вниз (пропущенное ревью).
#[test]
fn review_marker_neither_counts_as_a_turn_nor_moves_the_boundary() {
    let dir = std::env::temp_dir().join(format!("surface-review-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Config {
        sessions_dir: dir.display().to_string(),
        ..Default::default()
    };
    let path = dir.join("s.jsonl");
    let mut lines = vec!["{\"kind\":\"meta\",\"ts\":0,\"model\":\"m\"}".to_string()];
    for i in 0..20 {
        lines.push(format!(
            "{{\"kind\":\"assistant\",\"ts\":{i},\"text\":\"ответ {i}\",\"tool_calls\":[]}}"
        ));
    }
    // Отметка ревью и старая форма записи без `turns`/`text` — лог должен читаться.
    lines.push("{\"kind\":\"review\",\"ts\":20,\"review_id\":\"s-review\",\"turns\":20,\"text\":\"саморевью · 20 ходов\"}".to_string());
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();

    let turns = session_turns(&cfg, "s");
    assert_eq!(turns, 20, "маркер посчитался ходом");
    assert!(review_due(20, turns, 0), "маркер съел границу 0");
    // Ревью прошло на 20: отметка в журнале не делает ревью «пора» снова.
    assert!(
        !review_due(20, turns, 20),
        "маркер сдвинул границу вверх"
    );
    // +20 ходов после отметки — снова пора.
    assert!(review_due(20, 40, 20));

    // Запись без полей (ручная/будущая правка формата) не роняет чтение.
    std::fs::write(&path, "{\"kind\":\"review\"}\n").unwrap();
    assert_eq!(session_turns(&cfg, "s"), 0);
    std::fs::remove_dir_all(&dir).ok();
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

