// Ревью: не свежая сессия, а транскрипт этой же сессии. Раньше — полная копия
// лога, теперь ссылка `Rec::Replay` на его первые записи: проекция получает те
// же записи до байта (провайдерский кеш остаётся тёплым), а транскрипт лежит в
// файлах один раз.
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
    // Отметка саморевью в журнале самой сессии — ДО счёта длины ниже. После
    // неё журнал вырос на одну строку, и если считать префикс раньше, он бы
    // оттяпал последнюю запись сессии (её настоящий ход) или не сошёлся бы с
    // проверкой длины в `rewrite_with_replay`.
    //
    // На поверхности маркер уже поставил `schedule_review` (он держит писательский
    // лок), и открыть журнал здесь нельзя — это не ошибка, а знак того, что
    // граница отмечена. Из-за маркера ревью не отменяем:
    // сам разбор полезнее строки-отметки.
    if let Err(error) = mark_review_start(cfg, id) {
        eprintln!("[snens] ревью {rid}: маркер в журнал сессии не записан ({error}) — продолжаю");
    }
    let dst = dir.join(format!("{rid}.jsonl"));
    // Не копия, а ссылка на хвост сессии (`Rec::Replay`). Проекция всё равно
    // получает те же записи — префикс провайдерского кеша остаётся тёплым, — но
    // байты префикса лежат в одном файле, а не в двух.
    let existed = dst.exists();
    let sess = log::Session::open(&dir, &rid)?;
    // Префикс — раскрытые записи сессии. Повторный прогон по той же сессии
    // (после первого её работы хвост уже в её логе) остаётся на ревью-логе:
    // нижняя граница ссылки на префикс не может сдвигаться вперёд, а вместе
    // лог и хвост — это ровно раскрытый ревью-лог.
    // Длина источника — чтением файла, а не `Session::open`: лок эксклюзивный, и
    // открытие сессии на запись подняло бы ошибку, если она живёт в поверхности.
    let src_len = log::expand(&log::read_file(&src)?, &dir).len();
    let (link, prefix_len) = match log::replay_link(&log::read_head(&dst, 2)?) {
        Some((prev, lines)) if prev == id && src_len < lines => (rid.as_str(), lines),
        _ => (id, src_len),
    };
    sess.rewrite_with_replay(link, prefix_len, false)?;
    drop(sess);
    if existed {
        eprintln!("[snens] {rid}: лог перезаписан, прошлый прогон ревью этой сессии потерян");
    }
    let sess = log::Session::open(&dir, &rid)?;

    let mut audit = audit_report(cfg, &src).await?;
    // Строка про артефакты: у ревью нет других сессий, и «повторяется ли
    // разметка» оно решить не может — но заметить, что вопросы артефактов
    // остались без ответа или что их много, обязано. Считается по журналу,
    // а не по HTML: разбор интерфейса — работа `genui check`.
    if let Ok(log) = log::read_file(&src) {
        let stats = crate::surface_artifact::AskStats::of(&log);
        let line = stats.line();
        if !line.is_empty() {
            audit.push_str(&format!("\n{line}\n"));
        }
    }
    let tickets = ticket_list(&cfg.tickets_path());
    let template = match &cfg.review_prompt {
        Some(p) => std::fs::read_to_string(p).map_err(|e| format!("review_prompt {p}: {e}"))?,
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

    sess.append(log::Rec::User {
        ts: log::now(),
        text,
        source: "review".into(),
        parts: Vec::new(),
        ask: None,
    })?;
    eprintln!("[snens] ревью {rid} → {} ({})", cfg.base_url, cfg.model);
    agent::run_agent(&sess, cfg, llm, &crate::render::Ui::plain()).await
}

/// Отметка запуска ревью в журнале сессии. Пишется с числом ходов на момент
/// запуска — это и есть граница зоны ответственности следующего прогона.
///
/// Маркер — служебная строка: в проекции промпта её нет (см. `project::build`),
/// а в транскрипте поверхности она едет отдельной ролью `review`. Поэтому
/// раскрытый источник растёт ровно на одну запись, а содержимое прежних не
/// меняется — префикс-кеш провайдера остаётся тёплым: проекция до маркера и
/// после него совпадает байт-в-байт.
pub fn mark_review_start(cfg: &Config, id: &str) -> R<()> {
    let dir = cfg.sessions_path();
    let path = dir.join(format!("{id}.jsonl"));
    let turns = log::read_file(&path)?
        .iter()
        .filter(|r| matches!(r, log::Rec::Assistant { .. }))
        .count() as u64;
    let sess = log::Session::open(&dir, id)?;
    sess.append(mark(id, turns))?;
    Ok(())
}

