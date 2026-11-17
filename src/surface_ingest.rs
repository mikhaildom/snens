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
    tx: Option<mpsc::Sender<Msg>>,
    /// Переполнение уже объявлено: иначе строка в stderr печаталась бы на
    /// каждую потерянную дельту и залила бы лог подагента.
    reported_overflow: Arc<std::sync::atomic::AtomicBool>,
}

/// Потолок очереди на отправку.
///
/// Дельта приходит на каждый токен ответа, а отправщик умеет ждать до 5 с на
/// запрос: неограниченный канал копил бы события без границы, если поверхность
/// тормозит или лежит. Телеметрия — не повод съесть память подагента, поэтому
/// канал ограничен, а переполнение теряет события (`try_send`).
const QUEUE: usize = 1024;

/// Что уезжает в отправщик: обычное событие или требование закрыть пачку.
/// Закрытие отдельным сообщением, а не просто «дропнуть канал»: подагент
/// обязан дождаться, пока кадр уйдёт, иначе процесс выйдет раньше запроса и
/// терминатор `Done` никто не увидит — ровно так карточка подагента висела
/// «работает» до перезагрузки окна.
enum Msg {
    Event(String, serde_json::Value),
    Flush(tokio::sync::oneshot::Sender<()>),
}

impl Reporter {
    /// Отправитель для сессии. `None` — поверхности нет, телеметрия выключена
    /// или это не подагент (у корневой сессии шина своя).
    pub fn new(session: &str, parent: &str) -> Self {
        if parent.is_empty() {
            return Self {
                tx: None,
                reported_overflow: Default::default(),
            };
        }
        let base = std::env::var("SNENS_SURFACE_URL").unwrap_or_else(|_| DEFAULT_BASE.to_string());
        let token = std::env::var("SNENS_SURFACE_TOKEN").unwrap_or_default();
        if token.is_empty() {
            return Self {
                tx: None,
                reported_overflow: Default::default(),
            };
        }
        // Адрес без хвостового `/` и только http(s): переменную выставляет
        // окружение (в т.ч. heredoc-скрипты), и `http://host:7420//v1/…` или
        // `ftp://…` уехали бы с bearer-токеном без предупреждения.
        let base = base.trim_end_matches('/');
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            eprintln!("[snens] SNENS_SURFACE_URL не http(s), телеметрия выключена: {base}");
            return Self {
                tx: None,
                reported_overflow: Default::default(),
            };
        }
        let (tx, mut rx) = mpsc::channel::<Msg>(QUEUE);
        let url = format!("{base}/v1/sessions/{session}/ingest");
        tokio::spawn(async move {
            let client = match reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
            {
                Ok(c) => Arc::new(c),
                Err(_) => return,
            };
            let mut batch: Vec<serde_json::Value> = Vec::new();
            // Отправка одной пачки. Пустая пачка — тоже отправка: это просьба
            // пересчитать карточку подагента (см. `ingest` на поверхности).
            async fn post(
                client: &reqwest::Client,
                url: &str,
                token: &str,
                batch: &mut Vec<serde_json::Value>,
            ) -> bool {
                let body = json!({"events": std::mem::take(batch)});
                match client.post(url).bearer_auth(token).json(&body).send().await {
                    Ok(response) => {
                        // 401 (чужой токен) и 404 (неизвестная сессия) полностью
                        // выключали стрим молча — без этой строки причину
                        // «карточка не двигается» искать нечем.
                        if !response.status().is_success() {
                            eprintln!(
                                "[snens] ingest: поверхность ответила {} — телеметрия подагента не доходит",
                                response.status()
                            );
                            return false;
                        }
                        true
                    }
                    // Поверхности нет (TUI, `snens run` без сервера): молчим,
                    // работа важнее телеметрии.
                    Err(_) => false,
                }
            }
            loop {
                // Ждём первое событие бесконечно (пока жив отправитель), дальше
                // добираем пачку коротким окном.
                let first = match rx.recv().await {
                    Some(Msg::Event(kind, fields)) => Msg::Event(kind, fields),
                    Some(Msg::Flush(ack)) => {
                        post(&client, &url, &token, &mut batch).await;
                        let _ = ack.send(());
                        continue;
                    }
                    None => break,
                };
                if let Msg::Event(kind, fields) = first {
                    batch.push(flat(&kind, fields));
                }
                let mut flush: Option<tokio::sync::oneshot::Sender<()>> = None;
                // Дедлайн — на ВСЮ пачку, а не на каждое событие: окно от
                // последнего события растягивало пачку на секунды при ровном
                // потоке (30 событий по 200 мс), хотя комментарий обещает
                // «больше этого в пачке не ждём».
                let deadline = tokio::time::Instant::now() + FLUSH_EVERY;
                while batch.len() < 32 {
                    match tokio::time::timeout_at(deadline, rx.recv()).await {
                        Ok(Some(Msg::Event(kind, fields))) => batch.push(flat(&kind, fields)),
                        Ok(Some(Msg::Flush(ack))) => {
                            flush = Some(ack);
                            break;
                        }
                        _ => break,
                    }
                }
                post(&client, &url, &token, &mut batch).await;
                if let Some(ack) = flush {
                    let _ = ack.send(());
                }
            }
        });
        Self {
            tx: Some(tx),
            reported_overflow: Default::default(),
        }
    }

    /// Сигнал агента → событие поверхности. Имена и поля совпадают с теми, что
    /// эмитит сама поверхность: разбор на клиенте один и тот же.
    pub fn send(&self, signal: SurfaceSignal) {
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        let (kind, fields) = match signal {
            SurfaceSignal::Delta { text, kind } => {
                ("assistant.delta", json!({"text":text,"kind":kind}))
            }
            SurfaceSignal::ToolStarted {
                name,
                call_id,
                args,
            } => (
                "tool.started",
                json!({"name":name,"call_id":call_id,"args":args}),
            ),
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
        // Очередь полная — событие теряем: телеметрия не стоит того, чтобы
        // растить память процесса (и это видно строкой в stderr).
        if let Err(_error) = tx.try_send(Msg::Event(kind.to_string(), fields)) {
            if !self
                .reported_overflow
                .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                eprintln!("[snens] ingest: очередь событий переполнена, дельты теряются");
            }
        }
    }

    /// Дождаться, пока последняя пачка уйдёт.
    ///
    /// Зовётся после цикла и после того, как в журнал лёг `Done`: пустая пачка
    /// заставляет поверхность пересчитать карточку, и та видит завершение.
    /// Без этого подтверждения процесс мог выйти раньше запроса, и карточка
    /// подагента висела «работает» до перезагрузки окна.
    pub async fn finish(&self) {
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        // Здесь именно `send().await`, а не `try_send`: подтверждение не должно
        // теряться на переполненной очереди, иначе `Done` так и не доедет.
        if tx.send(Msg::Flush(ack_tx)).await.is_err() {
            return;
        }
        // Потолок, чтобы подагент не висел на мёртвой поверхности.
        let _ = tokio::time::timeout(Duration::from_secs(10), ack_rx).await;
    }
}
