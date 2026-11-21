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
    /// Токены рассуждения по счётчику провайдера
    /// (`completion_tokens_details.reasoning_tokens`). У моделей, которые
    /// прячут рассуждение, это единственный след его настоящей цены: видимый
    /// текст и usage по выходу показывают только короткий ответ.
    pub reason_tok: u64,
    /// Провайдер отдал рассуждение зашифрованным (`reasoning.encrypted`):
    /// вместо текста пришёл блоб, который нам не прочитать. Видно только
    /// сводку (`reasoning.summary`), если она была. Флаг — чтобы не молчать
    /// там, где reasoning на самом деле был и стоил денег.
    pub encrypted: bool,
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

/// Провайдер отверг запрос по размеру тела (`413 Payload Too Large`, у Nous —
/// «Downloaded image content cannot exceed 30MB»). Это не сбой хода, а сигнал,
/// что вложений в проекции больше, чем провайдер готов принять: повтор без
/// картинок осмыслен, повтор с ними — нет.
pub fn is_payload_too_large(error: &(dyn std::error::Error + Send + Sync)) -> bool {
    let text = error.to_string();
    let text = text.as_str();
    // Мы сами пишем `HTTP 413`, чужие обёртки могут писать `status: 413`.
    let status = text.contains("HTTP 413") || text.contains(" 413:") || text.contains("status 413");
    status || text.contains("cannot exceed 30MB") || text.contains("Payload Too Large")
}

/// Ключ или провайдер не работает: отказ по ключу/оплате (401, 402, 403), лимит
/// (429), сбой на стороне провайдера (5xx, у Anthropic 529 «перегружен»), сеть
/// или молчащий стрим. Повтор тем же ключом тут не поможет, а запасной профиль —
/// может. 400 и 413 сюда не входят: это вопрос к запросу, а не к ключу, и у
/// другого провайдера он упал бы так же.
pub fn is_provider_down(error: &(dyn std::error::Error + Send + Sync)) -> bool {
    let text = error.to_string();
    let status = text
        .strip_prefix("HTTP ")
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse::<u16>().ok());
    match status {
        Some(code) => matches!(code, 401 | 402 | 403 | 429) || (500..600).contains(&code),
        None => {
            text.contains("error sending request")
                || text.contains("стрим молчит")
                || text.contains("стрим оборван после 0 символов")
        }
    }
}

// ---------- разбор SSE ----------

/// Накопитель кадров SSE. Транспорт (`bytes_stream`) режет поток где угодно — в том
/// числе посреди многобайтного символа, а кириллица в аргументах вызова (write/edit)
/// состоит из двухбайтных. Поэтому копим **байты** и декодируем только целый кадр:
/// граница чанка перестаёт быть границей символа.
#[derive(Default)]
struct SseFrames {
    buf: Vec<u8>,
}

impl SseFrames {
    fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Следующий целый кадр (до `\n` включительно). Декодируем **строго**: так
    /// битые байты видны как ошибка, а не превращаются в U+FFFD внутри аргумента.
    fn next(&mut self) -> Option<crate::R<String>> {
        let pos = self.buf.iter().position(|b| *b == b'\n')?;
        let raw: Vec<u8> = self.buf.drain(..=pos).collect();
        Some(match std::str::from_utf8(&raw) {
            Ok(s) => Ok(s.to_string()),
            Err(e) => Err(format!(
                "кадр стрима не UTF-8 ({e}) — аргументам вызова доверять нельзя"
            )
            .into()),
        })
    }

    /// Что осталось после конца потока. Непустой остаток — оборванный кадр:
    /// стрим закрылся посреди строки, и молча дочитывать его как аргумент нельзя.
    fn remainder(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let raw = std::mem::take(&mut self.buf);
        Some(String::from_utf8_lossy(&raw).into_owned())
    }
}

// ---------- подпись приложения ----------

/// Атрибуция приложения в OpenRouter: роутер группирует запросы по этим заголовкам,
/// и в аналитике/рейтингах мы видим «snens», а не безымянный источник.
///
/// `HTTP-Referer` — обязательный идентификатор (без него приложение не заводится),
/// `X-OpenRouter-Title` — отображаемое имя. Заголовки ставим только для роутера:
/// у строгих апстримов (Nous) лишний `Origin`/`Referer` — повод для 403.
///
/// `X-OpenRouter-Categories` — место в маркетплейсе: `personal-agent` (личный
/// агент). Список слияется с прежними категориями приложения, а не перезаписывает их.
const APP_REFERER: (&str, &str) = ("HTTP-Referer", "https://snens.svuvi.ch");
const APP_TITLE: (&str, &str) = ("X-OpenRouter-Title", "snens agent");
const APP_CATEGORIES: (&str, &str) = ("X-OpenRouter-Categories", "personal-agent");

