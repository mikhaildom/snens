// Будильник поверхности: пока сессия спит, в ящик может лечь итог подагента или
// фонового задания. Ход тогда запускается сам, тем же путём, что и слово
// человека — иначе результат лежал бы до следующего сообщения клиента.
//
// Нейтральная часть (что забирать из ящика и где каталог заданий) — в `wake.rs`:
// ею пользуется и TUI, у которого своего сервера нет.
use super::Runtime;
use std::sync::Arc;
use tokio::time::Duration;

/// Период опроса. Файловая система не даёт событий без платформенных API, а
/// ящик маленький: пара stat-ов раз в две секунды дешевле, чем inotify с его
/// граблями на сетевых ФС и в контейнерах.
const TICK: Duration = Duration::from_millis(2000);

/// Наблюдатель поверхности: будит простаивающие сессии, когда пришёл результат.
pub(super) fn spawn(s: Arc<Runtime>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(TICK).await;
            let live = s.cfg.live();
            // Будим только простаивающие: идущий ход сам прочитает ящик на
            // ближайшей границе, а второй параллельный ход — это гонка.
            let idle: Vec<String> = {
                let data = s.data.lock().await;
                data.sessions
                    .iter()
                    .filter(|(_, x)| x.state == "idle")
                    .map(|(id, _)| id.clone())
                    .collect()
            };
            for id in crate::wake::mailboxes(&live, idle.into_iter()) {
                if !crate::wake::has_pending(&live, &id) {
                    continue;
                }
                // Заслон от шторма. Будить сессию можно только в двух случаях:
                // в ящике есть слово человека или сессия молодая (её работа реально
                // ждёт ответа). Старая сессия со случайным report в ящике — не повод
                // запускать ход: раньше такие поднимались сотнями, переписывались
                // steer'ами и не давали друг другу закончить.
                if !crate::wake::may_wake(&live, &id) {
                    continue;
                }
                // Замок сессии: пока его держит кто-то другой, ход уже начался
                // или начинается — второй не нужен.
                let Some(guard) = s.session_lock(&id).await.try_lock_owned().ok() else {
                    continue;
                };
                wake(s.clone(), &live, &id).await;
                drop(guard);
            }
        }
    })
}

