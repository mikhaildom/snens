// Поток событий: SSE одной сессии, приём событий подагента, история и список.
//
// Транскрипт и список карточек живут двумя потоками: событие транскрипта несёт
// `session_id`, а изменение списка меняет сам набор сессий. Клиент не опрашивает
// историю — она нужна только на входе в сессию и после обрыва.

use crate::surface_runtime::{self as rt, Event, Runtime};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse,
    },
    Json,
};
use futures_util::{stream, StreamExt};
use serde::Deserialize;
use std::{collections::VecDeque, sync::Arc, time::Duration};

use super::discovery;
use super::history;
use super::http::err;

#[derive(Deserialize)]
pub(super) struct After {
    after: Option<u64>,
}

/// Что ещё вернуть вместе с транскриптом. `recall` — справку долгой памяти:
/// человек включает её показ галочкой в настройках клиента, по умолчанию она
/// остаётся служебным контекстом промпта и в транскрипт не едет.
#[derive(Deserialize)]
pub(super) struct HistoryQuery {
    /// Строкой, а не `bool`: в адресе пишут и `recall=1`, и `recall=true`, а
    /// serde ждёт от bool ровно `true`/`false` и отвечает 400 на `1`.
    #[serde(default)]
    recall: Option<String>,
}

impl HistoryQuery {
    fn with_recall(&self) -> bool {
        matches!(
            self.recall.as_deref(),
            Some("1") | Some("true") | Some("yes")
        )
    }
}
/// Событие от процесса-подагента. Тело — тот же набор полей, что присылает
/// `SurfaceSignal` в самой поверхности: `type` + поля.
#[derive(Deserialize)]
pub(super) struct IngestEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(flatten)]
    fields: serde_json::Value,
}

#[derive(Deserialize)]
pub(super) struct IngestBody {
    events: Vec<IngestEvent>,
}

/// Приём пачки событий подагента: эмитим их в шину под его же `session_id`.
///
/// Почему так: SSE-канал есть только у поверхности (`snens serve`), а подагент
/// живёт отдельным процессом `snens run-child` и своей шины не имеет. Раньше
/// клиент опрашивал его историю — дешевле и честнее гнать события push'ем.
pub(super) async fn ingest(
    State(s): State<Arc<Runtime>>,
    Path(id): Path<String>,
    Json(b): Json<IngestBody>,
) -> impl IntoResponse {
    if !rt::valid_id(&id) {
        return err("invalid", "invalid session id", StatusCode::BAD_REQUEST).into_response();
    }
    if b.events.is_empty() {
        // Пустая пачка — не «ничего не случилось», а просьба пересчитать
        // карточку: так подагент говорит «я закончил» после того, как `Done`
        // лёг в журнал. Без этого карточка висела «работает» до перезагрузки
        // окна: последняя пачка событий уходит раньше терминатора, а нового
        // повода пересчитать у поверхности нет.
        s.refresh_child(&id).await;
        return Json(serde_json::json!({"accepted":0})).into_response();
    }
    // Сессия подагента могла появиться после старта поверхности: её лог пишет
    // другой процесс, а discovery наполняет store только при старте и в
    // `GET /v1/sessions`. Поэтому подхватываем её из журнала на месте — иначе
    // первые события ребёнка уезжали в 404 и стрим начинался с середины.
    let known = s.data.lock().await.sessions.contains_key(&id);
    if !known {
        // Чтение журнала — блокирующее: не на воркере.
        let cfg = s.cfg.clone();
        let read_id = id.clone();
        let found = tokio::task::spawn_blocking(move || discovery::session_by_id(&cfg, &read_id))
            .await
            .ok()
            .flatten();
        match found {
            Some(session) => {
                // Проверка и вставка — в одном замке: иначе два параллельных
                // `ingest` оба видели `None` и второй перетирал слот журнальной
                // копией, сбрасывая `running`, который только что поставила
                // поверхность. Запись в базу — уже без замка.
                let saved = {
                    let mut d = s.data.lock().await;
                    if d.sessions.contains_key(&id) {
                        None
                    } else {
                        d.sessions.insert(id.clone(), session.clone());
                        d.events.entry(id.clone()).or_default();
                        Some(session)
                    }
                };
                if let Some(session) = saved {
                    let _ = s.save_session(&session);
                }
            }
            None => {
                return err("not_found", "unknown session", StatusCode::NOT_FOUND).into_response();
            }
        }
    }
    let mut accepted = 0u64;
    for e in b.events {
        // `run.started`/`run.finished` подагент не шлёт — состоянием процесса
        // владеет поверхность; остальное эмитим как есть. Поля события идут
        // плоско (`flatten`), поэтому одного `Value` достаточно.
        if s.emit(&id, e.run_id.clone(), &e.kind, e.fields)
            .await
            .is_ok()
        {
            accepted += 1;
        } else {
            // Событие осталось в памяти, но в базу и в поток не ушло: без
            // строки в stderr сбой хранилища неотличим от «событий не было».
            eprintln!(
                "[snens] ingest: событие {kind} сессии {id} не доставлено",
                kind = e.kind
            );
        }
    }
    // Карточка подагента: пересчитать по журналу и pid и опубликовать, если
    // есть что перерисовать. `emit` в цикле выше сам зовёт `refresh_child`, и
    // для обычной сессии это уже ничего не читает (признак — из слота store),
    // поэтому лишний вызов здесь не стоит. Нужен он для пустой пачки ниже и для
    // случаев, когда все события оказались дельтами без записи в историю.
    Json(serde_json::json!({"accepted":accepted})).into_response()
}

