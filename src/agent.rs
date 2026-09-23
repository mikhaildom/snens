// Цикл: запрос → стрим → диспатч → append. Состояние — лог, поэтому resume бесплатен:
// незавершённый ход = User без Assistant, продолжение с того же места.
use serde_json::Value;

use crate::{
    cfg::Config,
    dispatch,
    llm::{Fake, Llm, Reply, Usage},
    log,
    log::{Body, Call, Rec, Session},
    memory,
    project, R,
};

pub async fn run_agent<L: Llm>(
    sess: &Session,
    cfg: &Config,
    llm: &L,
    ui: &crate::render::Ui,
) -> R<()> {
    // Промпт сессии фиксируется в логе один раз и дальше читается оттуда: файлы
    // инстанса (soul/user/memory, скиллы) правятся по ходу, и пересборка на каждом
    // ходу промахивалась бы мимо префикс-кеша. Здесь — единственная точка, через
    // которую проходят все сессии, поэтому и запись здесь.
    if !sess.read()?.iter().any(|r| matches!(r, Rec::System { .. })) {
        sess.append(Rec::System {
            ts: log::now(),
            text: crate::cfg::system_prompt(cfg),
        })?;
    }
    let (defs, reg, path) = dispatch::prepare(cfg).await?;
    // Справка памяти впрыскивается ровно один раз, перед первым ходом сессии, и
    // записью в лог — проекция дальше читает её оттуда, как всё остальное.
    memory::recall_note(sess, cfg, &path).await;
    // Полные выводы обрезанных команд: модель получает путь и может дочитать.
    let out_dir = sess.path().with_extension("out");
    let mut turn = 0u32;
    // Пустой ответ — не конец работы, а сбой (обрыв стрима, непарсимый формат
    // вызова). Считаем подряд: первый повторяем запросом, второй — выходим явной
    // ошибкой, чтобы прогон не закрылся «успешно», ничего не сделав.
    let mut empty_replies = 0u32;
    loop {
        turn += 1;
        let log = sess.read()?;
        let (msgs, report) = project::project(&log, cfg);
        let approx = project::estimate_tokens(&msgs);
        // Оба порога пока только сигналят: это точки, где должна сработать компакция (M3).
        let ratio = approx as f64 / cfg.ctx_window as f64;
        if ratio > cfg.hard {
            eprintln!(
                "[snens] ~{approx} токенов — пройден порог саммаризации ({:.0}%), саммари ещё нет",
                cfg.hard * 100.0
            );
        } else if ratio > cfg.soft {
            eprintln!(
                "[snens] ~{approx} токенов — пройден порог элизии ({:.0}%)",
                cfg.soft * 100.0
            );
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

        let reply: Reply = stream_reply(cfg, msgs, defs.clone(), llm, ui).await?;

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

        // Пусто (ни текста, ни вызовов) — это сбой, а не «работа кончилась»:
        // провайдер мог оборвать стрим или прислать вызов в непарсимом формате.
        // Ход пишем только записью расхода (пустая assistant-запись сбила бы
        // проекцию), делаем один повтор; второй пустой подряд — явная ошибка,
        // иначе прогон закрывается «успешно», ничего не сделав.
        if reply.calls.is_empty() && reply.text.trim().is_empty() {
            sess.append(usage_rec(&reply))?;
            empty_replies += 1;
            let hint = if reply.finish_reason.is_empty() {
                "провайдер не сообщил finish_reason".to_string()
            } else {
                format!("finish_reason={}", reply.finish_reason)
            };
            if empty_replies >= 2 {
                return Err(format!(
                    "пустой ответ модели дважды подряд (ход {turn}, {hint}) — \
                     прерываю, работа не сделана"
                )
                .into());
            }
            eprintln!("[snens] пустой ответ модели ({hint}), повтор запроса");
            continue;
        }
        empty_replies = 0;

        sess.append(Rec::Assistant {
            ts: log::now(),
            text: if reply.text.is_empty() {
                None
            } else {
                Some(reply.text.clone())
            },
            tool_calls: reply.calls.clone(),
            reasoning: if reply.reasoning.is_empty() {
                None
            } else {
                Some(reply.reasoning.clone())
            },
        })?;
        sess.append(usage_rec(&reply))?;

        if reply.calls.is_empty() {
            memory::retain_turn(sess, cfg, &path, &reply.text).await;
            return Ok(());
        }

        for call in &reply.calls {
            let out = dispatch::dispatch(call, cfg, &reg, &path, &out_dir).await;
            ui.tool_call(
                &call.name,
                &hint(&call.args),
                out.exit,
                out.bytes,
                out.ms,
                if out.timed_out {
                    " (таймаут)"
                } else {
                    ""
                },
            );
            // Модель видит результат целиком, человек — голову: без этого в
            // терминале не понять, что агент делает, пока он делает.
            if let Some(p) = preview(&out.text) {
                ui.tool_result(&p);
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            // Человеку в терминал: вложение не покажешь текстом, поэтому говорим,
            // что оно есть и откуда его взять.
            if !out.parts.is_empty() {
                for p in &out.parts {
                    println!(
                        "  [вложение] {} {}x{} {} Б — {}",
                        p.kind, p.width, p.height, p.bytes, p.path
                    );
                }
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
                parts: out.parts,
            })?;
        }

        // Лимит исчерпан — не падаем и не закрываем поверхность: просим
        // промежуточный итог и на этом заканчиваем запуск (см. final_answer).
        if turn >= cfg.max_turns {
            final_answer(sess, cfg, llm, ui, &path, turn).await?;
            return Ok(());
        }
    }
}

/// Просьба к модели при исчерпании лимита: инструменты уже не исполняются.
const LIMIT_NOTE: &str = "Достигнут лимит вызова инструментов — инструменты больше не \
исполняются. Напиши промежуточный итог: что уже сделано и что осталось сделать. \
Просто ответь текстом, не вызывай инструменты.";

/// Достойный выход по лимиту ходов: один запрос без инструментов, ответ модели
/// ложится обычной записью assistant. Если она всё же прислала вызовы, они
/// отбрасываются — лог обязан остаться валидным (assistant с tool_calls без
/// ответов провайдер отвергает).
async fn final_answer<L: Llm>(
    sess: &Session,
    cfg: &Config,
    llm: &L,
    ui: &crate::render::Ui,
    path: &str,
    turn: u32,
) -> R<()> {
    sess.append(Rec::User {
        ts: log::now(),
        text: LIMIT_NOTE.into(),
        source: "limit".into(),
    })?;
    let log = sess.read()?;
    let (msgs, _) = project::project(&log, cfg);
    let reply = stream_reply(cfg, msgs, Vec::new(), llm, ui).await?;
    sess.append(Rec::Assistant {
        ts: log::now(),
        text: if reply.text.is_empty() {
            None
        } else {
            Some(reply.text.clone())
        },
        tool_calls: Vec::new(),
        reasoning: if reply.reasoning.is_empty() {
            None
        } else {
            Some(reply.reasoning.clone())
        },
    })?;
    sess.append(usage_rec(&reply))?;
    ui.notice(&format!("лимит {turn} ходов: запрошен промежуточный итог"));
    memory::retain_turn(sess, cfg, path, &reply.text).await;
    Ok(())
}

/// Один запрос к модели с живым рендером: ответ и рассуждение стримятся через
/// `Stream`, таблицы склеиваются в рамку. Здесь же — единственное место, где
/// решение «показывать ли рассуждение» спрашивается у `Ui`.
async fn stream_reply<L: Llm>(
    cfg: &Config,
    msgs: Vec<Value>,
    tools: Vec<Value>,
    llm: &L,
    ui: &crate::render::Ui,
) -> R<Reply> {
    let mut stream = crate::render::Stream::new(ui.color);
    let show = ui.show_reasoning();
    let mut reason_on = false;
    let reply = llm
        .reply(cfg, msgs, tools, &mut |d| match d {
            crate::llm::Delta::Reasoning(r) => {
                if !reason_on {
                    reason_on = true;
                    stream.reasoning(show);
                }
                if show {
                    stream.reasoning_chunk(r);
                }
            }
            crate::llm::Delta::Content(c) => stream.content(c),
        })
        .await?;
    stream.finish();
    Ok(reply)
}

/// Запись расхода хода. Отпечаток апстрима кладём рядом с токенами: промах
/// кеша ищется по тому, сменился ли между ходами бэкенд провайдера.
fn usage_rec(reply: &Reply) -> Rec {
    Rec::Usage {
        ts: log::now(),
        in_tok: reply.usage.in_tok,
        cached_tok: reply.usage.cached_tok,
        out_tok: reply.usage.out_tok,
        cost_micro: reply.usage.cost_micro,
        upstream_id: reply.upstream.id.clone(),
        fingerprint: reply.upstream.fingerprint.clone(),
        finish_reason: reply.finish_reason.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Fake;

    /// Исчерпание лимита ходов — не ошибка и не закрытие поверхности: цикл
    /// просит у модели промежуточный итог (запись `limit`) и возвращает Ok,
    /// а вызовы, которые модель всё равно прислала, не исполняются.
    #[tokio::test]
    async fn turn_limit_asks_for_summary_instead_of_failing() -> R<()> {
        let dir = std::env::temp_dir().join(format!("snens-limit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let cfg = Config {
            sessions_dir: dir.to_string_lossy().into_owned(),
            tools_dir: dir.join("tools").to_string_lossy().into_owned(),
            max_turns: 2,
            ..Config::default()
        };
        let sess = Session::open(&dir, "t")?;
        sess.append(Rec::User {
            ts: log::now(),
            text: "поработай".into(),
            source: "test".into(),
        })?;
        let run_once = || Reply {
            reasoning: String::new(),
            upstream: Default::default(),
            text: String::new(),
            calls: vec![Call {
                id: "c".into(),
                name: "run".into(),
                args: r#"{"command":"true"}"#.into(),
            }],
            usage: Usage::default(),
            finish_reason: String::new(),
        };
        let fake = Fake::new(vec![
            run_once(),
            run_once(),
            // Ответ на просьбу о промежуточном итоге.
            Reply {
                reasoning: String::new(),
                upstream: Default::default(),
                text: "сделал половину".into(),
                calls: vec![],
                usage: Usage::default(),
                finish_reason: String::new(),
            },
        ]);
        // Ok, а не Err: поверхность остаётся жива.
        run_agent(&sess, &cfg, &fake, &crate::render::Ui::plain()).await?;
        let recs = sess.read()?;
        assert!(
            recs.iter()
                .any(|r| matches!(r, Rec::User { source, .. } if source == "limit")),
            "нет записи о лимите"
        );
        assert!(
            recs.iter().rev().any(|r| matches!(
                r,
                Rec::Assistant { text: Some(t), tool_calls, .. }
                    if tool_calls.is_empty() && t.contains("сделал половину")
            )),
            "промежуточный итог не записан"
        );
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    fn cfg_tmp(dir: &std::path::Path) -> Config {
        Config {
            sessions_dir: dir.to_string_lossy().into_owned(),
            tools_dir: dir.join("tools").to_string_lossy().into_owned(),
            ..Config::default()
        }
    }

    fn text_reply(text: &str) -> Reply {
        Reply {
            reasoning: String::new(),
            upstream: Default::default(),
            text: text.into(),
            calls: vec![],
            usage: Usage::default(),
            finish_reason: String::new(),
        }
    }

    /// Пустой ответ (обрыв стрима/непарсимый вызов) не закрывает сессию «успешно»:
    /// первый пустой — повтор, затем честный ответ записывается в лог.
    #[tokio::test]
    async fn empty_reply_is_retried_not_treated_as_done() -> R<()> {
        let dir = std::env::temp_dir().join(format!("snens-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let cfg = cfg_tmp(&dir);
        let sess = Session::open(&dir, "e")?;
        sess.append(Rec::User {
            ts: log::now(),
            text: "поработай".into(),
            source: "test".into(),
        })?;
        let fake = Fake::new(vec![text_reply(""), text_reply("готово")]);
        run_agent(&sess, &cfg, &fake, &crate::render::Ui::plain()).await?;
        let recs = sess.read()?;
        // Пустая assistant-запись не попала в лог, честный ответ — попал.
        assert!(
            !recs
                .iter()
                .any(|r| matches!(r, Rec::Assistant { text: None, tool_calls, .. } if tool_calls.is_empty())),
            "пустой ответ записан в лог как ход"
        );
        assert!(
            recs.iter().rev().any(|r| matches!(
                r,
                Rec::Assistant { text: Some(t), tool_calls, .. } if tool_calls.is_empty() && t == "готово"
            )),
            "честный ответ не записан"
        );
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Два пустых подряд — явная ошибка, а не тихое закрытие: прогон не должен
    /// выглядеть успешным, ничего не сделав.
    #[tokio::test]
    async fn double_empty_reply_fails_loudly() -> R<()> {
        let dir = std::env::temp_dir().join(format!("snens-empty2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let cfg = cfg_tmp(&dir);
        let sess = Session::open(&dir, "e2")?;
        sess.append(Rec::User {
            ts: log::now(),
            text: "поработай".into(),
            source: "test".into(),
        })?;
        let fake = Fake::new(vec![text_reply(""), text_reply("")]);
        let err = run_agent(&sess, &cfg, &fake, &crate::render::Ui::plain()).await;
        assert!(err.is_err(), "два пустых ответа не дали ошибку");
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
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
         \x20 echo \"probe [args...] (stdin)\"\n\
         \x20 exit 0\n\
         fi\n\
         echo \"args:$*\"\n\
         cat\n",
    )?;
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755))?;

    let cfg = Config {
        sessions_dir: dir.to_string_lossy().into_owned(),
        tools_dir: dir.join("tools").to_string_lossy().into_owned(),
        ..Config::default()
    };

    let (defs, reg, path) = dispatch::prepare(&cfg).await?;
    assert!(reg.iter().any(|t| t.name == "probe"), "реестр не нашёл тул");
    let pd = defs
        .iter()
        .find(|d| d["function"]["name"] == "probe")
        .expect("нет определения probe");
    let desc = pd["function"]["description"].as_str().unwrap_or_default();
    assert!(desc.starts_with("probe —"), "описание не из --help: {desc}");
    assert!(
        desc.contains("probe [args...]"),
        "в описание не попал usage"
    );
    // usage со словом «stdin» → требование stdin доезжает в схему, а не через exit=1.
    assert!(
        desc.contains("stdin обязателен"),
        "в описание не попал признак stdin: {desc}"
    );
    let req = pd["function"]["parameters"]["required"]
        .as_array()
        .expect("required не массив");
    assert!(
        req.iter().any(|v| v == "stdin"),
        "stdin не помечен обязательным"
    );
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
            reasoning: String::new(),
            upstream: Default::default(),
            text: String::new(),
            calls: vec![Call {
                id: "c1".into(),
                name: "probe".into(),
                args: r#"{"args":["a","b"],"stdin":"привет"}"#.into(),
            }],
            usage: Usage::default(),
            finish_reason: String::new(),
        },
        Reply {
            reasoning: String::new(),
            upstream: Default::default(),
            text: String::new(),
            calls: vec![Call {
                id: "c2".into(),
                name: "nosuch".into(),
                args: r#"{"args":[]}"#.into(),
            }],
            usage: Usage::default(),
            finish_reason: String::new(),
        },
        Reply {
            reasoning: String::new(),
            upstream: Default::default(),
            text: String::new(),
            calls: vec![Call {
                id: "c3".into(),
                name: "run".into(),
                args: r#"{"command":"probe via-path"}"#.into(),
            }],
            usage: Usage::default(),
            finish_reason: String::new(),
        },
        Reply {
            reasoning: String::new(),
            upstream: Default::default(),
            text: "готово".into(),
            calls: vec![],
            usage: Usage::default(),
            finish_reason: String::new(),
        },
    ]);

    run_agent(&sess, &cfg, &fake, &crate::render::Ui::plain()).await?;
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
    assert!(
        probe_out.contains("args:a b"),
        "argv не доехал: {probe_out}"
    );
    assert!(probe_out.contains("привет"), "stdin не доехал: {probe_out}");
    assert!(
        tool_text("nosuch")
            .unwrap_or_default()
            .contains("нет в реестре"),
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
    assert!(
        msgs.iter().any(|m| m["role"] == "tool"),
        "проекция потеряла tool-сообщение"
    );
    assert_eq!(
        msgs[0]["role"], "system",
        "проекция должна начинаться с system"
    );
    // Промпт сессии записан в лог один раз и проекция берёт его оттуда, а не
    // собирает заново: иначе правка файлов инстанса по ходу сессии сдвинула бы
    // начало промпта и убила префикс-кеш.
    let systems: Vec<&str> = recs
        .iter()
        .filter_map(|r| match r {
            Rec::System { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(systems.len(), 1, "запись System должна быть ровно одна");
    assert_eq!(
        msgs[0]["content"].as_str().unwrap_or_default(),
        systems[0],
        "проекция собрала промпт заново вместо чтения из лога"
    );

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
        reasoning: String::new(),
        upstream: Default::default(),
        text: String::new(),
        calls: vec![Call {
            id: "b".into(),
            name: "run".into(),
            args: r#"{"command":"seq 1 3000"}"#.into(),
        }],
        usage: Usage::default(),
        finish_reason: String::new(),
    };
    let fake2 = Fake::new(vec![
        big(),
        big(),
        Reply {
            reasoning: String::new(),
            upstream: Default::default(),
            text: "готово".into(),
            calls: vec![],
            usage: Usage::default(),
            finish_reason: String::new(),
        },
    ]);
    run_agent(&sess2, &cfg2, &fake2, &crate::render::Ui::plain()).await?;
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
    assert!(
        first.contains("вырезан при компакции"),
        "старый результат не вырезан: {first}"
    );
    assert!(
        last.starts_with('1'),
        "последний результат должен остаться как есть"
    );
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

    // --- долгая память: ядро знает только argv-контракт, провайдер — тул ---
    let mdir = dir.join("tools/memory");
    std::fs::create_dir_all(&mdir)?;
    let mbin = mdir.join("memory");
    let capture = dir.join("turn.json");
    std::fs::write(
        &mbin,
        format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
             \x20 --help) echo \"memory — тестовый провайдер памяти\"; echo \"memory recall|retain\"; exit 0;;\n\
             \x20 recall) echo \"факт: $2\"; exit 0;;\n\
             \x20 retain) cat > {}; echo \"op t1\"; exit 0;;\n\
             esac\n",
            capture.display()
        ),
    )?;
    std::fs::set_permissions(&mbin, std::fs::Permissions::from_mode(0o755))?;

    let mut cfg3 = cfg.clone();
    // Канал выключен по умолчанию: без программы ядро не зовёт ничего.
    let sess_off = Session::open(&dir, "t3-off")?;
    sess_off.append(Rec::User {
        ts: log::now(),
        text: "проверь память".into(),
        source: "selftest".into(),
    })?;
    let fake_off = Fake::new(vec![Reply {
        reasoning: String::new(),
        upstream: Default::default(),
        text: "готово".into(),
        calls: vec![],
        usage: Usage::default(),
        finish_reason: String::new(),
    }]);
    run_agent(&sess_off, &cfg3, &fake_off, &crate::render::Ui::plain()).await?;
    assert!(
        !sess_off
            .read()?
            .iter()
            .any(|r| matches!(r, Rec::Recall { .. })),
        "выключенный канал всё равно впрыснул справку"
    );

    cfg3.memory.cmd = "memory".into();
    let sess3 = Session::open(&dir, "t3")?;
    sess3.append(Rec::User {
        ts: log::now(),
        text: "проверь память".into(),
        source: "selftest".into(),
    })?;
    let fake3 = Fake::new(vec![Reply {
        reasoning: String::new(),
        upstream: Default::default(),
        text: "готово".into(),
        calls: vec![],
        usage: Usage::default(),
        finish_reason: String::new(),
    }]);
    run_agent(&sess3, &cfg3, &fake3, &crate::render::Ui::plain()).await?;
    let recs3 = sess3.read()?;
    let recall = recs3
        .iter()
        .find_map(|r| match r {
            Rec::Recall { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("справка памяти не записана в журнал");
    assert!(
        recall.contains("факт: проверь память"),
        "запрос не доехал до провайдера: {recall}"
    );
    assert!(
        project::msgs(&recs3, &cfg3)
            .iter()
            .any(|m| m["role"] == "system"
                && m["content"].as_str().unwrap_or_default().contains("факт:")),
        "справка не попала в проекцию"
    );
    let sent = std::fs::read_to_string(&capture).unwrap_or_default();
    assert!(
        sent.contains("проверь память") && sent.contains("готово"),
        "ход не доехал до retain: {sent}"
    );
    // Ответ записан ровно один раз: повторный прогон не дублирует справку.
    assert_eq!(
        recs3
            .iter()
            .filter(|r| matches!(r, Rec::Recall { .. }))
            .count(),
        1,
        "справка впрыснута больше одного раза"
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
