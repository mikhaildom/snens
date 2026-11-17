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
///
/// Смотрим только путь ОТНОСИТЕЛЬНО `dist`: абсолютный путь включит в проверку
/// каталоги-предки, и поставка, распакованная в `/srv/assets/web/dist`, отдавала
/// бы `index.html` с `immutable` — то есть обновление никогда бы не доехало.
fn cache_control(rel: &std::path::Path) -> &'static str {
    let immutable = rel.components().any(|c| c.as_os_str() == "assets");
    if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

pub async fn serve(State(st): State<Shared>, req: Request) -> Response {
    let raw = req.uri().path().trim_start_matches('/');
    let rel = if raw.is_empty() { "index.html" } else { raw };
    // Только обычные компоненты: это одним движением отсекает `..`, `.`,
    // абсолютные пути и виндовые префиксы диска (`C:/...` — `join` вернул бы
    // его как есть, и файл нашёлся бы ВНЕ `dist`). Заодно не ломаются
    // легитимные имена вида `app..js`.
    let rel_path = std::path::Path::new(rel);
    let bad = rel_path
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)));
    if bad {
        return (StatusCode::BAD_REQUEST, "плохой путь").into_response();
    }
    let candidate = st.dist.join(rel_path);

    // Читаем сразу, без предварительного `is_file()`: `stat` + `read` — это
    // check-then-act, и пересборка клиента между ними давала 500 на месте 404.
    match tokio::fs::read(&candidate).await {
        Ok(body) => return file(&candidate, rel_path, body),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        // Остальные ошибки — наши, а не клиента: в лог, наружу общая строка.
        Err(e) => {
            eprintln!("snens-web: не прочитать {}: {e}", candidate.display());
            return (StatusCode::INTERNAL_SERVER_ERROR, "не прочитать файл").into_response();
        }
    }

    // Нет файла. С расширением — честный 404 (картинка, шрифт, чанк),
    // без расширения — SPA-маршрут, отдаём index.
    let has_ext = rel_path.extension().is_some_and(|e| !e.is_empty());
    if has_ext {
        return (StatusCode::NOT_FOUND, "не найдено").into_response();
    }
    // Фолбэк тоже читается с диска, а не из памяти: иначе после `npm run build`
    // без рестарта `/` отдавал бы старый index со ссылками на старые хеши
    // ассетов, а сами ассеты были бы уже новые.
    let index_path = st.dist.join("index.html");
    let body = match tokio::fs::read(&index_path).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("snens-web: нет {}: {e}", index_path.display());
            return (StatusCode::INTERNAL_SERVER_ERROR, "нет index.html").into_response();
        }
    };
    let mut response = Response::new(Body::from(body));
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

fn file(path: &std::path::Path, rel: &std::path::Path, body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    if let Ok(value) = header::HeaderValue::from_str(content_type(path)) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    // Кеш-политика — от пути ВНУТРИ клиента, не от абсолютного: см. `cache_control`.
    if let Ok(value) = header::HeaderValue::from_str(cache_control(rel)) {
        headers.insert(header::CACHE_CONTROL, value);
    }
    response
}
