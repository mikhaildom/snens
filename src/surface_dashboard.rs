// Дашборд инстанса: тикеты и ревью-сессии как данные, а не как текст в терминале.
//
// Оба маршрута только читают: `tickets/` — это файлы, `*-review.jsonl` — журналы
// рядом с сессиями. Ничего не создаём, не правим и не открываем на запись:
// единственный писатель журнала держит `.lock`, и чтение его не касается.
//
// Дорогая часть у обоих одна и та же — обход каталога и разбор файлов, поэтому
// она уезжает в `spawn_blocking`: в async-воркере чтение сотни журналов
// останавливало половину рантайма (как в `surface_query`).

use crate::log::Rec;
use crate::surface_runtime::{self as rt, Runtime};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Serialize;
use std::path::Path as FsPath;
use std::sync::Arc;

use super::http::err;

/// `GET /v1/tickets` — тикеты инстанса, свежие сверху.
///
/// Разбор front-matter живёт в [`crate::tickets`] (там же его тесты): здесь
/// только выборка и раскладка в JSON. Открытые и закрытые — вместе: дашборд
/// фильтрует на клиенте, а `status` — поле строки.
pub(super) async fn tickets(State(s): State<Arc<Runtime>>) -> impl IntoResponse {
    let dir = s.cfg.tickets_path();
    match tokio::task::spawn_blocking(move || crate::tickets::summaries(&dir)).await {
        Ok(rows) => Json(serde_json::json!({ "tickets": rows })).into_response(),
        Err(e) => err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response(),
    }
}

/// `GET /v1/tickets/{name}` — тело тикета целиком.
///
/// `name` — имя файла без `.md`. Формат проверяется в [`crate::tickets::find`]:
/// имя приходит из URL, и без проверки `../` читал бы файлы вне каталога.
pub(super) async fn ticket(
    State(s): State<Arc<Runtime>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if name.is_empty() || !crate::tickets::valid_name(&name) {
        return err("invalid", "invalid ticket name", StatusCode::BAD_REQUEST).into_response();
    }
    let dir = s.cfg.tickets_path();
    // `name` больше не нужен: передаём владением, без клона.
    let found = tokio::task::spawn_blocking(move || crate::tickets::find(&dir, &name)).await;
    match found {
        Ok(Some(t)) => Json(serde_json::json!({ "name": t.name, "body": t.body })).into_response(),
        Ok(None) => err("not_found", "ticket not found", StatusCode::NOT_FOUND).into_response(),
        Err(e) => err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response(),
    }
}

/// Одна ревью-сессия в том виде, в каком её рисует дашборд.
#[derive(Clone, Debug, Serialize)]
pub struct ReviewRow {
    /// id ревью-сессии: `<session>-review`.
    pub id: String,
    /// Сессия-источник — из ссылки `Rec::Replay`.
    pub session: String,
    /// Когда ревью запустили (по инструкции в его журнале), unix-сек.
    pub ts: u64,
    /// Исход: `done`-запись журнала (`ok`/`error`), иначе `running`.
    pub status: String,
    /// Первые ~300 символов финального текста ассистента.
    pub summary: String,
}

/// `GET /v1/reviews` — ревью-сессии инстанса, свежие сверху.
pub(super) async fn reviews(State(s): State<Arc<Runtime>>) -> impl IntoResponse {
    let dir = s.cfg.sessions_path();
    match tokio::task::spawn_blocking(move || rows(&dir)).await {
        Ok(Ok(rows)) => Json(serde_json::json!({ "reviews": rows })).into_response(),
        Ok(Err(e)) => {
            err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response()
        }
        Err(e) => err("storage", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR).into_response(),
    }
}