/// Разбудить сессию одним ходом с тем, что накопилось. Если будить нечего или
/// сессия занята, вход остаётся в ящике и следующий тик попробует снова.
async fn wake(s: Arc<Runtime>, cfg: &crate::cfg::Config, id: &str) {
    let (busy, surface) = {
        let d = s.data.lock().await;
        match d.sessions.get(id) {
            Some(x) => (
                x.state == "running" || x.state == "stopping",
                x.surface.clone(),
            ),
            None => (false, String::new()),
        }
    };
    if busy {
        return;
    }
    let surface = if surface.is_empty() {
        // Сессии нет в store, а ящик есть: поверхность поднимает её из журнала
        // так же, как это делает первое сообщение от клиента.
        match super::discovery::path(&s.cfg, id) {
            Some(path) if path.is_file() => super::discovery::surface(&path),
            _ => return,
        }
    } else {
        surface
    };
    // Писательский замок журнала: без него `launch` откажется начинать ход.
    if super::commands::ensure_writer(&s, id).await.is_err() {
        return;
    }
    let inbox = crate::wake::collect(cfg, id);
    if inbox.is_empty() {
        return;
    }
    super::launch::launch(
        s,
        id.to_string(),
        uuid::Uuid::new_v4().to_string(),
        // Источник — `wake`: это вернувшаяся работа, а не слово человека. Раньше
        // здесь уезжал `surface`, и итог фонового задания рисовался в ленте
        // полноценной репликой человека.
        super::launch::Entry::wake(inbox.as_turn()),
        surface,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface_runtime::Session;
    use tokio::sync::{broadcast, Mutex};

    /// Runtime с настоящим store и своим каталогом данных: будильник работает по
    /// файлам, поэтому подменять их нечем — только временный каталог.
    fn runtime(root: &std::path::Path) -> Arc<Runtime> {
        let cfg: crate::cfg::Config = toml::from_str(&format!(
            "sessions_dir = \"{}/sessions\"\nmail_dir = \"{}/mail\"\n",
            root.display(),
            root.display()
        ))
        .unwrap();
        std::fs::create_dir_all(cfg.sessions_path()).unwrap();
        std::fs::create_dir_all(cfg.mail_path()).unwrap();
        let (mut store, db) = crate::surface_runtime::open_store(&root.join("surface-api")).unwrap();
        store.sessions.insert(
            "1700000000-111".into(),
            Session::new("1700000000-111", "web"),
        );
        crate::surface_runtime::persist_store(&db, &store).unwrap();
        let (tx, _) = broadcast::channel(64);
        let (tx_updates, _) = broadcast::channel(64);
        Arc::new(Runtime {
            data: Mutex::new(store),
            db: std::sync::Mutex::new(db),
            tx,
            cfg,
            // Ключа нет намеренно: ход упадёт на провайдере, но проверяется не
            // он, а то, что сессию разбудили и ящик забрали.
            llm: Arc::new(crate::llm::OpenAi::without_key().unwrap()),
            locks: Default::default(),
            controls: Default::default(),
            pending_tools: Default::default(),
            writers: Default::default(),
            event_seq: Default::default(),
            review_gen: Default::default(),
            approvals: Default::default(),
            pending: Default::default(),
            registry: Default::default(),
            tx_updates,
        })
    }

    /// Главный контракт: итог, лежащий в ящике, поднимает ход спящей сессии без
    /// чьего-либо сообщения — и забирается из ящика, чтобы не доехать дважды.
    #[tokio::test]
    async fn pending_mail_wakes_idle_session_once() {
        let root = std::env::temp_dir().join(format!("snens-wake-live-{}", uuid::Uuid::new_v4()));
        let s = runtime(&root);
        let id = "1700000000-111";
        // Журнал есть у любой реально существующей сессии: по его mtime считается
        // возраст, а без журнала будильник не судит — нечего судить.
        std::fs::write(s.cfg.sessions_path().join(format!("{id}.jsonl")), "").unwrap();
        crate::mail::send(
            &s.cfg.mail_path(),
            id,
            "1700000000-222",
            "report",
            "миграции применены",
        )
        .unwrap();
        spawn(s.clone());
        let journal = s.cfg.sessions_path().join(format!("{id}.jsonl"));
        let mut found = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let text = std::fs::read_to_string(&journal).unwrap_or_default();
            if text.contains("миграции применены") {
                found = true;
                break;
            }
        }
        assert!(found, "сессия не разбужена: в журнале нет текста из ящика");
        assert!(
            !crate::mail::has_pending(&s.cfg.mail_path(), id),
            "ящик не забрали: сообщение доедет второй раз"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Заслон от шторма 17.11: старая сессия, которой сосед написал `steer`, не
    /// должна подниматься. Иначе круги «проснулся → ответил → разбудил соседа →
    /// получил ответ» не заканчиваются, а агенты из прошлых месяцев ведут
    /// переписку между собой.
    #[tokio::test]
    async fn old_session_is_not_woken_by_inner_mail() {
        let root = std::env::temp_dir().join(format!("snens-wake-storm-{}", uuid::Uuid::new_v4()));
        let s = runtime(&root);
        let id = "1700000000-111";
        let journal = s.cfg.sessions_path().join(format!("{id}.jsonl"));
        std::fs::write(&journal, "").unwrap();
        // Состарить журнал: сессия из прошлого месяца.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 24 * 3600);
        let f = std::fs::OpenOptions::new().write(true).open(&journal).unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        crate::mail::send(
            &s.cfg.mail_path(),
            id,
            "1700000000-222",
            "steer",
            "ответ соседа",
        )
        .unwrap();
        let before = std::fs::read_to_string(&journal).unwrap();
        spawn(s.clone());
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let after = std::fs::read_to_string(&journal).unwrap();
        assert_eq!(before, after, "старую сессию разбудили внутренней перепиской");
        assert!(
            crate::mail::has_pending(&s.cfg.mail_path(), id),
            "вход пропал из ящика, хотя будить было нельзя"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// А слово человека поднимает любую сессию — даже старую: это не шторм, это
    /// человек вернулся к своей задаче.
    #[tokio::test]
    async fn human_word_wakes_old_session() {
        let root = std::env::temp_dir().join(format!("snens-wake-human-{}", uuid::Uuid::new_v4()));
        let s = runtime(&root);
        let id = "1700000000-111";
        let journal = s.cfg.sessions_path().join(format!("{id}.jsonl"));
        std::fs::write(&journal, "").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(400 * 24 * 3600);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&journal)
            .unwrap()
            .set_modified(old)
            .unwrap();
        crate::mail::send(&s.cfg.mail_path(), id, "human", "steer", "вернись к задаче").unwrap();
        spawn(s.clone());
        let mut found = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if std::fs::read_to_string(&journal).unwrap_or_default().contains("вернись к задаче") {
                found = true;
                break;
            }
        }
        assert!(found, "слово человека не разбудило старую сессию");
        let _ = std::fs::remove_dir_all(&root);
    }
}
