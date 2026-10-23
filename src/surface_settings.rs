// Настройки инстанса: модель, конфиг, память и профили провайдеров.
//
// Общее у них одно и то же правило: меню показывает то, что лежит в файле, а не
// снимок старта процесса, поэтому каждый обработчик перечитывает конфиг
// (`Config::fresh`/`model_now`/`memory_now`) вместо `s.cfg`.

use crate::surface_runtime::Runtime;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
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
pub(super) async fn put_model(State(s): State<Arc<Runtime>>, Json(body): Json<ModelBody>) -> impl IntoResponse {
    let slug = body.model.trim();
    if slug.is_empty() {
        return err("invalid", "empty model slug", StatusCode::BAD_REQUEST).into_response();
    }
    // Пишем в тот же конфиг, что читает этот процесс (учёт `--config`).
    let mut cfg = s.cfg.clone();
    match cfg.set_model_now(slug) {
        Ok(path) => Json(serde_json::json!({"model": slug, "config": path.display().to_string()}))
            .into_response(),
        Err(e) => err("config", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response(),
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
    let activate = live.providers.is_empty()
        || body.get("activate").and_then(|v| v.as_bool()) == Some(true);
    if let Err(e) = crate::providers::write_profile(&s.cfg, &name, &draft, None, activate) {
        return err("invalid", &e.to_string(), StatusCode::BAD_REQUEST).into_response();
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
        return err("invalid", &e.to_string(), StatusCode::BAD_REQUEST).into_response();
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
        Err(e) => err("conflict", &e.to_string(), StatusCode::CONFLICT).into_response(),
    }
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
        Err(e) => err("invalid", &e.to_string(), StatusCode::BAD_REQUEST).into_response(),
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
        Err(e) => return err("config", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response(),
    };
    Json(crate::settings::state(&s.cfg, &path)).into_response()
}

/// Состояние канала долгой памяти: тумблеры читаются перечитанными из конфига,
/// как модель. Правки — те же ключи `[memory]`, что у `snens memory …`.
pub(super) async fn get_memory(State(s): State<Arc<Runtime>>) -> impl IntoResponse {
    let mem = s.cfg.memory_now();
    Json(serde_json::json!({
        "enabled": mem.enabled,
        "recall": match mem.recall {
            crate::cfg::RecallMode::Always => "always",
            crate::cfg::RecallMode::Session => "session",
            crate::cfg::RecallMode::Off => "off",
        },
        "retain": mem.retain,
        "cmd": mem.cmd,
    }))
    .into_response()
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
pub(super) async fn put_memory(State(s): State<Arc<Runtime>>, Json(body): Json<MemoryBody>) -> impl IntoResponse {
    if body.enabled.is_none() && body.recall.is_none() && body.retain.is_none() {
        return err("invalid", "empty memory patch", StatusCode::BAD_REQUEST).into_response();
    }
    let mut changes: Vec<(&str, String)> = Vec::new();
    if let Some(v) = body.enabled {
        changes.push(("enabled", v.to_string()));
    }
    if let Some(v) = &body.recall {
        if !matches!(v.as_str(), "always" | "session" | "off") {
            return err("invalid", "recall must be always|session|off", StatusCode::BAD_REQUEST)
                .into_response();
        }
        changes.push(("recall", format!("\"{v}\"")));
    }
    if let Some(v) = body.retain {
        changes.push(("retain", v.to_string()));
    }
    for (key, value) in changes {
        if let Err(e) = s.cfg.set_memory_key(key, &value) {
            return err("config", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
    }
    // Ответ — новое состояние канала.
    let mem = s.cfg.memory_now();
    Json(serde_json::json!({
        "enabled": mem.enabled,
        "recall": match mem.recall {
            crate::cfg::RecallMode::Always => "always",
            crate::cfg::RecallMode::Session => "session",
            crate::cfg::RecallMode::Off => "off",
        },
        "retain": mem.retain,
    }))
    .into_response()
}

#[cfg(test)]
#[path = "surface_providers_tests.rs"]
mod provider_tests;
