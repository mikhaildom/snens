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
    pairing,
    commands::{message, stop, tool_result},
    events::{events, history, ingest, session_updates},
    files::{session_file, upload_attachment},
    http::{AuthCtx, auth},
    query::{context_breakdown, search},
    sessions::{create, delete_session, get_session, list, register_tools},
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

    protected.merge(claim)
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
