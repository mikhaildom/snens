// Маршруты профилей провайдеров на уровне HTTP: вызываем сами хендлеры и
// разбираем ответ (статус + JSON). Проверяем именно то, что видит клиент —
// коды ошибок, отсутствие ключа в выдаче, — а предметные запреты (неполный
// профиль, активный не удаляется) живут в `providers_tests.rs`.

use super::*;
use crate::cfg::Config;
use crate::surface_runtime::{self as rt, Runtime};
use axum::body::to_bytes;
use axum::response::IntoResponse;
use tokio::sync::{Mutex, broadcast};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Runtime с настоящим конфигом во временном каталоге: маршруты пишут файл, и
/// подменять его фейком нельзя — иначе тест не увидит, что попало на диск.
async fn provider_runtime(name: &str) -> (Arc<Runtime>, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "surface-prov-{}-{name}-{}",
        std::process::id(),
        name.len()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        "# шапка человека\nbase_url = \"http://top/v1\"\nmodel = \"top/model\"\nkey_env = \"TOP_KEY\"\n",
    )
    .unwrap();
    let cfg = Config::load(Some(&path.display().to_string())).unwrap();
    // Каталог данных рядом, но отдельно от конфига: маршруты профилей его не
    // трогают, а держать настоящий store дешевле, чем собирать фейк.
    let (store, db) = rt::open_store(&dir.join("data")).unwrap();
    let runtime = Arc::new(Runtime {
        data: Mutex::new(store),
        db: std::sync::Mutex::new(db),
        tx: broadcast::channel(16).0,
        cfg,
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
        tx_updates: broadcast::channel(16).0,
    });
    (runtime, dir)
}

