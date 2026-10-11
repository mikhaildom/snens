//! Editable JSON view. Systems are protected; non-text content parts are opaque references.
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

pub(crate) struct View {
    pub document: Document,
    attachments: BTreeMap<String, Value>,
    // Protected system messages precede the next original block, or the end.
    systems: Vec<(usize, Value)>,
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
        Ok(Self {
            document: Document { revision, blocks },
            attachments,
            systems,
        })
    }

    pub fn restore(&self, edited: &Document) -> R<Vec<Value>> {
        if edited.revision != self.document.revision {
            return Err("stale revision; reread LIVE_CONTEXT.json".into());
        }
        let originals: HashSet<&str> = self.document.blocks.iter().map(|b| b.id.as_str()).collect();
        // Жёсткий запрет на амнезию: запрос пользователя и текст ответа модели —
        // самая ценная часть контекста, и уборка их не трогает. Правкой зеркала
        // можно снять результаты инструментов и аргументы вызовов, но не ходы
        // диалога. Тела скиллов и длинные вставки — отдельный механизм (в файл).
        let protected = |messages: &[Value]| -> Vec<Value> {
            messages
                .iter()
                .filter(|m| {
                    m["role"] == "user"
                        || (m["role"] == "assistant"
                            && m["content"].as_str().is_some_and(|s| !s.is_empty()))
                })
                .cloned()
                .collect()
        };
        let mut ids = HashSet::new();
        let mut restored = Vec::new();
        for original in &self.document.blocks {
            match edited.blocks.iter().find(|b| b.id == original.id) {
                None if !protected(&original.messages).is_empty() => {
                    return Err(
                        "cannot delete blocks containing user messages or assistant text".into(),
                    )
                }
                Some(block) if protected(&block.messages) != protected(&original.messages) => {
                    return Err(
                        "cannot edit or reorder user messages and assistant text; clear tool results instead"
                            .into(),
                    )
                }
                _ => {}
            }
        }
        for block in &edited.blocks {
            if !ids.insert(block.id.as_str()) {
                return Err("duplicate block id".into());
            }
            if !originals.contains(block.id.as_str()) && !block.id.starts_with("new-") {
                return Err("unknown block id (new notes require new- prefix)".into());
            }
            if block.messages.is_empty() {
                return Err("empty block; delete it instead".into());
            }
            let mut messages = block.messages.clone();
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
            validate(&messages, false)?;
            if block.id.starts_with("new-") && messages.iter().any(|m| m["role"] != "user") {
                return Err("new blocks must contain user notes".into());
            }
            restored.push(messages);
        }
        let mut protected: BTreeMap<usize, Vec<Value>> = BTreeMap::new();
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
            protected.entry(position).or_default().push(message.clone());
        }
        let mut out = Vec::new();
        for (i, messages) in restored.into_iter().enumerate() {
            out.extend(protected.remove(&i).unwrap_or_default());
            out.extend(messages);
        }
        out.extend(protected.remove(&edited.blocks.len()).unwrap_or_default());
        validate(&out, true)?;
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
