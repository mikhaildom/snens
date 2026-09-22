// Ревью: не свежая сессия, а дубликат лога. Префикс совпадает до байта, поэтому
// провайдерский кеш тёплый и ревью стартует, уже зная всё, что знала сессия.
use std::{collections::BTreeMap, path::Path};

use crate::{agent, cfg::Config, exec, llm, log, registry, R};

const PROMPT: &str = include_str!("../prompts/review.md");

pub async fn cmd(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = Config::load(flags.get("config").map(|s| s.as_str()))?;
    let id = crate::session_id(&cfg, flags)?;
    let llm = llm::OpenAi::new(cfg.api_key()?)?;
    run(&cfg, &id, &llm).await
}

pub async fn run(cfg: &Config, id: &str, llm: &llm::OpenAi) -> R<()> {
    let dir = cfg.sessions_path();
    let src = dir.join(format!("{id}.jsonl"));
    if !src.is_file() {
        return Err(format!("сессии {id} нет: {}", src.display()).into());
    }
    let rid = format!("{id}-review");
    let dst = dir.join(format!("{rid}.jsonl"));
    // Дампы обрезанных выводов не копируем: в логе лежат абсолютные пути к
    // <id>.out/, они живы и после копии.
    if dst.exists() {
        eprintln!("[snens] {rid}: лог перезаписан, прошлый прогон ревью этой сессии потерян");
    }
    std::fs::copy(&src, &dst)?;

    let audit = audit_report(cfg, &src).await?;
    let tickets = ticket_list(&cfg.tickets_path());
    let template = match &cfg.review_prompt {
        Some(p) => std::fs::read_to_string(p)
            .map_err(|e| format!("review_prompt {p}: {e}"))?,
        None => PROMPT.to_string(),
    };
    let text = render_prompt(
        &template,
        id,
        &audit,
        &tickets,
        &cfg.tickets_path().display().to_string(),
        &ymd(log::now()),
    );

    let sess = log::Session::open(&dir, &rid)?;
    sess.append(log::Rec::User {
        ts: log::now(),
        text,
        source: "review".into(),
    })?;
    eprintln!("[snens] ревью {rid} → {} ({})", cfg.base_url, cfg.model);
    agent::run_agent(&sess, cfg, llm).await
}

/// Разбор сессии делает тул, а не ядро: он живёт в реестре, у него свой репозиторий,
/// и его же правит ревью-цикл.
async fn audit_report(cfg: &Config, log_path: &Path) -> R<String> {
    let tools = registry::discover(cfg).await;
    let Some(audit) = tools.iter().find(|t| t.name == "audit") else {
        return Err("тул audit не найден в реестре — собери: snens build".into());
    };
    let mut sp = exec::Spawn::argv(
        &audit.bin.to_string_lossy(),
        vec![log_path.display().to_string()],
    );
    sp.timeout_s = 60;
    sp.cap = 200_000;
    let out = exec::spawn(sp).await?;
    if out.exit != 0 {
        return Err(format!("audit вернул код {}: {}", out.exit, out.text.trim()).into());
    }
    Ok(out.text)
}

/// Открытые тикеты одной строкой на каждый: ревью должно видеть, что уже есть,
/// иначе оно придумает ту же проблему заново.
fn ticket_list(dir: &Path) -> String {
    let mut rows: Vec<String> = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let field = |k: &str| fm(&text, k);
        if field("status") != "open" {
            continue;
        }
        let symptom = text
            .lines()
            .skip_while(|l| !l.trim_start().starts_with("## Симптом"))
            .skip(1)
            .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with("## "))
            .unwrap_or("")
            .trim();
        rows.push(format!(
            "- {} | {} | {} | hits {} | {}",
            p.file_stem().and_then(|s| s.to_str()).unwrap_or("?"),
            field("kind"),
            field("target"),
            field("hits"),
            clip(symptom, 140)
        ));
    }
    rows.sort();
    if rows.is_empty() {
        "(открытых тикетов нет)".into()
    } else {
        rows.join("\n")
    }
}

/// Поле front-matter тикета.
fn fm(text: &str, key: &str) -> String {
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}:")))
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

/// Снимок каталога тикетов: равен до ревью и после — значит ревью ничего не
/// записало, и улучшение звать нечего.
pub fn tickets_state(cfg: &Config) -> R<Vec<(String, u64, std::time::SystemTime)>> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(cfg.tickets_path()).into_iter().flatten().flatten() {
        let md = e.metadata()?;
        v.push((
            e.file_name().to_string_lossy().into_owned(),
            md.len(),
            md.modified()?,
        ));
    }
    v.sort();
    Ok(v)
}

/// Полные тела открытых тикетов: improve разбирает их без хождения по файлам.
pub fn tickets_full(cfg: &Config) -> String {
    let mut out = String::new();
    for e in std::fs::read_dir(cfg.tickets_path()).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        if fm(&text, "status") != "open" {
            continue;
        }
        let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
        out.push_str(&format!("=== {name} ===\n{text}\n"));
    }
    if out.is_empty() {
        "(открытых тикетов нет)".into()
    } else {
        out
    }
}

fn render_prompt(
    template: &str,
    session: &str,
    audit: &str,
    tickets: &str,
    tickets_dir: &str,
    date: &str,
) -> String {
    template
        .replace("{{SESSION}}", session)
        .replace("{{AUDIT}}", audit.trim_end())
        .replace("{{TICKETS}}", tickets.trim_end())
        .replace("{{TICKETS_DIR}}", tickets_dir)
        .replace("{{DATE}}", date)
}

fn clip(s: &str, n: usize) -> String {
    let mut t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        t.push('…');
    }
    t
}

/// Дата из unix-времени: без зависимостей и без внешней `date` (её нет на Windows).
pub fn ymd(secs: u64) -> String {
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_is_computed_not_shelled_out() {
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(86_400), "1970-01-02");
        assert_eq!(ymd(1_000_000_000), "2001-09-09");
        assert_eq!(ymd(1_787_000_000), "2026-08-17");
        assert_eq!(ymd(1_790_097_212), "2026-09-22");
    }

    #[test]
    fn prompt_gets_no_placeholders_left() {
        let text = render_prompt(PROMPT, "123-45", "## Таймлайн\n  1. run ls\n", "- t | x | y", "/tmp/t", "2026-09-22");
        assert!(!text.contains("{{"), "незамещённый плейсхолдер: {text}");
        assert!(text.contains("1. run ls"), "разбор сессии не попал в промпт");
        assert!(text.contains("/tmp/t/2026-09-22"), "путь тикета не собран");
    }

    #[test]
    fn ticket_list_takes_open_only_and_reads_symptom() {
        let dir = std::env::temp_dir().join(format!("snens-tickets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("2026-09-22-read.md"),
            "---\nstatus: open\nkind: defect\ntarget: read\nhits: 2\n---\n## Симптом / что повторяется\nОтказ без подсказки\n## История\n- что-то\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("2026-09-22-old.md"),
            "---\nstatus: done\nkind: defect\ntarget: read\nhits: 1\n---\n## Симптом / что повторяется\nУже починено\n",
        )
        .unwrap();
        let list = ticket_list(&dir);
        assert!(list.contains("2026-09-22-read"), "{list}");
        assert!(list.contains("Отказ без подсказки"), "{list}");
        assert!(!list.contains("2026-09-22-old"), "закрытый тикет в списке: {list}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_tickets_dir_is_not_an_error() {
        assert!(ticket_list(Path::new("/нет/такого/каталога")).contains("тикетов нет"));
    }
}
