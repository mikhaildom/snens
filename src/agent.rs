// Цикл: запрос → стрим → диспатч → append. Состояние — лог, поэтому resume бесплатен:
// незавершённый ход = User без Assistant, продолжение с того же места.
use serde_json::{json, Value};
use std::path::Path;

use crate::{
    cfg::Config,
    exec, log,
    log::{Body, Call, Rec, Session},
    llm::{Fake, Llm, Reply, Usage},
    project, registry,
    R,
};

/// Вшитый инструмент. Всё остальное — процессы из реестра.
pub fn builtin_defs() -> Vec<Value> {
    vec![json!({
        "type": "function",
        "function": {
            "name": "run",
            "description": "Выполнить shell-команду (sh -c) и получить вывод: stdout, затем stderr, и код возврата.\nrun <команда> [--cwd каталог]",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Команда для sh -c"},
                    "cwd": {"type": "string", "description": "Рабочий каталог"},
                    "timeout_seconds": {"type": "integer", "description": "Таймаут в секундах"}
                },
                "required": ["command"]
            }
        }
    })]
}

/// Определения тулов для модели + сами тулы + PATH дочерних процессов.
pub async fn prepare(cfg: &Config) -> R<(Vec<Value>, Vec<registry::Tool>, String)> {
    let tools = registry::discover(cfg).await;
    let bin = registry::sync_bin_dir(cfg, &tools)?;
    let path = registry::child_path(&bin);
    let mut defs = builtin_defs();
    defs.extend(registry::definitions(&tools));
    Ok((defs, tools, path))
}

