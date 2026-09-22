// Промпт — чистая проекция лога. Бухгалтерия (Usage/Meta/Ctx) сюда не попадает,
// поэтому префикс стабилен и кеш не промахивается.
//
// Компакция — правило проекции, а не перезапись лога: лог неизменяем, элизия
// обратима, потому что исходные результаты по-прежнему лежат в файле сессии.
use serde_json::{json, Value};

use crate::{
    cfg::{self, Config},
    exec::truncate,
    log::{Body, Rec},
};

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

    for r in log {
        match r {
            Rec::User { text, source, .. } => {
                // Инструкция ревью — системная по смыслу: это не запрос пользователя,
                // а рамка задачи. Меняем роль в проекции; лог остаётся как есть.
                let role = if source == "review" { "system" } else { "user" };
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