/// Заголовки атрибуции для адреса провайдера. Пусто — не роутер: подписываться
/// нечем и незачем.
pub fn attribution_headers(base_url: &str) -> Vec<(String, String)> {
    let host = base_url.trim().to_ascii_lowercase();
    if !host.contains("openrouter") {
        return Vec::new();
    }
    vec![
        (APP_REFERER.0.to_string(), APP_REFERER.1.to_string()),
        (APP_TITLE.0.to_string(), APP_TITLE.1.to_string()),
        (APP_CATEGORIES.0.to_string(), APP_CATEGORIES.1.to_string()),
    ]
}

/// Anthropic (OpenAI-совместимый `api.anthropic.com/v1`) отвергает схему
/// параметров с `oneOf`/`anyOf`/`allOf` на верхнем уровне (HTTP 400 на весь
/// запрос). У `edit` и `desk` там лежат межполевые правила («либо old_string,
/// либо edits»); тулы проверяют их сами, модели это лишь подсказка. Поэтому
/// для этого провайдера правила срезаются, остальным схема уходит как есть.
fn needs_flat_tool_schema(base_url: &str) -> bool {
    base_url.trim().to_ascii_lowercase().contains("api.anthropic.com")
}

fn flatten_tool_schemas(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            let mut t = t.clone();
            if let Some(p) = t
                .get_mut("function")
                .and_then(|f| f.get_mut("parameters"))
                .and_then(Value::as_object_mut)
            {
                for k in ["oneOf", "anyOf", "allOf"] {
                    p.remove(k);
                }
            }
            t
        })
        .collect()
}

// ---------- openai-compatible ----------

pub struct OpenAi {
    client: reqwest::Client,
    key: String,
}

impl OpenAi {
    /// Провайдер без ключа: сервер поднимается, онбординг открывается, а первый
    /// ход честно отвечает «ключ не задан». Раньше `serve` требовал ключ на
    /// старте и падал — то есть приложение нельзя было даже открыть, чтобы его
    /// ввести (на этом падал десктопный запуск: ядро умирало, окно оставалось
    /// пустым).
    pub fn without_key() -> R<Self> {
        Self::new(String::new())
    }

