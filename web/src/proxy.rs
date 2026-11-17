//! Прокси `/api` → ядро.
//!
//! Зачем он вообще нужен, если ядро само отдаёт статику: токен инстанса — секрет,
//! и в браузер он попадать не должен. Vite-прокси решал это тем, что подставлял
//! `Authorization` на сервере; здесь то же самое, только отдельным бинарём без
//! node. Клиент в этом режиме ходит на относительный `/api` и заголовков не
//! ставит (`lib/connection.ts`, режим `proxy`).
//!
//! Что важно и не очевидно:
//!  - SSE потоковое: тело отдаётся как есть, без буферизации (иначе стрим ленты
//!    приезжает одним куском в конце хода);
//!  - `Accept-Encoding: identity` обязателен: gzip ломает поток;
//!  - ответы ядра не переписываются, поэтому контракт `/v1` остаётся одним.

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use std::sync::Arc;

/// Что прокси знает о ядре. Токен перечитывается на каждый запрос, а не
/// кешируется: `snens pair`/перезапуск ядра кладут новый файл, и клиент не
/// должен требовать перезапуска прокси.
pub struct Upstream {
    pub base: String,
    pub token_file: Option<std::path::PathBuf>,
    pub client: reqwest::Client,
}

pub type Shared = Arc<Upstream>;

/// Прочитать токен и пожаловаться в лог, если файл настроен, но не читается.
///
/// Молчаливый `.ok()` хуже: запрос уходил бы без `Authorization`, и человек
/// видел бы невнятный 401 от ядра вместо «файл токена не читается».
/// `tokio::fs`, а не `std::fs`: блокирующее чтение в async-обработчике
/// занимало бы воркер tokio на каждый запрос через прокси.
async fn token_or_log(up: &Upstream) -> Option<String> {
    if let Some(path) = up.token_file.as_ref() {
        match tokio::fs::read_to_string(path).await {
            Ok(text) if !text.trim().is_empty() => return Some(text.trim().to_string()),
            Ok(_) => eprintln!("snens-web: файл токена пуст: {}", path.display()),
            // Ещё не создан — штатное состояние до первого старта ядра.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("snens-web: не прочитать токен {}: {e}", path.display()),
        }
    }
    None
}

/// Заголовки, которые нельзя копировать как есть.
///
/// `host` относится к нам, а не к ядру: с нашим host'ом ядро ответило бы не туда
/// (виртуальные хосты, Caddy). `authorization` — потому что мы ставим свой;
/// чужой означает «клиент спарен с ядром сам», и в режиме прокси это не так.
/// Остальные `hop-by-hop` запрещены RFC и ломают соединение.
const SKIP: &[&str] = &[
    "host",
    "authorization",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "proxy-authorization",
    "proxy-authenticate",
];

fn forward_headers(src: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in src {
        if SKIP.contains(&name.as_str()) {
            continue;
        }
        // `append`, а не `insert`: у повторяющегося заголовка (`Accept`, `X-*`)
        // `insert` оставил бы только последнее значение.
        out.append(name.clone(), value.clone());
    }
    // Поток не должен сжиматься: SSE с gzip приезжает целиком в конце.
    out.insert(header::ACCEPT_ENCODING, "identity".parse().unwrap());
    out
}

fn skip_downstream(name: &str) -> bool {
    matches!(
        name,
        "connection" | "keep-alive" | "transfer-encoding" | "content-length"
    )
}

pub async fn proxy(State(up): State<Shared>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    // Путь клиента — `/api/v1/...`, у ядра — `/v1/...`: префикс снимаем ровно
    // один, чтобы `/api` не превращался в корень ядра (как это делал vite).
    let path = parts
        .uri
        .path()
        .strip_prefix("/api")
        .filter(|p| !p.is_empty())
        .unwrap_or("/");
    // `..`/`.` в пути схлопывает сам reqwest: `GET /api/../admin` ушёл бы в ядро
    // как `/admin` и достал бы ручку вне `/v1` — с нашим Bearer-токеном в
    // заголовке. То есть обоёл бы ровно ту границу, ради которой сделан прокси.
    if path.split('/').any(|seg| seg == ".." || seg == ".") || path.contains('\\') {
        return (StatusCode::BAD_REQUEST, "недопустимый путь").into_response();
    }
    let url = match parts.uri.query() {
        Some(q) => format!("{}{path}?{q}", up.base),
        None => format!("{}{path}", up.base),
    };
    // Собранный из частей URL тоже проверяем: `Url` нормализует путь, и если
    // после нормализации он перестал начинаться с `/`, запрос ушёл бы не туда.
    if !path.starts_with('/') {
        return (StatusCode::BAD_REQUEST, "недопустимый путь").into_response();
    }

    let method = parts.method;
    let mut out = up
        .client
        .request(method, &url)
        .headers(forward_headers(&parts.headers));
    if let Some(t) = token_or_log(&up).await {
        out = out.bearer_auth(t);
    }

    // Тело запроса — потоком: загрузка вложения не должна оседать в памяти
    // прокси целиком.
    let stream = body.into_data_stream();
    let out = out.body(reqwest::Body::wrap_stream(stream));
    let upstream = match out.send().await {
        Ok(r) => r,
        Err(e) => {
            // Ядро лежит — это не 502 «кто-то виноват», а понятная человеку
            // строка: клиент покажет её в ленте ошибок.
            return (
                StatusCode::BAD_GATEWAY,
                format!("ядро не ответило ({}): {e}", up.base),
            )
                .into_response();
        }
    };

    let status = upstream.status();
    let mut headers = HeaderMap::new();
    for (name, value) in upstream.headers() {
        if skip_downstream(name.as_str()) {
            continue;
        }
        // `append`: `Set-Cookie` в ответе нельзя схлопывать до последнего.
        headers.append(name.clone(), value.clone());
    }
    headers.insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("статическая строка"),
    );
    // Тело — поток: SSE и стриминг ответа модели сохраняются как есть.
    let body = Body::from_stream(
        upstream
            .bytes_stream()
            .map(|chunk| chunk.map_err(|e| std::io::Error::other(e.to_string()))),
    );
    (status, headers, body).into_response()
}