/// Все ревью-сессии каталога, отсортированные по времени запуска (свежие сверху).
///
/// Ревью — не отдельный вид хранилища, а журнал `<id>-review.jsonl` рядом с
/// сессиями (`review::run`). Перечисляем по имени файла, а не по содержимому:
/// разбирать 400 МБ журналов ради списка незачем.
pub fn rows(dir: &FsPath) -> crate::R<Vec<ReviewRow>> {
    // Ошибку `read_dir` возвращаем, а не выдаём за «ревью нет»: `Result::into_iter()`
    // давал пустой итератор, и сбой хранилища был неотличим от пустого списка.
    let entries = std::fs::read_dir(dir)?;
    let mut out: Vec<ReviewRow> = entries
        // Отдельная битая запись каталога не должна ронять весь список.
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                return None;
            }
            let id = path.file_stem().and_then(|s| s.to_str())?;
            let session = id.strip_suffix("-review")?;
            // `valid_id` отсекает мусор в каталоге: у ревью id = `<session>-review`,
            // и источник обязан быть настоящей сессией.
            if !rt::valid_id(session) {
                return None;
            }
            Some(summarize(&path, id, session))
        })
        .collect();
    // `ts` у самых старых журналов может быть нулём (меты нет): тогда наверх их
    // вытягивает mtime, а не пустая метка.
    out.sort_by(|a, b| b.ts.cmp(&a.ts).then(a.id.cmp(&b.id)));
    Ok(out)
}

/// Разбор одного журнала ревью.
///
/// Статус берётся только из факта: `Rec::Done` пишет харнесс в конце сессии на
/// любом пути выхода (`ok`/`error`). Отдельного «вердикта» ревью в журнале нет —
/// итог ревью это текст, он и уезжает в `summary`. Нет `Done` — ревью ещё идёт
/// (или процесс убили), и честный ответ — `running`.
fn summarize(path: &FsPath, id: &str, session: &str) -> ReviewRow {
    let recs = crate::log::read_file(path).unwrap_or_default();
    // Ссылка на префикс лежит второй строкой: в раскрытии она не нужна, а
    // читать источник (мегабайты чужого лога) — тем более.
    let replay_session = crate::log::replay_link(&recs)
        .map(|(s, _)| s.to_string())
        .unwrap_or_else(|| session.to_string());
    // Время запуска — запись с инструкцией ревью (`source: "review"`), а не
    // `Meta.ts`: мета пишется при открытии журнала и у перезапущенного ревью
    // остаётся прежней, а инструкция дописывается каждым прогоном.
    let ts = recs
        .iter()
        .find_map(|r| match r {
            Rec::User { ts, source, .. } if source == "review" => Some(*ts),
            _ => None,
        })
        // Первая запись с `ts: 0` (старые журналы без настоящей меты) раньше
        // глушила `mtime`: `Some(0)` не доходил до `unwrap_or_else`, и такие
        // строки уезжали в низ списка с нулевым временем.
        .or_else(|| recs.first().map(Rec::ts).filter(|ts| *ts > 0))
        .unwrap_or_else(|| mtime(path));
    let last_done = recs.iter().rev().find_map(|r| match r {
        Rec::Done { status, .. } => Some(status.clone()),
        _ => None,
    });
    // Финальный текст — последний непустой ответ ассистента: ревью заканчивается
    // отчётом в 3-6 строк, а хвост журнала после него — только `usage`/`done`.
    let final_text = recs.iter().rev().find_map(|r| match r {
        Rec::Assistant { text: Some(t), .. } if !t.trim().is_empty() => Some(t.trim()),
        _ => None,
    });
    ReviewRow {
        id: id.to_string(),
        session: replay_session,
        ts,
        status: last_done.unwrap_or_else(|| "running".to_string()),
        summary: clip(final_text.unwrap_or(""), 300),
    }
}

