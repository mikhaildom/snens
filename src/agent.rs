// Цикл: запрос → стрим → диспатч → append. Состояние — лог, поэтому resume бесплатен:
// незавершённый ход = User без Assistant, продолжение с того же места.
use serde_json::json;

use crate::{
    cfg::Config,
    exec, log,
    log::{Body, Call, Rec, Session},
    llm::{Fake, Llm, Reply, Usage},
    project,
    R,
};

pub fn tools() -> Vec<serde_json::Value> {
    vec![json!({
        "type": "function",
        "function": {
            "name": "run",
            "description": "Выполнить shell-команду (sh -c) и получить вывод: stdout, затем stderr, и код возврата.",
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

/// Ошибки инструмента не прерывают ход: они возвращаются как результат,
/// чтобы модель могла исправиться.
pub async fn dispatch(call: &Call, cfg: &Config) -> exec::Out {
    let fail = |msg: String| exec::Out {
        exit: -1,
        text: msg,
        bytes: 0,
        ms: 0,
        timed_out: false,
    };
    match call.name.as_str() {
        "run" => {
            let a: serde_json::Value =
                serde_json::from_str(&call.args).unwrap_or_else(|_| json!({}));
            let Some(command) = a.get("command").and_then(|c| c.as_str()) else {
                return fail("run: не передан command".into());
            };
            if command.trim().is_empty() {
                return fail("run: пустая команда".into());
            }
            let cwd = a.get("cwd").and_then(|c| c.as_str());
            let t = a
                .get("timeout_seconds")
                .and_then(|t| t.as_u64())
                .unwrap_or(cfg.timeout_seconds);
            match exec::run(command, cwd, t, cfg.max_process_chars).await {
                Ok(o) => o,
                Err(e) => fail(format!("run: {e}")),
            }
        }
        other => fail(format!("неизвестный инструмент: {other}")),
    }
}

pub async fn run_agent<L: Llm>(sess: &Session, cfg: &Config, llm: &L) -> R<()> {
    let tools = tools();
    let mut turn = 0u32;
    loop {
        turn += 1;
        let log = sess.read()?;
        // ponytail: компакция (элизия → саммари) вставляется здесь, перед вызовом модели (M3)
        let msgs = project::msgs(&log, cfg);
        let approx = project::estimate_tokens(&msgs);
        if approx as f64 > cfg.ctx_window as f64 * cfg.hard {
            eprintln!(
                "[snens] контекст ~{approx} токенов при окне {} — компакция ещё не реализована",
                cfg.ctx_window
            );
        }

        let reply: Reply = llm
            .reply(cfg, msgs, tools.clone(), &mut |d| {
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

        if reply.calls.is_empty() {
            return Ok(());
        }

        for call in &reply.calls {
            let out = dispatch(call, cfg).await;
            eprintln!(
                "[{}] exit={} {} Б {} мс{}",
                call.name,
                out.exit,
                out.bytes,
                out.ms,
                if out.timed_out { " (таймаут)" } else { "" }
            );
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

/// Единственная проверка, которая падает, если цикл сломан:
/// ход с tool_call → результат → финальный ответ, плюс корректная проекция.
pub async fn selftest() -> R<()> {
    let dir = std::env::temp_dir().join(format!("snens-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let mut cfg = Config::default();
    cfg.sessions_dir = dir.to_string_lossy().into_owned();

    let sess = Session::open(&dir, "t")?;
    sess.append(Rec::User {
        ts: log::now(),
        text: "выполни echo hi и скажи готово".into(),
        source: "selftest".into(),
    })?;

    let fake = Fake::new(vec![
        Reply {
            text: String::new(),
            calls: vec![Call {
                id: "c1".into(),
                name: "run".into(),
                args: r#"{"command": "echo hi"}"#.into(),
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

    assert!(
        recs.iter()
            .any(|r| matches!(r, Rec::Assistant { tool_calls, .. } if tool_calls.len() == 1)),
        "нет хода с tool_call"
    );
    assert!(
        recs.iter().any(|r| matches!(
            r,
            Rec::ToolResult { body: Body::Inline { text }, exit: 0, .. } if text.contains("hi")
        )),
        "нет результата run с выводом команды"
    );
    assert!(
        recs.iter().rev().any(|r| matches!(
            r,
            Rec::Assistant { text: Some(t), tool_calls, .. } if tool_calls.is_empty() && t.contains("готово")
        )),
        "нет финального ответа"
    );

    let msgs = project::msgs(&recs, &cfg);
    assert!(
        msgs.iter().any(|m| m["role"] == "tool"),
        "проекция потеряла tool-сообщение"
    );
    assert_eq!(msgs[0]["role"], "system", "проекция должна начинаться с system");

    // повторная проекция обязана быть байт-в-байт той же (иначе кеш промахивается)
    let a = serde_json::to_string(&project::msgs(&recs, &cfg))?;
    let b = serde_json::to_string(&project::msgs(&sess.read()?, &cfg))?;
    assert_eq!(a, b, "проекция недетерминирована");

    std::fs::remove_dir_all(&dir).ok();
    println!(
        "selftest ok: {} записей, {} сообщений в проекции, ~{} токенов",
        recs.len(),
        msgs.len(),
        project::estimate_tokens(&msgs)
    );
    Ok(())
}