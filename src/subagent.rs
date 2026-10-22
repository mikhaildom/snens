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

/// Прочитать журнал ребёнка **один раз** и достать оба признака.
///
/// Раньше `done_of` и `started_of` открывали и разбирали один и тот же файл
/// каждый сам: на карточку подагента это два полных прохода, а третий рядом
/// делал `progress` ради счётчика ходов.
pub fn journal_signs(path: &Path) -> (Option<String>, Option<u32>) {
    let Ok(records) = log::read_file(path) else {
        return (None, None);
    };
    let mut done = None;
    let mut pid = None;
    for rec in &records {
        match rec {
            log::Rec::Done { status, .. } => done = Some(status.clone()),
            log::Rec::Started { pid: p, .. } => {
                pid.get_or_insert(*p);
            }
            _ => {}
        }
    }
    (done, pid)
}

/// Индекс детей в ящике: pid из регистрации при спавне. Нужен только когда в
/// журнале нет `Started` — то есть процесс умер, не успев записать первую строку
/// (или лог остался от версии до этой записи).
fn pid_of(mail_dir: &Path, parent: &str, child: &str) -> Option<u32> {
    std::fs::read_to_string(children_dir(mail_dir, parent).join(child))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Дети этой сессии и их состояние.
///
/// Источник списка — журналы: `Meta.parent` в логе ребёнка и есть его связь с
/// родителем. Ящик (`<mail>/<parent>/children/<child>` = pid) остаётся
/// **индексом**, а не правдой: он появляется только у детей, запущенных с
/// `--parent`, и легко расходится с журналом (подагент, поднятый вручную или
/// переехавший при resume, в индексе отсутствует). Поэтому оба источника
/// складываются, а состояние всё равно считается по журналу.
pub fn children(cfg: &Config, parent: &str) -> Vec<Child> {
    let mut ids: std::collections::BTreeSet<String> = Default::default();
    for e in std::fs::read_dir(children_dir(&cfg.mail_path(), parent))
        .into_iter()
        .flatten()
        .flatten()
    {
        if let Some(id) = e.file_name().to_str() {
            ids.insert(id.to_string());
        }
    }
    for id in log::list(&cfg.sessions_path()).unwrap_or_default() {
        if ids.contains(&id) {
            continue;
        }
        if meta_parent(cfg, &id).as_deref() == Some(parent) {
            ids.insert(id);
        }
    }
    ids.into_iter()
        .map(|id| Child {
            state: state_of(cfg, parent, &id),
            id,
        })
        .collect()
}

/// Родитель из `Meta` журнала. Читаем шапку, а не весь лог: у детей журналы
/// бывают длинными, а метка лежит второй строкой.
fn meta_parent(cfg: &Config, id: &str) -> Option<String> {
    log::read_head(&cfg.sessions_path().join(format!("{id}.jsonl")), 3)
        .ok()?
        .into_iter()
        .find_map(|r| match r {
            log::Rec::Meta { parent, .. } => Some(parent),
            _ => None,
        })
        .filter(|p| !p.trim().is_empty())
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

/// Состояние одного ребёнка: `Done` в журнале решает всё, а до него — жив ли
/// процесс, чей pid записан самим ребёнком (`Started`). Файл регистрации в ящике
/// читается только как запасной источник pid.
///
/// Раньше pid был единственным признаком жизни, и поверхность узнавала о ребёнке
/// третьей стороной. Теперь состояние подагента — факт его журнала, а ящик лишь
/// индекс.
pub fn state_of(cfg: &Config, parent: &str, id: &str) -> State {
    let path = cfg.sessions_path().join(format!("{id}.jsonl"));
    let (done, started) = journal_signs(&path);
    state_from_signs(cfg, parent, id, done, started)
}

/// То же по уже прочитанным признакам журнала: список сессий читает файл один
/// раз на все нужды (факты, ходы, состояние) и не должен повторять проход.
pub fn state_from_signs(
    cfg: &Config,
    parent: &str,
    id: &str,
    done: Option<String>,
    started: Option<u32>,
) -> State {
    let pid = started.or_else(|| pid_of(&cfg.mail_path(), parent, id));
    let live = pid.map(alive).unwrap_or(false);
    match done {
        Some(s) if s == "ok" => State::Done,
        // `Done` есть — процесс закрылся сам, даже если pid ещё не вышел.
        Some(s) => State::Failed(s),
        None if live => State::Running,
        // Процесса нет, а `Done` не записан: снесли (OOM, SIGKILL).
        None => State::Crashed,
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

    /// Живость подагента — из его же журнала (`Started`), а не из файла в ящике:
    /// процесс сам объявляет, что жив, и состояние не зависит от третьей стороны.
    #[test]
    fn state_comes_from_own_started_record() {
        let dir = std::env::temp_dir().join(format!("snens-sub-st-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        let cfg = Config {
            sessions_dir: dir.join("sessions").to_string_lossy().into_owned(),
            mail_dir: dir.join("mail").to_string_lossy().into_owned(),
            ..Config::default()
        };
        // Свой pid в `Started`, и никакой регистрации в ящике вовсе.
        let me = std::process::id();
        let s = log::Session::open_child(&cfg.sessions_path(), "kid", "me").unwrap();
        s.append(log::Rec::Started { ts: 1, pid: me }).unwrap();
        assert_eq!(state_of(&cfg, "me", "kid"), State::Running);

        // Запись соврала про pid — процесса нет: значит снесли наглухо.
        let s2 = log::Session::open_child(&cfg.sessions_path(), "dead", "me").unwrap();
        s2.append(log::Rec::Started {
            ts: 1,
            pid: 999_999_999,
        })
        .unwrap();
        assert_eq!(state_of(&cfg, "me", "dead"), State::Crashed);

        // Старый лог без `Started`: остаётся запасной путь через ящик.
        let s3 = log::Session::open_child(&cfg.sessions_path(), "old", "me").unwrap();
        s3.append(log::Rec::User {
            ts: 1,
            text: "задача".into(),
            source: "agent".into(),
            parts: vec![],
        })
        .unwrap();
        register_child(&cfg.mail_path(), "me", "old", me).unwrap();
        assert_eq!(state_of(&cfg, "me", "old"), State::Running);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
