//! Editable JSON view. Systems are protected; non-text content parts are opaque references.
//!
//! Политика правки — CLM: контекст принадлежит модели. Неизменяемы ровно два
//! блока — *первый* и *последний* user-блок (исходная задача и текущая
//! инструкция человека). Всё остальное — assistant-текст, user-сообщения из
//! середины, результаты тулов — можно сокращать, удалять и переставлять.
use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::R;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Document {
    pub revision: String,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Block {
    pub id: String,
    pub messages: Vec<Value>,
}

pub(crate) fn hash(bytes: &[u8]) -> String {
    let mut h = 0xcbf29ce484222325u64;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("fnv64:{h:016x}")
}

/// Текстовая форма `content`: строка и массив текстовых parts дают одно и то же.
/// Защита сравнивает содержимое, а не сериализацию — иначе одна и та же запись,
/// пришедшая массивом `parts`, выглядела бы другой и «незащищённой».
fn text_of(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                if part["type"] != "text" {
                    return None;
                }
                text.push_str(part["text"].as_str()?);
            }
            Some(text)
        }
        _ => None,
    }
}

/// Сообщение в сравнимой форме: форма `content` (строка или `parts`) не влияет
/// на решение о защите. Нетекстовые части остаются как есть.
fn canonical(message: &Value) -> Value {
    let mut out = message.clone();
    if let Some(text) = message.get("content").and_then(text_of) {
        if let Some(object) = out.as_object_mut() {
            object.insert("content".into(), Value::String(text));
        }
    }
    out
}

fn canonical_block(messages: &[Value]) -> Vec<Value> {
    messages.iter().map(canonical).collect()
}

/// Короткое описание блока для отказа: id, роль, размер одной строкой. Модель
/// должна понять, о каком блоке речь, не перечитывая зеркало целиком.
fn describe(block: &Block) -> String {
    let role = block
        .messages
        .first()
        .and_then(|m| m["role"].as_str())
        .unwrap_or("?");
    let chars: usize = block
        .messages
        .iter()
        .map(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0))
        .sum();
    format!("{} ({role}, {chars} chars)", block.id)
}

pub(crate) struct View {
    pub document: Document,
    attachments: BTreeMap<String, Value>,
    // Protected system messages precede the next original block, or the end.
    systems: Vec<(usize, Value)>,
    // Индексы блоков первого и последнего user-блока: только они неприкосновенны.
    protected: Vec<usize>,
}

