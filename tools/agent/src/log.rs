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

/// Имя сессии приходит из argv и переменных окружения, а склеивается в путь:
/// `agent status ../../etc/passwd` читал файл за пределами каталога сессий, а
/// абсолютный id отбрасывал каталог целиком (`PathBuf::join`). Допускаем только
/// то, что порождает сам харнесс: `<секунды>-<pid>` и uuid.
pub fn valid_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 128 {
        return false;
    }
    if id.starts_with('.') || id.contains('/') || id.contains('\\') || id.contains(':') {
        return false;
    }
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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

/// Живой ли процесс. `EPERM` — это «жив, но чужой»: `kill` возвращает -1 и с
/// ним, а прежний `== 0` объявлял работающую сессию убитой. pid больше
/// `i32::MAX` в отрицательный не превращаем: отрицательный pid у `kill` значит
/// группу процессов, и проверка задела бы чужие.
#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // kill(pid, 0): сигнал 0 не шлётся, только проверка существования.
    let rc = unsafe {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        kill(pid, 0)
    };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc_eperm())
}

#[cfg(unix)]
fn libc_eperm() -> i32 {
    1
}

/// Не-unix: своего `kill` нет, поэтому честный ответ даёт свежесть лога, а не
/// вечное «работает» для давно завершённой сессии.
#[cfg(not(unix))]
pub fn alive(_pid: u32) -> bool {
    false
}

/// Состояние по журналу и pid.
///
/// «Убит» — утверждение, и его нельзя выдавать за незнание: `liveness` отвечает
/// «мёртв» и для мёртвого процесса, и для живого, который молчит дольше 90 с
/// при неизвестном pid. Поэтому третье состояние — «не знаю».
pub fn state(cfg: &Cfg, id: &str) -> String {
    match done_status(cfg, id).as_deref() {
        None => match liveness(cfg, id) {
            Live::Yes => "работает".into(),
            Live::No => "убит".into(),
            Live::Unknown => "работает (лог молчит, pid неизвестен)".into(),
        },
        Some("ok") => "готов".into(),
        Some(s) => format!("неудача ({s})"),
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Live {
    Yes,
    No,
    Unknown,
}

/// Живость с третьим ответом «не знаю»: pid нет, а лог молчит.
///
/// pid регистрируется у родителя, который сессию запустил; для вызовов из чужой
/// сессии пробуем оба ящика — свой и родительский.
pub fn liveness(cfg: &Cfg, id: &str) -> Live {
    let pid = child_pid(cfg, &cfg.parent_or_me(), id)
        .or_else(|| child_pid(cfg, &cfg.me, id))
        .or_else(|| child_pid(cfg, id, id));
    // Проверять pid умеет только unix; на других платформах ответ даёт лог.
    match (cfg!(unix).then_some(pid).flatten(), log_grows(cfg, id)) {
        (Some(pid), _) if alive(pid) => Live::Yes,
        (Some(_), _) => Live::No,
        (None, true) => Live::Yes,
        (None, false) => Live::Unknown,
    }
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
/// `None` во втором поле значит «время неизвестно»: раньше на этом месте
/// печаталось «0 с назад», то есть «только что», и отличить одно от другого
/// было нельзя.
pub fn stats_recs(recs: &[LooseRec], mtime_ago: Option<u64>) -> (usize, Option<u64>) {
    let steps = recs.iter().filter(|r| r.kind == "assistant").count();
    let ago = recs
        .iter()
        .rev()
        .map(|r| r.ts)
        .find(|t| *t > 0)
        .map(|t| crate::log::now().saturating_sub(t))
        .or(mtime_ago);
    (steps, ago)
}

pub fn stats(cfg: &Cfg, id: &str) -> (usize, Option<u64>) {
    let Ok(text) = std::fs::read_to_string(log_path(cfg, id)) else {
        return (0, None);
    };
    let recs: Vec<LooseRec> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<LooseRec>(l).ok())
        .collect();
    stats_recs(&recs, mtime_ago(&log_path(cfg, id)))
}

/// Секунды с изменения файла — запасной источник времени, когда в записях его нет.
fn mtime_ago(path: &std::path::Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|e| e.as_secs())
}

/// Неблокирующий снимок состояния подагента: то, что родитель раньше добывал
/// `agent list` + `ps aux`.
pub fn snapshot(cfg: &Cfg, id: &str) -> String {
    let path = log_path(cfg, id);
    let Ok(text) = std::fs::read_to_string(&path) else {
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
    // Считаем по уже прочитанным записям: `stats` и `state` читали файл заново,
    // то есть путь, обещанный неблокирующим, трижды парсил журнал целиком.
    let (steps, ago) = stats_recs(&recs, mtime_ago(&path));
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
    let state = match done_status(cfg, id).as_deref() {
        Some("ok") => "готов".to_string(),
        Some(s) => format!("неудача ({s})"),
        None => match liveness(cfg, id) {
            Live::Yes => "работает".to_string(),
            Live::No => "убит".to_string(),
            Live::Unknown => "работает (лог молчит, pid неизвестен)".to_string(),
        },
    };
    let mut out =
        format!("подагент {id}: {state}\nходов: {steps}; последний инструмент: {last_tool}");
    if !hint.is_empty() {
        let short: String = hint.chars().take(120).collect();
        out.push_str(&format!(" ({short})"));
    }
    match ago {
        Some(ago) => out.push_str(&format!("\nзапись в логе: {ago} с назад")),
        None => out.push_str("\nзапись в логе: время неизвестно"),
    }
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
