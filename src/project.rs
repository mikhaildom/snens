// Промпт — чистая проекция лога. Бухгалтерия (Usage/Meta/Ctx) сюда не попадает,
// поэтому префикс стабилен и кеш не промахивается.
use std::{collections::HashSet, sync::Mutex};

use base64::Engine;
use serde_json::{json, Value};

use crate::{
    cfg::Config,
    log::{Body, Part, Rec},
    reduce::truncate,
};

/// Пометка перед рамкой ревью/улучшения: выше лежит транскрипт уже закрытой
/// сессии. Без неё модель принимает чужой запрос за свой живой ход и выполняет
/// его вместо ревью. Роль system, потому что это тоже рамка, а не запрос.
const REPLAY_NOTE: &str = "Всё выше — транскрипт уже закрытой сессии: запросы в нём выполнены, \
вызовы инструментов не твои, повторять их нельзя. Не продолжай ту работу: дальше — новая \
инструкция, и выполнять нужно только её.";

pub fn raw_msgs(log: &[Rec], cfg: &Config) -> Vec<Value> {
    build(log, cfg)
}

/// Карта вырезанных результатов: `call_id -> заглушка`. Собирается из всех
/// записей `Rec::Ctx`. Элизия — правило проекции, а не перезапись лога:
/// исходный результат остаётся в файле сессии, и уборка переживает resume.
pub fn cleared_map(log: &[Rec]) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for rec in log {
        if let Rec::Ctx { cleared, .. } = rec {
            map.extend(cleared.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
    }
    map
}

/// Заменить содержимое tool-результатов по карте уборки. Применяется и к
/// свежей проекции, и к базе ревизии: иначе элизия не пережила бы commit.
pub fn apply_cleared(msgs: &mut [Value], cleared: &std::collections::BTreeMap<String, String>) {
    if cleared.is_empty() {
        return;
    }
    for m in msgs {
        if m["role"] != "tool" {
            continue;
        }
        if let Some(id) = m["tool_call_id"].as_str() {
            if let Some(placeholder) = cleared.get(id) {
                m["content"] = json!(placeholder);
            }
        }
    }
}

/// Проекция, которую видит провайдер: база ревизии плюс элизия. Отдельно от
/// `context::effective` (база для зеркала): в зеркале должны лежать настоящие
/// результаты, а заглушки уборки накладываются только на провод.
pub fn msgs(log: &[Rec], cfg: &Config) -> Vec<Value> {
    // Inspection can show the raw journal if a checkpoint was damaged.
    let mut out = crate::context::effective(log, cfg).unwrap_or_else(|_| raw_msgs(log, cfg));
    apply_cleared(&mut out, &cleared_map(log));
    // Справка памяти — не часть префикса: её видно только на том ходу, ради
    // которого она снята. Это делается здесь, а не при сборке из лога, потому
    // что та же точка применяется и к базе ревизии контекста.
    append_turn_recall(&mut out, log);
    out
}

/// Дописать справку памяти последним сообщением хода.
///
/// В логе справка лежит сразу после запроса человека (`Rec::User`), а её
/// содержимое — снимок на момент снятия. В проекцию уходит та, что записана
/// после **последнего** запроса: у более поздних ходов (.next) хвост окажется
/// после ответов, но справка старого хода не воскресает.
pub fn append_turn_recall(out: &mut Vec<Value>, log: &[Rec]) {
    let mut tail: Option<&str> = None;
    let mut current = false;
    for r in log {
        match r {
            Rec::User { .. } => {
                current = true;
                tail = None;
            }
            Rec::Recall { text, .. } if current => tail = Some(text.as_str()),
            _ => {}
        }
    }
    let Some(text) = tail.map(str::trim).filter(|t| !t.is_empty()) else {
        return;
    };
    // Роль system: это найденные факты, а не запрос человека, и `user` сразу
    // после `user` (а также после `tool`) ломает чередование ролей.
    out.push(json!({"role": "system", "content": text}));
}

fn build(log: &[Rec], cfg: &Config) -> Vec<Value> {
    // Промпт сессии — из лога. Пересборка из файлов только как запасной путь для
    // логов, записанных до появления записи System.
    let system = log
        .iter()
        .find_map(|r| match r {
            Rec::System { text, .. } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_else(|| crate::prompt::system_prompt(cfg));
    let mut out = vec![json!({"role": "system", "content": system})];
    // Есть ли выше рамки настоящий ход (запрос пользователя и/или вызовы): тогда
    // рамка — это ревью/улучшение поверх транскрипта, и транскрипт надо пометить.
    let mut had_work = false;
    let mut replay_marked = false;
    // Лог ссылается на транскрипт источника (`Rec::Replay`).
    let mut replay = false;
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
            Rec::User {
                text,
                source,
                parts,
                ..
            } => {
                // Инструкции ревью и улучшения — системные по смыслу: это не запрос
                // пользователя, а рамка задачи. Роль в проекции, лог остаётся как есть.
                let frame = source == "review" || source == "improve";
                // Перед рамкой весь лог — транскрипт уже закрытой сессии. Без явной
                // пометки модель принимает запрос из транскрипта за свой живой ход и
                // выполняет его вместо ревью. Пометка ставится только если транскрипт
                // есть: у свежего улучшения до рамки ничего, кроме Meta.
                if !frame {
                    had_work = true;
                }
                // Лог-хвост поверх `Replay`: в записях лежит только собственная
                // работа, а перед ней — чужой транскрипт (или ничего). Без пометки
                // модель сшивает рамку с транскриптом в один живой диалог.
                if frame && !replay_marked && (replay || had_work) {
                    replay_marked = true;
                    out.push(json!({"role": "system", "content": REPLAY_NOTE}));
                }
                let role = if frame { "system" } else { "user" };
                let content = if parts.is_empty() {
                    json!(text)
                } else {
                    content_value(text, parts, cfg)
                };
                out.push(json!({"role": role, "content": content}));
            }
            Rec::Assistant {
                text, tool_calls, ..
            } => {
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
                                "function": {"name": c.name, "arguments": historical_arguments(&c.args)}
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
                for c in tool_calls
                    .iter()
                    .filter(|c| !answered.contains(c.id.as_str()))
                {
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": c.id,
                        "content": "[вызов не завершён: ход прерван]"
                    }));
                }
            }
            Rec::ToolResult {
                call_id,
                body,
                parts,
                ..
            } => {
                if out.iter().any(|m| {
                    m.get("role").and_then(Value::as_str) == Some("tool")
                        && m.get("tool_call_id").and_then(Value::as_str) == Some(call_id.as_str())
                }) {
                    continue;
                }
                // Кап на результат живёт здесь, а не в инструменте: это единственная
                // точка, через которую проходит любой вывод любого тула.
                let text = match body {
                    Body::Inline { text } => truncate(text, cfg.max_result_chars),
                };
                let content = content_value(&text, parts, cfg);
                out.push(json!({"role": "tool", "tool_call_id": call_id, "content": content}));
            }
            // Справка памяти в проекцию из лога НЕ попадает: она живёт ровно
            // один ход и дописывается в конце (`msgs`), а не оседает в префиксе.
            // Иначе её токены платятся на каждом следующем ходу и сдвигают кеш.
            Rec::Recall { .. } => {}
            // Системные пометки (например «контекст ужат») — контекст, а не
            // запрос человека: второй `user` подряд ломает чередование ролей.
            Rec::ContextNotice { text, .. } => out.push(json!({"role":"system","content":text})),
            // Сообщение от другой сессии (родителя/ребёнка) или steer человека:
            // это слово, адресованное нам, поэтому роль `user` — но с пометкой
            // отправителя, чтобы модель не спутала его со своим запросом.
            Rec::Steer {
                from, via, text, ..
            } => {
                let label = match (from.is_empty(), via.as_str()) {
                    (true, _) => String::new(),
                    (false, "ask") => format!("[вопрос от {from}] "),
                    (false, "report") => format!("[итог от {from}] "),
                    (false, "steer") => "[сообщение от человека] ".to_string(),
                    (false, other) => format!("[{other} от {from}] "),
                };
                out.push(json!({"role": "user", "content": format!("{label}{text}")}));
            }
            // У записи нет собственного текста: дальше идёт транскрипт в этом же
            // логе. Пометку ставим не здесь, а перед рамкой — вставка в начало
            // сдвинула бы весь транскрипт и убила бы префикс-кеш.
            Rec::Replay { .. } => replay = true,
            Rec::System { .. }
            | Rec::Meta { .. }
            | Rec::Usage { .. }
            | Rec::ContextRevision { .. }
            | Rec::Ctx { .. }
            | Rec::Done { .. } => {}
        }
    }
    out
}

