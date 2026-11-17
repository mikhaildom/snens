// Настройки инстанса: модель, конфиг, память и профили провайдеров.
//
// Общее у них одно и то же правило: меню показывает то, что лежит в файле, а не
// снимок старта процесса, поэтому каждый обработчик перечитывает конфиг
// (`Config::fresh`/`model_now`/`memory_now`) вместо `s.cfg`.

use crate::surface_runtime::Runtime;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

use super::http::err;

/// Текущая модель: перечитанная из конфига, а не снимок старта сервера.
/// Тем самым `snens model` и `/model` на любой поверхности видят одно и то же.
pub(super) async fn get_model(State(s): State<Arc<Runtime>>) -> impl IntoResponse {
    Json(serde_json::json!({"model": s.cfg.model_now()})).into_response()
}

#[derive(Deserialize)]
pub(super) struct ModelBody {
    model: String,
}
/// Сменить основную модель: правит `model = "..."` в конфиге и сразу делает её
/// текущей для новых ходов (перечитка в `run_loop`). Активный ход не трогаем.
pub(super) async fn put_model(
    State(s): State<Arc<Runtime>>,
    Json(body): Json<ModelBody>,
) -> impl IntoResponse {
    let slug = body.model.trim();
    if slug.is_empty() {
        return err("invalid", "empty model slug", StatusCode::BAD_REQUEST).into_response();
    }
    // Та же проверка, что у `PUT /v1/config` (`settings::prepare` → `text()`):
    // slug с переводом строки/табом проходил тут, но отвергался там.
    if slug.chars().any(char::is_control) {
        return err(
            "invalid",
            "model slug contains control characters",
            StatusCode::BAD_REQUEST,
        )
        .into_response();
    }
    // Пишем в тот же конфиг, что читает этот процесс (учёт `--config`).
    let mut cfg = s.cfg.clone();
    match cfg.set_model_now(slug) {
        Ok(path) => Json(serde_json::json!({"model": slug, "config": path.display().to_string()}))
            .into_response(),
        // `set_model` отказывает и по делу (активный провайдер диктует модель,
        // пустой slug), и по I/O. Домен — 400, запись — 500.
        Err(e) => {
            let message = e.to_string();
            if provider_error_is_client(&message) {
                err("invalid", &message, StatusCode::BAD_REQUEST).into_response()
            } else {
                err("config", &message, StatusCode::INTERNAL_SERVER_ERROR).into_response()
            }
        }
    }
}

/// Меню настроек инстанса: все правленые ключи одним ответом.
///
/// Ответ — то, что лежит в файле, а не снимок старта сервиса: поверхность
/// показывает реальность, а действие правки («со следующего хода»)
/// обеспечено перечиткой в `agent::run_loop` (`Config::live`).
pub(super) async fn get_config(State(s): State<Arc<Runtime>>) -> impl IntoResponse {
    let path = s
        .cfg
        .source_path
        .clone()
        .unwrap_or_else(crate::cfg::config_path);
    Json(crate::settings::state(&s.cfg, &path)).into_response()
}

/// Список профилей провайдеров: ключей в ответе нет — только `key_set`.
/// Читаем из файла (как и настройки): меню показывает диск, а не снимок старта.
pub(super) async fn get_providers(State(s): State<Arc<Runtime>>) -> impl IntoResponse {
    Json(crate::providers::list(&s.cfg)).into_response()
}

/// Разбор тела профиля — общий для POST/PUT. Ошибка здесь всегда вина клиента,
/// поэтому возвращается текстом: ответ собирает вызывающий (у него статус).
pub(super) fn provider_draft(
    body: &serde_json::Value,
    name: &str,
) -> Result<crate::providers::Draft, String> {
    crate::providers::Draft::parse(body, name).map_err(|e| e.to_string())
}

/// Имя из пути: пустое или невалидное — 400 до всякой записи.
pub(super) fn provider_name(path_param: &str) -> Result<String, String> {
    let name = crate::providers::normalize_name(path_param);
    crate::providers::validate_name(&name).map_err(|e| e.to_string())?;
    Ok(name)
}

