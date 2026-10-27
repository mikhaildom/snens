//! Отдача собранного клиента: статика и SPA-фолбэк.
//!
//! Клиент — одностраничник, но с одним исключением: `/releases/` и прочие
//! корни сайта он не обслуживает (это дело сайта, не клиента). Поэтому фолбэк
//! на `index.html` включается только для путей без расширения — иначе запрос
//! отсутствующей картинки возвращал бы HTML вместо 404, и человек видел бы
//! «сломанную ленту» без причины в консоли.

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};

pub struct Static {
    pub dist: std::path::PathBuf,
    pub index: String,
}

pub type Shared = std::sync::Arc<Static>;

fn content_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("map") => "application/json",
        _ => "application/octet-stream",
    }
}

/// Ассеты с хешем в имени кешируются навсегда: имя меняется вместе с
/// содержимым. `index.html` — никогда: иначе обновление клиента не доедет.
fn cache_control(path: &std::path::Path) -> &'static str {
    let immutable = path.components().any(|c| c.as_os_str() == "assets");
    if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

pub async fn serve(State(st): State<Shared>, req: Request) -> Response {
    let raw = req.uri().path().trim_start_matches('/');
    // Защита от `..`: каталог клиента плоский снаружи, но проверка дешёвая.
    if raw.contains("..") {
        return (StatusCode::BAD_REQUEST, "плохой путь").into_response();
    }
    let rel = if raw.is_empty() { "index.html" } else { raw };
    let candidate = st.dist.join(rel);

    if candidate.is_file() {
        return file(&candidate).await;
    }

    // Нет файла. С расширением — честный 404 (картинка, шрифт, чанк),
    // без расширения — SPA-маршрут, отдаём index.
    let has_ext = std::path::Path::new(rel)
        .extension()
        .is_some_and(|e| !e.is_empty());
    if has_ext {
        return (StatusCode::NOT_FOUND, "не найдено").into_response();
    }
    let mut response = (StatusCode::OK, st.index.clone()).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache"),
    );
    response
}

async fn file(path: &std::path::Path) -> Response {
    let body = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("не прочитать {}: {e}", path.display()),
            )
                .into_response();
        }
    };
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    if let Ok(value) = header::HeaderValue::from_str(content_type(path)) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Ok(value) = header::HeaderValue::from_str(cache_control(path)) {
        headers.insert(header::CACHE_CONTROL, value);
    }
    response
}