/// Контент сообщения: плоская строка, если вложений нет (как раньше), иначе
/// массив частей в формате openai-compatible. Картинки едут data-URL-ом;
/// видео/аудио/файлы ядро пока не умеет отдавать — они остаются пометкой.
///
/// Ссылку на путь не подставляем вместо base64: у провайдера нет доступа к
/// нашей файловой системе, он запросит URL и получит 400.
fn content_value(text: &str, parts: &[Part], cfg: &Config) -> Value {
    if parts.is_empty() {
        return json!(text);
    }
    let mut arr = vec![json!({"type": "text", "text": text})];
    for p in parts {
        // Канал есть, а модель вложений не понимает — не шлём и говорим прямо.
        if !cfg.attachments {
            arr.push(json!({
                "type": "text",
                "text": format!("[вложение не отправлено: attachments выключены — {} {} Б]", p.mime, p.bytes)
            }));
            continue;
        }
        if p.kind != "image" {
            arr.push(json!({
                "type": "text",
                "text": format!("[вложение {}: {} ({}), {} Б]", p.kind, p.path, p.mime, p.bytes)
            }));
            continue;
        }
        match data_url(&p.path, &p.mime) {
            Some(url) => {
                arr.push(json!({"type": "text", "text": format!("[изображение: {} ({}), {} Б]", p.path, p.mime, p.bytes)}));
                arr.push(json!({"type": "image_url", "image_url": {"url": url}}));
            }
            None => arr.push(json!({
                "type": "text",
                "text": format!("[вложение недоступно: {}]", p.path)
            })),
        }
    }
    Value::Array(arr)
}