/// Время правки файла в unix-секундах; `0`, если метаданные не читаются.
fn mtime(path: &FsPath) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Обрезка по символам, а не по байтам: иначе кириллицу разрежет посреди буквы.
fn clip(s: &str, n: usize) -> String {
    // Один проход: берём n+1 символ и проверяем, остался ли лишний, вместо
    // повторного `chars().count()` по всей строке ради той же проверки.
    let mut t: String = s.chars().take(n).collect();
    if s.chars().nth(n).is_some() {
        t.push('…');
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("snens-dash-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn review_log(rid: &str, session: &str, done: Option<&str>) -> String {
        let mut out = format!(
            "{{\"kind\":\"meta\",\"ts\":100,\"model\":\"\"}}\n\
             {{\"kind\":\"replay\",\"session\":\"{session}\",\"lines\":19}}\n\
             {{\"kind\":\"user\",\"ts\":170,\"text\":\"# Ревью\",\"source\":\"review\",\"parts\":[]}}\n\
             {{\"kind\":\"assistant\",\"ts\":175,\"text\":\"промежуточный ответ\",\"tool_calls\":[]}}\n\
             {{\"kind\":\"assistant\",\"ts\":180,\"text\":\"Ревью готово.\\n\\nТикеты: один.\",\"tool_calls\":[]}}\n"
        );
        if let Some(status) = done {
            out.push_str(&format!(
                "{{\"kind\":\"done\",\"ts\":190,\"status\":\"{status}\",\"turns\":3}}\n"
            ));
        }
        // id не используется внутри тела, но важен вызывающему: файл зовётся по нему.
        debug_assert_eq!(rid, format!("{session}-review"));
        out
    }

    #[test]
    fn review_row_takes_session_from_replay_and_text_from_last_answer() {
        let d = tmp("one");
        let text = review_log("1790081331-79251-review", "1790081331-79251", Some("ok"));
        fs::write(d.join("1790081331-79251-review.jsonl"), text).unwrap();
        let rows = rows(&d).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let r = &rows[0];
        assert_eq!(r.id, "1790081331-79251-review");
        assert_eq!(r.session, "1790081331-79251", "источник — из Replay");
        assert_eq!(r.ts, 170, "время запуска — запись инструкции ревью");
        assert_eq!(r.status, "ok");
        assert!(r.summary.starts_with("Ревью готово."), "{}", r.summary);
        assert!(!r.summary.contains("промежуточный"), "{}", r.summary);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn review_without_done_is_running() {
        let d = tmp("running");
        let text = review_log("1790182549-79145-review", "1790182549-79145", None);
        fs::write(d.join("1790182549-79145-review.jsonl"), text).unwrap();
        assert_eq!(rows(&d).unwrap()[0].status, "running");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn reviews_are_newest_first_and_ignore_other_logs() {
        let d = tmp("order");
        fs::write(
            d.join("1790182549-79145-review.jsonl"),
            review_log("1790182549-79145-review", "1790182549-79145", Some("ok")),
        )
        .unwrap();
        let mut newer = review_log("1790198941-96971-review", "1790198941-96971", Some("ok"));
        newer = newer.replace("\"ts\":170", "\"ts\":270");
        fs::write(d.join("1790198941-96971-review.jsonl"), newer).unwrap();
        // Не ревью: обычная сессия и мусор рядом — в список не попадают.
        fs::write(
            d.join("1790198941-96971.jsonl"),
            "{\"kind\":\"meta\",\"ts\":1}\n",
        )
        .unwrap();
        fs::write(d.join("notes.txt"), "x").unwrap();
        let rows = rows(&d).unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].id, "1790198941-96971-review");
        assert!(rows[0].ts > rows[1].ts);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn empty_dir_gives_empty_review_list() {
        let d = tmp("empty");
        assert!(rows(&d).unwrap().is_empty());
        assert!(
            rows(FsPath::new("/нет/такого/каталога")).is_err(),
            "сбой чтения виден"
        );
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn summary_is_clipped_by_chars() {
        let long = "я".repeat(400);
        let cut = clip(&long, 300);
        assert_eq!(cut.chars().count(), 301);
        assert!(cut.ends_with('…'));
        assert_eq!(clip("коротко", 300), "коротко");
    }

    /// Журнал ревью, у которого ссылка на источник битая: `session` берётся из
    /// имени файла, а `ts` — по mtime файла (в логе времени нет вовсе).
    #[test]
    fn broken_log_still_gives_a_row() {
        let d = tmp("broken");
        fs::write(d.join("1790182549-79145-review.jsonl"), "не json\n").unwrap();
        let rows = rows(&d).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session, "1790182549-79145");
        assert!(rows[0].ts > 0, "mtime подставился: {rows:?}");
        assert_eq!(rows[0].status, "running");
        assert!(rows[0].summary.is_empty());
        fs::remove_dir_all(&d).ok();
    }
}
