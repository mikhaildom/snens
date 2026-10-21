//! Отправка событий из процесса подагента в поверхность.
//!
//! У SSE-шины ровно один владелец — процесс `snens serve`: он держит подписчиков
//! и историю событий. Подагент живёт отдельным процессом (`snens run-child`) и
//! своей шины не имеет — из-за этого его работа раньше не стримилась, клиент
//! видел только готовую историю после перечитывания журнала.
//!
//! Здесь обратная сторона `POST /v1/sessions/{id}/ingest`: сигналы агента
//! уезжают в поверхность пачками, та эмитит их в ту же шину под `session_id`
//! подагента. Клиент не знает, откуда пришло событие: формат один.
//! Ошибки сети подагента не роняют — работа важнее телеметрии; нет поверхности
//! (TUI, `snens run` без сервера) — отправка просто молчит.

use std::{sync::Arc, time::Duration};

use serde_json::json;
use tokio::sync::mpsc;

use crate::agent::SurfaceSignal;

/// Адрес поверхности по умолчанию: та же loopback-точка, что у веб-клиента.
const DEFAULT_BASE: &str = "http://127.0.0.1:7420";

/// Больше этого в пачке не ждём: дельт приходит много, и запрос на каждую —
/// лишний сокет на каждый токен ответа.
const FLUSH_EVERY: Duration = Duration::from_millis(250);

/// Событие плоским объектом: `type` рядом с полями, без вложенного `fields`.
/// Поверхность разворачивает его в `Event` через `#[serde(flatten)]` — так же,
/// как это делает сама с сигналами агента. Вложенность разваливала разбор на
/// клиенте: он ждал `{"type":"usage","in_tok":…}`, а получал `fields:{…}`.
fn flat(kind: &str, fields: serde_json::Value) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("type".into(), serde_json::Value::String(kind.into()));
    if let serde_json::Value::Object(inner) = fields {
        for (k, v) in inner {
            map.insert(k, v);
        }
    }
    serde_json::Value::Object(map)
}

#[derive(Clone)]
pub struct Reporter {
    tx: Option<mpsc::UnboundedSender<(String, serde_json::Value)>>,
}

impl Reporter {
    /// Отправитель для сессии. `None` — поверхности нет, телеметрия выключена
    /// или это не подагент (у корневой сессии шина своя).
    pub fn new(session: &str, parent: &str) -> Self {
        if parent.is_empty() {
            return Self { tx: None };
        }
        let base = std::env::var("SNENS_SURFACE_URL").unwrap_or_else(|_| DEFAULT_BASE.to_string());
        let token = std::env::var("SNENS_SURFACE_TOKEN").unwrap_or_default();
        if token.is_empty() {
            return Self { tx: None };
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, serde_json::Value)>();
        let url = format!("{base}/v1/sessions/{session}/ingest");
        tokio::spawn(async move {
            let client = match reqwest::Client::builder().timeout(Duration::from_secs(5)).build() {
                Ok(c) => Arc::new(c),
                Err(_) => return,
            };
            let mut batch: Vec<serde_json::Value> = Vec::new();
            loop {
                // Ждём первое событие бесконечно (пока жив отправитель), дальше
                // добираем пачку коротким окном.
                let Some((kind, fields)) = rx.recv().await else {
                    break;
                };
                batch.push(flat(&kind, fields));
                while batch.len() < 32 {
                    match tokio::time::timeout(FLUSH_EVERY, rx.recv()).await {
                        Ok(Some((kind, fields))) => batch.push(flat(&kind, fields)),
                        _ => break,
                    }
                }
                let body = json!({"events": std::mem::take(&mut batch)});
                // Ответ не проверяем: поверхность могла закрыться, а подагент
                // обязан доработать. Следующая пачка попробует снова.
                let _ = client
                    .post(&url)
                    .bearer_auth(&token)
                    .json(&body)
                    .send()
                    .await;
            }
        });
        Self { tx: Some(tx) }
    }

    /// Сигнал агента → событие поверхности. Имена и поля совпадают с теми, что
    /// эмитит сама поверхность: разбор на клиенте один и тот же.
    pub fn send(&self, signal: SurfaceSignal) {
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        let (kind, fields) = match signal {
            SurfaceSignal::Delta { text, kind } => ("assistant.delta", json!({"text":text,"kind":kind})),
            SurfaceSignal::ToolStarted { name, call_id, args } => {
                ("tool.started", json!({"name":name,"call_id":call_id,"args":args}))
            }
            SurfaceSignal::ToolCompleted {
                name,
                call_id,
                exit,
                preview,
                parts,
                ms,
            } => (
                "tool.completed",
                json!({"name":name,"call_id":call_id,"exit":exit,"preview":preview,"parts":parts,"ms":ms}),
            ),
            SurfaceSignal::Usage {
                in_tok,
                cached_tok,
                out_tok,
                reason_tok,
                cost_micro,
            } => (
                "usage",
                json!({"in_tok":in_tok,"cached_tok":cached_tok,"out_tok":out_tok,"reason_tok":reason_tok,"cost_micro":cost_micro}),
            ),
            SurfaceSignal::Notice(text) => ("notice", json!({"text":text})),
            SurfaceSignal::ContextPressure { approx, budget } => {
                ("context.pressure", json!({"approx":approx,"budget":budget}))
            }
        };
        let _ = tx.send((kind.to_string(), fields));
    }
}
