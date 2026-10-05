//! Append-only context revisions plus an atomically refreshed editable mirror.
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::{json, Value};

use crate::{
    cfg::Config,
    context_document::{self, Document, View},
    log::{self, Rec, Session},
    project, R,
};

pub struct Mirror {
    path: PathBuf,
    view: Option<View>,
    anchor: usize,
    digest: String,
}

/// FNV-1a64 of serialized *raw records*, not provider messages. Covers all
/// records, including prior revisions. Format: `fnv64:0123456789abcdef`.
fn digest(raw: &[Rec]) -> R<String> {
    Ok(context_document::hash(&serde_json::to_vec(raw)?))
}

/// Effective unelided projection for every provider path, including resume.
/// Invalid revisions fail closed rather than silently resurrecting old context.
pub fn effective(raw: &[Rec], cfg: &Config) -> R<Vec<Value>> {
    for (index, rec) in raw.iter().enumerate().rev() {
        if let Rec::ContextRevision {
            anchor,
            digest: expected,
            messages,
            ..
        } = rec
        {
            if *anchor > index || digest(&raw[..*anchor])? != *expected {
                return Err("context revision anchor/digest mismatch".into());
            }
            context_document::validate(messages, true)?;
            return append_suffix(messages.clone(), raw, *anchor, cfg);
        }
    }
    Ok(project::raw_msgs(raw, cfg))
}

// Project the suffix WITH its raw prefix: preserves the original system,
// replay framing and tail repair. A non-projecting revision is ignored by
// raw_msgs; a temporary user marker gives the exact record boundary even when
// projection repaired incomplete calls. The marker is never persisted.
fn append_suffix(mut base: Vec<Value>, raw: &[Rec], anchor: usize, cfg: &Config) -> R<Vec<Value>> {
    if anchor == raw.len() {
        return Ok(base);
    }
    let marker = format!("__snens_context_boundary_{}__", digest(raw)?);
    let mut marked = raw[..anchor].to_vec();
    marked.push(Rec::Steer {
        ts: 0,
        text: marker.clone(),
        from: String::new(),
        via: String::new(),
    });
    marked.extend_from_slice(&raw[anchor..]);
    let projected = project::raw_msgs(&marked, cfg);
    let boundary = projected
        .iter()
        .position(|m| m == &json!({"role":"user", "content":marker}))
        .ok_or("projection lost context boundary")?;
    for message in projected.into_iter().skip(boundary + 1) {
        // A result completing a repaired raw prefix replaces its synthetic
        // answer in the effective prefix, instead of becoming an orphan.
        if message["role"] == "tool" {
            if let Some(position) = base
                .iter()
                .position(|m| m["role"] == "tool" && m["tool_call_id"] == message["tool_call_id"])
            {
                base[position] = message;
                continue;
            }
        }
        base.push(message);
    }
    context_document::validate(&base, true)?;
    Ok(base)
}

impl Mirror {
    pub fn open(sess: &Session, cfg: &Config) -> R<Self> {
        let mut mirror = Self {
            path: sess.path().with_extension("out").join("LIVE_CONTEXT.json"),
            view: None,
            anchor: 0,
            digest: String::new(),
        };
        // Existing drafts are not silently overwritten on resume. prepare
        // preserves them so apply can explicitly reject stale/malformed input.
        let raw = sess.read()?;
        mirror.install(&raw, cfg, true)?;
        Ok(mirror)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn prepare(&mut self, raw: &[Rec], cfg: &Config) -> R<Vec<Value>> {
        self.install(raw, cfg, true)
    }

    fn install(&mut self, raw: &[Rec], cfg: &Config, preserve_draft: bool) -> R<Vec<Value>> {
        let messages = effective(raw, cfg)?;
        let digest = digest(raw)?;
        let view = View::new(&messages, format!("{}:{digest}", raw.len()))?;
        let draft = if preserve_draft && self.path.exists() {
            let bytes = fs::read(&self.path)?;
            match self.view.as_ref() {
                Some(previous) => {
                    serde_json::from_slice::<Document>(&bytes).ok().as_ref()
                        != Some(&previous.document)
                }
                // An existing canonical mirror of this exact prefix is safe.
                None => !canonical_draft(&bytes, raw, cfg)?,
            }
        } else {
            false
        };
        // A multi-step edit may be followed by ctx commit on a later request.
        // Keep its original baseline; new raw messages are appended at commit.
        if draft && self.view.is_some() {
            return Ok(messages);
        }
        if !draft {
            atomic_write(&self.path, &serde_json::to_vec_pretty(&view.document)?)?;
        }
        self.anchor = raw.len();
        self.digest = digest;
        self.view = Some(view);
        Ok(messages)
    }

    /// Called only after all results of a turn. Rejections reset the mirror;
    /// persistence errors propagate and never activate an unpersisted revision.
    pub fn apply(&mut self, sess: &Session, cfg: &Config) -> R<Option<String>> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return self.reject(sess, cfg, "mirror file missing");
            }
            Err(error) => return Err(error.into()),
        };
        let view = self.view.as_ref().ok_or("prepare context before apply")?;
        let edited = match serde_json::from_slice::<Document>(&bytes) {
            Ok(document) => document,
            Err(error) => return self.reject(sess, cfg, &format!("malformed JSON: {error}")),
        };
        if edited == view.document {
            return Ok(None);
        }
        let current = sess.read()?;
        if self.anchor > current.len() || digest(&current[..self.anchor])? != self.digest {
            return self.reject(sess, cfg, "raw prefix changed");
        }
        let messages = match view.restore(&edited).and_then(|messages| {
            let messages = purge_message_calls(messages, &self.path);
            let mut filtered = current[..self.anchor].to_vec();
            filtered.extend(purge_context_calls(&current[self.anchor..], &self.path));
            append_suffix(messages, &filtered, self.anchor, cfg)
        }) {
            Ok(messages) => messages,
            Err(error) => return self.reject(sess, cfg, &error.to_string()),
        };
        // Anchor includes all current turn records. Their ordinary messages are
        // already in messages; editor service pairs have deliberately vanished.
        let before = project::estimate_tokens(&effective(&current, cfg)?);
        let after = project::estimate_tokens(&messages);
        let marker = format!("{}:{}", current.len(), digest(&current)?);
        sess.append(Rec::ContextRevision {
            ts: log::now(),
            anchor: current.len(),
            digest: digest(&current)?,
            messages,
        })?;
        self.install(&sess.read()?, cfg, false)?;
        Ok(Some(format!(
            "context applied: revision {marker}; tokens ~{before} -> ~{after}"
        )))
    }

    fn reject(&mut self, sess: &Session, cfg: &Config, reason: &str) -> R<Option<String>> {
        self.install(&sess.read()?, cfg, false)?;
        Ok(Some(format!("context rejected: {reason}")))
    }
}

