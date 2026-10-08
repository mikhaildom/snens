// Улучшение: единственный, кто меняет код харнесса. Свежая сессия, полный набор
// тулов, работа в репозитории исходников — не там, где запущен snens.
use std::collections::BTreeMap;

use crate::{agent, cfg::Config, llm, log, review, R};

const PROMPT: &str = include_str!("../prompts/improve.md");

pub async fn cmd(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = Config::load(flags.get("config").map(|s| s.as_str()))?;
    let llm = llm::OpenAi::new(cfg.api_key()?)?;
    run(&cfg, &llm).await
}

pub async fn run(cfg: &Config, llm: &llm::OpenAi) -> R<()> {
    let repo = cfg.repo_path()?;
    let text = PROMPT
        .replace("{{REPO}}", &repo.display().to_string())
        // Оглавление, а не полные тела: тикетов много, и они растут. Тело
        // конкретного модель дотянет через `snens tickets show <имя>`.
        .replace(
            "{{TICKETS}}",
            &crate::tickets::open_brief(&cfg.tickets_path()),
        )
        .replace("{{TICKETS_DIR}}", &cfg.tickets_path().display().to_string())
        .replace("{{INSTANCE}}", &cfg.data_path().display().to_string())
        .replace("{{DATE}}", &review::ymd(log::now()));

    // Дети наследуют текущий каталог процесса: переносим его в репозиторий.
    // Это последний шаг автопрогона, обратно cwd не нужен.
    std::env::set_current_dir(&repo)?;

    let id = log::new_id();
    let sess = log::Session::open(&cfg.sessions_path(), &id)?;
    sess.append(log::Rec::User {
        ts: log::now(),
        text,
        source: "improve".into(),
        parts: Vec::new(),
    })?;
    eprintln!("[snens] улучшение {id} → {} ({})", cfg.base_url, cfg.model);
    // Рендер как у TUI: `Ui::default()` сам включает цвет только на tty, поэтому
    // улучшение, запущенное руками в терминале, выглядит тем же потоком, что и
    // живая сессия (рассуждение, цвет), а перенаправленное в файл — без цвета.
    let outcome = agent::run_agent(&sess, cfg, llm, &crate::render::Ui::default()).await;
    // Улучшение — единственный, кто правит код харнесса: пересобираем ядро и
    // кладём бинарник на место, чтобы новая сборка вступила в силу без ручного
    // `cargo build`. Ошибка пересборки не отменяет уже сделанную работу.
    if let Err(e) = crate::rebuild_and_install(&repo) {
        eprintln!("[snens] пересборка ядра не удалась: {e}");
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn improve_prompt_has_no_placeholders() {
        let text = PROMPT
            .replace("{{REPO}}", "/r")
            .replace("{{TICKETS}}", "t")
            .replace("{{TICKETS_DIR}}", "/t")
            .replace("{{INSTANCE}}", "/i")
            .replace("{{DATE}}", "d");
        assert!(!text.contains("{{"), "незамещённый плейсхолдер: {text}");
    }
}
