// Тесты команд хода: идемпотентность по `command_id` и доставка результата
// инструмента. Отдельно от `surface_server_tests.rs`: у них свой предмет —
// что происходит при повторе запроса и при гонке двух result'ов.

use super::{
    commands::{ask_matches, resolve_ask, tool_result, SendAsk, ToolResultBody},
    sessions::{create, validate_tools, Create},
    test_support::runtime,
};
use crate::{cfg::Config, surface_runtime::Event};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use tokio::sync::oneshot;

/// Повтор команды сравнивается по сохранённому `ask`: пустые поля запроса
/// означают «возьми из журнала» и совпадают с любым сохранённым значением.
#[test]
fn repeat_payload_compares_text_attachments_and_ask() {
    let want = SendAsk {
        seq: 7,
        artifact: String::new(),
        id: String::new(),
        question: "какой вариант?".into(),
    };
    let have = Some(crate::log::AskRef {
        seq: 7,
        artifact: "quiz".into(),
        id: "q".into(),
        question: "какой вариант?".into(),
    });
    // Пустые artifact/id — не «другой артефакт», а «тот, что в журнале».
    assert!(ask_matches(&have, Some(&want)));
    // Заполненное — обязано совпасть точно.
    let mut other = SendAsk {
        seq: 7,
        artifact: "poll".into(),
        id: String::new(),
        question: String::new(),
    };
    assert!(!ask_matches(&have, Some(&other)));
    other.artifact = "quiz".into();
    assert!(ask_matches(&have, Some(&other)));
    // Наличие/отсутствие `ask` — разные команды.
    assert!(!ask_matches(&have, None));
    assert!(!ask_matches(&None, Some(&want)));
    assert!(ask_matches(&None, None));
}

#[test]
fn resolve_ask_rejects_invalid_header_value_and_repeat() {
    use crate::log::{AskRef, Rec};
    let assistant = |text: &str| Rec::Assistant {
        ts: 0,
        text: Some(text.into()),
        reasoning: None,
        tool_calls: Vec::new(),
    };
    let answered = Rec::User {
        ts: 0,
        text: "1".into(),
        source: "artifact".into(),
        parts: Vec::new(),
        ask: Some(AskRef {
            seq: 1,
            artifact: "quiz".into(),
            id: "q".into(),
            question: String::new(),
        }),
    };
    let log = vec![
        assistant("```artifact id=quiz\n<script></script>\n```"),
        answered,
    ];
    let ask = SendAsk {
        seq: 1,
        artifact: String::new(),
        id: String::new(),
        question: "x".into(),
    };
    // Вопрос уже отвечен — ключ структурный, повтор не проходит.
    let err = resolve_ask(&log, &ask).expect_err("ответ не должен пройти");
    assert_eq!(err.status(), StatusCode::CONFLICT);
}

