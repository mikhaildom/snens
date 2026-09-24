// TUI = терминал и есть интерфейс: строка-редактор на входе, поток вывода на
// выходе, скроллбек терминала — история диалога. Панелей и собственного скролла
// нет намеренно: они ломают выделение и копипаст, а история и так есть.
//
// Steer: поле ввода остаётся живым, пока агент работает. Для этого readline живёт
// в своём потоке (он и держит приглашение), вывод агента идёт через
// `ExternalPrinter` — печатается НАД приглашением, а не поверх него. Строка,
// введённая во время хода, не блокирует агент: она кладётся в ящик сессии
// (`mail::send`, тот же канал, что у подагентов) и впрыскивается на границе хода.
use std::{collections::BTreeMap, io::IsTerminal, path::PathBuf, sync::mpsc, thread};

use rustyline::{error::ReadlineError, DefaultEditor, ExternalPrinter};

use crate::{agent, cfg::Config, llm, log, mail, render::Ui, R};

const HELP: &str = "/new — новая сессия · /reasoning — скрыть/показать рассуждение · \
/help · /quit (или Ctrl+D) · Ctrl+C — отменить ход · во время хода строка уходит агенту \
как steer (после текущего шага)";

/// Что приходит от потока ввода в основной цикл.
enum In {
    Line(String),
    /// Ctrl+C: на приглашении — «передумал», в ходе — отмена хода.
    Interrupt,
    Eof,
}

pub async fn cmd(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = Config::load(flags.get("config").map(|s| s.as_str()))?;
    let llm = llm::OpenAi::new(cfg.api_key()?)?;

    // Без tty (пайп, скрипт, CI) приглашения и ExternalPrinter нет: читаем строки
    // построчно и гоняем ходы последовательно. Steer тут недоступен — он требует
    // живой поверхности, а не потока строк. Так `printf '…\n/quit\n' | snens tui`
    // остаётся проверяемым.
    if !std::io::stdin().is_terminal() {
        return plain(&cfg, &llm, flags).await;
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<In>();
    let (ptx, prx) = mpsc::channel::<Result<Box<dyn ExternalPrinter + Send>, String>>();
    let hist = history_path(&cfg);
    spawn_reader(tx, ptx, hist);

    // Принтер приходит из потока ввода: он создаётся из того же редактора, что
    // держит приглашение, и только так умеет печатать поверх него.
    let printer = prx
        .recv()
        .map_err(|e| format!("ввод: поток не поднялся: {e}"))??;
    let ui = Ui::with_printer(printer);

    let mut id = match flags.get("session") {
        Some(s) => s.clone(),
        None => log::new_id(),
    };
    let mut sess = log::Session::open(&cfg.sessions_path(), &id)?;

    let cwd = std::env::current_dir().unwrap_or_default();
    ui.line(&format!("сессия {id} · {} · {}", cfg.model, cwd.display()));
    ui.line(&format!("продолжить: snens tui --session {id}"));
    ui.line(HELP);

    loop {
        let Some(msg) = rx.recv().await else { break };
        let line = match msg {
            In::Eof => break,
            In::Interrupt => {
                ui.line(&format!("({HELP})"));
                continue;
            }
            In::Line(l) => l,
        };
        match line.as_str() {
            "/quit" | "/q" => break,
            "/help" | "/h" => {
                ui.line(HELP);
                continue;
            }
            "/new" => {
                id = log::new_id();
                sess = log::Session::open(&cfg.sessions_path(), &id)?;
                ui.line(&format!("сессия {id}"));
                continue;
            }
            "/reasoning" => {
                let on = ui.toggle_reasoning();
                ui.line(&format!(
                    "рассуждение: {}",
                    if on {
                        "показано"
                    } else {
                        "скрыто"
                    }
                ));
                continue;
            }
            _ => {}
        }

        sess.append(log::Rec::User {
            ts: log::now(),
            text: line,
            source: "tui".into(),
        })?;
        // Ход не блокирует ввод: пока он идёт, строки из канала уезжают агенту
        // как steer. Ctrl+C или Ctrl+D прерывают ход.
        match run_turn(&sess, &cfg, &llm, &ui, &mut rx, &id).await? {
            Flow::Next => {}
            Flow::Quit => break,
        }
    }

    // Ревью уезжает фоновым процессом (см. main::spawn_review): окно можно
    // закрывать, ревью доработает само.
    crate::autopilot(&cfg, &id).await?;
    Ok(())
}

enum Flow {
    Next,
    Quit,
}

/// Ход агента с живым вводом. Пока future цикла выполняется, канал ввода не
/// простаивает: каждая строка становится steer'ом в ящике сессии. Инжект делает
/// сам цикл на ближайшей границе хода (после текущего шага).
async fn run_turn(
    sess: &log::Session,
    cfg: &Config,
    llm: &llm::OpenAi,
    ui: &Ui,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<In>,
    id: &str,
) -> R<Flow> {
    let mut turn = Box::pin(agent::run_agent(sess, cfg, llm, ui));
    loop {
        tokio::select! {
            r = &mut turn => {
                r?;
                return Ok(Flow::Next);
            }
            msg = rx.recv() => match msg {
                // Ввод кончился (поток умер) — не рвём ход, даём ему завершиться.
                None => {
                    turn.await?;
                    return Ok(Flow::Next);
                }
                Some(In::Interrupt) => {
                    ui.notice("ход отменён");
                    return Ok(Flow::Next);
                }
                Some(In::Eof) => {
                    ui.notice("ход отменён, выход");
                    return Ok(Flow::Quit);
                }
                Some(In::Line(l)) => {
                    if l.is_empty() {
                        continue;
                    }
                    mail::send(&cfg.mail_path(), id, "human", "steer", &l)?;
                    ui.notice(&format!("steer принят: {l}"));
                }
            },
        }
    }
}

/// Не-tty поверхность: строки со stdin, ход последовательно, вывод в stdout.
/// Без приглашения и без steer — это путь для скриптов и проверок.
async fn plain(cfg: &Config, llm: &llm::OpenAi, flags: &BTreeMap<String, String>) -> R<()> {
    use std::io::BufRead;
    let mut id = match flags.get("session") {
        Some(s) => s.clone(),
        None => log::new_id(),
    };
    let mut sess = log::Session::open(&cfg.sessions_path(), &id)?;
    let ui = Ui::plain();
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?.trim().to_string();
        if line.is_empty() {
            continue;
        }
        match line.as_str() {
            "/quit" | "/q" => break,
            "/new" => {
                id = log::new_id();
                sess = log::Session::open(&cfg.sessions_path(), &id)?;
                println!("сессия {id}");
                continue;
            }
            "/help" | "/h" => {
                println!("{HELP}");
                continue;
            }
            "/reasoning" => continue,
            _ => {}
        }
        sess.append(log::Rec::User {
            ts: log::now(),
            text: line,
            source: "tui".into(),
        })?;
        agent::run_agent(&sess, cfg, llm, &ui).await?;
    }
    crate::autopilot(cfg, &id).await?;
    Ok(())
}