/// base64 файла, закешированный по пути: проекция строится каждый ход, а файл
/// в хранилище неизменяем — иначе мегабайтная картинка кодировалась бы впустую.
/// Кеш не влияет на результат, только на цену, поэтому детерминизм не страдает.
static B64_CACHE: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
const B64_CACHE_MAX: usize = 16;

fn data_url(path: &str, mime: &str) -> Option<String> {
    if let Ok(c) = B64_CACHE.lock() {
        if let Some((_, url)) = c.iter().find(|(p, _)| p == path) {
            return Some(url.clone());
        }
    }
    let bytes = std::fs::read(path).ok()?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    let url = format!("data:{mime};base64,{b64}");
    if let Ok(mut c) = B64_CACHE.lock() {
        if c.len() >= B64_CACHE_MAX {
            c.remove(0);
        }
        c.push((path.to_string(), url.clone()));
    }
    Some(url)
}

/// Автоэлизия: какие результаты тулов вырезать, чтобы вернуться к target.
/// Детерминированная функция лога и конфига — при одном и том же логе даёт
/// один и тот же ответ, поэтому не «срабатывает каждый ход»: уже вырезанное
/// лежит в карте `cleared` и в кандидаты не попадает.
///
/// Последние `keep_results` результатов не трогаем — модель работает именно
/// с ними. Вырезаем самое старое вперёд, пока оценка не опустится до цели.
pub fn plan_elision(log: &[Rec], cfg: &Config) -> std::collections::BTreeMap<String, String> {
    let already = cleared_map(log);
    let mut msgs = crate::context::effective(log, cfg).unwrap_or_else(|_| raw_msgs(log, cfg));
    apply_cleared(&mut msgs, &already);
    let current = calibrated_tokens(log, &msgs);
    if current < cfg.context_budget() {
        return std::collections::BTreeMap::new();
    }

    // Результаты в порядке появления, без уже вырезанных.
    let candidates: Vec<(usize, &str)> = log
        .iter()
        .enumerate()
        .filter_map(|(i, r)| match r {
            Rec::ToolResult { call_id, .. } if !already.contains_key(call_id) => {
                Some((i, call_id.as_str()))
            }
            _ => None,
        })
        .collect();
    let keep_from = candidates.len().saturating_sub(cfg.keep_results);

    // Размер заглушки для каждого кандидата и текущий размер сообщения.
    let mut cleared = std::collections::BTreeMap::new();
    let mut remaining = current;
    let target = cfg.context_target();
    for candidate in candidates.iter().take(keep_from) {
        if remaining <= target {
            break;
        }
        let call_id = candidate.1;
        let Some(text) = msgs.iter().find_map(|m| {
            (m["role"] == "tool" && m["tool_call_id"].as_str() == Some(call_id))
                .then(|| m["content"].clone())
        }) else {
            continue;
        };
        let placeholder = placeholder_for(log, call_id);
        let freed = ((estimate_tokens(std::slice::from_ref(&text)) as f64
            - estimate_tokens(std::slice::from_ref(&json!(placeholder))) as f64)
            * calibration(log))
            .max(0.0) as u64;
        cleared.insert(call_id.to_string(), placeholder);
        remaining = remaining.saturating_sub(freed);
    }
    cleared
}

