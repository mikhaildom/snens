// Промпт — чистая проекция лога. Бухгалтерия (Usage/Meta) сюда не попадает,
// поэтому префикс стабилен и кеш не промахивается.
use serde_json::{json, Value};

use crate::{
    cfg::{self, Config},
    exec::truncate,
    log::{Body, Rec},
};

pub fn msgs(log: &[Rec], cfg: &Config) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": cfg::system_prompt(cfg)})];
    for r in log {
        match r {
            Rec::User { text, .. } => out.push(json!({"role": "user", "content": text})),
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
            Rec::ToolResult { call_id, body, .. } => {
                // Кап на результат живёт здесь, а не в инструменте: это единственная
                // точка, через которую проходит любой вывод любого тула.
                let text = match body {
                    Body::Inline { text } => truncate(text, cfg.max_result_chars),
                    Body::Elided { bytes } => format!("[результат вырезан компакцией, {bytes} Б]"),
                };
                out.push(json!({"role": "tool", "tool_call_id": call_id, "content": text}));
            }
            Rec::Meta { .. } | Rec::Usage { .. } => {}
        }
    }
    out
}

/// Грубая оценка токенов проекции: точные числа берём из usage провайдера,
/// а это — только для порога компакции (M3).
pub fn estimate_tokens(msgs: &[Value]) -> u64 {
    let bytes: usize = msgs
        .iter()
        .map(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0))
        .sum();
    // ponytail: 1 токен ≈ 4 символа для латиницы; для кириллицы это занижает,
    // поэтому считаем по 3 символам — лучше сработать чуть раньше.
    (bytes / 3) as u64
}