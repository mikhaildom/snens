// Точка сборки поверхности: подготовка состояния и запуск сервера.
//
// Раньше здесь же лежали все обработчики (2850 строк), и в этом файле никто не
// видел ни контракта, ни границ подсистем. Швы проведены по подсистемам:
// маршруты — `surface_router`, сессии — `surface_sessions`, команды хода —
// `surface_commands`, файлы — `surface_files`, запуск хода — `surface_launch`,
// авторевью — `surface_review`, одобрения — `surface_approvals`, чтение —
// `surface_query`, поток событий — `surface_events`, настройки — `surface_settings`.
// Здесь остаётся ровно то, что не принадлежит ни одной из них: восстановление
// store при старте и корректная остановка.

use crate::cfg::Config;
use crate::surface_runtime::{self as rt, Runtime};
use fs2::FileExt;
use std::{net::SocketAddr, sync::Arc};

#[path = "surface_http.rs"]
mod http;
#[path = "surface_sessions.rs"]
mod sessions;
#[path = "surface_commands.rs"]
mod commands;
#[path = "surface_files.rs"]
mod files;
#[path = "surface_query.rs"]
mod query;
#[path = "surface_launch.rs"]
mod launch;
#[path = "surface_review.rs"]
mod review;
#[path = "surface_approvals.rs"]
mod approvals;
#[path = "surface_events.rs"]
mod events;
#[path = "surface_settings.rs"]
mod settings;
#[path = "surface_pairing.rs"]
mod pairing;
#[path = "surface_router.rs"]
mod router;
#[path = "surface_history.rs"]
pub(crate) mod history;
#[path = "surface_discovery.rs"]
pub mod discovery;

pub async fn serve(cfg: Config, addr: SocketAddr, token: String) -> crate::R<()> {
    // Адрес проверил вызывающий (`production::serve`): здесь политики нет, иначе
    // правило разъехалось бы по двум местам (см. src/remote.rs, docs/pairing.md).
    let key = cfg.api_key()?;
    let llm = Arc::new(crate::llm::OpenAi::new(key)?);
    let dir = cfg.data_path().join("surface-api");
    std::fs::create_dir_all(&dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("server.lock"))?;
    lock.try_lock_exclusive()
        .map_err(|e| format!("surface store already locked: {e}"))?;
    let (tx, _) = tokio::sync::broadcast::channel(1024);
    // Отдельный канал для списка сессий: событие транскрипта несёт `session_id`
    // одной сессии, а тут меняется сам список (подагент начал/закончил).
    let (tx_updates, _) = tokio::sync::broadcast::channel(256);
    let (mut store, db) = rt::open_store(&dir)?;
    for discovered in discovery::sessions(&cfg, &store)? {
        // Не `or_insert`: запись в store могла лечь до появления метки в журнале
        // (и до колонки `parent`), и тогда признак «это подагент» терялся —
        // карточка в доке не обновлялась вовсе. Метаданные журнала точнее,
        // поэтому обновляем их, а состояние процесса остаётся от поверхности.
        match store.sessions.get_mut(&discovered.id) {
            Some(slot) => {
                slot.parent = discovered.parent;
                slot.title = slot.title.take().or(discovered.title);
                slot.surface = discovered.surface;
            }
            None => {
                store.sessions.insert(discovered.id.clone(), discovered);
            }
        }
    }
    let mut discovered_ids = rt::event_sequences(&db)?;
    for (id, events) in &store.events {
        let high = events.iter().map(|event| event.seq).max().unwrap_or(0);
        discovered_ids
            .entry(id.clone())
            .and_modify(|seq| *seq = (*seq).max(high))
            .or_insert(high);
    }
    let had_running = store
        .sessions
        .values()
        .any(|s| s.state == "running" || s.state == "stopping");
    rt::interrupt_running(&mut store, &mut discovered_ids);
    if had_running {
        // Persist restart recovery before accepting requests.
        rt::persist_store(&db, &store)?;
        for (id, seq) in &discovered_ids {
            db.execute("INSERT INTO event_sequences(session_id,seq) VALUES(?1,?2) ON CONFLICT(session_id) DO UPDATE SET seq=excluded.seq", rusqlite::params![id, *seq as i64])?;
        }
    }
    // Реестр спаренных устройств читается один раз при старте: проверка токена
    // на каждом запросе идёт по памяти, а `DELETE /v1/clients/{id}` правит и
    // память, и файл.
    let registry = crate::clients::Registry::load(&cfg.data_path());
    let state = Arc::new(Runtime {
        data: tokio::sync::Mutex::new(store),
        db: std::sync::Mutex::new(db),
        tx,
        cfg,
        llm,
        locks: Default::default(),
        controls: Default::default(),
        pending_tools: Default::default(),
        writers: Default::default(),
        event_seq: tokio::sync::Mutex::new(discovered_ids),
        review_gen: Default::default(),
        approvals: Default::default(),
        pending: Default::default(),
        registry,
        tx_updates,
    });
    // Канал одобрений привилегированных действий: headless-сессия не может спросить
    // пароль, поэтому `sudo`-шим спрашивает согласие владельца кнопкой в чате.
    approvals::spawn_approval_server(state.clone());
    let app = router::app(state.clone(), token);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tokio::select! {
        r=axum::serve(listener,app)=>{r?;},
        _=async {
            #[cfg(unix)] {
                // Без обработчика SIGTERM сервис всё равно должен подниматься:
                // падать на старте из-за сигнала — хуже, чем остановиться только
                // по SIGINT (systemd дошлёт SIGKILL).
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(mut term) => tokio::select! { _ = tokio::signal::ctrl_c()=>{}, _ = term.recv()=>{} },
                    Err(_) => { let _ = tokio::signal::ctrl_c().await; }
                }
            }
            #[cfg(not(unix))] { let _ = tokio::signal::ctrl_c().await; }
        } => {}
    }
    shutdown(&state).await?;
    Ok(())
}
async fn shutdown(s: &Arc<Runtime>) -> crate::R<()> {
    {
        let controls = s.controls.lock().await;
        for control in controls.values() {
            let _ = control.cancel.send(true);
        }
    }
    let mut d = s.data.lock().await;
    let mut sequences = s.event_seq.lock().await;
    rt::interrupt_running(&mut d, &mut sequences);
    // `db` — обычный `std::sync::Mutex`, и это осознанно: критическая секция —
    // синхронные вызовы rusqlite, ни одного `await` под замком нет, так что
    // async-воркер на ней не стоит. `unwrap` здесь оставлен намеренно: отравленный
    // замок значит, что в другом потоке паника уже случилась посреди записи в
    // store, и продолжать запись на диск — хуже, чем упасть.
    let db = s.db.lock().unwrap();
    for (id, seq) in sequences.iter() {
        db.execute("INSERT INTO event_sequences(session_id,seq) VALUES(?1,?2) ON CONFLICT(session_id) DO UPDATE SET seq=excluded.seq", rusqlite::params![id, *seq as i64])?;
    }
    rt::persist_store(&db, &d)
}

#[cfg(test)]
#[path = "surface_test_support.rs"]
mod test_support;
#[cfg(test)]
#[path = "surface_server_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "surface_commands_tests.rs"]
mod command_tests;
