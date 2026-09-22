// Промпт — чистая проекция лога. Бухгалтерия (Usage/Meta/Ctx) сюда не попадает,
// поэтому префикс стабилен и кеш не промахивается.
//
// Компакция — правило проекции, а не перезапись лога: лог неизменяем, элизия
// обратима, потому что исходные результаты по-прежнему лежат в файле сессии.
use std::collections::HashSet;

use serde_json::{json, Value};

use crate::{
    cfg::{self, Config},
    exec::truncate,
    log::{Body, Rec},
};

/// Пометка перед рамкой ревью/улучшения: выше лежит транскрипт уже закрытой
/// сессии. Без неё модель принимает чужой запрос за свой живой ход и выполняет
/// его вместо ревью. Роль system, потому что это тоже рамка, а не запрос.
const REPLAY_NOTE: &str = "Всё выше — транскрипт уже закрытой сессии: запросы в нём выполнены, \
вызовы инструментов не твои, повторять их нельзя. Не продолжай ту работу: дальше — новая \
инструкция, и выполнять нужно только её.";

#[derive(Default, Clone, Copy, Debug)]
pub struct Report {
    pub elided: usize,
    pub freed_bytes: usize,
}

/// Проекция с учётом переполнения: если она не влезает в мягкий порог,
/// старые результаты тулов схлопываются до пометки. Решение детерминировано —
/// зависит только от лога и конфига, поэтому кеш не промахивается сам по себе.
pub fn project(log: &[Rec], cfg: &Config) -> (Vec<Value>, Report) {
    let (full, _) = build(log, cfg, false);
    if estimate_tokens(&full) <= soft_limit(cfg) {
        return (full, Report::default());
    }
    build(log, cfg, true)
}

pub fn msgs(log: &[Rec], cfg: &Config) -> Vec<Value> {
    project(log, cfg).0
}

pub fn soft_limit(cfg: &Config) -> u64 {
    (cfg.ctx_window as f64 * cfg.soft) as u64
}

fn build(log: &[Rec], cfg: &Config, elide: bool) -> (Vec<Value>, Report) {
    let mut out = vec![json!({"role": "system", "content": cfg::system_prompt(cfg)})];
    let mut report = Report::default();
    // Последние keep_results остаются как есть при любом размере: модель работает
    // именно с ними. Всё, что раньше, — уже переваренный материал.
    let total_results = log
        .iter()
        .filter(|r| matches!(r, Rec::ToolResult { .. }))
        .count();
    let keep_from = total_results.saturating_sub(cfg.keep_results);
    let mut seen_results = 0usize;
    // Есть ли выше рамки настоящий ход (запрос пользователя и/или вызовы): тогда
    // рамка — это ревью/улучшение поверх транскрипта, и транскрипт надо пометить.
    let mut had_work = false;
    let mut replay_marked = false;
    // Какие вызовы получили результат. Нужно, чтобы оборванный ход не уехал
    // провайдеру как assistant с tool_calls без ответов.
    let answered: HashSet<&str> = log
        .iter()
        .filter_map(|r| match r {
            Rec::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();

    for r in log {
        match r {
            Rec::User { text, source, .. } => {
                // Инструкции ревью и улучшения — системные по смыслу: это не запрос
                // пользователя, а рамка задачи. Роль в проекции, лог остаётся как есть.
                let frame = source == "review" || source == "improve";
                // Перед рамкой весь лог — транскрипт уже закрытой сессии. Без явной
                // пометки модель принимает запрос из транскрипта за свой живой ход и
                // выполняет его вместо ревью. Пометка ставится только если транскрипт
                // есть: у свежего улучшения до рамки ничего, кроме Meta.
                if frame && !replay_marked && had_work {
                    replay_marked = true;
                    out.push(json!({"role": "system", "content": REPLAY_NOTE}));
                }
                if !frame {
                    had_work = true;
                }
                let role = if frame { "system" } else { "user" };
                out.push(json!({"role": role, "content": text}));
            }
            Rec::Assistant { text, tool_calls, .. } => {
                let content = match text {
                    Some(t) if !t.is_empty() => json!(t),
                    _ => Value::Null,
                };
                let mut m = json!({"role": "assistant", "content": content});
                if !tool_calls.is_empty() {
                    let calls: Vec<Value> = tool_calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "type": "function",
                                "function": {"name": c.name, "arguments": c.args}
                            })
                        })
                        .collect();
                    m["tool_calls"] = json!(calls);
                }
                out.push(m);
                // Ход, оборванный на середине (Ctrl+C, падение, убитый процесс),
                // оставляет assistant с tool_calls без ответов, и провайдер такой
                // запрос отвергает. Ремонт живёт здесь, а не в обработчике Ctrl+C:
                // лог обязан остаться правдой, а проекция — быть валидной при любом
                // хвосте, откуда бы обрыв ни взялся.
                for c in tool_calls.iter().filter(|c| !answered.contains(c.id.as_str())) {
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": c.id,
                        "content": "[вызов не завершён: ход прерван]"
                    }));
                }
            }
            Rec::ToolResult {
                call_id,
                tool,
                argv,
                exit,
                bytes,
                body,
                ..
            } => {
                let idx = seen_results;
                seen_results += 1;
                // Кап на результат живёт здесь, а не в инструменте: это единственная
                // точка, через которую проходит любой вывод любого тула.
                let text = match body {
                    Body::Inline { text } => truncate(text, cfg.max_result_chars),
                };
                let text = if elide && idx < keep_from {
                    report.elided += 1;
                    report.freed_bytes += text.len();
                    format!(
                        "[результат вырезан при компакции: {tool}, {bytes} Б, exit {exit} — {}]",
                        command_hint(argv)
                    )
                } else {
                    text
                };
                out.push(json!({"role": "tool", "tool_call_id": call_id, "content": text}));
            }
            Rec::Meta { .. } | Rec::Usage { .. } | Rec::Ctx { .. } => {}
        }
    }
    (out, report)
}

