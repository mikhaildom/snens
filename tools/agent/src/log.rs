// Лог сессии: конец ребёнка, финальный текст и неблокирующий снимок состояния.
// Лог — интерфейс, поэтому здесь только чтение чужого jsonl, без внутренностей
// харнесса: `Done` пишет сам цикл на всех путях выхода.
use serde::Deserialize;

use crate::cfg::Cfg;
use crate::mail;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Rec {
    Meta {},
    Assistant {
        text: Option<String>,
        #[serde(default)]
        tool_calls: Vec<serde_json::Value>,
    },
    Done {
        status: String,
    },
    #[serde(other)]
    Other,
}

/// Снимок живого лога: что сессия делает сейчас. Строки, не разбирающиеся в `Rec`
/// (usage, steer и прочее), тоже нужны — из них берём след инструмента и время.
#[derive(Deserialize)]
pub struct LooseRec {
    #[serde(default)]
    pub ts: u64,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<serde_json::Value>,
    #[serde(default)]
    pub argv: Vec<String>,
}

/// Секунды от эпохи — общий штамп времени для ящика и логов.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn log_path(cfg: &Cfg, id: &str) -> std::path::PathBuf {
    cfg.sessions_dir.join(format!("{id}.jsonl"))
}

pub fn read_log(cfg: &Cfg, id: &str) -> Vec<Rec> {
    let Ok(text) = std::fs::read_to_string(log_path(cfg, id)) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Rec>(l).ok())
        .collect()
}

pub fn done_status(cfg: &Cfg, id: &str) -> Option<String> {
    read_log(cfg, id).into_iter().rev().find_map(|r| match r {
        Rec::Done { status } => Some(status),
        _ => None,
    })
}

pub fn final_text(cfg: &Cfg, id: &str) -> Option<String> {
    read_log(cfg, id).into_iter().rev().find_map(|r| match r {
        Rec::Assistant {
            text: Some(t),
            tool_calls,
            ..
        } if tool_calls.is_empty() && !t.trim().is_empty() => Some(t),
        _ => None,
    })
}

/// pid ребёнка, зарегистрированный его родителем.
pub fn child_pid(cfg: &Cfg, parent: &str, id: &str) -> Option<u32> {
    std::fs::read_to_string(
        mail::box_dir(&cfg.mail_dir, parent)
            .join("children")
            .join(id),
    )
    .ok()?
    .trim()
    .parse()
    .ok()
}

#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    unsafe { libc_kill(pid) }
}
#[cfg(not(unix))]
pub fn alive(_pid: u32) -> bool {
    true
}

#[cfg(unix)]
unsafe fn libc_kill(pid: u32) -> bool {
    // kill(pid, 0): сигнал 0 не шлётся, только проверка существования.
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid as i32, 0) == 0
}

pub fn state(cfg: &Cfg, id: &str) -> String {
    match done_status(cfg, id).as_deref() {
        None => {
            if is_alive(cfg, id) {
                "работает".into()
            } else {
                "убит".into()
            }
        }
        Some("ok") => "готов".into(),
        Some(s) => format!("неудача ({s})"),
    }
}

/// Живой ли процесс. pid регистрируется у родителя, который сессию запустил; для
/// вызовов из чужой сессии пробуем оба ящика — свой и родительский.
pub fn is_alive(cfg: &Cfg, id: &str) -> bool {
    child_pid(cfg, &cfg.parent_or_me(), id)
        .or_else(|| child_pid(cfg, &cfg.me, id))
        .or_else(|| child_pid(cfg, id, id))
        .map(alive)
        .unwrap_or_else(|| log_grows(cfg, id))
}

/// Запасной признак жизни, когда pid неизвестен: лог менялся последние 90 с.
/// Сессия может думать долго без записей — тогда ответ честно «не знаю».
fn log_grows(cfg: &Cfg, id: &str) -> bool {
    std::fs::metadata(log_path(cfg, id))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|e| e.as_secs() < 90)
        .unwrap_or(false)
}

/// Ходы и секунды с последней записи — то, что печатает дерево и прогресс wait.
pub fn stats(cfg: &Cfg, id: &str) -> (usize, u64) {
    let Ok(text) = std::fs::read_to_string(log_path(cfg, id)) else {
        return (0, 0);
    };
    let recs: Vec<LooseRec> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<LooseRec>(l).ok())
        .collect();
    let steps = recs.iter().filter(|r| r.kind == "assistant").count();
    let ago = recs
        .iter()
        .rev()
        .map(|r| r.ts)
        .find(|t| *t > 0)
        .map(|t| crate::log::now().saturating_sub(t))
        .unwrap_or(0);
    (steps, ago)
}

/// Неблокирующий снимок состояния подагента: то, что родитель раньше добывал
/// `agent list` + `ps aux`.
pub fn snapshot(cfg: &Cfg, id: &str) -> String {
    let Ok(text) = std::fs::read_to_string(log_path(cfg, id)) else {
        return format!("подагент {id}: {}\nлога нет", state(cfg, id));
    };
    let recs: Vec<LooseRec> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<LooseRec>(l).ok())
        .collect();
    let last = recs.iter().rev().find(|r| r.kind == "assistant");
    let last_tool = recs
        .iter()
        .rev()
        .find_map(|r| r.tool.clone().or_else(|| tool_name(r)))
        .unwrap_or_else(|| "—".into());
    let (steps, ago) = stats(cfg, id);
    let hint = last
        .and_then(|r| r.tool_calls.first())
        .map(crate::hint_of)
        .unwrap_or_default();
    let hint = if hint.is_empty() {
        last.and_then(|r| r.argv.first().cloned())
            .unwrap_or_default()
    } else {
        hint
    };
    let mut out = format!(
        "подагент {id}: {}\nходов: {steps}; последний инструмент: {last_tool}",
        state(cfg, id)
    );
    if !hint.is_empty() {
        let short: String = hint.chars().take(120).collect();
        out.push_str(&format!(" ({short})"));
    }
    out.push_str(&format!("\nзапись в логе: {ago} с назад"));
    out
}

fn tool_name(r: &LooseRec) -> Option<String> {
    let c = r.tool_calls.first()?;
    c.get("name")
        .and_then(|n| n.as_str())
        .map(str::to_string)
        .or_else(|| {
            c.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
}