/// Ошибки инструмента не прерывают ход: они возвращаются как результат,
/// чтобы модель могла исправиться.
pub async fn dispatch(
    call: &Call,
    cfg: &Config,
    tools: &[registry::Tool],
    path: &str,
    out_dir: &Path,
) -> exec::Out {
    let fail = |msg: String| exec::Out {
        exit: -1,
        text: msg,
        bytes: 0,
        ms: 0,
        timed_out: false,
    };
    let a: Value = serde_json::from_str(&call.args).unwrap_or_else(|_| json!({}));

    if call.name == "run" {
        let Some(command) = a.get("command").and_then(|c| c.as_str()) else {
            return fail("run: не передан command".into());
        };
        if command.trim().is_empty() {
            return fail("run: пустая команда".into());
        }
        let t = a
            .get("timeout_seconds")
            .and_then(|t| t.as_u64())
            .unwrap_or(cfg.timeout_seconds);
        let mut sp = exec::Spawn::sh(&cfg.shell, command, t, cfg.max_process_chars);
        sp.cwd = a.get("cwd").and_then(|c| c.as_str());
        sp.path = Some(path);
        sp.reduce = cfg.reduce_output;
        sp.dump_dir = Some(out_dir);
        return match exec::spawn(sp).await {
            Ok(o) => o,
            Err(e) => fail(format!("run: {e}")),
        };
    }

    let Some(tool) = tools.iter().find(|t| t.name == call.name) else {
        return fail(format!(
            "неизвестный инструмент: {} — его нет в реестре. Список: snens tools",
            call.name
        ));
    };
    let args: Vec<String> = match a.get("args") {
        Some(Value::Array(v)) => v
            .iter()
            .map(|x| match x {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        Some(Value::String(s)) => s.split_whitespace().map(str::to_string).collect(),
        None => Vec::new(),
        Some(other) => return fail(format!("args: ожидался массив или строка, получено {other}")),
    };
    let bin = tool.bin.to_string_lossy().into_owned();
    let mut sp = exec::Spawn::argv(&bin, args);
    sp.stdin = a.get("stdin").and_then(|s| s.as_str()).map(str::to_string);
    sp.cwd = a.get("cwd").and_then(|c| c.as_str());
    sp.cap = cfg.max_process_chars;
    sp.timeout_s = a
        .get("timeout_seconds")
        .and_then(|t| t.as_u64())
        .unwrap_or(cfg.timeout_seconds);
    sp.path = Some(path);
    // Тулы отдают точный контракт (файл, JSON-событие) — повторы в них схлопывать
    // нельзя, это исказило бы содержимое. Сокращение живёт только на shell-выводе.
    sp.dump_dir = Some(out_dir);
    match exec::spawn(sp).await {
        Ok(o) => o,
        Err(e) => fail(format!("{}: {e}", call.name)),
    }
}

pub async fn run_agent<L: Llm>(sess: &Session, cfg: &Config, llm: &L) -> R<()> {
    let (defs, reg, path) = prepare(cfg).await?;
    // Полные выводы обрезанных команд: модель получает путь и может дочитать.
    let out_dir = sess.path().with_extension("out");
    let mut turn = 0u32;
    loop {
        turn += 1;
        let log = sess.read()?;
        let (msgs, report) = project::project(&log, cfg);
        let approx = project::estimate_tokens(&msgs);
        // Оба порога пока только сигналят: это точки, где должна сработать компакция (M3).
        let ratio = approx as f64 / cfg.ctx_window as f64;
        if ratio > cfg.hard {
            eprintln!("[snens] ~{approx} токенов — пройден порог саммаризации ({:.0}%), саммари ещё нет", cfg.hard * 100.0);
        } else if ratio > cfg.soft {
            eprintln!("[snens] ~{approx} токенов — пройден порог элизии ({:.0}%)", cfg.soft * 100.0);
        }
        if report.elided > last_elided(&log) {
            eprintln!(
                "[snens] компакция: вырезано результатов {}, освобождено {} Б",
                report.elided, report.freed_bytes
            );
            sess.append(Rec::Ctx {
                ts: log::now(),
                action: "elide".into(),
                elided: report.elided,
                freed_bytes: report.freed_bytes,
            })?;
        }

        let reply: Reply = llm
            .reply(cfg, msgs, defs.clone(), &mut |d| {
                use std::io::Write;
                print!("{d}");
                let _ = std::io::stdout().flush();
            })
            .await?;
        println!();

        sess.append(Rec::Assistant {
            ts: log::now(),
            text: if reply.text.is_empty() {
                None
            } else {
                Some(reply.text.clone())
            },
            tool_calls: reply.calls.clone(),
        })?;
        sess.append(Rec::Usage {
            ts: log::now(),
            in_tok: reply.usage.in_tok,
            cached_tok: reply.usage.cached_tok,
            out_tok: reply.usage.out_tok,
            cost_micro: reply.usage.cost_micro,
        })?;
        // Расход по ходу, а не только в `show` после сессии: в терминале видно,
        // какой ход чего стоил.
        if reply.usage.in_tok + reply.usage.out_tok > 0 {
            let cost = if reply.usage.cost_micro > 0 {
                format!(" ${:.4}", reply.usage.cost_micro as f64 / 1e6)
            } else {
                String::new()
            };
            eprintln!(
                "[snens] ход {turn}: ↑{} (кеш {}) ↓{}{cost}",
                reply.usage.in_tok, reply.usage.cached_tok, reply.usage.out_tok
            );
        }

        if reply.calls.is_empty() {
            return Ok(());
        }

        for call in &reply.calls {
            let out = dispatch(call, cfg, &reg, &path, &out_dir).await;
            eprintln!(
                "[{}] {} → exit={} {} Б {} мс{}",
                call.name,
                hint(&call.args),
                out.exit,
                out.bytes,
                out.ms,
                if out.timed_out { " (таймаут)" } else { "" }
            );
            // Модель видит результат целиком, человек — голову: без этого в
            // терминале не понять, что агент делает, пока он делает.
            if let Some(p) = preview(&out.text) {
                use std::io::Write;
                println!("{p}");
                let _ = std::io::stdout().flush();
            }
            sess.append(Rec::ToolResult {
                ts: log::now(),
                call_id: call.id.clone(),
                tool: call.name.clone(),
                argv: vec![call.args.clone()],
                exit: out.exit,
                bytes: out.bytes,
                ms: out.ms,
                body: Body::Inline { text: out.text },
            })?;
        }

        if turn > 50 {
            return Err("цикл прерван: больше 50 ходов на один запуск".into());
        }
    }
}

/// Сколько строк результата показывать человеку в терминале.
const PREVIEW_LINES: usize = 15;

/// Что именно вызвано — одной строкой. Аргументы знает и модель; это для того,
/// кто смотрит в терминал.
fn hint(args: &str) -> String {
    let a: Value = serde_json::from_str(args).unwrap_or(Value::Null);
    let raw = match a.get("command").and_then(|c| c.as_str()) {
        Some(c) => c.to_string(),
        None => match a.get("args") {
            Some(Value::Array(v)) => v
                .iter()
                .map(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| x.to_string())
                })
                .collect::<Vec<_>>()
                .join(" "),
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        },
    };
    // Многострочную команду в одну строку: статус — это одна строка.
    let flat = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let s: String = flat.chars().take(120).collect();
    if flat.chars().count() > 120 {
        format!("{s}…")
    } else {
        s
    }
}