/// Статус и тело ответа хендлера.
async fn reply(resp: axum::response::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn create(rt: &Arc<Runtime>, body: Value) -> (StatusCode, Value) {
    reply(post_provider(State(rt.clone()), Json(body)).await.into_response()).await
}

async fn list_of(rt: &Arc<Runtime>) -> (StatusCode, Value) {
    reply(get_providers(State(rt.clone())).await.into_response()).await
}

async fn edit(rt: &Arc<Runtime>, name: &str, body: Value) -> (StatusCode, Value) {
    reply(
        put_provider(State(rt.clone()), Path(name.to_string()), Json(body))
            .await
            .into_response(),
    )
    .await
}

async fn activate(rt: &Arc<Runtime>, name: &str) -> (StatusCode, Value) {
    reply(
        activate_provider(State(rt.clone()), Path(name.to_string()))
            .await
            .into_response(),
    )
    .await
}

async fn delete(rt: &Arc<Runtime>, name: &str) -> (StatusCode, Value) {
    reply(
        delete_provider(State(rt.clone()), Path(name.to_string()))
            .await
            .into_response(),
    )
    .await
}

#[tokio::test]
async fn crud_round_trip_keeps_key_out_of_config() {
    let (rt, dir) = provider_runtime("crud").await;
    // Конфига без секции достаточно, чтобы список что-то показал: отдаём
    // виртуальный профиль из top-level ключей.
    let (status, body) = list_of(&rt).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["providers"][0]["name"], crate::providers::TOP_LEVEL);
    assert_eq!(body["providers"][0]["base_url"], "http://top/v1");

    // Создание первого профиля: он же становится активным.
    let (status, body) = create(
        &rt,
        json!({"name": "nous", "preset": "nous", "key": "sk-nous-1"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["active"], "nous");
    let nous = &body["providers"][0];
    assert_eq!(nous["name"], "nous");
    assert_eq!(nous["key_set"], true);
    assert_eq!(
        nous["base_url"], "https://inference-api.nousresearch.com/v1"
    );
    // Ключа в ответе нет ни в каком виде.
    assert!(!body.to_string().contains("sk-nous-1"), "{body}");

    let toml = std::fs::read_to_string(dir.join("config.toml")).unwrap();
    assert!(toml.contains("# шапка человека"), "комментарий съеден: {toml}");
    assert!(!toml.contains("sk-nous-1"), "ключ попал в конфиг: {toml}");
    let env = std::fs::read_to_string(dir.join(".env")).unwrap();
    assert!(env.contains("sk-nous-1"), "{env}");

    // Дубль имени — 409, а не второй профиль.
    let (status, _) = create(&rt, json!({"name": "nous", "base_url": "https://x/v1", "model": "m"})).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Клиент шлёт `activate` (и может прислать вычисляемые поля ответа) — это не
    // 400: иначе панель не заведёт профиль из пресета.
    let (status, body) = create(
        &rt,
        json!({"name": "прибывший", "base_url": "https://y/v1", "model": "m",
               "key": "k", "activate": false, "active": false, "env": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = delete(&rt, "прибывший").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Правка без ключа: ключ остаётся, адрес меняется.
    let (status, body) = edit(&rt, "nous", json!({"model": "другая/модель"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["providers"][0]["model"], "другая/модель");
    assert_eq!(body["providers"][0]["key_set"], true);

    // Второй профиль заводим неактивным, активируем маршрутом.
    let (status, body) = create(
        &rt,
        json!({"name": "openrouter", "base_url": "https://openrouter.ai/api/v1/", "model": "m"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["active"], "nous", "второй профиль перебил активного");
    // Хвостовой слэш убран: иначе `…/v1//chat/completions`.
    assert_eq!(
        body["providers"][1]["base_url"], "https://openrouter.ai/api/v1"
    );
    assert_eq!(body["providers"][1]["key_set"], false, "ключ второго профиля не задан");

    // Активного удалить нельзя — 409; после переключения можно.
    let (status, _) = delete(&rt, "nous").await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, body) = activate(&rt, "openrouter").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["active"], "openrouter");
    let (status, body) = delete(&rt, "nous").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let names: Vec<&str> = body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["openrouter"], "удалённый профиль остался: {body}");

    // Неизвестное имя — 404 на всех трёх именованных маршрутах.
    assert_eq!(edit(&rt, "nous", json!({"model": "m"})).await.0, StatusCode::NOT_FOUND);
    assert_eq!(activate(&rt, "nous").await.0, StatusCode::NOT_FOUND);
    assert_eq!(delete(&rt, "nous").await.0, StatusCode::NOT_FOUND);
    drop(rt);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn validation_errors_are_client_errors_and_write_nothing() {
    let (rt, dir) = provider_runtime("invalid").await;
    let before = std::fs::read_to_string(dir.join("config.toml")).unwrap();
    for (body, why) in [
        (json!({"name": "a.b", "base_url": "https://x/v1", "model": "m"}), "точка в имени"),
        (json!({"name": "", "base_url": "https://x/v1", "model": "m"}), "пустое имя"),
        (json!({"name": "a", "base_url": "", "model": "m"}), "пустой base_url"),
        (json!({"name": "a", "base_url": "ftp://x", "model": "m"}), "не http"),
        (json!({"name": "a", "base_url": "https://x/v1", "model": " "}), "пустая модель"),
        (json!({"name": "a", "preset": "нет"}), "неизвестный пресет"),
        (json!({"name": "a", "base_url": 1}), "не строка"),
        (json!({"name": "a", "nosuch": "x"}), "неизвестное поле"),
    ] {
        let (status, body) = create(&rt, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why}: {body}");
    }
    // Ни одна ошибка не оставила следа на диске.
    assert_eq!(std::fs::read_to_string(dir.join("config.toml")).unwrap(), before);
    assert!(!dir.join(".env").exists(), "создан .env без ключа");

    // Профиль без модели активировать нельзя: ядро осталось бы без slug.
    let (status, _) = create(&rt, json!({"name": "half", "base_url": "https://x/v1", "model": "m"})).await;
    assert_eq!(status, StatusCode::OK);
    std::fs::write(
        dir.join("config.toml"),
        "model = \"top\"\nactive_provider = \"\"\n\n[providers.half]\nbase_url = \"https://x/v1\"\n",
    )
    .unwrap();
    let (status, _) = activate(&rt, "half").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "неполный профиль активен");
    drop(rt);
    std::fs::remove_dir_all(dir).ok();
}