/// Команду из лога оставляем в пометке: без неё модель не знает, что именно
/// было вырезано, и не может решить, перезапускать или нет.
fn command_hint(argv: &[String]) -> String {
    let raw = argv.join(" ").replace('\n', " ");
    let mut s: String = raw.chars().take(160).collect();
    if raw.chars().count() > 160 {
        s.push('…');
    }
    s
}

/// Грубая оценка токенов проекции: точные числа берём из usage провайдера,
/// а это — только для порога компакции.

pub fn estimate_tokens(msgs: &[Value]) -> u64 {
    let bytes: usize = msgs
        .iter()
        .map(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0))
        .sum();
    // ponytail: 1 токен ≈ 4 символа для латиницы; для кириллицы это занижает,
    // поэтому считаем по 3 символам — лучше сработать чуть раньше.
    (bytes / 3) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Rec;

    fn user(text: &str, source: &str) -> Rec {
        Rec::User {
            ts: 0,
            text: text.into(),
            source: source.into(),
        }
    }

    #[test]
    fn replay_note_precedes_review_frame_and_keeps_prefix() {
        let cfg = Config::default();
        let log = vec![
            user("добавь строку и собери", "cli"),
            Rec::Assistant {
                ts: 0,
                text: Some("готово".into()),
                tool_calls: vec![],
            },
            user("# Ревью сессии\nпервое действие — тикет", "review"),
        ];
        let msgs = msgs(&log, &cfg);
        let note = msgs
            .iter()
            .position(|m| m["content"].as_str().unwrap_or_default().contains("транскрипт уже закрытой сессии"))
            .expect("нет пометки о реплее");
        assert_eq!(msgs[note]["role"], "system", "пометка должна быть системной");
        let frame = msgs
            .iter()
            .position(|m| m["content"].as_str().unwrap_or_default().contains("# Ревью сессии"))
            .unwrap();
        assert!(note < frame, "пометка должна стоять перед рамкой ревью");
        // Префикс до пометки — это транскрипт как есть: кеш провайдера не промахнётся.
        assert_eq!(msgs[1]["content"], "добавь строку и собери");
    }

    #[test]
    fn fresh_frame_without_transcript_gets_no_note() {
        let cfg = Config::default();
        let log = vec![user("разбери тикеты", "improve")];
        let msgs = msgs(&log, &cfg);
        assert!(
            !msgs.iter().any(|m| m["content"].as_str().unwrap_or_default().contains("транскрипт")),
            "пометка не нужна, когда транскрипта нет"
        );
        assert_eq!(msgs[1]["role"], "system", "рамка улучшения — системная");
    }

    /// Оборванный ход (Ctrl+C, падение) не должен уезжать провайдеру как
    /// assistant с tool_calls без ответов — он такой запрос отвергает.
    #[test]
    fn interrupted_turn_is_closed_in_projection() {
        let cfg = Config::default();
        let call = |id: &str| crate::log::Call {
            id: id.into(),
            name: "read".into(),
            args: "{}".into(),
        };
        let log = vec![
            user("почини", "tui"),
            Rec::Assistant {
                ts: 0,
                text: None,
                tool_calls: vec![call("c1"), call("c2")],
            },
            Rec::ToolResult {
                ts: 0,
                call_id: "c1".into(),
                tool: "read".into(),
                argv: vec![],
                exit: 0,
                bytes: 1,
                ms: 1,
                body: crate::log::Body::Inline { text: "ok".into() },
            },
        ];
        let first = msgs(&log, &cfg);
        let second = msgs(&log, &cfg);
        let answered: Vec<&str> = first
            .iter()
            .filter(|m| m["role"] == "tool")
            .map(|m| m["tool_call_id"].as_str().unwrap_or_default())
            .collect();
        assert!(answered.contains(&"c1"), "настоящий результат потерян");
        assert!(answered.contains(&"c2"), "оборванный вызов остался без ответа");
        // Ремонт детерминирован — иначе промахнётся префикс-кеш.
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
    }
}