/// Создать провайдера: `{"name", "base_url", "model", "key"}` или
/// `{"name", "preset": "nous"}`. Пресет ключа не несёт — подставить чужой
/// секрет нечем и незачем.
///
/// Первый созданный профиль сразу становится активным: иначе человек завёл
/// провайдера и не понимает, почему ядро ходит старым адресом.
pub(super) async fn post_provider(
    State(s): State<Arc<Runtime>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let raw = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let name = match provider_name(&raw) {
        Ok(name) => name,
        Err(e) => return err("invalid", &e, StatusCode::BAD_REQUEST).into_response(),
    };
    let live = s.cfg.fresh().unwrap_or_else(|| s.cfg.clone());
    if live.providers.contains_key(&name) {
        return err(
            "exists",
            &format!("профиль «{name}» уже есть: правь его, а не создавай второй"),
            StatusCode::CONFLICT,
        )
        .into_response();
    }
    let draft = match provider_draft(&body, &name) {
        Ok(draft) => draft,
        Err(e) => return err("invalid", &e, StatusCode::BAD_REQUEST).into_response(),
    };
    let activate =
        live.providers.is_empty() || body.get("activate").and_then(|v| v.as_bool()) == Some(true);
    if let Err(e) = crate::providers::write_profile(&s.cfg, &name, &draft, None, activate) {
        return provider_error(&e.to_string(), StatusCode::BAD_REQUEST);
    }
    Json(crate::providers::list(&s.cfg)).into_response()
}

/// Изменить провайдера: любое подмножество полей. Отсутствующий ключ не стирается
/// (иначе правка модели без ключа отключала бы провайдера).
pub(super) async fn put_provider(
    State(s): State<Arc<Runtime>>,
    Path(raw): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let name = match provider_name(&raw) {
        Ok(name) => name,
        Err(e) => return err("invalid", &e, StatusCode::BAD_REQUEST).into_response(),
    };
    let live = s.cfg.fresh().unwrap_or_else(|| s.cfg.clone());
    let Some(existing) = live.providers.get(&name).cloned() else {
        return err(
            "not_found",
            &format!("профиль «{name}» не найден"),
            StatusCode::NOT_FOUND,
        )
        .into_response();
    };
    let draft = match provider_draft(&body, &name) {
        Ok(draft) => draft,
        Err(e) => return err("invalid", &e, StatusCode::BAD_REQUEST).into_response(),
    };
    // Пустое тело — не ошибка: PUT без полей значит «оставь как было».
    if let Err(e) = crate::providers::write_profile(&s.cfg, &name, &draft, Some(&existing), false) {
        return provider_error(&e.to_string(), StatusCode::BAD_REQUEST);
    }
    Json(crate::providers::list(&s.cfg)).into_response()
}

pub(super) async fn delete_provider(
    State(s): State<Arc<Runtime>>,
    Path(raw): Path<String>,
) -> impl IntoResponse {
    let name = match provider_name(&raw) {
        Ok(name) => name,
        Err(e) => return err("invalid", &e, StatusCode::BAD_REQUEST).into_response(),
    };
    let live = s.cfg.fresh().unwrap_or_else(|| s.cfg.clone());
    if !live.providers.contains_key(&name) {
        return err(
            "not_found",
            &format!("профиль «{name}» не найден"),
            StatusCode::NOT_FOUND,
        )
        .into_response();
    }
    // Активный не удаляем: `active_provider` указывал бы в пустоту. Это правило
    // предметной области — 409, а не 400.
    match crate::providers::remove_profile(&s.cfg, &name) {
        Ok(()) => Json(crate::providers::list(&s.cfg)).into_response(),
        // «Нельзя удалить активный» — 409, сбой записи — 500.
        Err(e) => provider_error(&e.to_string(), StatusCode::CONFLICT),
    }
}

/// Доменная ли это ошибка правки профиля (400/409), а не сбой записи (500).
///
/// `providers::*` возвращают один `crate::R<()>` и на «имя пустое», и на
/// «не смог записать файл». Ошибку записи отличаем по форме: она несёт путь
/// файла или `io::Error`-текст из `env_file`/`cfg_edit`, а доменные сообщения
/// перечислены явно. Иначе клиент повторял бы валидный запрос вечно, а диск бы
/// молчал.
fn provider_error_is_client(message: &str) -> bool {
    const DOMAIN: [&str; 14] = [
        "имя профиля",
        "base_url",
        "model",
        "key_env",
        "профиль «",
        "нечего применять",
        "недопустим",
        "переключись",
        "неполный",
        "нужен адрес вида",
        "str чужой",
        "дубликат",
        "должен начинаться",
        "содержит недопустимые",
    ];
    DOMAIN.iter().any(|needle| message.contains(needle))
}

