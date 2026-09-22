mod agent;
mod cfg;
mod exec;
mod llm;
mod log;
mod project;
mod registry;

use std::{collections::BTreeMap, error::Error};

pub type R<T> = Result<T, Box<dyn Error + Send + Sync>>;

const USAGE: &str = "\
snens — узкое ядро агента

  snens run [--new] [--session ID] [--config PATH] \"запрос\"
  snens show [--session ID]
  snens log  [--session ID]
  snens sessions
  snens tools
  snens selftest
";

#[tokio::main]
async fn main() -> R<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, flags, positional) = parse_args(&argv);
    match cmd.as_deref() {
        Some("run") => cmd_run(&flags, positional).await,
        Some("show") => cmd_show(&flags),
        Some("log") => cmd_log(&flags),
        Some("sessions") => {
            let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
            for id in log::list(&cfg.sessions_path())? {
                println!("{id}");
            }
            Ok(())
        }
        Some("tools") => cmd_tools(&flags).await,
        Some("selftest") => agent::selftest().await,
        _ => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

#[allow(clippy::type_complexity)]
fn parse_args(argv: &[String]) -> (Option<String>, BTreeMap<String, String>, Vec<String>) {
    let mut flags = BTreeMap::new();
    let mut positional = Vec::new();
    let mut it = argv.iter();
    let cmd = it.next().cloned();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--session" | "--config" => {
                if let Some(v) = it.next() {
                    flags.insert(a.trim_start_matches("--").to_string(), v.clone());
                }
            }
            other if other.starts_with("--") => {
                flags.insert(other.trim_start_matches("--").to_string(), "1".into());
            }
            other => positional.push(other.to_string()),
        }
    }
    (cmd, flags, positional)
}

fn session_id(cfg: &cfg::Config, flags: &BTreeMap<String, String>) -> R<String> {
    let dir = cfg.sessions_path();
    if let Some(s) = flags.get("session") {
        return Ok(s.clone());
    }
    if flags.contains_key("new") {
        return Ok(log::new_id());
    }
    Ok(log::latest(&dir).unwrap_or_else(|_| log::new_id()))
}

async fn cmd_run(flags: &BTreeMap<String, String>, positional: Vec<String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let prompt = positional.join(" ");
    if prompt.trim().is_empty() {
        return Err("нужен текст запроса".into());
    }
    let id = session_id(&cfg, flags)?;
    let sess = log::Session::open(&cfg.sessions_path(), &id)?;
    sess.append(log::Rec::User {
        ts: log::now(),
        text: prompt,
        source: "cli".into(),
    })?;

    let key = cfg.api_key()?;
    let llm = llm::OpenAi::new(key)?;
    eprintln!("[snens] {id} → {} ({})", cfg.base_url, cfg.model);
    agent::run_agent(&sess, &cfg, &llm).await?;
    Ok(())
}

fn cmd_show(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let id = session_id(&cfg, flags)?;
    let sess = log::Session::open(&cfg.sessions_path(), &id)?;
    let recs = sess.read()?;

    let (mut calls, mut in_tok, mut cached, mut out_tok, mut cost) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for r in &recs {
        match r {
            log::Rec::ToolResult { .. } => calls += 1,
            log::Rec::Usage {
                in_tok: i,
                cached_tok: c,
                out_tok: o,
                cost_micro: m,
                ..
            } => {
                in_tok += i;
                cached += c;
                out_tok += o;
                cost += m;
            }
            _ => {}
        }
    }
    println!("сессия:  {id}");
    println!("файл:    {}", sess.path().display());
    println!("записей: {}", recs.len());
    println!("тулов:   {calls}");
    println!("токены:  вход {in_tok} (из них кеш {cached}), выход {out_tok}");
    if cost > 0 {
        println!("цена:    ${:.4}", cost as f64 / 1e6);
    }
    Ok(())
}

fn cmd_log(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let id = session_id(&cfg, flags)?;
    let path = cfg.sessions_path().join(format!("{id}.jsonl"));
    print!("{}", std::fs::read_to_string(&path)?);
    Ok(())
}

/// Что видит модель: те же summary/usage, что уезжают в определения тулов.
async fn cmd_tools(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let tools = registry::discover(&cfg).await;
    let bin = registry::sync_bin_dir(&cfg, &tools)?;
    println!("каталог тулов: {}", cfg.tools_path().display());
    println!("PATH (дети и человек): {}\n", bin.display());
    if tools.is_empty() {
        println!("(пусто)");
    }
    for t in &tools {
        println!("{}\n  {}\n  {}", t.name, t.summary, t.usage);
    }
    Ok(())
}