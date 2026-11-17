// Пробуждение сессии по входящему: ящик (итоги подагентов) и фоновые задания.
//
// Модель не ждёт длинную работу вызовом инструмента — она заканчивает ход и
// отпускает цикл. Ход возобновляется сам, когда работа закончилась: ящик и
// каталог заданий проверяются здесь, а будит ход та поверхность, которая на
// этот момент существует (TUI читает ввод, поверхность API — свой watcher).
use crate::cfg::Config;

/// Путь журнала сессии: он нужен заданиям и будильнику, а `log::Session`
/// открывать ради пути нельзя — это взяло бы писательский замок.
pub fn sess_path(cfg: &Config, id: &str) -> std::path::PathBuf {
    cfg.sessions_path().join(format!("{id}.jsonl"))
}

/// Что пришло в сессию, пока она спала.
#[derive(Debug, Default)]
pub struct Inbox {
    pub texts: Vec<String>,
}

impl Inbox {
    pub fn is_empty(&self) -> bool {
        self.texts.is_empty()
    }
    /// Один user-ход: тексты склеиваются, каждый со своим источником в скобках —
    /// модель должна видеть, что это не человек, а вернувшаяся работа.
    pub fn as_turn(&self) -> String {
        self.texts.join("\n\n")
    }
}

/// Забрать всё, что ждёт сессию: сообщения ящика (итоги подагентов, steer) и
/// завершившиеся фоновые задания. Забранное уезжает из очереди, поэтому
/// повторный вызов пуст — пробуждение ровно одно.
pub fn collect(cfg: &crate::cfg::Config, id: &str) -> Inbox {
    let mut inbox = Inbox::default();
    for msg in crate::mail::drain(&cfg.mail_path(), id) {
        if msg.text.trim().is_empty() {
            continue;
        }
        inbox
            .texts
            .push(format!("[{} от {}]\n{}", msg.kind, msg.from, msg.text));
    }
    inbox
        .texts
        .extend(crate::jobs::collect_finished(&sess_path(cfg, id)));
    inbox
}

/// Есть ли что забирать — без изъятия. Нужен там, где решение «будить или нет»
/// принимается до дорогого запуска хода.
pub fn has_pending(cfg: &crate::cfg::Config, id: &str) -> bool {
    crate::mail::has_pending(&cfg.mail_path(), id)
        || crate::jobs::has_finished(sess_path(cfg, id).as_path())
}

/// Кандидаты на пробуждение: сессии, известные поверхности, и те, у кого есть
/// каталог ящика. Писать в ящик умеет только тот, кто знает получателя, поэтому
/// одного чтения каталога достаточно и оно дешевле перебора журналов.
pub fn mailboxes(cfg: &Config, known: impl Iterator<Item = String>) -> Vec<String> {
    let mut ids: std::collections::BTreeSet<String> = known.collect();
    if let Ok(rd) = std::fs::read_dir(cfg.mail_path()) {
        for e in rd.flatten() {
            if let Some(name) = e.file_name().to_str() {
                if crate::surface_runtime::valid_id(name) {
                    ids.insert(name.to_string());
                }
            }
        }
    }
    ids.into_iter().collect()
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_joins_sources_so_the_model_sees_what_returned() {
        let inbox = Inbox {
            texts: vec![
                "[report от 179-1]\nготово".into(),
                "[задание 2 завершилось: exit 0, 1.5 с]\nвывод".into(),
            ],
        };
        let text = inbox.as_turn();
        assert!(text.contains("[report от 179-1]"), "{text}");
        assert!(text.contains("[задание 2"), "{text}");
        assert!(!inbox.is_empty());
    }

    #[test]
    fn empty_inbox_is_empty() {
        assert!(Inbox::default().is_empty());
    }

    #[test]
    fn job_dir_comes_from_session_path() {
        let cfg: Config = toml::from_str("sessions_dir = \"/tmp/sessions\"\n").unwrap();
        assert_eq!(
            crate::jobs::dir(&sess_path(&cfg, "1-2")),
            std::path::Path::new("/tmp/sessions/1-2.out/jobs")
        );
    }

    /// Главный контракт будильника: всё, что вернулось, попадает в текст хода с
    /// пометкой источника, и забирается ровно один раз.
    #[test]
    fn collect_takes_mail_and_finished_jobs_once() {
        let root = std::env::temp_dir().join(format!("snens-wake-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sessions")).unwrap();
        let cfg: Config = toml::from_str(&format!(
            "sessions_dir = \"{}/sessions\"\nmail_dir = \"{}/mail\"\n",
            root.display(),
            root.display()
        ))
        .unwrap();
        let id = "1700000000-111";
        // Итог подагента лежит в ящике.
        crate::mail::send(&cfg.mail_path(), id, "1700000000-222", "report", "база собрана")
            .unwrap();
        // И готовое фоновое задание — рядом с журналом сессии.
        let sess = sess_path(&cfg, id);
        let jobs = crate::jobs::dir(&sess);
        std::fs::create_dir_all(&jobs).unwrap();
        let log = jobs.join("7-1.log");
        std::fs::write(&log, "тесты прошли\n").unwrap();
        std::fs::write(
            jobs.join("7-1.json"),
            serde_json::to_vec(&crate::jobs::Job {
                id: "7-1".into(),
                command: "cargo test".into(),
                cwd: root.display().to_string(),
                timeout_s: 600,
                log: log.display().to_string(),
                pid: std::process::id(),
                started: 0,
            })
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            jobs.join("7-1.done"),
            serde_json::to_vec(&crate::jobs::Done {
                exit: 0,
                ms: 900,
                timed_out: false,
                bytes: 12,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(has_pending(&cfg, id));
        let first = collect(&cfg, id);
        let text = first.as_turn();
        assert!(text.contains("[report от 1700000000-222]"), "{text}");
        assert!(text.contains("база собрана"), "{text}");
        assert!(text.contains("[задание 7-1 завершилось"), "{text}");
        assert!(text.contains("тесты прошли"), "{text}");
        assert!(!has_pending(&cfg, id), "второй раз будить нечем");
        assert!(collect(&cfg, id).is_empty(), "выдано дважды");
        let _ = std::fs::remove_dir_all(&root);
    }
}
