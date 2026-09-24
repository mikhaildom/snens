// Подагенты: сессия = процесс, а надёжный признак жизни — запись `Done` в её
// логе. Состояние детей читается из логов, а не спрашивается у модели: модель
// может «забыть» отчитаться, лог — нет.
use std::path::{Path, PathBuf};

use crate::{cfg::Config, log, R};

/// Регистрация ребёнка: `<mail>/<parent>/children/<child>` = pid. Нужна ровно
/// чтобы отличить «процесс убит наглухо» (нет `Done`, а pid мёртв) от «ещё жив».
pub fn children_dir(mail_dir: &Path, parent: &str) -> PathBuf {
    mail_dir.join(parent).join("children")
}

pub fn register_child(mail_dir: &Path, parent: &str, child: &str, pid: u32) -> R<()> {
    let dir = children_dir(mail_dir, parent);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(child), pid.to_string())?;
    Ok(())
}

/// Живой ли процесс: `kill(pid, 0)`. Не-unix — считаем живым (не гадаем).
pub fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, 0) == 0
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Running,
    /// Завершился нормально (`Done.status == ok`).
    Done,
    /// Завершился по лимиту ходов или с ошибкой.
    Failed(String),
    /// Пид мёртв, а `Done` так и не появился: процесс снесли (OOM, SIGKILL).
    Crashed,
}

#[derive(Clone, Debug)]
pub struct Child {
    pub id: String,
    pub state: State,
}

fn done_of(cfg: &Config, id: &str) -> Option<String> {
    log::read_file(&cfg.sessions_path().join(format!("{id}.jsonl")))
        .ok()?
        .into_iter()
        .rev()
        .find_map(|r| match r {
            log::Rec::Done { status, .. } => Some(status),
            _ => None,
        })
}

fn pid_of(mail_dir: &Path, parent: &str, child: &str) -> Option<u32> {
    std::fs::read_to_string(children_dir(mail_dir, parent).join(child))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Дети этой сессии и их состояние. Единственный источник правды — лог ребёнка
/// (`Done`), файл регистрации нужен только для случая «убили наглухо».
pub fn children(cfg: &Config, parent: &str) -> Vec<Child> {
    let mail_dir = cfg.mail_path();
    let mut out = Vec::new();
    for e in std::fs::read_dir(children_dir(&mail_dir, parent))
        .into_iter()
        .flatten()
        .flatten()
    {
        let Some(id) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let state = state_of(cfg, parent, &id);
        out.push(Child { id, state });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Финальный текст ребёнка: последний `Assistant` без вызовов. Его пишет сам
/// цикл — именно поэтому отчёт не зависит от того, позвал ли ребёнок инструмент.
pub fn final_text(cfg: &Config, id: &str) -> Option<String> {
    log::read_file(&cfg.sessions_path().join(format!("{id}.jsonl")))
        .ok()?
        .into_iter()
        .rev()
        .find_map(|r| match r {
            log::Rec::Assistant {
                text: Some(t),
                tool_calls,
                ..
            } if tool_calls.is_empty() && !t.trim().is_empty() => Some(t),
            _ => None,
        })
}

/// Человекочитаемое состояние для `agent list`.
pub fn describe(state: &State) -> &'static str {
    match state {
        State::Running => "работает",
        State::Done => "готов",
        State::Failed(_) => "неудача",
        State::Crashed => "убит",
    }
}

/// Состояние одного ребёнка (не по регистру всех, а по его логу/pid).
pub fn state_of(cfg: &Config, parent: &str, id: &str) -> State {
    match done_of(cfg, id) {
        Some(s) if s == "ok" => State::Done,
        Some(s) => State::Failed(s),
        None => {
            let live = pid_of(&cfg.mail_path(), parent, id)
                .map(alive)
                .unwrap_or(false);
            if live {
                State::Running
            } else {
                State::Crashed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Состояние читается из логов: `Done` в логе ребёнка важнее, чем pid.
    #[test]
    fn state_comes_from_done_record() {
        let dir = std::env::temp_dir().join(format!("snens-sub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        let cfg = Config {
            sessions_dir: dir.join("sessions").to_string_lossy().into_owned(),
            mail_dir: dir.join("mail").to_string_lossy().into_owned(),
            ..Config::default()
        };
        // Ребёнок, который уже закончил.
        let s = log::Session::open_child(&cfg.sessions_path(), "kid", "me").unwrap();
        s.append(log::Rec::Assistant {
            ts: 1,
            text: Some("готово".into()),
            tool_calls: vec![],
            reasoning: None,
        })
        .unwrap();
        s.append(log::Rec::Done {
            ts: 2,
            status: "ok".into(),
            turns: 1,
        })
        .unwrap();
        register_child(&cfg.mail_path(), "me", "kid", 999_999_999).unwrap();

        let kids = children(&cfg, "me");
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].state, State::Done, "Done должен победить pid");
        assert_eq!(final_text(&cfg, "kid").as_deref(), Some("готово"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