/// Заглушка с именем тула, размером и хвостом команды: без неё модель не
/// знает, что именно вырезано, и не может решить, перезапускать ли вызов.
fn placeholder_for(log: &[Rec], call_id: &str) -> String {
    for rec in log {
        if let Rec::ToolResult {
            call_id: id,
            tool,
            argv,
            bytes,
            exit,
            ..
        } = rec
        {
            if id == call_id {
                return format!(
                    "[результат вырезан уборкой контекста: {tool}, {bytes} Б, exit {exit} — {}]",
                    command_hint(argv)
                );
            }
        }
    }
    "[результат вырезан уборкой контекста]".to_string()
}

/// Команду из лога оставляем в пометке: без неё модель не знает, что именно
/// было вырезано, и не может решить, перезапускать или нет.
pub fn command_hint(argv: &[String]) -> String {
    let raw = argv.join(" ").replace('\n', " ");
    let mut s: String = raw.chars().take(160).collect();
    if raw.chars().count() > 160 {
        s.push('…');
    }
    s
}

/// Множитель оценки по токенам провайдера: `in_tok / est_tok` того же хода.
/// Свежие ходы весят больше — токенизатор мог смениться вместе с моделью.
/// Нет ни одного замера — единица, то есть сырая оценка `len/3`.
pub fn calibration(log: &[Rec]) -> f64 {
    let mut weighted = 0.0f64;
    let mut weights = 0.0f64;
    let mut w = 1.0f64;
    for rec in log.iter().rev() {
        if let Rec::Usage { in_tok, est_tok, .. } = rec {
            if *est_tok > 0 && *in_tok > 0 {
                weighted += (*in_tok as f64 / *est_tok as f64) * w;
                weights += w;
                w *= 0.7;
            }
        }
    }
    if weights > 0.0 {
        weighted / weights
    } else {
        1.0
    }
}

/// Оценка размера проекции в токенах модели: сырая эвристика, помноженная на
/// откалиброванный множитель. Так порог считается по числам провайдера, а не
/// по «1 символ ≈ 1/3 токена», которое для кириллицы и CJK завышает вслепую.
pub fn calibrated_tokens(log: &[Rec], msgs: &[Value]) -> u64 {
    (estimate_tokens(msgs) as f64 * calibration(log)).round() as u64
}

/// Грубая оценка токенов проекции: точные числа берём из usage провайдера,
/// а это — только для порога компакции.
pub fn estimate_tokens(msgs: &[Value]) -> u64 {
    fn size(value: &Value) -> u64 {
        match value {
            Value::Object(_) if value["type"] == "image_url" => 4096,
            Value::Object(map) => map
                .iter()
                .map(|(k, v)| k.len() as u64 / 3 + size(v) + 4)
                .sum(),
            Value::Array(items) => items.iter().map(size).sum(),
            Value::String(s) => s.len() as u64 / 3 + 1,
            _ => 1,
        }
    }
    msgs.iter().map(size).sum()
}