impl View {
    pub fn new(messages: &[Value], revision: String) -> R<Self> {
        validate(messages, true)?;
        let mut blocks: Vec<Block> = Vec::new();
        let mut systems = Vec::new();
        for m in messages {
            if m["role"] == "system" {
                systems.push((blocks.len(), m.clone()));
            } else if m["role"] == "tool" {
                blocks
                    .last_mut()
                    .ok_or("orphan tool result")?
                    .messages
                    .push(m.clone());
            } else {
                blocks.push(Block {
                    id: String::new(),
                    messages: vec![m.clone()],
                });
            }
        }
        let mut counts = BTreeMap::<String, usize>::new();
        let mut attachments = BTreeMap::new();
        for block in &mut blocks {
            let digest = hash(&serde_json::to_vec(&block.messages)?);
            let count = counts.entry(digest.clone()).or_default();
            block.id = format!("b-{}-{count}", digest.trim_start_matches("fnv64:"));
            *count += 1;
            for m in &mut block.messages {
                if let Some(parts) = m["content"].as_array_mut() {
                    for part in parts {
                        if part["type"] != "text" {
                            let key = hash(&serde_json::to_vec(part)?);
                            if let Some(previous) = attachments.insert(key.clone(), part.clone()) {
                                if previous != *part {
                                    return Err("attachment hash collision".into());
                                }
                            }
                            *part = json!({"$context_attachment": key});
                        }
                    }
                }
            }
        }
        let users: Vec<usize> = blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| block.messages.iter().any(|m| m["role"] == "user"))
            .map(|(index, _)| index)
            .collect();
        let protected = match (users.first(), users.last()) {
            (Some(first), Some(last)) if first == last => vec![*first],
            (Some(first), Some(last)) => vec![*first, *last],
            _ => Vec::new(),
        };
        Ok(Self {
            document: Document { revision, blocks },
            attachments,
            systems,
            protected,
        })
    }

    /// Как называется защищённый блок в отказе: у сессии из одного user-блока он
    /// сразу и первый, и последний.
    fn label(&self, index: usize) -> &'static str {
        match (self.protected.len(), self.protected.first() == Some(&index)) {
            (1, _) => "first and last",
            (_, true) => "first",
            _ => "last",
        }
    }

    /// Развернуть непрозрачные ссылки вложений обратно в части. Отдельной
    /// функцией — её же использует счётчик размеров блоков для подсказки.
    fn resolve(&self, messages: &[Value]) -> R<Vec<Value>> {
        let mut messages = messages.to_vec();
        for m in &mut messages {
            if let Some(parts) = m["content"].as_array_mut() {
                for part in parts {
                    if let Some(key) = part.get("$context_attachment") {
                        if part.as_object().map(|o| o.len()) != Some(1) {
                            return Err("attachment reference must be opaque".into());
                        }
                        *part = self
                            .attachments
                            .get(key.as_str().ok_or("invalid attachment reference")?)
                            .ok_or("unknown attachment reference")?
                            .clone();
                    } else if part["type"] != "text" {
                        return Err("new attachments are not allowed in context drafts".into());
                    }
                }
            }
        }
        Ok(messages)
    }

    /// Крупнейшие блоки текущей проекции: id, роль, оценка токенов. Нужно для
    /// отказа по сжатию — модель получает механический ответ «что снимать», а не
    /// «правка не понравилась».
    pub fn block_sizes(&self) -> Vec<(String, String, u64)> {
        let mut sizes: Vec<(String, String, u64)> = self
            .document
            .blocks
            .iter()
            .map(|block| {
                let role = block
                    .messages
                    .first()
                    .and_then(|m| m["role"].as_str())
                    .unwrap_or("?")
                    .to_string();
                let messages = self
                    .resolve(&block.messages)
                    .unwrap_or_else(|_| block.messages.clone());
                (
                    block.id.clone(),
                    role,
                    crate::project::estimate_tokens(&messages),
                )
            })
            .collect();
        sizes.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        sizes
    }

    pub fn restore(&self, edited: &Document) -> R<Vec<Value>> {
        if edited.revision != self.document.revision {
            return Err(format!(
                "stale revision {} (current {}); reread LIVE_CONTEXT.json and edit the fresh copy",
                edited.revision, self.document.revision
            )
            .into());
        }
        let originals: HashSet<&str> = self.document.blocks.iter().map(|b| b.id.as_str()).collect();
        // Политика защиты: первый и последний user-блок неприкосновенны. Всё
        // остальное — assistant-текст, середина диалога, результаты тулов —
        // модель вправе сократить, удалить или переставить.
        let mut seen = HashSet::new();
        let mut positions = BTreeMap::<&str, usize>::new();
        for (index, block) in edited.blocks.iter().enumerate() {
            if !seen.insert(block.id.as_str()) {
                return Err(format!(
                    "duplicate block id {}; keep one copy and delete the other",
                    describe(block)
                )
                .into());
            }
            positions.insert(block.id.as_str(), index);
        }
        let mut previous: Option<usize> = None;
        for &index in &self.protected {
            let original = &self.document.blocks[index];
            let label = self.label(index);
            let Some(&at) = positions.get(original.id.as_str()) else {
                return Err(format!(
                    "block {} is protected: it is the {label} human request and cannot be removed; \
                     edit or remove other blocks instead",
                    describe(original)
                )
                .into());
            };
            if let Some(previous) = previous {
                if at < previous {
                    return Err(format!(
                        "block {} is protected and was moved: the first human request must stay \
                         before the last one; reorder other blocks instead",
                        describe(original)
                    )
                    .into());
                }
            }
            previous = Some(at);
            let block = &edited.blocks[at];
            if canonical_block(&block.messages) != canonical_block(&original.messages) {
                return Err(format!(
                    "block {} is protected: it is the {label} human request and cannot be edited; \
                     change other blocks, or add a new-* note with what you need to remember",
                    describe(original)
                )
                .into());
            }
        }
        let mut restored = Vec::new();
        for block in &edited.blocks {
            if !originals.contains(block.id.as_str()) && !block.id.starts_with("new-") {
                return Err(format!(
                    "unknown block id {}; use ids from the current mirror headers, or prefix a new \
                     block's id with new-",
                    describe(block)
                )
                .into());
            }
            if block.messages.is_empty() {
                return Err(format!(
                    "block {} is empty; delete the block instead of emptying it",
                    block.id
                )
                .into());
            }
            let messages = self.resolve(&block.messages)?;
            validate(&messages, false).map_err(|error| format!("block {}: {error}", block.id))?;
            if block.id.starts_with("new-") && messages.iter().any(|m| m["role"] != "user") {
                return Err(format!(
                    "block {} is a new note and must contain user messages only; \
                     notes carry text, not tool calls",
                    block.id
                )
                .into());
            }
            restored.push(messages);
        }
        let mut systems: BTreeMap<usize, Vec<Value>> = BTreeMap::new();
        for (at, message) in &self.systems {
            // Initial system stays first. Other systems stay ahead of the next
            // surviving original block, even when their immediate neighbour was deleted.
            let position = if *at == 0 {
                0
            } else {
                self.document.blocks[*at..]
                    .iter()
                    .find_map(|original| edited.blocks.iter().position(|b| b.id == original.id))
                    .unwrap_or(edited.blocks.len())
            };
            systems.entry(position).or_default().push(message.clone());
        }
        let mut out = Vec::new();
        for (i, messages) in restored.into_iter().enumerate() {
            out.extend(systems.remove(&i).unwrap_or_default());
            out.extend(messages);
        }
        out.extend(systems.remove(&edited.blocks.len()).unwrap_or_default());
        validate(&out, true).map_err(|error| format!("restored projection: {error}"))?;
        Ok(out)
    }
}