/// Поток ввода: редактор, приглашение и печать поверх него. Владение редактором
/// остаётся здесь (он не `Send`), наружу отдаём только принтер и строки.
fn spawn_reader(
    tx: tokio::sync::mpsc::UnboundedSender<In>,
    ptx: mpsc::Sender<Result<Box<dyn ExternalPrinter + Send>, String>>,
    hist: PathBuf,
) {
    thread::spawn(move || {
        let mut rl = match DefaultEditor::new() {
            Ok(r) => r,
            Err(e) => {
                let _ = ptx.send(Err(format!("редактор: {e}")));
                return;
            }
        };
        let _ = rl.load_history(&hist);
        match rl.create_external_printer() {
            Ok(p) => {
                if ptx.send(Ok(Box::new(p))).is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = ptx.send(Err(format!("принтер: {e}")));
                return;
            }
        }
        loop {
            match rl.readline("> ") {
                Ok(l) => {
                    let t = l.trim().to_string();
                    if t.is_empty() {
                        continue;
                    }
                    let _ = rl.add_history_entry(t.as_str());
                    // Сохраняем сразу: выход из TUI может случиться в любой момент.
                    let _ = rl.save_history(&hist);
                    if tx.send(In::Line(t)).is_err() {
                        break;
                    }
                }
                Err(ReadlineError::Interrupted) => {
                    if tx.send(In::Interrupt).is_err() {
                        break;
                    }
                }
                Err(ReadlineError::Eof) | Err(_) => {
                    let _ = tx.send(In::Eof);
                    break;
                }
            }
        }
    });
}

/// История ввода живёт рядом с сессиями: она про человека, а не про сессию.
fn history_path(cfg: &Config) -> PathBuf {
    cfg.sessions_path()
        .parent()
        .map(|p| p.join("history.txt"))
        .unwrap_or_else(|| PathBuf::from("snens-history.txt"))
}