/// A provider may emit broken JSON. Dispatch reports it, but history must remain
/// sendable so the model can correct the call. Only the projection is repaired.
fn historical_arguments(args: &str) -> String {
    if serde_json::from_str::<Value>(args).is_ok() {
        args.to_string()
    } else {
        json!({"_snens_invalid_arguments": args}).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Rec;

    #[test]
    fn image_estimate_ignores_base64_length() {
        let small = vec![
            json!({"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,a"}}]}),
        ];
        let large = vec![
            json!({"role":"user","content":[{"type":"image_url","image_url":{"url":format!("data:image/png;base64,{}", "a".repeat(1_400_000))}}]}),
        ];
        assert_eq!(estimate_tokens(&small), estimate_tokens(&large));
        assert!(estimate_tokens(&large) < 5000);
    }

    #[test]
    fn malformed_provider_arguments_repaired_in_projection_only() {
        let call = crate::log::Call {
            id: "broken".into(),
            name: "run".into(),
            args: "{broken".into(),
        };
        let raw = vec![Rec::Assistant {
            ts: 0,
            text: None,
            tool_calls: vec![call],
            reasoning: None,
        }];
        let msgs = raw_msgs(&raw, &Config::default());
        assert!(serde_json::from_str::<Value>(
            msgs[1]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap()
        )
        .is_ok());
        crate::context_document::validate(&msgs, true).unwrap();
        assert!(serde_json::to_string(&raw).unwrap().contains("{broken"));
    }

    fn user(text: &str, source: &str) -> Rec {
        Rec::User {
            ts: 0,
            text: text.into(),
            source: source.into(),
            parts: Vec::new(),
        }
    }

    fn recall(text: &str) -> Rec {
        Rec::Recall {
            ts: 0,
            text: text.into(),
        }
    }

    /// Справка памяти живёт ровно один ход: в проекции она есть только пока
    /// запрос человека — последний. На следующем ходу её нет, и в префиксе
    /// остаётся только диалог.
    #[test]
    fn recall_is_visible_for_one_turn_only() {
        let cfg = Config::default();
        let mut log = vec![user("первый", "cli"), recall("факт про X")];
        let first = msgs(&log, &cfg);
        assert_eq!(
            first.iter().filter(|m| m["content"] == json!("факт про X")).count(),
            1,
            "справка не доехала до текущего хода"
        );
        log.push(Rec::Assistant {
            ts: 0,
            text: Some("ответ".into()),
            tool_calls: vec![],
            reasoning: None,
        });
        log.push(user("второй", "cli"));
        log.push(recall("факт про Y"));
        let second = msgs(&log, &cfg);
        assert!(!second.iter().any(|m| m["content"] == json!("факт про X")), "старая справка осела в префиксе: {second:#?}");
        assert_eq!(
            second.iter().filter(|m| m["content"] == json!("факт про Y")).count(),
            1
        );
    }

    /// Справка внутри одного хода не вытесняется шагами инструментов: она
    /// снята по запросу человека и остаётся к нему до конца хода.
    #[test]
    fn recall_survives_tool_steps_within_the_turn() {
        let cfg = Config::default();
        let log = vec![
            user("первый", "cli"),
            recall("факт про Z"),
            Rec::Assistant {
                ts: 0,
                text: None,
                tool_calls: vec![crate::log::Call {
                    id: "c1".into(),
                    name: "run".into(),
                    args: "{}".into(),
                }],
                reasoning: None,
            },
            tool_rec("c1", "ok", 2),
        ];
        let projected = msgs(&log, &cfg);
        assert!(projected.iter().any(|m| m["content"] == json!("факт про Z")));
        // И после справки ничего не превращается в orphan: роли валидны.
        crate::context_document::validate(&projected, true).unwrap();
    }

    #[test]
    fn duplicate_surface_tool_result_projects_once() {
        let cfg = Config::default();
        let log = vec![
            user("do it", "surface"),
            Rec::Assistant {
                ts: 0,
                text: None,
                reasoning: None,
                tool_calls: vec![crate::log::Call {
                    id: "call-1".into(),
                    name: "send_file".into(),
                    args: "{}".into(),
                }],
            },
            Rec::ToolResult {
                ts: 1,
                call_id: "call-1".into(),
                tool: "send_file".into(),
                argv: vec![],
                exit: 0,
                bytes: 0,
                ms: 1,
                body: Body::Inline { text: "ok".into() },
                parts: vec![],
            },
            Rec::ToolResult {
                ts: 2,
                call_id: "call-1".into(),
                tool: "send_file".into(),
                argv: vec![],
                exit: 0,
                bytes: 0,
                ms: 1,
                body: Body::Inline {
                    text: "duplicate".into(),
                },
                parts: vec![],
            },
        ];
        let projected = raw_msgs(&log, &cfg);
        assert_eq!(
            projected
                .iter()
                .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool")
                    && m.get("tool_call_id").and_then(Value::as_str) == Some("call-1"))
                .count(),
            1
        );
    }

    #[test]
    fn replay_note_precedes_review_frame_and_keeps_prefix() {
        let cfg = Config::default();
        let log = vec![
            user("добавь строку и собери", "cli"),
            Rec::Assistant {
                reasoning: None,
                ts: 0,
                text: Some("готово".into()),
                tool_calls: vec![],
            },
            user("# Ревью сессии\nпервое действие — тикет", "review"),
        ];
        let msgs = msgs(&log, &cfg);
        let note = msgs
            .iter()
            .position(|m| {
                m["content"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("транскрипт уже закрытой сессии")
            })
            .expect("нет пометки о реплее");
        assert_eq!(
            msgs[note]["role"], "system",
            "пометка должна быть системной"
        );
        let frame = msgs
            .iter()
            .position(|m| {
                m["content"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("# Ревью сессии")
            })
            .unwrap();
        assert!(note < frame, "пометка должна стоять перед рамкой ревью");
        // Префикс до пометки — это транскрипт как есть: кеш провайдера не промахнётся.
        assert_eq!(msgs[1]["content"], "добавь строку и собери");
    }

    /// Лог со ссылкой на источник (`Replay`): пометка о чужом транскрипте всё
    /// равно встаёт перед рамкой, но не раньше — вставка в начало сдвинула бы
    /// транскрипт и убила бы префикс-кеш.
    #[test]
    fn replay_link_marks_the_frame_without_shifting_the_prefix() {
        let cfg = Config::default();
        let linked = vec![
            Rec::Meta {
                ts: 0,
                model: String::new(),
                parent: String::new(),
                surface: None,
                title: None,
            },
            Rec::Replay {
                ts: 0,
                session: "base".into(),
                lines: 3,
            },
            user("# Ревью сессии", "review"),
        ];
        let got = raw_msgs(&linked, &cfg);
        let note = got
            .iter()
            .position(|m| m["content"].as_str().unwrap_or_default().contains(REPLAY_NOTE))
            .expect("нет пометки о транскрипте");
        let frame = got
            .iter()
            .position(|m| m["content"].as_str() == Some("# Ревью сессии"))
            .unwrap();
        assert!(note < frame, "пометка должна стоять перед рамкой");
        assert_eq!(note, got.len() - 2, "пометка влезла не в конец транскрипта");
    }

    #[test]
    fn fresh_frame_without_transcript_gets_no_note() {        let cfg = Config::default();
        let log = vec![user("разбери тикеты", "improve")];
        let msgs = msgs(&log, &cfg);
        // Ищем именно пометку реплея, а не слово «транскрипт»: оно встречается
        // в memory.md и тест ловил бы память инстанса вместо пометки.
        assert!(
            !msgs.iter().any(|m| m["content"]
                .as_str()
                .unwrap_or_default()
                .contains(REPLAY_NOTE)),
            "пометка не нужна, когда транскрипта нет"
        );
        assert_eq!(msgs[1]["role"], "system", "рамка улучшения — системная");
    }

    /// Промпт сессии берётся из лога, а не собирается заново: иначе правка файлов
    /// инстанса по ходу сессии сдвинула бы начало промпта и убила префикс-кеш.
    #[test]
    fn system_prompt_is_read_from_log() {
        let cfg = Config::default();
        let log = vec![
            Rec::System {
                ts: 0,
                text: "ПРОМПТ ИЗ ЛОГА".into(),
            },
            user("привет", "cli"),
        ];
        assert_eq!(msgs(&log, &cfg)[0]["content"], "ПРОМПТ ИЗ ЛОГА");
        // Лог без записи System (записанный до неё) — собираем, иначе сессия мертва.
        assert!(msgs(&[user("привет", "cli")], &cfg)[0]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("Ты агент"));
    }

    /// Вложение из канала едет как image-часть; без него — плоская строка, как
    /// раньше (совместимость со старыми логами и текстовыми тулами).
    #[test]
    fn user_attachment_projects_image_and_document_path() {
        let dir = std::env::temp_dir().join(format!("user-attach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("managed.png");
        std::fs::write(
            &png,
            [
                0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 0, 0, 0, 13, b'I', b'H', b'D',
                b'R', 0, 0, 0, 1, 0, 0, 0, 1,
            ],
        )
        .unwrap();
        let image = Part {
            kind: "image".into(),
            path: png.to_string_lossy().into_owned(),
            mime: "image/png".into(),
            bytes: 24,
            width: 1,
            height: 1,
        };
        let document = Part {
            kind: "file".into(),
            path: dir.join("managed.blob").to_string_lossy().into_owned(),
            mime: "application/octet-stream".into(),
            bytes: 9,
            width: 0,
            height: 0,
        };
        let log = vec![Rec::User {
            ts: 0,
            text: "inspect".into(),
            source: "surface".into(),
            parts: vec![image, document],
        }];
        let projected = msgs(&log, &Config::default());
        let content = projected.iter().find(|m| m["role"] == "user").unwrap()["content"]
            .as_array()
            .unwrap();
        assert!(content.iter().any(|p| p["type"] == "image_url"));
        assert!(content
            .iter()
            .any(|p| p["type"] == "text" && p["text"].as_str().unwrap().contains("managed.blob")));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn attachment_becomes_image_part() {
        let dir = std::env::temp_dir().join(format!("snens-attach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 1x1 PNG: заголовок + пустой IDAT достаточно, чтобы base64 собрать.
        let png: Vec<u8> = vec![
            0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 0, 0, 0, 13, b'I', b'H', b'D', b'R',
            0, 0, 0, 1, 0, 0, 0, 1,
        ];
        let img = dir.join("px.png");
        std::fs::write(&img, &png).unwrap();
        let cfg = Config::default();
        let log = vec![
            user("посмотри", "cli"),
            Rec::Assistant {
                reasoning: None,
                ts: 0,
                text: None,
                tool_calls: vec![crate::log::Call {
                    id: "c1".into(),
                    name: "view".into(),
                    args: "{}".into(),
                }],
            },
            Rec::ToolResult {
                ts: 0,
                call_id: "c1".into(),
                tool: "view".into(),
                argv: vec![],
                exit: 0,
                bytes: png.len(),
                ms: 1,
                body: Body::Inline {
                    text: "px.png".into(),
                },
                parts: vec![Part {
                    kind: "image".into(),
                    path: img.to_string_lossy().into_owned(),
                    mime: "image/png".into(),
                    bytes: png.len(),
                    width: 1,
                    height: 1,
                }],
            },
        ];
        let projected = msgs(&log, &cfg);
        let tool = projected.iter().find(|m| m["role"] == "tool").unwrap();
        let content = tool["content"]
            .as_array()
            .expect("вложение не собралось в части");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[2]["type"], "image_url");
        assert!(content[2]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));
        // Детерминизм: кеш base64 не влияет на результат.
        assert_eq!(
            serde_json::to_string(&projected).unwrap(),
            serde_json::to_string(&msgs(&log, &cfg)).unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Переполнение окна само по себе не должно менять текст или терять картинки.
    #[test]
    fn attachments_are_preserved_regardless_of_window() {
        let dir =
            std::env::temp_dir().join(format!("snens-attach-preserve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png: Vec<u8> = vec![
            0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 0, 0, 0, 13, b'I', b'H', b'D', b'R',
            0, 0, 0, 1, 0, 0, 0, 1,
        ];
        let mut cfg = Config { ctx_window: 40_000, ..Config::default() };
        let part = |path: &std::path::Path| Part {
            kind: "image".into(),
            path: path.to_string_lossy().into_owned(),
            mime: "image/png".into(),
            bytes: png.len(),
            width: 1,
            height: 1,
        };
        let img1 = dir.join("a.png");
        std::fs::write(&img1, &png).unwrap();
        let img2 = dir.join("b.png");
        std::fs::write(&img2, &png).unwrap();
        let log = vec![
            user(&"x".repeat(150_000), "cli"),
            Rec::Assistant {
                reasoning: None,
                ts: 0,
                text: None,
                tool_calls: vec![crate::log::Call {
                    id: "c1".into(),
                    name: "view".into(),
                    args: "{}".into(),
                }],
            },
            Rec::ToolResult {
                ts: 0,
                call_id: "c1".into(),
                tool: "view".into(),
                argv: vec![],
                exit: 0,
                bytes: 1,
                ms: 1,
                body: Body::Inline { text: "a".into() },
                parts: vec![part(&img1)],
            },
            Rec::Assistant {
                reasoning: None,
                ts: 0,
                text: None,
                tool_calls: vec![crate::log::Call {
                    id: "c2".into(),
                    name: "view".into(),
                    args: "{}".into(),
                }],
            },
            Rec::ToolResult {
                ts: 0,
                call_id: "c2".into(),
                tool: "view".into(),
                argv: vec![],
                exit: 0,
                bytes: 1,
                ms: 1,
                body: Body::Inline { text: "b".into() },
                parts: vec![part(&img2)],
            },
        ];
        let projected = msgs(&log, &cfg);
        let tools: Vec<&Value> = projected.iter().filter(|m| m["role"] == "tool").collect();
        assert!(estimate_tokens(&projected) > cfg.ctx_window);
        assert_eq!(tools.len(), 2);
        for (tool, text) in tools.iter().zip(["a", "b"]) {
            let content = tool["content"].as_array().unwrap();
            assert_eq!(content.len(), 3);
            assert_eq!(content[0], json!({"type": "text", "text": text}));
            assert_eq!(content[1]["type"], "text");
            assert_eq!(content[2]["type"], "image_url");
            assert!(content[2]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,"));
        }
        assert_eq!(projected, raw_msgs(&log, &cfg));
        assert_eq!(projected, msgs(&log, &cfg));
        cfg.ctx_window = 400_000;
        assert_eq!(projected, msgs(&log, &cfg));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// attachments=false: канал есть, но модель вложений не понимает — не шлём
    /// картинку (провайдер ответит 400) и говорим об этом текстом.
    #[test]
    fn attachments_off_keeps_placeholder() {
        let cfg = Config { attachments: false, ..Config::default() };
        let log = vec![
            user("посмотри", "cli"),
            Rec::Assistant {
                reasoning: None,
                ts: 0,
                text: None,
                tool_calls: vec![crate::log::Call {
                    id: "c1".into(),
                    name: "view".into(),
                    args: "{}".into(),
                }],
            },
            Rec::ToolResult {
                ts: 0,
                call_id: "c1".into(),
                tool: "view".into(),
                argv: vec![],
                exit: 0,
                bytes: 1,
                ms: 1,
                body: Body::Inline {
                    text: "px.png".into(),
                },
                parts: vec![Part {
                    kind: "image".into(),
                    path: "/нет/такого".into(),
                    mime: "image/png".into(),
                    bytes: 1,
                    width: 1,
                    height: 1,
                }],
            },
        ];
        let tool = msgs(&log, &cfg)
            .into_iter()
            .find(|m| m["role"] == "tool")
            .unwrap();
        let s = serde_json::to_string(&tool["content"]).unwrap();
        assert!(s.contains("attachments выключены"), "нет пометки: {s}");
        assert!(
            !s.contains("image_url"),
            "картинка уехала при выключенном канале"
        );
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
                reasoning: None,
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
                parts: vec![],
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
        assert!(
            answered.contains(&"c2"),
            "оборванный вызов остался без ответа"
        );
        // Ремонт детерминирован — иначе промахнётся префикс-кеш.
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
    }

    fn tool_rec(call_id: &str, text: &str, bytes: usize) -> Rec {
        Rec::ToolResult {
            ts: 0,
            call_id: call_id.into(),
            tool: "run".into(),
            argv: vec![json!({"command": "echo hi"}).to_string()],
            exit: 0,
            bytes,
            ms: 1,
            body: crate::log::Body::Inline { text: text.into() },
            parts: vec![],
        }
    }

    #[test]
    fn auto_elision_skips_recent_and_is_idempotent() {
        let cfg: Config = toml::from_str(
            "ctx_window = 100000\ncontext_budget = 1000\ncontext_target = 500\nkeep_results = 1\nmax_result_chars = 1000000",
        )
        .unwrap();
        let log = vec![
            Rec::User {
                ts: 0,
                text: "goal".into(),
                source: "human".into(),
                parts: vec![],
            },
            tool_rec("old", &"x".repeat(6000), 6000),
            tool_rec("fresh", &"y".repeat(6000), 6000),
        ];
        let first = plan_elision(&log, &cfg);
        // Последний результат защищён: вырезан только старый.
        assert!(first.contains_key("old"), "старый результат не вырезан");
        assert!(!first.contains_key("fresh"), "свежий результат тронут");

        // Применение записью Ctx — и повторный план ничего не добавляет.
        let mut with_ctx = log.clone();
        with_ctx.push(Rec::Ctx {
            ts: 0,
            action: "auto".into(),
            elided: first.len(),
            freed_bytes: 0,
            cleared: first.clone(),
        });
        assert!(
            plan_elision(&with_ctx, &cfg).is_empty(),
            "элизия срабатывает повторно — та самая петля"
        );
    }

    #[test]
    fn elision_keeps_dialog_and_clears_large_result() {
        let cfg: Config = toml::from_str(
            "ctx_window = 100000\ncontext_budget = 1000\ncontext_target = 500\nkeep_results = 0\nmax_result_chars = 1000000",
        )
        .unwrap();
        let log = vec![
            Rec::User {
                ts: 0,
                text: "решение: не переспрашивать".into(),
                source: "human".into(),
                parts: vec![],
            },
            tool_rec("big", &"z".repeat(6000), 6000),
        ];
        let mut messages = raw_msgs(&log, &cfg);
        apply_cleared(&mut messages, &plan_elision(&log, &cfg));
        let wire = serde_json::to_string(&messages).unwrap();
        assert!(!wire.contains(&"z".repeat(100)));
        assert!(
            wire.contains("решение: не переспрашивать"),
            "текст пользователя потерян"
        );
    }

    #[test]
    fn calibration_weights_recent_usage() {
        let log = vec![Rec::Usage {
            ts: 0,
            in_tok: 1000,
            cached_tok: 0,
            out_tok: 0,
            cost_micro: 0,
            upstream_id: String::new(),
            fingerprint: String::new(),
            finish_reason: String::new(),
            reason_tok: 0,
            encrypted_reasoning: false,
            est_tok: 2000,
        }];
        assert!((calibration(&log) - 0.5).abs() < 1e-9);
        // Без замеров — единица, то есть сырая оценка len/3.
        assert_eq!(calibration(&[]), 1.0);
    }
}