    pub fn new(key: String) -> R<Self> {
        Ok(OpenAi {
            // Некоторые провайдеры (в т.ч. Nous Portal) отдают 403 без User-Agent.
            // Таймауты обязательны: залипший на середине стрим иначе висит вечно —
            // ход не закончится, `Done` не запишется, run останется running.
            client: reqwest::Client::builder()
                .user_agent(concat!("snens/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(std::time::Duration::from_secs(15))
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
        let flat;
        let tools = if needs_flat_tool_schema(&cfg.base_url) {
            flat = flatten_tool_schemas(tools);
            &flat[..]
        } else {
            tools
        };
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

        if cfg.base_url.trim().is_empty() {
            return Err("не задан base_url провайдера — укажи его в конфиге \
                        (например https://openrouter.ai/api/v1)"
                .into());
        }
        if cfg.model.trim().is_empty() {
            return Err("не задана модель — укажи model в конфиге или `snens model <slug>`".into());
        }
        let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
        // Ключ — по профилю этого запроса: поверхность живёт днями, а профиль
        // переключают на ходу (меню настроек, запасной профиль). Ключ снимка
        // старта — только если у профиля своего нет нигде.
        let key = cfg.key_value(&cfg.key_env).unwrap_or_else(|| self.key.clone());
        let mut req = self.client.post(&url).bearer_auth(&key).json(&body);
        // Подпись идут первыми: заголовки из конфига человека важнее дефолтов
        // (reqwest добавляет повторы, и побеждает последнее значение).
        for (k, v) in attribution_headers(&cfg.base_url) {
            req = req.header(k, v);
        }
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
        // Кадры SSE копим **байтами**: транспорт режет поток где угодно, в том числе
        // посреди многобайтного символа. Декодировать надо только целые строки.
        let mut buf = SseFrames::default();
        let mut text = String::new();
        // Рассуждение — отдельное поле стрима (`delta.reasoning`); держим его в
        // Reply и в логе, но провайдеру обратно не отдаём: контекст он не хранит.
        let mut reasoning = String::new();
        // index -> (id, name, arguments) — стрим приходит фрагментами
        let mut parts: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
        let mut usage = Usage::default();
        // Признак зашифрованного reasoning: сам блоб нам не прочитать, но его
        // наличие видно и объясняет, почему сводка такая короткая.
        let mut encrypted = false;
        // Отпечаток апстрима для диагностики кеша: id виден в каждом кадре,
        // fingerprint (vLLM) — только когда провайдер его пришлёт.
        let mut upstream = Upstream::default();
        // Последний непустой finish_reason: у OpenAI он приходит в кадре с
        // пустой delta, но иногда и раньше — держим последнее значение.
        let mut finish_reason = String::new();

        // Один кадр из стрима. Вынесено замыканием, чтобы одинаково обработать
        // и кадры с переводом строки, и последний кадр без него (провайдеры его
        // не всегда дописывают) — без этого хвост либо терялся, либо валил запрос.
        let frame = |data: &str,
                         text: &mut String,
                         reasoning: &mut String,
                         parts: &mut BTreeMap<usize, (String, String, String)>,
                         usage: &mut Usage,
                         encrypted: &mut bool,
                         upstream: &mut Upstream,
                         finish_reason: &mut String,
                         seen: &mut bool,
                         on_delta: &mut (dyn FnMut(Delta) + Send)|
         -> bool {
            let data = data.trim();
            if data == "[DONE]" {
                return true;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                return false;
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
                *usage = parse_usage(u);
            }
            if let Some(fr) = v.pointer("/choices/0/finish_reason").and_then(|x| x.as_str()) {
                if !fr.is_empty() {
                    *finish_reason = fr.to_string();
                }
            }
            let Some(d) = v.pointer("/choices/0/delta") else {
                return false;
            };
            if let Some(summary) = crate::reasoning::take_delta(d, encrypted) {
                reasoning.push_str(&summary);
                *seen = true;
                on_delta(Delta::Reasoning(&summary));
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
                    if let Some(a) = tc.pointer("/function/arguments").and_then(|a| a.as_str()) {
                        e.2.push_str(a);
                    }
                }
            }
            false
        };

        let mut stopped = false;
        while let Some(chunk) = tokio::time::timeout(
            std::time::Duration::from_secs(cfg.stream_idle_seconds),
            stream.next(),
        )
        .await
        .map_err(|_| {
            format!(
                "стрим молчит дольше {} с после {} символов",
                cfg.stream_idle_seconds,
                text.len() + parts.values().map(|p| p.2.len()).sum::<usize>()
            )
        })?
        {
            let chunk = chunk.map_err(|e| {
                format!(
                    "стрим оборван после {} символов: {e}",
                    text.len() + parts.values().map(|p| p.2.len()).sum::<usize>()
                )
            })?;
            buf.push(&chunk);
            while let Some(line) = buf.next() {
                let line = line?;
                let Some(data) = line.trim().strip_prefix("data:") else {
                    continue;
                };
                if frame(
                    data,
                    &mut text,
                    &mut reasoning,
                    &mut parts,
                    &mut usage,
                    &mut encrypted,
                    &mut upstream,
                    &mut finish_reason,
                    seen,
                    on_delta,
                ) {
                    stopped = true;
                    break;
                }
            }
            if stopped {
                break;
            }
        }

        // Хвост без перевода строки: у части провайдеров последний кадр так и
        // приходит. Целый разбираем, недописанный — это обрыв стрима, и
        // дочитывать его как аргумент нельзя: в файл уедет половина `content`.
        if let Some(rest) = buf.remainder() {
            let rest_trimmed = rest.trim();
            if !rest_trimmed.is_empty() {
                let Some(data) = rest_trimmed.strip_prefix("data:") else {
                    return Err(format!(
                        "стрим оборван: {} байт хвоста без перевода строки",
                        rest.len()
                    )
                    .into());
                };
                if serde_json::from_str::<Value>(data.trim()).is_err() {
                    return Err(format!(
                        "стрим оборван: последний кадр не разбирается ({} байт) — ответ неполный",
                        rest.len()
                    )
                    .into());
                }
                frame(
                    data,
                    &mut text,
                    &mut reasoning,
                    &mut parts,
                    &mut usage,
                    &mut encrypted,
                    &mut upstream,
                    &mut finish_reason,
                    seen,
                    on_delta,
                );
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
            usage: Usage { encrypted, ..usage },
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
        reason_tok: u
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        encrypted: false,
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

// ---------- тесты ----------

#[cfg(test)]
mod tests {
    use super::is_provider_down;
    #[test]
    fn provider_down_is_key_billing_limit_server_or_network() {
        let down = |t: &str| {
            let e: Box<dyn std::error::Error + Send + Sync> = t.into();
            is_provider_down(e.as_ref())
        };
        for t in [
            "HTTP 401 Unauthorized: invalid x-api-key",
            "HTTP 402 Payment Required: insufficient credits",
            "HTTP 403 Forbidden: ",
            "HTTP 429 Too Many Requests: ",
            "HTTP 529 <unknown status code>: overloaded",
            "HTTP 503 Service Unavailable: ",
            "error sending request for url (https://api.anthropic.com/v1/chat/completions)",
            "стрим молчит дольше 120 с после 0 символов",
        ] {
            assert!(down(t), "{t}");
        }
        for t in [
            "HTTP 400 Bad Request: bad tool schema",
            "HTTP 413 Payload Too Large: ",
            "cancelled",
            "не задана модель",
        ] {
            assert!(!down(t), "{t}");
        }
    }

    #[test]
    fn anthropic_gets_tool_schema_without_top_level_combinators() {
        use super::*;
        let tools = vec![json!({"type":"function","function":{"name":"edit","parameters":{
            "type":"object","properties":{"a":{"type":"string"}},
            "anyOf":[{"required":["a"]}],"allOf":[{}],"oneOf":[{}]}}})];
        assert!(needs_flat_tool_schema("https://api.anthropic.com/v1"));
        assert!(!needs_flat_tool_schema("https://openrouter.ai/api/v1"));
        let flat = flatten_tool_schemas(&tools);
        let p = &flat[0]["function"]["parameters"];
        assert!(p.get("anyOf").is_none() && p.get("allOf").is_none() && p.get("oneOf").is_none());
        assert_eq!(p["properties"]["a"]["type"], "string");
        // исходник не тронут: другим провайдерам схема уходит целиком
        assert!(tools[0]["function"]["parameters"].get("anyOf").is_some());
    }

    use super::*;

    /// Кадр стрима с аргументами вызова, как их присылает провайдер.
    fn frame(cyr: &str) -> String {
        let v = json!({"choices":[{"delta":{"tool_calls":[{"index":0,
            "function":{"name":"write","arguments":format!("{{\"content\":\"{cyr}\"}}")}}]}}]});
        format!("data: {v}\n\n")
    }

    /// Наблюдённый баг (тикет 2026-10-21-write-cyrillic-content-corrupt): транспорт
    /// режет поток посреди двухбайтного символа, lossy-декод чанка подменяет оба
    /// байта на U+FFFD — и битый аргумент молча уезжает в файл.
    #[test]
    fn chunk_split_inside_multibyte_char_keeps_content_intact() {
        let payload = "привет ёжик";
        let data = frame(payload).into_bytes();
        // Режем ровно между байтами «ё» (0xD1 0x91) — худший случай.
        let cut = data
            .windows(2)
            .position(|w| w == [0xD1, 0x91])
            .expect("в кадре нет «ё»")
            + 1;
        let mut f = SseFrames::default();
        f.push(&data[..cut]);
        f.push(&data[cut..]);

        let mut all = String::new();
        while let Some(line) = f.next() {
            all.push_str(&line.unwrap());
        }
        assert!(!all.contains('\u{fffd}'), "порча в кадре: {all}");
        assert!(all.contains(payload), "кириллица не дошла целиком: {all}");
    }

    /// Хвост без перевода строки не должен теряться молча: неполный кадр — это
    /// оборванный стрим, а не повод дописать его в аргументы как есть.
    #[test]
    fn partial_frame_is_not_emitted() {
        let mut f = SseFrames::default();
        f.push(b"data: {\"choices\":[");
        assert!(f.next().is_none(), "неполный кадр выдан за целый");
    }

    /// Подпись — только роутеру. У строгого апстрима лишний `Referer`/`Origin`
    /// может дать 403, поэтому проверяем и обратную сторону.
    #[test]
    fn attribution_only_for_openrouter() {
        let h = attribution_headers("https://openrouter.ai/api/v1");
        assert_eq!(
            h,
            vec![
                ("HTTP-Referer".to_string(), "https://snens.svuvi.ch".to_string()),
                ("X-OpenRouter-Title".to_string(), "snens agent".to_string()),
                (
                    "X-OpenRouter-Categories".to_string(),
                    "personal-agent".to_string()
                ),
            ]
        );
        assert!(attribution_headers("https://inference-api.nousresearch.com/v1").is_empty());
    }

    /// Заголовки реально уезжают в запрос, а правка человека их перебивает:
    /// reqwest допускает повторы, побеждать должен последний добавленный.
    #[tokio::test]
    async fn attribution_headers_reach_request_and_config_wins() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut b = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut b).unwrap();
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&b[..n]);
            }
            sock.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 14\r\n\r\ndata: [DONE]\n\n",
            )
            .unwrap();
            String::from_utf8_lossy(&head).to_ascii_lowercase()
        });

        // Подпись включается по подстроке base_url, а локальный сервер в его
        // адресе её не несёт — поэтому маркер живёт в пути, URL тот же.
        let mut cfg = crate::cfg::Config {
            base_url: format!("http://{addr}/openrouter.ai"),
            model: "test/model".into(),
            ..Default::default()
        };
        cfg.headers.insert("x-openrouter-title".into(), "мой snens".into());

        let llm = OpenAi::new("test-key".into()).unwrap();
        let mut noop = |_: Delta| {};
        llm.reply(&cfg, vec![], vec![], &mut noop).await.unwrap();
        let head = server.join().unwrap();

        assert!(
            head.contains("x-openrouter-categories: personal-agent"),
            "{head}"
        );
        assert!(head.contains("http-referer: https://snens.svuvi.ch"), "{head}");
        let titles = head.matches("x-openrouter-title:").count();
        assert_eq!(titles, 2, "подпись + правка человека: {head}");
        assert!(head.contains("x-openrouter-title: \u{43c}\u{43e}\u{439} snens"), "{head}");
    }

