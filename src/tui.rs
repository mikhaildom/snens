// TUI = терминал и есть интерфейс: строка-редактор на входе, поток вывода на
// выходе, скроллбек терминала — история диалога. Панелей и собственного скролла
// нет намеренно: они ломают выделение и копипаст, а история и так есть.
//
// Steer: поле ввода остаётся живым, пока агент работает. Читатель ввода (`input.rs`)
// живёт в своём потоке и владеет приглашением; вывод агента проходит через `Sink`
// и печатается НАД приглашением, а не поверх него. Строка, введённая во время хода,
// кладётся в ящик сессии (`mail::send`, тот же канал, что у подагентов) и
// впрыскивается на границе хода. Свой читатель вместо rustyline — чтобы тем же
// механизмом читать строку без эха (пароль для askpass) и владеть терминалом.
use std::{collections::BTreeMap, io::IsTerminal, path::PathBuf, sync::Arc};

use crate::{agent, cfg::Config, input, input::In, llm, log, mail, render::Ui, R};

const HELP: &str = "команды: /new — новая сессия · /reasoning — рассуждение · \
/memory [on|off] — долгая память · /compact [указание] — сжать контекст · \
/help · /quit (Ctrl+D) · во время хода: Ctrl+C — отменить ход, строка уйдёт агенту после шага";

pub async fn cmd(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = Config::load(flags.get("config").map(|s| s.as_str()))?;
    let llm = llm::OpenAi::new(cfg.api_key()?)?;

    // Без tty (пайп, скрипт, CI) приглашения и своего читателя нет: читаем строки
    // построчно и гоняем ходы последовательно. Steer тут недоступен — он требует
    // живой поверхности, а не потока строк. Так `printf '…\n/quit\n' | snens tui`
    // остаётся проверяемым.
    if !std::io::stdin().is_terminal() {
        return plain(&cfg, &llm, flags).await;
    }
    // Сырой режим не поднялся (нет tty или платформа) — тот же последовательный путь.
    let Ok(_raw) = input::raw_mode() else {
        return plain(&cfg, &llm, flags).await;
    };

    let term = Arc::new(input::Term::new("> "));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<In>();
    input::spawn(Arc::clone(&term), tx, history_path(&cfg));
    // Сервер пароля: sudo-вызовы инструментов спрашивают пароль у человека через
    // него, а не отбирают /dev/tty у читателя ввода.
    crate::askpass::spawn_server(
        crate::askpass::sock_path(&cfg.sessions_path()),
        Arc::clone(&term),
    );
    let ui = Ui::with_sink(Box::new(input::TermSink(Arc::clone(&term))));

    let mut id = match flags.get("session") {
        Some(s) => s.clone(),
        None => log::new_id(),
    };
    let mut sess = log::Session::open(&cfg.sessions_path(), &id)?;

    let cwd = std::env::current_dir().unwrap_or_default();
    ui.line(&format!("сессия {id} · {} · {}", cfg.model, cwd.display()));
    ui.line(HELP);

    loop {
        let Some(msg) = rx.recv().await else { break };
        let line = match msg {
            In::Eof => break,
            In::Interrupt => {
                ui.line("Ctrl+D — выход, /help — команды");
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
            "/memory" => {
                let mem = cfg.memory_now();
                ui.line(&format!(
                    "память: {} · справка {} · запись хода {}",
                    if mem.enabled { "вкл" } else { "выкл" },
                    match mem.recall {
                        crate::cfg::RecallMode::Always => "каждый ход",
                        crate::cfg::RecallMode::Session => "раз на сессию",
                        crate::cfg::RecallMode::Off => "нет",
                    },
                    if mem.retain { "да" } else { "нет" },
                ));
                ui.line("переключить: /memory on или /memory off");
                continue;
            }
            "/memory on" | "/memory off" => {
                let on = line.ends_with("on");
                cfg.set_memory_key("enabled", if on { "true" } else { "false" })?;
                ui.line(&format!("память: {}", if on { "вкл" } else { "выкл" }));
                continue;
            }
            _ => {}
        }

        let line = compact_request(line);
        sess.append(log::Rec::User {
            ts: log::now(),
            text: line,
            source: "tui".into(),
            parts: Vec::new(),
        })?;
        // Ход не блокирует ввод: пока он идёт, строки из канала уезжают агенту
        // как steer. Ctrl+C или Ctrl+D прерывают ход.
        match run_turn(&sess, &cfg, &llm, &ui, &mut rx, &id).await? {
            Flow::Next => {}
            Flow::Quit => break,
        }
    }

    // Инструкция «как вернуться» — при выходе, а не на входе: до первого хода
    // она отвечает на вопрос, которого человек ещё не задал.
    ui.line(&format!("продолжить: snens tui --session {id}"));

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
                    ui.notice(&format!("принято, уйдёт агенту после шага: {l}"));
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
        let line = compact_request(line);
        sess.append(log::Rec::User {
            ts: log::now(),
            text: line,
            source: "tui".into(),
            parts: Vec::new(),
        })?;
        agent::run_agent(&sess, cfg, llm, &ui).await?;
    }
    crate::autopilot(cfg, &id).await?;
    Ok(())
}

/// История ввода живёт рядом с сессиями: она про человека, а не про сессию.
fn history_path(cfg: &Config) -> PathBuf {
    cfg.sessions_path()
        .parent()
        .map(|p| p.join("history.txt"))
        .unwrap_or_else(|| PathBuf::from("snens-history.txt"))
}

/// Manual cleanup uses the same agent and editable context, not a summarizer.
fn compact_request(line: String) -> String {
    if line == "/compact" || line.starts_with("/compact ") {
        format!("Приведи рабочий контекст в порядок через LIVE_CONTEXT.json и ctx commit. Убери ненужное, сохрани точные полезные фрагменты, обнови записку для продолжения и завершения задачи. Не пересказывай историю. Дополнительная инструкция: {}", line.strip_prefix("/compact").unwrap_or("").trim())
    } else {
        line
    }
}