/// Ответ на ошибку правки профиля: домен — 400, всё остальное — 500 `storage`.
fn provider_error(message: &str, status: StatusCode) -> axum::response::Response {
    let status = if provider_error_is_client(message) {
        status
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    err(
        if status.is_server_error() {
            "storage"
        } else {
            "invalid"
        },
        message,
        status,
    )
    .into_response()
}

/// Сделать профиль активным. Адрес и модель действуют со следующего хода:
/// активный профиль подставляется в `Config::fresh`.
pub(super) async fn activate_provider(
    State(s): State<Arc<Runtime>>,
    Path(raw): Path<String>,
) -> impl IntoResponse {
    let name = match provider_name(&raw) {
        Ok(name) => name,
        Err(e) => return err("invalid", &e, StatusCode::BAD_REQUEST).into_response(),
    };
    let live = s.cfg.fresh().unwrap_or_else(|| s.cfg.clone());
    if !live.providers.contains_key(&name) {
        return err(
            "not_found",
            &format!("профиль «{name}» не найден"),
            StatusCode::NOT_FOUND,
        )
        .into_response();
    }
    match crate::providers::activate(&s.cfg, &name) {
        Ok(()) => Json(crate::providers::list(&s.cfg)).into_response(),
        Err(e) => provider_error(&e.to_string(), StatusCode::BAD_REQUEST),
    }
}

/// Правка настроек пачкой: `{"max_turns": 0}`. Одно битое поле — ни одна правка
/// не применяется: проверка целиком до записи.
pub(super) async fn put_config(
    State(s): State<Arc<Runtime>>,
    Json(patch): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Разбор правки и запись — два разных ответа: неверный ключ/тип — вина
    // клиента (400), несостоявшаяся запись — сервера (500). Слитый ответ не даёт
    // человеку понять, повторять запрос или менять данные.
    let edits = match crate::settings::prepare(&s.cfg, &patch) {
        Ok(edits) => edits,
        Err(e) => return err("invalid", &e.to_string(), StatusCode::BAD_REQUEST).into_response(),
    };
    let path = match crate::settings::commit(&s.cfg, &edits) {
        Ok(path) => path,
        Err(e) => {
            return err("config", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response()
        }
    };
    Json(crate::settings::state(&s.cfg, &path)).into_response()
}

/// Состояние канала долгой памяти: тумблеры читаются перечитанными из конфига,
/// как модель. Правки — те же ключи `[memory]`, что у `snens memory …`.
pub(super) async fn get_memory(State(s): State<Arc<Runtime>>) -> impl IntoResponse {
    Json(memory_view(&s.cfg.memory_now())).into_response()
}

/// Вид состояния канала памяти для обоих хендлеров.
///
/// Одна функция, а не две копии: `PUT` раньше не отдавал `cmd`, и клиент,
/// апсертивший ответ, терял это поле.
fn memory_view(mem: &crate::cfg::Memory) -> serde_json::Value {
    serde_json::json!({
        "enabled": mem.enabled,
        "recall": match mem.recall {
            crate::cfg::RecallMode::Always => "always",
            crate::cfg::RecallMode::Session => "session",
            crate::cfg::RecallMode::Off => "off",
        },
        "retain": mem.retain,
        "cmd": mem.cmd,
    })
}

#[derive(Deserialize)]
pub(super) struct MemoryBody {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    recall: Option<String>,
    #[serde(default)]
    retain: Option<bool>,
}

/// Сменить тумблеры памяти и сразу сделать их текущими для новых ходов
/// (перечитка в память-модуле). Активный ход не трогаем.
pub(super) async fn put_memory(
    State(s): State<Arc<Runtime>>,
    Json(body): Json<MemoryBody>,
) -> impl IntoResponse {
    if body.enabled.is_none() && body.recall.is_none() && body.retain.is_none() {
        return err("invalid", "empty memory patch", StatusCode::BAD_REQUEST).into_response();
    }
    let mut changes: Vec<(&str, String)> = Vec::new();
    if let Some(v) = body.enabled {
        changes.push(("enabled", v.to_string()));
    }
    if let Some(v) = &body.recall {
        if !matches!(v.as_str(), "always" | "session" | "off") {
            return err(
                "invalid",
                "recall must be always|session|off",
                StatusCode::BAD_REQUEST,
            )
            .into_response();
        }
        changes.push(("recall", format!("\"{v}\"")));
    }
    if let Some(v) = body.retain {
        changes.push(("retain", v.to_string()));
    }
    // Одна запись файла на всю пачку: `set_memory_key` пишет файл на каждый
    // ключ, и ошибка на втором оставляла первый уже применённым, а хендлер
    // отвечал 500 — секция `[memory]` оставалась полуприменённой.
    let batch: Vec<(Option<String>, String, String)> = changes
        .into_iter()
        .map(|(key, value)| (Some("memory".to_string()), key.to_string(), value))
        .collect();
    // Запись файла — блокирующая: не на async-воркере.
    let cfg = s.cfg.clone();
    let written = tokio::task::spawn_blocking(move || cfg.set_keys(&batch)).await;
    match written {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            return err("config", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
        Err(e) => {
            return err("config", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
    }
    // Ответ — новое состояние канала в том же виде, что `GET /v1/memory`.
    Json(memory_view(&s.cfg.memory_now())).into_response()
}

#[cfg(test)]
#[path = "surface_providers_tests.rs"]
mod provider_tests;
