// Провайдер за одним трейтом. Две реализации: сеть и скрипт для тестов.
// Без Fake цикл не отладить детерминированно, поэтому это не абстракция ради абстракции.
use std::{collections::BTreeMap, sync::Mutex};

use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::{
    cfg::Config,
    exec::truncate,
    log::Call,
    R,
};

#[derive(Default, Clone, Copy, Debug)]
pub struct Usage {
    pub in_tok: u64,
    pub cached_tok: u64,
    pub out_tok: u64,
    pub cost_micro: u64,
}

pub struct Reply {
    pub text: String,
    pub calls: Vec<Call>,
    pub usage: Usage,
}

pub trait Llm {
    async fn reply(
        &self,
        cfg: &Config,
        msgs: Vec<Value>,
        tools: Vec<Value>,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> R<Reply>;
}

// ---------- openai-compatible ----------

pub struct OpenAi {
    client: reqwest::Client,
    key: String,
}

impl OpenAi {
    pub fn new(key: String) -> R<Self> {
        Ok(OpenAi {
            client: reqwest::Client::builder().build()?,
            key,
        })
    }
}

impl Llm for OpenAi {
    async fn reply(
        &self,
        cfg: &Config,
        msgs: Vec<Value>,
        tools: Vec<Value>,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> R<Reply> {
        let mut body = json!({
            "model": cfg.model,
            "messages": msgs,
            "tools": tools,
            "stream": true,
        });
        if cfg.stream_usage {
            body["stream_options"] = json!({"include_usage": true});
        }
        for (k, v) in &cfg.body_patch {
            body[k] = v.clone();
        }

        let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
        let mut req = self.client.post(&url).bearer_auth(&self.key).json(&body);
        for (k, v) in &cfg.headers {
            req = req.header(k, v);
        }

        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let t = resp.text().await.unwrap_or_default();
            return Err(format!("HTTP {status}: {}", truncate(&t, 2000)).into());
        }

        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut text = String::new();
        // index -> (id, name, arguments) — стрим приходит фрагментами
        let mut parts: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
        let mut usage = Usage::default();

        'outer: while let Some(chunk) = stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk?));
            while let Some(pos) = buf.find('\n') {
                let line: String = buf.drain(..=pos).collect();
                let line = line.trim();
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    break 'outer;
                }
                let Ok(v) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                    usage = parse_usage(u);
                }
                let Some(d) = v.pointer("/choices/0/delta") else {
                    continue;
                };
                if let Some(c) = d.get("content").and_then(|c| c.as_str()) {
                    if !c.is_empty() {
                        text.push_str(c);
                        on_delta(c);
                    }
                }
                if let Some(tcs) = d.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tcs {
                        let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                        let e = parts.entry(idx).or_default();
                        if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                            if !id.is_empty() {
                                e.0 = id.to_string();
                            }
                        }
                        if let Some(n) = tc.pointer("/function/name").and_then(|n| n.as_str()) {
                            e.1 = format!("{}{}", e.1, n);
                        }
                        if let Some(a) = tc.pointer("/function/arguments").and_then(|a| a.as_str()) {
                            e.2.push_str(a);
                        }
                    }
                }
            }
        }

        let calls = parts
            .into_iter()
            .map(|(i, (id, name, args))| Call {
                id: if id.is_empty() { format!("call_{i}") } else { id },
                name,
                args: if args.is_empty() { "{}".into() } else { args },
            })
            .collect();

        Ok(Reply { text, calls, usage })
    }
}

fn parse_usage(u: &Value) -> Usage {
    Usage {
        in_tok: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        out_tok: u
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cached_tok: u
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cost_micro: u
            .get("cost")
            .and_then(|v| v.as_f64())
            .map(|c| (c * 1_000_000.0) as u64)
            .unwrap_or(0),
    }
}

// ---------- скриптованный провайдер ----------

pub struct Fake {
    script: Mutex<Vec<Reply>>,
}

impl Fake {
    pub fn new(script: Vec<Reply>) -> Self {
        Fake {
            script: Mutex::new(script),
        }
    }
}

impl Llm for Fake {
    async fn reply(
        &self,
        _cfg: &Config,
        _msgs: Vec<Value>,
        _tools: Vec<Value>,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> R<Reply> {
        let mut s = self
            .script
            .lock()
            .map_err(|_| "Fake: мьютекс отравлен".to_string())?;
        if s.is_empty() {
            return Err("Fake: скрипт исчерпан".into());
        }
        let r = s.remove(0);
        if !r.text.is_empty() {
            on_delta(&r.text);
        }
        Ok(r)
    }
}