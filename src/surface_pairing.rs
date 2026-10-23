// HTTP-часть пейринга: открыть приглашение, обменять код на токен, отозвать.
//
// Маршруты живут отдельно от `surface_server` не для красоты: это единственные
// ручки, которые обязаны работать ДО того, как у клиента есть токен, поэтому
// `claim` исключён из auth-слоя явно (см. `surface_server::router`). Держать это
// рядом с обработчиками сессий значило бы прятать границу в глубине файла.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{
    clients::{self, Client},
    surface_runtime::Runtime,
};

/// Ответ на открытие пейринга: код и куда подключаться.
#[derive(Serialize)]
pub struct InviteReply {
    pub code: String,
    /// Сколько секунд код ещё действует — клиент рисует таймер.
    pub seconds_left: u64,
    /// Адреса, на которых ядро доступно снаружи (может быть пусто на loopback).
    pub endpoints: Vec<String>,
}

/// Что клиент присылает при обмене кода.
#[derive(Deserialize)]
pub struct Claim {
    pub code: String,
    /// Имя устройства от человека: «телефон», «ноутбук».
    pub name: String,
}

#[derive(Serialize)]
pub struct ClaimReply {
    /// Постоянный токен ЭТОГО устройства. Показывается один раз.
    pub token: String,
    pub client_id: String,
}

fn err(code: &str, msg: &str, status: StatusCode) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({"error": {"code": code, "message": msg}})),
    )
        .into_response()
}

/// Сколько адресов показать. Больше двух человек всё равно не введёт руками,
/// а список нужен, чтобы выбрать доступный (tailnet, LAN).
fn endpoints(s: &Arc<Runtime>) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(env) = std::env::var("SNENS_SURFACE_URL") {
        if !env.trim().is_empty() {
            out.push(env.trim().to_string());
        }
    }
    // Явно заданный публичный адрес полезнее локального: по нему и будут
    // подключаться. Порядок: переменная, потом локальный.
    if out.is_empty() {
        out.push("http://127.0.0.1:7420".into());
    }
    let _ = s;
    out
}

/// `POST /v1/pair/start` — открыть приглашение. Требует токена: приглашение
/// открывает тот, кто уже управляет машиной (TUI, веб на самом ядре).
pub async fn start(State(s): State<Arc<Runtime>>) -> impl IntoResponse {
    let origin = "api";
    match s.pending.open(origin) {
        Ok(invite) => Json(InviteReply {
            seconds_left: invite.left(clients::now()),
            code: invite.code.clone(),
            endpoints: endpoints(&s),
        })
        .into_response(),
        Err(e) => err("pairing", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// `GET /v1/pair/status` — действует ли приглашение (для таймера в UI).
pub async fn status(State(s): State<Arc<Runtime>>) -> impl IntoResponse {
    match s.pending.current() {
        Some(invite) => Json(serde_json::json!({
            "open": true,
            "seconds_left": invite.left(clients::now()),
            // Сам код не отдаём повторно: он уже показан там, где открывали.
        }))
        .into_response(),
        None => Json(serde_json::json!({"open": false})).into_response(),
    }
}

/// `POST /v1/pair/claim` — обменять код на постоянный токен устройства.
///
/// Единственный маршрут без bearer-токена: у клиента его ещё нет. Всё, что его
/// защищает, — код (5 попыток, 5 минут) и то, что приглашение надо сначала
/// открыть с локального адреса.
pub async fn claim(
    State(s): State<Arc<Runtime>>,
    Json(b): Json<Claim>,
) -> impl IntoResponse {
    if let Err(e) = s.pending.claim(&b.code) {
        // 401, а не 400: это неудачная аутентификация, и клиент должен
        // показать «код не подошёл», а не «ошибка сервера».
        return err("pairing", e, StatusCode::UNAUTHORIZED);
    }
    let name = b.name.trim();
    let name = if name.is_empty() { "устройство" } else { name };
    let token = clients::new_token();
    let client = Client {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.chars().take(64).collect(),
        token_sha256: sha256_hex(&token),
        created_at: clients::now(),
        last_seen: Some(clients::now()),
    };
    if let Err(e) = clients::save(&s.cfg.data_path(), &client) {
        return err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
    }
    s.registry.add(client.clone());
    Json(ClaimReply {
        token,
        client_id: client.id,
    })
    .into_response()
}

/// `GET /v1/clients` — подключённые устройства (без хешей токенов).
pub async fn list(State(s): State<Arc<Runtime>>) -> impl IntoResponse {
    // Перечитываем с диска: реестр мог пополниться из другого процесса.
    s.registry.reload(&s.cfg.data_path());
    let items: Vec<serde_json::Value> = s
        .registry
        .snapshot()
        .into_iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "name": c.name,
                "created_at": c.created_at,
                "last_seen": c.last_seen,
            })
        })
        .collect();
    Json(serde_json::json!({"clients": items})).into_response()
}

/// `DELETE /v1/clients/{id}` — отозвать устройство.
pub async fn revoke(
    State(s): State<Arc<Runtime>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match clients::revoke(&s.cfg.data_path(), &id) {
        Ok(true) => {
            s.registry.remove(&id);
            Json(serde_json::json!({"revoked": id})).into_response()
        }
        Ok(false) => err("not_found", "нет такого устройства", StatusCode::NOT_FOUND),
        Err(e) => err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

fn sha256_hex(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
