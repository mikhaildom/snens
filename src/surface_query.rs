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
    // Чтение, разбор журнала и расклад — блокирующая работа на весь файл: в
    // async-хендлере она держала воркер (панель контекста занимала 0.5–0.65 с
    // на 4 МБ логе). Расклад считается по проекции (`project::msgs`), а это
    // ещё и сборка базы ревизии контекста — тем более не в async-воркере.
    let read_path = path.clone();
    let read_cfg = s.cfg.clone();
    let (breakdown, scale) =
        match tokio::task::spawn_blocking(move || {
            let log = crate::log::read_file(&read_path)?;
            let scale = crate::project::calibration(&log);
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
                crate::project::window_breakdown(&log, &read_cfg),
                scale,
            ))
        })
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                return err("history_read", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
                    .into_response()
            }
            Err(e) => {
                return err("history_read", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
                    .into_response()
            }
        };
    // Схемы инструментов тоже уезжают в запрос и занимают окно: считаем их
    // оценкой по JSON и приводим тем же коэффициентом, что и остальное.
    let tools = crate::registry::discover(&s.cfg).await;
    let schema: u64 = crate::registry::definitions(&tools)
        .iter()
        .map(|t| crate::project::estimate_message(t) + 8)
        .sum();
    let breakdown = breakdown.with_tool_schema(schema, scale);
    let budget = {
        let d = s.data.lock().await;
        window_limit(&s.cfg, d.sessions.get(&id))
    };
    Json(serde_json::json!({
        "breakdown": breakdown,
        "limit": budget,
    }))
    .into_response()
}

/// Знаменатель шкалы окна: бюджет уборки, а не цель.
///
/// `context_target` — ориентир, до которого контекст опускают уборкой. Как
/// знаменатель он врёт дважды: сразу после уборки `total ≈ target`, и шкала
/// показывает «свободно 0», хотя до потолка ещё треть бюджета; а без уборки
/// (бюджет задан, цель — его 70%) он рисует переполнение там, где порог ещё
/// не перейдён. Поэтому берём тот же порог, по которому ядро решает «пора
/// убирать» (`agent` сравнивает с `context_budget`), и тот же, что видит
/// поверхность при запуске (`review::session_cleanup_limits`).
///
/// Своё значение сессии перебивает конфиг (групповой чат живёт на своём
/// бюджете); ноль — «не задано», а не «окно нулевого размера».
pub(super) fn window_limit(cfg: &Config, session: Option<&rt::Session>) -> u64 {
    session
        .and_then(|x| x.context_budget)
        .filter(|b| *b > 0)
        .unwrap_or_else(|| cfg.context_budget())
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

#[cfg(test)]
mod tests {
    use super::window_limit;
    use crate::{cfg::Config, surface_runtime::Session};

    #[test]
    fn window_limit_is_the_cleanup_budget_not_the_target() {
        let cfg = Config {
            context_budget: 500_000,
            context_target: 350_000,
            ..Default::default()
        };
        // Сессии нет — общий бюджет конфига; цель на шкалу не попадает.
        assert_eq!(window_limit(&cfg, None), 500_000);
        // Сессия без своего бюджета живёт на общем.
        assert_eq!(window_limit(&cfg, Some(&Session::new("s", "web"))), 500_000);
        // Свой бюджет перебивает конфиг.
        let mut own = Session::new("s", "web");
        own.context_budget = Some(100_000);
        own.context_target = Some(70_000);
        assert_eq!(window_limit(&cfg, Some(&own)), 100_000);
        // Ноль — «не задано».
        let mut zero = Session::new("s", "web");
        zero.context_budget = Some(0);
        assert_eq!(window_limit(&cfg, Some(&zero)), 500_000);
    }
}
