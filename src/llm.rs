// Провайдер за одним трейтом. Две реализации: сеть и скрипт для тестов.
// Без Fake цикл не отладить детерминированно, поэтому это не абстракция ради абстракции.
use std::{collections::BTreeMap, sync::Mutex};

use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::{cfg::Config, log::Call, reduce::truncate, R};

#[derive(Default, Clone, Copy, Debug)]
pub struct Usage {
    pub in_tok: u64,
    pub cached_tok: u64,
    pub out_tok: u64,
    pub cost_micro: u64,
}

pub struct Reply {
    pub text: String,
    /// Ход рассуждения модели (провайдер присылает его отдельным полем `reasoning`).
    /// В лог пишется как есть, но провайдеру обратно не отдаётся.
    pub reasoning: String,
    pub calls: Vec<Call>,
    pub usage: Usage,
    /// `finish_reason` последнего кадра стрима: `stop` (модель закончила),
    /// `tool_calls`, `length` (обрыв по лимиту токенов), `content_filter`. Пустая
    /// строка — провайдер не прислал. Нужен, чтобы «пусто из-за обрыва» отличать
    /// от «модель честно ответила» при разборе сессии.
    pub finish_reason: String,
    /// Идентификатор ответа (`gen-…`) и отпечаток инференс-бэкенда, если их
    /// прислал провайдер. Нужны для диагностики кеша: роутер (OpenRouter за
    /// Nous) может увести запрос к другому апстриму, и тогда промах кеша — не
    /// наша вина, а его. По смене `id`/`fingerprint` между ходами это видно.
    pub upstream: Upstream,
}

#[derive(Default, Clone, Debug)]
pub struct Upstream {
    pub id: String,
    pub fingerprint: String,
}

/// Что стрим отдаёт наружу: ответ (content) или рассуждение — разные роли для
/// рендера, поэтому одним колбэком с тегом, а не двумя: второй mutable-колбэк
/// заставил бы держать два заимствования рендера одновременно.
pub enum Delta<'a> {
    Content(&'a str),
    Reasoning(&'a str),
}

pub trait Llm {
    async fn reply(
        &self,
        cfg: &Config,
        msgs: Vec<Value>,
        tools: Vec<Value>,
        on_delta: &mut (dyn FnMut(Delta) + Send),
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
            // Некоторые провайдеры (в т.ч. Nous Portal) отдают 403 без User-Agent.
            client: reqwest::Client::builder()
                .user_agent(concat!("snens/", env!("CARGO_PKG_VERSION")))
                .build()?,
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
        on_delta: &mut (dyn FnMut(Delta) + Send),
    ) -> R<Reply> {
        // Оборванный стрим до первых данных — это транспорт (провайдер закрыл
        // соединение), и повтор стоит дёшево: наружу ещё ничего не ушло.
        // После первых данных повтор не делаем — он бы напечатал ответ дважды.
        let mut seen = false;
        let mut last: Option<Box<dyn std::error::Error + Send + Sync>> = None;
        for attempt in 1..=2 {
            match self
                .stream_once(cfg, &msgs, &tools, on_delta, &mut seen)
                .await
            {
                Ok(r) => return Ok(r),
                Err(e) => {
                    if seen {
                        return Err(e);
                    }
                    eprintln!("[snens] запрос сорвался до первых данных ({e}), повтор {attempt}");
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| "запрос не удался".into()))
    }
}

impl OpenAi {
    async fn stream_once(
        &self,
        cfg: &Config,
        msgs: &[Value],
        tools: &[Value],
        on_delta: &mut (dyn FnMut(Delta) + Send),
        seen: &mut bool,
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
        // Рассуждение — отдельное поле стрима (`delta.reasoning`); держим его в
        // Reply и в логе, но провайдеру обратно не отдаём: контекст он не хранит.
        let mut reasoning = String::new();
        // index -> (id, name, arguments) — стрим приходит фрагментами
        let mut parts: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
        let mut usage = Usage::default();
        // Отпечаток апстрима для диагностики кеша: id виден в каждом кадре,
        // fingerprint (vLLM) — только когда провайдер его пришлёт.
        let mut upstream = Upstream::default();
        // Последний непустой finish_reason: у OpenAI он приходит в кадре с
        // пустой delta, но иногда и раньше — держим последнее значение.
        let mut finish_reason = String::new();

        'outer: while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                format!(
                    "стрим оборван после {} символов: {e}",
                    text.len() + parts.values().map(|p| p.2.len()).sum::<usize>()
                )
            })?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
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
                if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                    if !id.is_empty() {
                        upstream.id = id.to_string();
                    }
                }
                if let Some(fp) = v.get("system_fingerprint").and_then(|x| x.as_str()) {
                    if !fp.is_empty() {
                        upstream.fingerprint = fp.to_string();
                    }
                }
                if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                    usage = parse_usage(u);
                }
                if let Some(fr) = v
                    .pointer("/choices/0/finish_reason")
                    .and_then(|x| x.as_str())
                {
                    if !fr.is_empty() {
                        finish_reason = fr.to_string();
                    }
                }
                let Some(d) = v.pointer("/choices/0/delta") else {
                    continue;
                };
                if let Some(r) = d.get("reasoning").and_then(|c| c.as_str()) {
                    if !r.is_empty() {
                        reasoning.push_str(r);
                        *seen = true;
                        on_delta(Delta::Reasoning(r));
                    }
                }
                if let Some(c) = d.get("content").and_then(|c| c.as_str()) {
                    if !c.is_empty() {
                        text.push_str(c);
                        *seen = true;
                        on_delta(Delta::Content(c));
                    }
                }
                if let Some(tcs) = d.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tcs {
                        *seen = true;
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
                        if let Some(a) = tc.pointer("/function/arguments").and_then(|a| a.as_str())
                        {
                            e.2.push_str(a);
                        }
                    }
                }
            }
        }

        let calls = parts
            .into_iter()
            .map(|(i, (id, name, args))| Call {
                id: if id.is_empty() {
                    format!("call_{i}")
                } else {
                    id
                },
                name,
                args: if args.is_empty() { "{}".into() } else { args },
            })
            .collect();

        Ok(Reply {
            text,
            reasoning,
            calls,
            usage,
            upstream,
            finish_reason,
        })
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
        on_delta: &mut (dyn FnMut(Delta) + Send),
    ) -> R<Reply> {
        let mut s = self
            .script
            .lock()
            .map_err(|_| "Fake: мьютекс отравлен".to_string())?;
        if s.is_empty() {
            return Err("Fake: скрипт исчерпан".into());
        }
        let r = s.remove(0);
        if !r.reasoning.is_empty() {
            on_delta(Delta::Reasoning(&r.reasoning));
        }
        if !r.text.is_empty() {
            on_delta(Delta::Content(&r.text));
        }
        Ok(r)
    }
}