/// Запись-отметка ревью. Одна и та же для обоих путей запуска (поверхность и
/// `snens review`): строка в транскрипте не должна зависеть от того, кто начал.
pub fn mark(id: &str, turns: u64) -> log::Rec {
    log::Rec::Review {
        ts: log::now(),
        review_id: format!("{id}-review"),
        turns,
        text: log::review_line(turns),
    }
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
/// иначе оно придумает ту же проблему заново. Разбор живёт в [`crate::tickets`].
fn ticket_list(dir: &Path) -> String {
    crate::tickets::open_list(dir)
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
    let y = if m <= 2 {
        yoe + era * 400 + 1
    } else {
        yoe + era * 400
    };
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
        let text = render_prompt(
            PROMPT,
            "123-45",
            "## Таймлайн\n  1. run ls\n",
            "- t | x | y",
            "/tmp/t",
            "2026-09-22",
        );
        assert!(!text.contains("{{"), "незамещённый плейсхолдер: {text}");
        assert!(
            text.contains("1. run ls"),
            "разбор сессии не попал в промпт"
        );
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
        assert!(
            !list.contains("2026-09-22-old"),
            "закрытый тикет в списке: {list}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_tickets_dir_is_not_an_error() {
        assert!(ticket_list(Path::new("/нет/такого/каталога")).contains("тикетов нет"));
    }

    /// Маркер ревью появляется в журнале сессии ровно одной записью и границу
    /// не сдвигает: источник растёт на одну запись, а префикс (всё до маркера)
    /// остаётся байт-в-байт прежним.
    #[test]
    fn review_marker_is_written_and_does_not_move_the_boundary() {
        let dir = std::env::temp_dir().join(format!("snens-review-mark-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = Config {
            sessions_dir: dir.display().to_string(),
            ..Default::default()
        };
        let id = "sess";
        {
            let sess = log::Session::open(&dir, id).unwrap();
            for i in 0..3 {
                sess.append(log::Rec::User {
                    ts: i,
                    text: format!("ход {i}"),
                    source: "cli".into(),
                    parts: Vec::new(),
                    ask: None,
                })
                .unwrap();
                sess.append(log::Rec::Assistant {
                    ts: i,
                    text: Some(format!("ответ {i}")),
                    tool_calls: Vec::new(),
                    reasoning: None,
                })
                .unwrap();
            }
        }
        let path = dir.join("sess.jsonl");
        let before = log::expand(&log::read_file(&path).unwrap(), &dir).len();

        mark_review_start(&cfg, id).unwrap();

        let raw = log::read_file(&path).unwrap();
        assert_eq!(raw.len(), before + 1, "маркер — ровно одна запись");
        assert!(
            matches!(raw.last(), Some(log::Rec::Review { turns, text, .. })
                if *turns == 3 && text.contains("3 ход")),
            "маркер записан без числа ходов: {:?}",
            raw.last()
        );
        // Префикс (всё, что было до маркера) не переписан — иначе у провайдера
        // поехал бы префикс-кеш. Сравниваем ровно тот срез, который уйдёт в
        // `Replay` следующего прогона.
        let after = log::expand(&log::read_file(&path).unwrap(), &dir);
        assert_eq!(after.len(), before + 1);
        assert_eq!(
            serde_json::to_string(&after[..before]).unwrap(),
            serde_json::to_string(&raw[..before]).unwrap()
        );

        // Проекция промпта маркера не видит: служебная строка, а не сообщение.
        let wire = serde_json::to_string(&crate::project::raw_msgs(&after, &cfg)).unwrap();
        assert!(!wire.contains("саморевью"), "маркер уехал в промпт: {wire}");
        assert_eq!(
            serde_json::to_string(&crate::project::raw_msgs(&after[..before], &cfg)).unwrap(),
            wire,
            "проекция с маркером отличается от проекции без него"
        );

        // Второй прогон ставит вторую отметку, прежние записи по-прежнему целы:
        // граница для следующего разбора считается по числу ходов, а не по тексту.
        mark_review_start(&cfg, id).unwrap();
        let twice = log::read_file(&path).unwrap();
        assert_eq!(
            twice
                .iter()
                .filter(|r| matches!(r, log::Rec::Review { .. }))
                .count(),
            2
        );
        assert_eq!(twice.len(), before + 2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
