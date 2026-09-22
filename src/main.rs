mod agent;
mod cfg;
mod exec;
mod llm;
mod log;
mod project;
mod registry;
mod review;

use std::{collections::BTreeMap, error::Error};

pub type R<T> = Result<T, Box<dyn Error + Send + Sync>>;

const USAGE: &str = "\
snens — узкое ядро агента

  snens run [--new] [--session ID] [--config PATH] \"запрос\"
  snens show [--session ID]
  snens log  [--session ID]
  snens sessions
  snens tools
  snens build
  snens review [--session ID]
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
        Some("build") => cmd_build(&flags),
        Some("review") => review::cmd(&flags).await,
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

    // Порог по длине, а не по цене: цена слишком разбросана, чтобы быть сигналом.
    let turns = log::read_file(sess.path())?
        .iter()
        .filter(|r| matches!(r, log::Rec::Assistant { .. }))
        .count();
    if turns >= cfg.review_min_turns {
        eprintln!(
            "[snens] сессия длинная ({turns} ходов) — повод для ревью: snens review --session {id}"
        );
    }
    Ok(())
}

fn cmd_show(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let id = session_id(&cfg, flags)?;
    let sess = log::Session::open(&cfg.sessions_path(), &id)?;
    let recs = sess.read()?;

    let (mut calls, mut in_tok, mut cached, mut out_tok, mut cost) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut biggest: Option<(usize, &str)> = None;
    for r in &recs {
        match r {
            log::Rec::ToolResult { tool, bytes, .. } => {
                calls += 1;
                if biggest.map_or(true, |(b, _)| *bytes > b) {
                    biggest = Some((*bytes, tool.as_str()));
                }
            }
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
    if let Some((b, name)) = biggest {
        println!("макс. результат: {b} Б ({name})");
    }
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

/// Собрать вшитые тулы и положить бинарники туда, откуда их берёт реестр:
/// `tools/<имя>/<имя>[.exe]`. Шаг входит в дефолтную установку, поэтому он в
/// харнессе, а не в sh-скрипте: на Windows sh нет.
fn cmd_build(flags: &BTreeMap<String, String>) -> R<()> {
    let cfg = cfg::Config::load(flags.get("config").map(|s| s.as_str()))?;
    let tools_dir = cfg.tools_path();
    let root = tools_dir
        .parent()
        .ok_or("у tools_dir нет родителя — негде запускать cargo")?;

    let ok = std::process::Command::new("cargo")
        .args(["build", "--release", "--workspace"])
        .current_dir(root)
        .status()?;
    if !ok.success() {
        return Err("cargo build завершился с ошибкой".into());
    }

    let release = cargo_target_dir(root)?.join("release");
    let mut built = 0;
    for e in std::fs::read_dir(&tools_dir)?.flatten() {
        let dir = e.path();
        if !dir.join("Cargo.toml").is_file() {
            continue;
        }
        let Some(name) = dir.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        let from = release.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        if !from.is_file() {
            eprintln!("[snens] {name}: нет {} — пропускаю", from.display());
            continue;
        }
        let to = dir.join(from.file_name().unwrap());
        // Через симлинк (наследие сборки скриптом) копирование записало бы
        // бинарник сам в себя и обнулило его.
        let _ = std::fs::remove_file(&to);
        std::fs::copy(&from, &to)?;
        println!("{name} → {}", to.display());
        built += 1;
    }
    println!("собрано тулов: {built}");
    Ok(())
}

/// target может быть перенесён (`.cargo/config.toml`) — спрашиваем cargo, а не угадываем.
fn cargo_target_dir(root: &std::path::Path) -> R<std::path::PathBuf> {
    let out = std::process::Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(root)
        .output()?;
    if !out.status.success() {
        return Err("cargo metadata упал".into());
    }
    let j: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let dir = j
        .get("target_directory")
        .and_then(|v| v.as_str())
        .ok_or("cargo metadata без target_directory")?;
    Ok(std::path::PathBuf::from(dir))
}