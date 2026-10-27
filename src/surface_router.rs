// Маршруты поверхности — единственное место, где записан контракт с клиентом.
//
// Вынесено из `serve` ради одной цели: чтобы API читался списком. Ни одна
// строка контракта не менялась при разрезании — правила, за которые здесь
// отвечают (`auth` внутри, CORS снаружи), объяснены рядом со слоями.

use crate::surface_runtime::Runtime;
use axum::{
    Router,
    routing::{get, post},
};
use std::sync::Arc;

use super::{
    approvals::approve,
    dashboard::{reviews, ticket, tickets},
    pairing,
    commands::{message, stop, tool_result},
    events::{events, history, ingest, session_updates},
    files::{session_file, upload_attachment},
    http::{AuthCtx, auth},
    query::{context_breakdown, search},
    sessions::{create, delete_session, get_session, list, register_tools},
    genui::get as genui_lib,
    settings::{
        activate_provider, delete_provider, get_config, get_memory, get_model, get_providers,
        post_provider, put_config, put_memory, put_model, put_provider,
    },
};

/// Собрать приложение поверхности: маршруты, состояние, слои middleware.
///
/// Три роутера, а не один, потому что у них РАЗНЫЕ правила доступа:
///
/// - `protected` — всё обычное API: требует bearer-токен;
/// - `pairing_admin` — открыть приглашение и управлять устройствами: тоже требует
///   токен (приглашение открывает тот, кто уже внутри);
/// - `pairing_claim` — обмен кода на токен: единственный маршрут БЕЗ токена, у
///   клиента его ещё нет. Его защищает код (5 минут, 5 попыток).
///
/// Слои навешиваются на каждый роутер ДО `merge`: `merge` не переносит слои, и
/// общий `.layer` в конце покрыл бы `claim` тоже — проверено, он отвечал 401.
pub(super) fn app(state: Arc<Runtime>, token: String) -> Router {
    let auth_ctx = Arc::new(AuthCtx {
        token,
        runtime: state.clone(),
    });

    let protected = Router::new()
        .route("/v1/sessions", get(list).post(create))
        .route("/v1/sessions/{id}", get(get_session).delete(delete_session))
        .route("/v1/sessions/{id}/messages", post(message))
        .route(
            "/v1/sessions/{id}/tools",
            axum::routing::put(register_tools),
        )
        .route(
            "/v1/sessions/{sid}/runs/{rid}/tools/{call_id}/result",
            post(tool_result),
        )
        .route("/v1/search", get(search))
        // Дашборд: тикеты инстанса и ревью-сессии. Только чтение — это данные,
        // сложенные самим инстансом, а не действие клиента.
        .route("/v1/tickets", get(tickets))
        .route("/v1/tickets/{name}", get(ticket))
        .route("/v1/reviews", get(reviews))
        .route("/v1/sessions/{id}/context", get(context_breakdown))
        .route("/v1/sessions/{id}/attachments", post(upload_attachment))
        .route("/v1/sessions/{id}/file", get(session_file))
        .route("/v1/sessions/{id}/stop", post(stop))
        .route("/v1/sessions/{id}/approvals/{aid}", post(approve))
        .route("/v1/sessions/{id}/events", get(events))
        .route("/v1/sessions/{id}/history", get(history))
        // Приём событий от процесса подагента: у него нет собственной SSE-шины
        // (её держит только поверхность), поэтому он шлёт сигналы сюда, а
        // поверхность эмитит их в общий поток — клиент стримит подагента так же,
        // как корневую сессию.
        .route("/v1/sessions/{id}/ingest", post(ingest))
        // Библиотека компонентов генеративного UI: исходники целиком, чтобы
        // клиент инжектил их в артефакт инлайном (сети в песочнице нет).
        .route("/v1/genui", get(genui_lib))
        .route("/v1/model", get(get_model).put(put_model))
        .route("/v1/memory", get(get_memory).put(put_memory))
        .route("/v1/config", get(get_config).put(put_config))
        // Профили провайдеров: список без ключей (только «ключ задан»), CRUD и
        // отдельный маршрут переключения. Активация отдельным POST, а не флагом
        // в PUT: у неё свои требования (профиль полный) и своя цена ошибки
        // (ядро осталось бы без рабочего адреса).
        .route("/v1/providers", get(get_providers).post(post_provider))
        .route(
            "/v1/providers/{name}",
            axum::routing::put(put_provider).delete(delete_provider),
        )
        .route("/v1/providers/{name}/activate", post(activate_provider))
        // Список сессий без опроса: подагент растёт, пока его карточка на экране,
        // и клиент должен видеть ходы и конец работы, а не снимок на загрузке.
        .route("/v1/events", get(session_updates))
        .route("/v1/pair/start", post(pairing::start))
        .route("/v1/pair/status", get(pairing::status))
        .route("/v1/clients", get(pairing::list))
        .route("/v1/clients/{id}", axum::routing::delete(pairing::revoke))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(
            auth_ctx.clone(),
            auth,
        ));

    // Без auth: клиент приходит без токена и получает его здесь.
    let claim = Router::new()
        .route("/v1/pair/claim", post(pairing::claim))
        .with_state(state);

    // Собранный веб-клиент, если он лежит рядом с ядром: в десктопной поставке
    // это `resources/web` (или `SNENS_WEB_DIR`), в dev — пусто, и тогда клиент
    // живёт на vite, который сам проксирует /api. Отдача статики идёт ДО auth:
    // index.html и ассеты — не секрет, а токен клиент получит сам.
    let app = match web_root() {
        Some(dir) => {
            let index = dir.join("index.html");
            // SPA: клиент — одностраничник, и `/some/route` рисует тот же
            // index.html. Поэтому «не найдено в каталоге» отдаёт index, а не 404.
            protected.merge(claim).fallback_service(
                tower_http::services::ServeDir::new(&dir).fallback(
                    tower_http::services::ServeFile::new(index),
                ),
            )
        }
        None => protected.merge(claim),
    };
    app
        // Порядок слоёв важен: в axum последний `.layer` оказывается СНАРУЖИ.
        // CORS снаружи auth, иначе preflight `OPTIONS` (он не несёт Authorization)
        // получал бы 401, и браузер не отправил бы сам запрос.
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        )
}

/// Каталог собранного веб-клиента: `SNENS_WEB_DIR`, иначе `web/` рядом с
/// бинарником (так его кладёт установщик). Нет `index.html` — клиента нет.
fn web_root() -> Option<std::path::PathBuf> {
    let candidate = match std::env::var("SNENS_WEB_DIR") {
        Ok(dir) if !dir.trim().is_empty() => std::path::PathBuf::from(dir),
        _ => std::env::current_exe().ok()?.parent()?.join("web"),
    };
    candidate.join("index.html").is_file().then_some(candidate)
}