pub(super) async fn history(
    State(s): State<Arc<Runtime>>,
    Path(id): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> impl IntoResponse {
    if !rt::valid_id(&id) {
        return err("invalid", "invalid session id", StatusCode::BAD_REQUEST).into_response();
    }
    let Some(path) = discovery::path(&s.cfg, &id) else {
        // См. `get_session`: id проверен выше, но паника тут не нужна.
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    };
    // Снимок событий и курсор — в одном критическом сечении: `emit` двигает
    // `event_seq` под замком `data`, поэтому беря оба здесь в том же порядке
    // (data → event_seq) получаем согласованную пару. Иначе событие, пришедшее
    // за время разбора журнала, попадало в `cursor`, но не в `events`, и клиент,
    // переподключившийся с этим курсором, терял его навсегда.
    let (known, events, cursor) = {
        let d = s.data.lock().await;
        let events = d.events.get(&id).cloned().unwrap_or_default();
        let cursor = {
            let sequences = s.event_seq.lock().await;
            sequences
                .get(&id)
                .copied()
                .unwrap_or_else(|| events.iter().map(|event| event.seq).max().unwrap_or(0))
        };
        (d.sessions.contains_key(&id), events, cursor)
    };
    if !known && !path.is_file() {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    }
    let with_recall = q.with_recall();
    let cfg = s.cfg.clone();
    let read_path = path.clone();
    let messages = match tokio::task::spawn_blocking(move || {
        history::read_with(&read_path, &cfg, with_recall)
    })
    .await
    {
        Ok(Ok(messages)) => messages,
        Ok(Err(e)) => {
            return err(
                "history_read",
                &e.to_string(),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
            .into_response();
        }
        Err(e) => {
            return err(
                "history_read",
                &e.to_string(),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
            .into_response();
        }
    };
    Json(serde_json::json!({"cursor":cursor,"events":events,"messages":messages})).into_response()
}
pub(super) async fn events(
    State(s): State<Arc<Runtime>>,
    Path(id): Path<String>,
    Query(q): Query<After>,
) -> impl IntoResponse {
    let after = q.after.unwrap_or(0);
    let Some((rx, initial)) = snapshot_and_subscribe(&s, &id, after).await else {
        return err("not_found", "session not found", StatusCode::NOT_FOUND).into_response();
    };
    let (initial, cursor) = initial;
    let live = session_event_stream(s, id, rx, cursor, initial)
        .map(|event| Ok::<_, std::convert::Infallible>(sse(event)));
    Sse::new(live)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

pub(super) async fn snapshot_and_subscribe(
    s: &Runtime,
    id: &str,
    after: u64,
) -> Option<(
    tokio::sync::broadcast::Receiver<Event>,
    (VecDeque<Event>, u64),
)> {
    let d = s.data.lock().await;
    if !d.sessions.contains_key(id) {
        return None;
    }
    let initial = d
        .events
        .get(id)
        .into_iter()
        .flatten()
        .filter(|event| event.seq > after)
        .cloned()
        .collect();
    // emit also needs data lock, so no publish can cross this boundary.
    let rx = s.tx.subscribe();
    Some((rx, (initial, after)))
}

pub(super) fn session_event_stream(
    s: Arc<Runtime>,
    id: String,
    rx: tokio::sync::broadcast::Receiver<Event>,
    cursor: u64,
    initial: VecDeque<Event>,
) -> impl futures_util::Stream<Item = Event> {
    stream::unfold(
        (s, id, rx, cursor, initial),
        |(s, id, mut rx, mut cursor, mut pending)| async move {
            loop {
                if let Some(event) = pending.pop_front() {
                    if event.session_id == id && event.seq > cursor {
                        cursor = event.seq;
                        return Some((event, (s, id, rx, cursor, pending)));
                    }
                    continue;
                }
                match rx.recv().await {
                    Ok(event) if event.session_id == id && event.seq > cursor => {
                        cursor = event.seq;
                        return Some((event, (s, id, rx, cursor, pending)));
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Broadcast is only a wake-up/low-latency path. Replay from the
                        // durable in-memory store to close every overflow gap.
                        let d = s.data.lock().await;
                        pending.extend(
                            d.events
                                .get(&id)
                                .into_iter()
                                .flatten()
                                .filter(|event| event.seq > cursor)
                                .cloned(),
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    )
}
pub(super) fn sse(e: Event) -> SseEvent {
    SseEvent::default()
        .id(e.seq.to_string())
        .json_data(e)
        .unwrap_or_else(|_| SseEvent::default())
}

/// `GET /v1/events` — поток изменений списка сессий.
///
/// Зачем отдельно от `/v1/sessions/{id}/events`: тот поток про транскрипт одной
/// сессии, а карточки подагентов в доке живут другим списком. Раньше клиент видел
/// только снимок на загрузке и работающий подагент висел «ждёт» — без опроса
/// списка этого не починить.
pub(super) async fn session_updates(State(s): State<Arc<Runtime>>) -> impl IntoResponse {
    // Подписка до отдачи заголовков: клиент, подключившийся посреди хода, не
    // должен пропустить ближайшее изменение.
    let rx = s.tx_updates.subscribe();
    let live = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(value) => {
                    let frame = SseEvent::default()
                        .json_data(value)
                        .unwrap_or_else(|_| SseEvent::default());
                    return Some((Ok::<_, std::convert::Infallible>(frame), rx));
                }
                // Медленный клиент теряет кадры, а не поток: полный список он
                // перечитывает на старте и может перечитать после обрыва.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(live)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}