#[test]
fn surface_tool_definitions_are_validated_and_bounded() {
    // Конфиг смотрит в свой временный каталог: `Config::default()` указывает на
    // настоящие данные разработчика, и тест читал бы живой `tools/` — результат
    // зависел бы от машины.
    let dir = crate::surface_server::test_support::TempDir(
        std::env::temp_dir().join(format!("surface-tools-{}", uuid::Uuid::new_v4())),
    );
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Config {
        sessions_dir: dir.join("sessions").display().to_string(),
        tools_dir: dir.join("tools").display().to_string(),
        ..Config::default()
    };
    let valid = serde_json::json!({"type":"function","function":{"name":"send_file","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}});
    let rt = tokio::runtime::Runtime::new().unwrap();
    assert!(rt
        .block_on(validate_tools(vec![valid.clone()], &cfg))
        .is_ok());
    // Имя ядра занято.
    assert!(rt
        .block_on(validate_tools(
            vec![serde_json::json!({"type":"function","function":{"name":"run","parameters":{"type":"object"}}})],
            &cfg
        ))
        .is_err());
    // Дубль имени в одном запросе.
    assert!(rt
        .block_on(validate_tools(vec![valid.clone(), valid.clone()], &cfg))
        .is_err());
    // Больше восьми инструментов — отказ по количеству.
    let many: Vec<serde_json::Value> = (0..9)
        .map(|i| {
            serde_json::json!({"type":"function","function":{"name":format!("tool_{i}"),"parameters":{"type":"object"}}})
        })
        .collect();
    assert!(
        rt.block_on(validate_tools(many, &cfg)).is_err(),
        "лимит 8 не проверен"
    );
    // Определения больше 32 КиБ — отказ по размеру.
    let huge = serde_json::json!({"type":"function","function":{"name":"big","parameters":{"type":"object","description":"x".repeat(40 * 1024)}}});
    assert!(
        rt.block_on(validate_tools(vec![huge], &cfg)).is_err(),
        "лимит 32 КиБ не проверен"
    );
}

#[tokio::test]
async fn result_after_waiter_timeout_is_rejected_even_when_run_active() {
    let (runtime, _dir) = runtime().await;
    {
        let mut d = runtime.data.lock().await;
        let s = d.sessions.get_mut("a").unwrap();
        s.state = "running".into();
        s.run_id = Some("r".into());
        d.events.get_mut("a").unwrap().push(Event {
            seq: 1,
            session_id: "a".into(),
            run_id: Some("r".into()),
            kind: "tool.requested".into(),
            fields: serde_json::json!({"call_id":"c"}),
        });
        runtime.save_session(d.sessions.get("a").unwrap()).unwrap();
    }
    let (tx, rx) = oneshot::channel();
    drop(rx);
    runtime
        .pending_tools
        .lock()
        .await
        .insert("a/r/c".into(), tx);
    let response = tool_result(
        State(runtime.clone()),
        Path(("a".into(), "r".into(), "c".into())),
        Json(ToolResultBody {
            text: "late".into(),
            exit: 0,
        }),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(!runtime.data.lock().await.tool_results.contains_key("a/r/c"));
    drop(runtime);
}

/// Повтор result'а не роняет обработчик и не открывает второй ход.
///
/// Гонку «канал уже изъял брокер истёкшего waiter'а» тестом отсюда не поддеть
/// (оба захвата `pending_tools` идут в одном обработчике), поэтому в самом
/// `tool_result` на этом месте стоит комментарий и штатный 409 вместо `expect`.
/// Тест держит то, что проверяемо: повтор с тем же payload — 200, без паники.
#[tokio::test]
async fn repeated_tool_result_is_idempotent_and_never_panics() {
    let (runtime, _dir) = runtime().await;
    {
        let mut d = runtime.data.lock().await;
        let s = d.sessions.get_mut("a").unwrap();
        s.state = "running".into();
        s.run_id = Some("r".into());
        d.events.get_mut("a").unwrap().push(Event {
            seq: 1,
            session_id: "a".into(),
            run_id: Some("r".into()),
            kind: "tool.requested".into(),
            fields: serde_json::json!({"call_id":"c"}),
        });
        runtime.save_session(d.sessions.get("a").unwrap()).unwrap();
    }
    let (tx, rx) = oneshot::channel();
    runtime
        .pending_tools
        .lock()
        .await
        .insert("a/r/c".into(), tx);
    let first = tool_result(
        State(runtime.clone()),
        Path(("a".into(), "r".into(), "c".into())),
        Json(ToolResultBody {
            text: "первый".into(),
            exit: 0,
        }),
    )
    .await
    .into_response();
    assert_eq!(
        first.status(),
        StatusCode::OK,
        "первый result должен пройти"
    );
    assert_eq!(
        rx.await.unwrap().text,
        "первый",
        "waiter получил не тот текст"
    );

    // Канал уже изъят: повтор с тем же payload идемпотентен (200, ветка
    // `tool_results`), а не паника и не второй ход инструмента. Другой payload
    // на тот же key — 409 `result_conflict`.
    let second = tool_result(
        State(runtime.clone()),
        Path(("a".into(), "r".into(), "c".into())),
        Json(ToolResultBody {
            text: "первый".into(),
            exit: 0,
        }),
    )
    .await
    .into_response();
    assert_eq!(
        second.status(),
        StatusCode::OK,
        "тот же payload — идемпотентен"
    );
    drop(runtime);
}

/// Повтор `POST /v1/sessions` не открывает второй журнал и возвращает тот же id.
///
/// Заодно держит порядок блокировок: `create` больше не держит мьютекс store
/// над `open_surface` (create + `flock`), поэтому параллельный повтор не висит
/// на файле. Проверяем оба захода — до и после открытия журнала.
#[tokio::test]
async fn create_is_idempotent_per_command_id() {
    let (runtime, _dir) = runtime().await;
    let body = || Create {
        // `command_id` проходит `valid_id`: иначе `create` отвергает запрос.
        command_id: "1792710852-1".into(),
        surface: "web".into(),
        context_budget: None,
        context_target: None,
    };
    let first = create(State(runtime.clone()), Json(body()))
        .await
        .into_response();
    assert_eq!(first.status(), StatusCode::CREATED);
    let id = runtime
        .data
        .lock()
        .await
        .commands
        .get("1792710852-1")
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .expect("сессия записана");

    // Тот же payload — 200 и тот же id, второго журнала нет.
    let second = create(State(runtime.clone()), Json(body()))
        .await
        .into_response();
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(
        runtime.data.lock().await.sessions.len(),
        3,
        "store стартовал с двумя сессиями: создана ровно одна новая"
    );

    // Другой surface под тем же command_id — 409, а не тихая подмена.
    let mut other = body();
    other.surface = "telegram".into();
    let conflict = create(State(runtime.clone()), Json(other))
        .await
        .into_response();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);

    // Повтор обязан вернуть ТОТ ЖЕ id: раньше он только вычислялся и глушился
    // `let _ = id`, и регрессия «replay отдаёт другую сессию» проходила бы.
    let replay = create(State(runtime.clone()), Json(body()))
        .await
        .into_response();
    let bytes = axum::body::to_bytes(replay.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["id"].as_str(), Some(id.as_str()), "{body}");
    drop(runtime);
}
