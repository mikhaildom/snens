// Чтение по журналам: поиск, расклад контекста. Общее у них — полный разбор
// журнала, поэтому оба хендлера выносят его в `spawn_blocking`: в async-воркере
// чтение мегабайтного лога останавливало половину рантайма на двух ядрах.

use crate::cfg::Config;
use crate::surface_runtime::{self as rt, Runtime};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use std::sync::Arc;

use super::discovery;
use super::history;
use super::http::err;

/// Расклад рабочего окна сессии: из чего состоит проекция промпта.
///
/// Считается ядром по тем же записям, что уедут провайдеру в следующем ходе:
/// клиент не догадывается о размерах, а показывает измеренное. `limit` —
/// бюджет окна: у сессии может быть свой, иначе из конфига.
pub(super) async fn context_breakdown(
    State(s): State<Arc<Runtime>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if !rt::valid_id(&id) {
        return err("invalid", "invalid session id", StatusCode::BAD_REQUEST).into_response();
    }
    let Some(path) = discovery::path(&s.cfg, &id).filter(|p| p.is_file()) else {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    };
    // Чтение и разбор журнала — блокирующая работа на весь файл: в async-хендлере
    // она держала воркер (панель контекста занимала 0.5–0.65 с на 4 МБ логе).
    let read_path = path.clone();
    let log = match tokio::task::spawn_blocking(move || crate::log::read_file(&read_path)).await {
        Ok(Ok(log)) => log,
        Ok(Err(e)) => {
            return err("history_read", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
        Err(e) => {
            return err("history_read", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    };
    let breakdown = crate::project::window_breakdown(&log, &s.cfg);
    // Схемы инструментов тоже уезжают в запрос и занимают окно: считаем их
    // оценкой по JSON и приводим тем же коэффициентом, что и остальное.
    let tools = crate::registry::discover(&s.cfg).await;
    let schema: u64 = crate::registry::definitions(&tools)
        .iter()
        .map(|t| crate::project::estimate_message(t) + 8)
        .sum();
    let breakdown = breakdown.with_tool_schema(schema, crate::project::calibration(&log));
    let budget = s
        .data
        .lock()
        .await
        .sessions
        .get(&id)
        .and_then(|x| x.context_target.or(x.context_budget))
        .unwrap_or_else(|| s.cfg.context_budget());
    Json(serde_json::json!({
        "breakdown": breakdown,
        "limit": budget,
    }))
    .into_response()
}

#[derive(Deserialize)]
pub(super) struct SearchQuery {
    /// Подстрока для поиска (без учёта регистра). Пусто — пустой результат.
    q: String,
    #[serde(default)]
    limit: Option<usize>,
}

/// Поиск по сессиям: реплики человека и ответы модели, плюс заголовки.
///
/// Ищем по журналам (JSONL) — отдельного индекса нет: 480 файлов на 130 МБ
/// читаются единицы секунд, а индекс потребовал бы инвалидации на каждую
/// запись хода. Сопоставление — по записям, поэтому в результате видно, что
/// именно совпало, а не просто «файл подошёл».
pub(super) async fn search(
    State(s): State<Arc<Runtime>>,
    Query(q): Query<SearchQuery>,
) -> impl IntoResponse {
    let needle = q.q.trim().to_lowercase();
    if needle.is_empty() {
        return Json(serde_json::json!({"hits": []})).into_response();
    }
    let limit = q.limit.unwrap_or(50).min(200);
    let dir = s.cfg.sessions_path();
    let cfg = s.cfg.clone();
    let found = tokio::task::spawn_blocking(move || search_sessions(&dir, &cfg, &needle, limit))
        .await;
    match found {
        Ok(Ok(hits)) => Json(serde_json::json!({"hits": hits})).into_response(),
        Ok(Err(e)) => err("storage", &e, StatusCode::INTERNAL_SERVER_ERROR).into_response(),
        Err(e) => err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response(),
    }
}

/// Синхронный поиск (зовётся из `spawn_blocking`).
///
/// Сначала дешёвая проверка по байтам: если подстроки в файле нет вовсе,
/// разбирать JSON незачем. На инстансе с 400 МБ журналов это превращает
/// промах из 1.6 с в единицы миллисекунд.
pub(super) fn search_sessions(
    dir: &std::path::Path,
    cfg: &Config,
    needle: &str,
    limit: usize,
) -> Result<Vec<serde_json::Value>, String> {
    let entries = std::fs::read_dir(dir).map_err(|_| "sessions directory is missing".to_string())?;
    let mut files: Vec<(u64, std::path::PathBuf, String)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
            continue;
        };
        if !rt::valid_id(&id) {
            continue;
        }
        let updated = discovery::mtime(&path).unwrap_or(0);
        files.push((updated, path, id));
    }
    // Свежие сверху: чаще ищут недавнее, а лимит должен резать старое.
    files.sort_by_key(|(updated, _, _)| std::cmp::Reverse(*updated));
    let mut hits = Vec::new();
    for (updated, path, id) in files {
        if hits.len() >= limit {
            break;
        }
        if !file_contains(&path, needle) {
            continue;
        }
        let Ok(rows) = history::read_with(&path, cfg, false) else {
            continue;
        };
        let mut matched = Vec::new();
        for row in &rows {
            if row.role != "user" && row.role != "assistant" {
                continue;
            }
            let Some(text) = row.content.as_deref() else {
                continue;
            };
            let lower = text.to_lowercase();
            let Some(pos) = lower.find(needle) else {
                continue;
            };
            // Окно вокруг совпадения: символы, а не байты — иначе кириллицу
            // разрежет посреди буквы.
            let chars: Vec<char> = text.chars().collect();
            let byte_pos = lower[..pos].chars().count();
            let start = byte_pos.saturating_sub(40);
            let end = (byte_pos + needle.chars().count() + 60).min(chars.len());
            let snippet: String = chars[start..end].iter().collect();
            matched.push(serde_json::json!({
                "seq": row.seq,
                "role": row.role,
                "ts": row.ts,
                "snippet": snippet,
            }));
            if matched.len() >= 3 {
                break;
            }
        }
        let title = discovery::title(&path);
        let title_hit = title
            .as_deref()
            .is_some_and(|t| t.to_lowercase().contains(needle));
        if matched.is_empty() && !title_hit {
            continue;
        }
        hits.push(serde_json::json!({
            "id": id,
            "title": title,
            "surface": discovery::surface(&path),
            "updated_at": updated,
            "matches": matched,
        }));
    }
    Ok(hits)
}

/// Есть ли подстрока в файле (без учёта регистра, потоками по 256 КБ).
/// Регистр складываем в нижний на лету: ASCII-поиск дешевле полного разбора.
pub(super) fn file_contains(path: &std::path::Path, needle: &str) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    // Кириллица в UTF-8 при смене регистра меняет длину байтов, поэтому для
    // не-ASCII ищем как есть и в нижнем регистре по декодированному куску.
    let ascii = needle.is_ascii();
    let mut buf = vec![0u8; 256 * 1024];
    let mut carry = String::new();
    loop {
        let Ok(n) = file.read(&mut buf) else {
            return false;
        };
        if n == 0 {
            return false;
        }
        let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
        let combined = format!("{carry}{chunk}");
        let found = if ascii {
            combined.to_ascii_lowercase().contains(needle)
        } else {
            combined.to_lowercase().contains(needle)
        };
        if found {
            return true;
        }
        // Хвост может разрезать многобайтный символ: переносим последние байты.
        let keep = combined.len().saturating_sub(8);
        carry = combined[keep..].to_string();
    }
}