// A clean mirror from an older prefix is refreshed on resume; an edited
// draft survives refresh and is then explicitly rejected as stale.
fn canonical_draft(bytes: &[u8], raw: &[Rec], cfg: &Config) -> R<bool> {
    let Ok(document) = serde_json::from_slice::<Document>(bytes) else {
        return Ok(false);
    };
    let Some((length, expected)) = document.revision.split_once(':') else {
        return Ok(false);
    };
    let Ok(length) = length.parse::<usize>() else {
        return Ok(false);
    };
    if length > raw.len() || digest(&raw[..length])? != expected {
        return Ok(false);
    }
    let view = View::new(&effective(&raw[..length], cfg)?, document.revision.clone())?;
    Ok(document == view.document)
}

fn service_call(name: &str, args: &Value, path: &Path) -> bool {
    let first = args["args"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_str);
    if name == "ctx" {
        return args.as_object().is_some_and(|o| o.is_empty())
            || args["action"] == "commit"
            || first == Some("commit");
    }
    matches!(name, "read" | "edit" | "write") && first.is_some_and(|p| Path::new(p) == path)
}

fn purge_message_calls(messages: Vec<Value>, path: &Path) -> Vec<Value> {
    let removed: std::collections::HashSet<String> = messages
        .iter()
        .filter_map(|m| m["tool_calls"].as_array())
        .flatten()
        .filter(|c| {
            serde_json::from_str::<Value>(c["function"]["arguments"].as_str().unwrap_or(""))
                .is_ok_and(|args| {
                    service_call(c["function"]["name"].as_str().unwrap_or(""), &args, path)
                })
        })
        .filter_map(|c| c["id"].as_str().map(str::to_owned))
        .collect();
    messages
        .into_iter()
        .filter_map(|mut m| {
            if m["role"] == "tool"
                && m["tool_call_id"]
                    .as_str()
                    .is_some_and(|id| removed.contains(id))
            {
                return None;
            }
            if let Some(calls) = m.get_mut("tool_calls").and_then(Value::as_array_mut) {
                calls.retain(|c| !c["id"].as_str().is_some_and(|id| removed.contains(id)));
                if calls.is_empty() {
                    m.as_object_mut().unwrap().remove("tool_calls");
                    if m["content"].is_null() || m["content"] == "" {
                        return None;
                    }
                }
            }
            Some(m)
        })
        .collect()
}

/// Remove ONLY exact builtin commit and filesystem-tool calls on this mirror.
/// Mixed batches keep other calls and assistant text; shell commands are opaque.
fn purge_context_calls(raw: &[Rec], path: &Path) -> Vec<Rec> {
    let mut removed = std::collections::HashSet::new();
    for rec in raw {
        if let Rec::Assistant { tool_calls, .. } = rec {
            for call in tool_calls {
                let args = serde_json::from_str::<Value>(&call.args).ok();
                if args
                    .as_ref()
                    .is_some_and(|args| service_call(&call.name, args, path))
                {
                    removed.insert(call.id.clone());
                }
            }
        }
    }
    raw.iter()
        .filter_map(|rec| match rec {
            Rec::ToolResult { call_id, .. } if removed.contains(call_id) => None,
            Rec::Assistant {
                ts,
                text,
                tool_calls,
                reasoning,
            } => {
                let calls = tool_calls
                    .iter()
                    .filter(|c| !removed.contains(&c.id))
                    .cloned()
                    .collect::<Vec<_>>();
                if calls.is_empty() && text.as_ref().is_none_or(String::is_empty) {
                    None
                } else {
                    Some(Rec::Assistant {
                        ts: *ts,
                        text: text.clone(),
                        tool_calls: calls,
                        reasoning: reasoning.clone(),
                    })
                }
            }
            _ => Some(rec.clone()),
        })
        .collect()
}

fn atomic_write(path: &Path, bytes: &[u8]) -> R<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().ok_or("context path has no parent")?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".LIVE_CONTEXT.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> R<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
