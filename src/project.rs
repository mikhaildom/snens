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

pub fn msgs(log: &[Rec], cfg: &Config) -> Vec<Value> {
    // Inspection can show the raw journal if a checkpoint was damaged.
    crate::context::effective(log, cfg).unwrap_or_else(|_| raw_msgs(log, cfg))
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
                // Кап на результат живёт здесь, а не в инструменте: это единственная
                // точка, через которую проходит любой вывод любого тула.
                let text = match body {
                    Body::Inline { text } => truncate(text, cfg.max_result_chars),
                };
                let content = content_value(&text, parts, cfg);
                out.push(json!({"role": "tool", "tool_call_id": call_id, "content": content}));
            }
            // Справка памяти — системная роль: это контекст, а не запрос
            // пользователя, и второй `user` подряд ломает чередование ролей.
            Rec::ContextNotice { text, .. } => out.push(json!({"role":"system","content":text})),
            Rec::Recall { text, .. } => out.push(json!({"role": "system", "content": text})),
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
                "text": format!("[вложение {} не поддерживается провайдером (пока): {} Б]", p.kind, p.bytes)
            }));
            continue;
        }
        match data_url(&p.path, &p.mime) {
            Some(url) => arr.push(json!({"type": "image_url", "image_url": {"url": url}})),
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
        }
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

    #[test]
    fn fresh_frame_without_transcript_gets_no_note() {
        let cfg = Config::default();
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
        assert_eq!(content[1]["type"], "image_url");
        assert!(content[1]["image_url"]["url"]
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
        let mut cfg = Config::default();
        cfg.ctx_window = 40_000;
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
            assert_eq!(content.len(), 2);
            assert_eq!(content[0], json!({"type": "text", "text": text}));
            assert_eq!(content[1]["type"], "image_url");
            assert!(content[1]["image_url"]["url"]
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
        let mut cfg = Config::default();
        cfg.attachments = false;
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
}