    /// Сквозная проверка production-пути: локальный HTTP-сервер отдаёт SSE, где
    /// кадр с кириллицей разрезан посреди двухбайтного символа — ровно так, как
    /// это делает транспорт. До фикса аргументы вызова приезжали с U+FFFD.
    #[tokio::test]
    async fn reply_keeps_cyrillic_across_split_chunks() {
        use std::io::{Read, Write};

        let payload = "// выпълнение: ёжик в тумане, объявление и счётчик";
        // Один кадр с полным аргументом — режется его уже транспорт (ниже, по 7
        // байт), как и в жизни: провайдер шлёт целые куски JSON, а чанки режет TCP.
        let arg = json!({ "content": payload }).to_string();
        let v = json!({"choices":[{"delta":{"tool_calls":[{"index":0,
            "function":{"name":"write","arguments":arg}}]}}]});
        let mut s = format!("data: {v}\n\n");
        s.push_str("data: [DONE]\n\n");
        let body = s.into_bytes();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut head = [0u8; 4096];
            let _ = sock.read(&mut head);
            let hdr = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
                body.len()
            );
            sock.write_all(hdr.as_bytes()).unwrap();
            // Режем по байту: границы чанков не совпадают с границами символов.
            for b in body.chunks(7) {
                sock.write_all(b).unwrap();
                sock.flush().unwrap();
            }
        });

        // Модель и провайдер больше не зашиты в дефолты — тест задаёт их сам.
        let cfg = crate::cfg::Config {
            base_url: format!("http://{addr}"),
            model: "test/model".into(),
            ..Default::default()
        };
        let llm = OpenAi::new("test-key".into()).unwrap();
        let mut noop = |_: Delta| {};
        let reply = llm
            .reply(&cfg, vec![], vec![], &mut noop)
            .await
            .expect("запрос к локальному серверу");

        let args = &reply.calls[0].args;
        assert!(!args.contains('\u{fffd}'), "порча в аргументе: {args}");
        let v: Value = serde_json::from_str(args).expect("аргумент не JSON");
        assert_eq!(v["content"].as_str().unwrap(), payload);
        server.join().unwrap();
    }
}
