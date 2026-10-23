// Мелочи HTTP-слоя поверхности: единый формат ошибки и проверка токена.
//
// Отдельным модулем, а не внутри роутера: `err` зовут все обработчики, и
// импортировать их из файла с маршрутами значило бы тянуть друг друга по кругу
// ради шести строк.

use axum::{
    Json,
    http::{StatusCode, header},
    response::IntoResponse,
};
use std::sync::Arc;

use crate::surface_runtime::Runtime;

/// Что нужно проверке токена: сам токен инстанса и реестр спаренных устройств.
///
/// `runtime` целиком, а не копия реестра: отзыв устройства должен действовать
/// немедленно, и проверка обязана смотреть в то же состояние, что и `DELETE
/// /v1/clients/{id}`.
pub(super) struct AuthCtx {
    pub token: String,
    pub runtime: Arc<Runtime>,
}

pub(super) async fn auth(
    axum::extract::State(ctx): axum::extract::State<Arc<AuthCtx>>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> impl IntoResponse {
    let header_token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|x| x.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    // `EventSource` в браузере не умеет заголовков, и `fetch`-обёртка над SSE
    // лишает нас авто-переподключения. Поэтому для SSE-маршрутов токен
    // принимается ещё и в query (`?token=…`). Цена решения честная: query
    // попадает в логи прокси и историю. Поэтому — только для GET-потоков, у
    // которых нет тела и которые и так живут в браузере; для остальных API
    // остаётся заголовок.
    let query_token = if req.method() == axum::http::Method::GET && is_stream_path(req.uri().path())
    {
        req.uri().query().and_then(|q| {
            q.split('&').find_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                (k == "token" && !v.is_empty()).then(|| v.to_string())
            })
        })
    } else {
        None
    };
    let Some(given) = header_token.map(str::to_string).or(query_token) else {
        return unauthorized();
    };
    // Два законных способа войти: токен инстанса (локальные клиенты: TUI, веб
    // на том же хосте) и токен спаренного устройства из реестра. Второй можно
    // отозвать поштучно, не меняя секрет остальных.
    if given == ctx.token {
        return next.run(req).await;
    }
    if ctx.runtime.registry.owner(&given).is_some() {
        return next.run(req).await;
    }
    unauthorized()
}

/// Потоки, где токен в query допустим: `EventSource` не умеет заголовков.
fn is_stream_path(path: &str) -> bool {
    path.ends_with("/events") || path == "/v1/events"
}

fn unauthorized() -> axum::response::Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"error":{"code":"unauthorized","message":"Bearer token required"}})),
    )
        .into_response()
}

pub(super) fn err(code: &str, msg: &str, status: StatusCode) -> impl IntoResponse {
    (
        status,
        Json(serde_json::json!({"error":{"code":code,"message":msg}})),
    )
}
