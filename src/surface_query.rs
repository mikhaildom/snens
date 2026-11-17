// Чтение по журналам: поиск, расклад контекста. Общее у них — полный разбор
// журнала, поэтому оба хендлера выносят его в `spawn_blocking`: в async-воркере
// чтение мегабайтного лога останавливало половину рантайма на двух ядрах.

use crate::cfg::Config;
use crate::surface_runtime::{self as rt, Runtime};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
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
    let (breakdown, scale) = match tokio::task::spawn_blocking(move || {
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
            return err(
                "history_read",
                &e.to_string(),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
            .into_response()
        }
        Err(e) => {
            return err(
                "history_read",
                &e.to_string(),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
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
    let found =
        tokio::task::spawn_blocking(move || search_sessions(&dir, &cfg, &needle, limit)).await;
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
    let entries =
        std::fs::read_dir(dir).map_err(|_| "sessions directory is missing".to_string())?;
    let mut files: Vec<(u64, std::path::PathBuf, String)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(id) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
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
            // Ошибка чтения отдельного файла не должна выглядеть как «нет
            // совпадений»: без строки в stderr проблема прав/диска невидима.
            eprintln!("[snens] search: журнал {} не прочитан", path.display());
            continue;
        };
        // Совпадение в тексте ищем в том же поле `lower`, в котором его нашёл
        // `file_contains`. Исходный текст с ним не совпадает по смещению: ищем
        // подстроку напрямую в исходнике (без учёта регистра — через `lower`,
        // когда длины совпадают).
        let mut matched = Vec::new();
        for row in &rows {
            if row.role != "user" && row.role != "assistant" {
                continue;
            }
            let Some(text) = row.content.as_deref() else {
                continue;
            };
            // Окно вокруг совпадения: символы, а не байты — иначе кириллицу
            // разрежет посреди буквы. Смещение ищем в исходном тексте: `pos` из
            // `lower` не переводится в `text` (у 'İ' lowercase длиннее),
            // а срез за длину `chars` паниковал прямо с `/search`.
            let Some(snippet) = snippet_window(text, needle) else {
                continue;
            };
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

/// Сниппет вокруг первого совпадения (до 40 символов до и 60 после).
///
/// Совпадение ищется без учёта регистра, но `to_lowercase` иногда меняет число
/// символов ('İ' → два), поэтому перевести смещение из `lower` в `text` нечем.
/// Ищем в обоих вариантах и режем тот, в котором смещение настоящее:
///
/// 1. исходный текст содержит `needle` как есть (регистр совпал);
/// 2. иначе — `lower`, и сниппет строится тоже по `lower`.
///
/// Границы всегда зажаты в длину выбранного текста, так что срез не паникует ни
/// на кириллице, ни на символах, разворачивающихся при смене регистра.
fn snippet_window(text: &str, needle: &str) -> Option<String> {
    const BEFORE: usize = 40;
    const AFTER: usize = 60;
    let lower = text.to_lowercase();
    let (hay, pos) = match text.find(needle) {
        Some(pos) => (text, pos),
        None => {
            let pos = lower.find(needle)?;
            (lower.as_str(), pos)
        }
    };
    // Байтовое смещение → счётчик символов в том же тексте (по определению
    // границы символа это безопасно: `pos` пришёл из `find`).
    let at = hay[..pos].chars().count();
    let len = hay.chars().count();
    let start = at.saturating_sub(BEFORE);
    let end = (at + needle.chars().count() + AFTER).min(len).max(start);
    Some(hay.chars().skip(start).take(end - start).collect())
}
/// Есть ли подстрока в файле (без учёта регистра, кусками по 256 КБ).
///
/// Два ограничения честно заложены в форме цикла:
///
/// 1. Регистр складываем на уже собранном тексте, а не на байтах куска: для
///    кириллицы `to_lowercase` меняет длину (и иногда число символов).
/// 2. Граница куска не должна разрезать совпадение — поэтому в `carry` держим
///    `needle.len() - 1` байт, а не фиксированные 8: у длинного `needle` голова
///    совпадения иначе оставалась в предыдущем куске и терялась.
///
/// Остаточный край: если подстрока сама короче двух байт или `to_lowercase`
/// удлиняет символ на границе, окно в `needle.len() - 1` байт теоретически
/// может не вместить развёрнутый символ. Для поиска по журналам (запросы
/// человека, обычно десятки байт) это не встречается; полное решение —
/// скользящее окно в символах, цена которого выше пользы.
pub(super) fn file_contains(path: &std::path::Path, needle: &str) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let ascii = needle.is_ascii();
    // Нижний регистр `needle` — один раз на файл, а не на кусок.
    let needle_lower = if ascii {
        needle.to_ascii_lowercase()
    } else {
        needle.to_lowercase()
    };
    // Держим столько байт хвоста, сколько может понадобиться совпадению на стыке.
    let keep_bytes = needle.len().saturating_sub(1).max(1);
    let mut carry: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let Ok(n) = file.read(&mut buf) else {
            return false;
        };
        if n == 0 {
            return false;
        }
        let mut combined = std::mem::take(&mut carry);
        combined.extend_from_slice(&buf[..n]);
        // `from_utf8_lossy` здесь безопасен для поиска в ASCII (байты как есть)
        // и теряет только разрезанный на стыке символ, который остаётся в хвосте
        // сырых байт ниже.
        let text = String::from_utf8_lossy(&combined);
        let found = if ascii {
            text.to_ascii_lowercase().contains(&needle_lower)
        } else {
            text.to_lowercase().contains(&needle_lower)
        };
        if found {
            return true;
        }
        // Хвост переносим СЫРЫМИ байтами: `combined` уже прошёл через lossy, и
        // копия из него тащила бы в следующий кусок U+FFFD вместо разрезанного
        // символа.
        let keep = combined.len().saturating_sub(keep_bytes);
        carry = combined[keep..].to_vec();
    }
}

#[cfg(test)]
mod tests {
    use super::{file_contains, snippet_window, window_limit};
    use crate::{cfg::Config, surface_runtime::Session};

    #[test]
    fn snippet_window_never_slices_past_the_text() {
        // Кириллица: смещение совпадения в середине текста.
        let text = "предисловие ".repeat(20) + "искомое" + &" хвост".repeat(20);
        let snippet = snippet_window(&text, "искомое").unwrap();
        assert!(snippet.contains("искомое"), "{snippet}");
    }

    #[test]
    fn snippet_window_survives_case_expanding_characters() {
        // 'İ' в нижнем регистре — два символа: смещение из `lower` в `text` не
        // переводится, и старый код резал `chars` за длиной (паника с /search).
        let text = format!("{}ИСКОМОЕ{}", "İ".repeat(200), "x".repeat(200));
        let snippet = snippet_window(&text, "искомое").unwrap();
        assert!(snippet.contains("искомое"), "{snippet}");
    }

    #[test]
    fn file_contains_keeps_a_carry_as_long_as_the_needle() {
        let dir = std::env::temp_dir().join(format!("surface-query-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("j.jsonl");
        // Строка ставит совпадение ровно на стык 256-килобайтных кусков: старая
        // версия держала 8 байт хвоста и теряла голову длинного `needle`.
        let needle = "пограничное совпадение";
        let mut text = "а".repeat(256 * 1024 - 10);
        text.push_str(needle);
        text.push_str(&"б".repeat(100));
        std::fs::write(&path, &text).unwrap();
        assert!(file_contains(&path, needle));
        assert!(!file_contains(&path, "такого в журнале нет"));
        std::fs::remove_dir_all(dir).unwrap();
    }

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
