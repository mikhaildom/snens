// TUI = терминал и есть интерфейс: строка-редактор на входе, поток вывода на
// выходе, скроллбек терминала — история диалога. Панелей и собственного скролла
// нет намеренно: они ломают выделение и копипаст, а история и так есть.
use std::{collections::BTreeMap, path::PathBuf};

use rustyline::{error::ReadlineError, DefaultEditor};

use crate::{agent, cfg::Config, llm, log, render::Ui, R};

const HELP: &str = "/new — новая сессия · /reasoning — скрыть/показать рассуждение · \
/help · /quit (или Ctrl+D) · Ctrl+C — отменить ход";

pub async fn cmd(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = Config::load(flags.get("config").map(|s| s.as_str()))?;
    let llm = llm::OpenAi::new(cfg.api_key()?)?;
    // Рендер один на поверхность: переключатель рассуждения живёт между ходами.
    let ui = Ui::default();

    let mut id = match flags.get("session") {
        Some(s) => s.clone(),
        None => log::new_id(),
    };
    let mut sess = log::Session::open(&cfg.sessions_path(), &id)?;

    let mut rl = DefaultEditor::new()?;
    let hist = history_path(&cfg);
    let _ = rl.load_history(&hist);

    let cwd = std::env::current_dir().unwrap_or_default();
    println!("сессия {id} · {} · {}", cfg.model, cwd.display());
    println!("продолжить: snens tui --session {id}");
    println!("{HELP}");

    loop {
        let line = match rl.readline("> ") {
            Ok(l) => l.trim().to_string(),
            // Ctrl+C на приглашении — не выход, а «передумал».
            Err(ReadlineError::Interrupted) => {
                println!("({HELP})");
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(format!("ввод: {e}").into()),
        };
        if line.is_empty() {
            continue;
        }
        match line.as_str() {
            "/quit" | "/q" => break,
            "/help" | "/h" => {
                println!("{HELP}");
                continue;
            }
            "/new" => {
                id = log::new_id();
                sess = log::Session::open(&cfg.sessions_path(), &id)?;
                println!("сессия {id}");
                continue;
            }
            "/reasoning" => {
                let on = ui.toggle_reasoning();
                println!(
                    "рассуждение: {}",
                    if on {
                        "показано"
                    } else {
                        "скрыто"
                    }
                );
                continue;
            }
            _ => {}
        }

        let _ = rl.add_history_entry(line.as_str());
        sess.append(log::Rec::User {
            ts: log::now(),
            text: line,
            source: "tui".into(),
        })?;
        // Ход отменяем, а не роняем процесс: будущее снимается, а kill_on_drop
        // в exec.rs добивает детей. Незавершённый ход проекция достроит сама.
        tokio::select! {
            r = agent::run_agent(&sess, &cfg, &llm, &ui) => r?,
            _ = tokio::signal::ctrl_c() => eprintln!("[snens] ход отменён"),
        }
    }

    let _ = rl.save_history(&hist);
    // Ревью уезжает фоновым процессом (см. main::spawn_review): окно можно
    // закрывать, ревью доработает само. Ctrl+C тут не нужен — autopilot только
    // запускает процесс и сразу возвращается.
    crate::autopilot(&cfg, &id).await?;
    Ok(())
}

/// История ввода живёт рядом с сессиями: она про человека, а не про сессию.
fn history_path(cfg: &Config) -> PathBuf {
    cfg.sessions_path()
        .parent()
        .map(|p| p.join("history.txt"))
        .unwrap_or_else(|| PathBuf::from("snens-history.txt"))
}