/// Validate, never repair edited messages. Each call batch must be followed by
/// exactly its results; IDs cannot be reused anywhere in the conversation.
pub(crate) fn validate(messages: &[Value], systems: bool) -> R<()> {
    let mut pending = HashSet::new();
    let mut used = HashSet::new();
    for m in messages {
        let role = m["role"].as_str().ok_or("message role must be a string")?;
        if !m.is_object() {
            return Err("message must be an object".into());
        }
        if role != "tool" && !pending.is_empty() {
            return Err("tool calls lack contiguous results".into());
        }
        match role {
            "system" if systems => {}
            "user" | "assistant" | "tool" => {}
            _ => return Err("unsupported or protected role".into()),
        }
        match m.get("content") {
            Some(Value::String(_)) => {}
            Some(Value::Null) | None if role == "assistant" => {}
            Some(Value::Array(parts)) => {
                for part in parts {
                    match part["type"].as_str() {
                        Some("text") if part["text"].is_string() => {}
                        Some("image_url") if part["image_url"]["url"].is_string() => {}
                        Some("input_audio") if part["input_audio"].is_object() => {}
                        _ => return Err("invalid content part".into()),
                    }
                }
            }
            _ => return Err("invalid message content".into()),
        }
        if let Some(calls) = m.get("tool_calls") {
            if role != "assistant" {
                return Err("only assistant may call tools".into());
            }
            let calls = calls.as_array().ok_or("tool_calls must be an array")?;
            for call in calls {
                let id = call["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or("missing call id")?;
                if !used.insert(id.to_string()) {
                    return Err("duplicate tool call id".into());
                }
                if call["type"] != "function"
                    || call["function"]["name"].as_str().is_none_or(str::is_empty)
                {
                    return Err("invalid function call".into());
                }
                let args = call["function"]["arguments"]
                    .as_str()
                    .ok_or("arguments must be a JSON string")?;
                serde_json::from_str::<Value>(args)
                    .map_err(|_| "arguments must contain valid JSON")?;
                pending.insert(id.to_string());
            }
        }
        if role == "tool" {
            let id = m["tool_call_id"]
                .as_str()
                .ok_or("tool result lacks call id")?;
            if !pending.remove(id) {
                return Err("orphan or duplicate tool result".into());
            }
        } else if m.get("tool_call_id").is_some() {
            return Err("tool_call_id on non-tool message".into());
        }
    }
    if !pending.is_empty() {
        return Err("tool calls lack results".into());
    }
    Ok(())
}