/// Живой поток для человека: модель видит полный результат, человек — голову.
fn preview(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return None;
    }
    let mut s = lines
        .iter()
        .take(PREVIEW_LINES)
        .map(|l| format!("  {}", crate::exec::truncate(l, 300)))
        .collect::<Vec<_>>()
        .join("\n");
    if lines.len() > PREVIEW_LINES {
        s.push_str(&format!("\n  … ещё {} строк", lines.len() - PREVIEW_LINES));
    }
    Some(s)
}

/// Сколько результатов проекция уже вырезала по последней записи журнала.
fn last_elided(log: &[Rec]) -> usize {
    log.iter()
        .rev()
        .find_map(|r| match r {
            Rec::Ctx { elided, .. } => Some(*elided),
            _ => None,
        })
        .unwrap_or(0)
}

/// Единственная проверка, которая падает, если цикл сломан: ход с tool_call →
/// результат → финальный ответ, плюс реестр (хелп-контракт, argv, stdin, PATH)
/// и корректная проекция.
pub async fn selftest() -> R<()> {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("snens-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    // Фальшивый тул: реестр, хелп-контракт, argv, stdin, PATH — всё настоящее.
    let tdir = dir.join("tools/probe");
    std::fs::create_dir_all(&tdir)?;
    let probe = tdir.join("probe");
    std::fs::write(
        &probe,
        "#!/bin/sh\n\
         if [ \"$1\" = \"--help\" ]; then\n\
         \x20 echo \"probe — тестовый тул: печатает argv и stdin.\"\n\
         \x20 echo \"probe [args...]\"\n\
         \x20 exit 0\n\
         fi\n\
         echo \"args:$*\"\n\
         cat\n",
    )?;
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755))?;

    let mut cfg = Config::default();
    cfg.sessions_dir = dir.to_string_lossy().into_owned();
    cfg.tools_dir = dir.join("tools").to_string_lossy().into_owned();

    let (defs, reg, path) = prepare(&cfg).await?;
    assert!(reg.iter().any(|t| t.name == "probe"), "реестр не нашёл тул");
    let pd = defs
        .iter()
        .find(|d| d["function"]["name"] == "probe")
        .expect("нет определения probe");
    let desc = pd["function"]["description"].as_str().unwrap_or_default();
    assert!(desc.starts_with("probe —"), "описание не из --help: {desc}");
    assert!(desc.contains("probe [args...]"), "в описание не попал usage");
    assert!(
        std::fs::symlink_metadata(cfg.bin_path().join("probe")).is_ok(),
        "нет симлинка в PATH-каталоге"
    );
    let bin_s = cfg.bin_path().to_string_lossy().into_owned();
    assert!(
        path.starts_with(&bin_s),
        "PATH детей не начинается с каталога симлинков: {path}"
    );

    let sess = Session::open(&dir, "t")?;
    sess.append(Rec::User {
        ts: log::now(),
        text: "проверь тулы".into(),
        source: "selftest".into(),
    })?;

    let fake = Fake::new(vec![
        Reply {
            text: String::new(),
            calls: vec![Call {
                id: "c1".into(),
                name: "probe".into(),
                args: r#"{"args":["a","b"],"stdin":"привет"}"#.into(),
            }],
            usage: Usage::default(),
        },
        Reply {
            text: String::new(),
            calls: vec![Call {
                id: "c2".into(),
                name: "nosuch".into(),
                args: r#"{"args":[]}"#.into(),
            }],
            usage: Usage::default(),
        },
        Reply {
            text: String::new(),
            calls: vec![Call {
                id: "c3".into(),
                name: "run".into(),
                args: r#"{"command":"probe via-path"}"#.into(),
            }],
            usage: Usage::default(),
        },
        Reply {
            text: "готово".into(),
            calls: vec![],
            usage: Usage::default(),
        },
    ]);

    run_agent(&sess, &cfg, &fake).await?;
    let recs = sess.read()?;
    let tool_text = |call: &str| -> Option<String> {
        recs.iter().find_map(|r| match r {
            Rec::ToolResult {
                tool,
                body: Body::Inline { text },
                ..
            } if tool == call => Some(text.clone()),
            _ => None,
        })
    };

    let probe_out = tool_text("probe").expect("нет результата probe");
    assert!(probe_out.contains("args:a b"), "argv не доехал: {probe_out}");
    assert!(probe_out.contains("привет"), "stdin не доехал: {probe_out}");
    assert!(
        tool_text("nosuch").unwrap_or_default().contains("нет в реестре"),
        "неизвестный тул не дал понятной ошибки"
    );
    let via_path = tool_text("run").unwrap_or_default();
    assert!(
        via_path.contains("args:via-path"),
        "тул не виден из PATH внутри run: {via_path}"
    );
    assert!(
        recs.iter().rev().any(|r| matches!(
            r,
            Rec::Assistant { text: Some(t), tool_calls, .. } if tool_calls.is_empty() && t.contains("готово")
        )),
        "нет финального ответа"
    );

    let msgs = project::msgs(&recs, &cfg);
    assert!(msgs.iter().any(|m| m["role"] == "tool"), "проекция потеряла tool-сообщение");
    assert_eq!(msgs[0]["role"], "system", "проекция должна начинаться с system");

    // повторная проекция обязана быть байт-в-байт той же (иначе кеш промахивается)
    let a = serde_json::to_string(&project::msgs(&recs, &cfg))?;
    let b = serde_json::to_string(&project::msgs(&sess.read()?, &cfg))?;
    assert_eq!(a, b, "проекция недетерминирована");

    // --- компакция: элизия старых результатов при переполнении окна ---
    let mut cfg2 = cfg.clone();
    cfg2.ctx_window = 400; // крошечное окно: мягкий порог пройден сразу
    cfg2.keep_results = 1;
    let sess2 = Session::open(&dir, "t2")?;
    sess2.append(Rec::User {
        ts: log::now(),
        text: "два больших вывода".into(),
        source: "selftest".into(),
    })?;
    let big = || Reply {
        text: String::new(),
        calls: vec![Call {
            id: "b".into(),
            name: "run".into(),
            args: r#"{"command":"seq 1 3000"}"#.into(),
        }],
        usage: Usage::default(),
    };
    let fake2 = Fake::new(vec![
        big(),
        big(),
        Reply {
            text: "готово".into(),
            calls: vec![],
            usage: Usage::default(),
        },
    ]);
    run_agent(&sess2, &cfg2, &fake2).await?;
    let recs2 = sess2.read()?;
    assert!(
        recs2.iter().any(|r| matches!(r, Rec::Ctx { .. })),
        "компакция не записала Ctx в журнал"
    );
    let msgs2 = project::msgs(&recs2, &cfg2);
    let tool_msgs: Vec<&Value> = msgs2.iter().filter(|m| m["role"] == "tool").collect();
    assert_eq!(
        tool_msgs.len(),
        2,
        "элизия не должна выбрасывать сообщения: assistant с tool_calls обязан иметь ответы"
    );
    let first = tool_msgs[0]["content"].as_str().unwrap_or_default();
    let last = tool_msgs[1]["content"].as_str().unwrap_or_default();
    assert!(first.contains("вырезан при компакции"), "старый результат не вырезан: {first}");
    assert!(last.starts_with('1'), "последний результат должен остаться как есть");
    let raw_bytes: usize = recs2
        .iter()
        .filter_map(|r| match r {
            Rec::ToolResult {
                body: Body::Inline { text },
                ..
            } => Some(text.len()),
            _ => None,
        })
        .sum();
    assert!(
        project::estimate_tokens(&msgs2) * 3 < raw_bytes as u64,
        "элизия не уменьшила проекцию: {} Б против {raw_bytes} Б в логе",
        project::estimate_tokens(&msgs2) * 3
    );
    assert_eq!(
        serde_json::to_string(&project::msgs(&recs2, &cfg2))?,
        serde_json::to_string(&project::msgs(&sess2.read()?, &cfg2))?,
        "проекция с элизией недетерминирована"
    );

    std::fs::remove_dir_all(&dir).ok();
    println!(
        "selftest ok: {} тулов в реестре, {} записей, {} сообщений в проекции, ~{} токенов",
        reg.len(),
        recs.len(),
        msgs.len(),
        project::estimate_tokens(&msgs)
    );
    Ok(())
}
