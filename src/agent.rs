// Цикл: запрос → стрим → диспатч → append. Состояние — лог, поэтому resume бесплатен:
// незавершённый ход = User без Assistant, продолжение с того же места.
use serde_json::Value;

use crate::{
    cfg::Config,
    dispatch,
    llm::{Llm, Reply},
    log,
    log::{Body, Rec, Session},
    mail, memory, project, R,
};

/// Исход прогона: `ok` — модель закончила сама, `limit` — упёрлась в лимит ходов
/// (цикл не падал, а попросил промежуточный итог).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Finish {
    Ok,
    Limit,
}

impl Finish {
    fn status(self) -> &'static str {
        match self {
            Finish::Ok => "ok",
            Finish::Limit => "limit",
        }
    }
}

/// Точка входа цикла. Терминатор `Done` пишет харнесс, а не модель: по нему
/// родитель узнаёт о конце подагента даже если тот «забыл» отчитаться. Пишется
/// на всех путях — и при нормальном финале, и при ошибке.
pub async fn run_agent<L: Llm>(
    sess: &Session,
    cfg: &Config,
    llm: &L,
    ui: &crate::render::Ui,
) -> R<()> {
    let r = run_loop(sess, cfg, llm, ui).await;
    let status = match &r {
        Ok(f) => f.status(),
        Err(_) => "error",
    };
    let turns = sess
        .read()
        .map(|rs| {
            rs.iter()
                .filter(|x| matches!(x, Rec::Assistant { .. }))
                .count() as u32
        })
        .unwrap_or(0);
    let _ = sess.append(Rec::Done {
        ts: log::now(),
        status: status.to_string(),
        turns,
    });
    r.map(|_| ())
}

async fn run_loop<L: Llm>(
    sess: &Session,
    cfg: &Config,
    llm: &L,
    ui: &crate::render::Ui,
) -> R<Finish> {
    // Промпт сессии фиксируется в логе один раз и дальше читается оттуда: файлы
    // инстанса (soul/user/memory, скиллы) правятся по ходу, и пересборка на каждом
    // ходу промахивалась бы мимо префикс-кеша. Здесь — единственная точка, через
    // которую проходят все сессии, поэтому и запись здесь.
    if !sess.read()?.iter().any(|r| matches!(r, Rec::System { .. })) {
        sess.append(Rec::System {
            ts: log::now(),
            text: crate::prompt::system_prompt(cfg),
        })?;
    }
    let (defs, reg, path) = dispatch::prepare(cfg).await?;
    // Справка памяти впрыскивается ровно один раз, перед первым ходом сессии, и
    // записью в лог — проекция дальше читает её оттуда, как всё остальное.
    memory::recall_note(sess, cfg, &path).await;
    // Полные выводы обрезанных команд: модель получает путь и может дочитать.
    let out_dir = sess.path().with_extension("out");
    let mail_dir = cfg.mail_path();
    let mut turn = 0u32;
    // Пустой ответ — не конец работы, а сбой (обрыв стрима, непарсимый формат
    // вызова). Считаем подряд: первый повторяем запросом, второй — выходим явной
    // ошибкой, чтобы прогон не закрылся «успешно», ничего не сделав.
    let mut empty_replies = 0u32;
    loop {
        turn += 1;
        // Граница хода: после всех результатов предыдущего шага и перед новым
        // запросом. Инвариант провайдера (assistant с tool_calls обязан иметь
        // ответы) уже соблюдён, поэтому сюда безопасно впрыскивать чужое слово —
        // steer человека или сообщение родителя/ребёнка. В лог — отдельной
        // записью, чтобы резюме не доставило его второй раз.
        for msg in mail::drain(&mail_dir, sess.id()) {
            if msg.text.trim().is_empty() {
                continue;
            }
            let _ = sess.append(Rec::Steer {
                ts: log::now(),
                from: msg.from,
                via: msg.kind,
                text: msg.text,
            });
        }
        let log = sess.read()?;
        let (msgs, report) = project::project(&log, cfg);
        let approx = project::estimate_tokens(&msgs);
        // Оба порога пока только сигналят: это точки, где должна сработать компакция (M3).
        let ratio = approx as f64 / cfg.ctx_window as f64;
        if ratio > cfg.hard {
            ui.notice(&format!(
                "~{approx} токенов — пройден порог саммаризации ({:.0}%), саммари ещё нет",
                cfg.hard * 100.0
            ));
        } else if ratio > cfg.soft {
            ui.notice(&format!(
                "~{approx} токенов — пройден порог элизии ({:.0}%)",
                cfg.soft * 100.0
            ));
        }
        if report.elided > last_elided(&log) {
            ui.notice(&format!(
                "компакция: вырезано результатов {}, освобождено {} Б",
                report.elided, report.freed_bytes
            ));
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
            // Рассуждение отдельной строкой только когда оно было и было видно:
            // иначе ↓ молча обманывает — выход кажется дешёвым, а в нём сидел
            // зашифрованный ход, за который мы всё равно заплатили.
            let reasoning = if reply.usage.reason_tok > 0 {
                format!(
                    " (рассуж. {}{})",
                    reply.usage.reason_tok,
                    if reply.usage.encrypted { " зашифровано" } else { "" }
                )
            } else {
                String::new()
            };
            ui.notice(&format!(
                "ход {turn}: ↑{} (кеш {}) ↓{}{reasoning}{cost}",
                reply.usage.in_tok, reply.usage.cached_tok, reply.usage.out_tok
            ));
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
            ui.notice(&format!("пустой ответ модели ({hint}), повтор запроса"));
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
            // Подагенты: пока есть живые дети, не выходим — ждём их вопросы и
            // итоги. Иначе вопрос ребёнка родителю никто не прочитает и он уедет
            // в таймаут. Это supervisor вокруг цикла, а не вызов инструмента.
            if supervisor(sess, cfg, ui).await {
                continue;
            }
            return Ok(Finish::Ok);
        }

        for call in &reply.calls {
            let out = dispatch::dispatch(call, cfg, &reg, &path, &out_dir, sess).await;
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
                    ui.line(&format!(
                        "  [вложение] {} {}x{} {} Б — {}",
                        p.kind, p.width, p.height, p.bytes, p.path
                    ));
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
            return Ok(Finish::Limit);
        }
    }
}

/// Сколько держим родителя живым ради детей, после чего отпускаем (дети всё
/// равно отсоединены и доработают; просто их вопросы больше некому прочитать).
const SUPERVISE_LIMIT_SECS: u64 = 1800;

/// Пока у сессии есть работающие дети — не выходим: ждём их вопросов и итогов.
/// Это харнесс вокруг цикла, а не вызов инструмента, поэтому «модель забыла» тут
/// ничего не ломает. Возвращает true, если в ящике появилось сообщение — тогда
/// нужен ещё ход (его прочитает дренаж в начале цикла).
async fn supervisor(sess: &Session, cfg: &Config, ui: &crate::render::Ui) -> bool {
    let parent = sess.id();
    let mail_dir = cfg.mail_path();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(SUPERVISE_LIMIT_SECS);
    let mut noticed = 0;
    loop {
        // Сообщение уже ждёт — не нужно ждать детей, отдаём ход.
        if mail::has_pending(&mail_dir, parent) {
            return true;
        }
        let running = crate::subagent::children(cfg, parent)
            .into_iter()
            .filter(|c| c.state == crate::subagent::State::Running)
            .count();
        if running == 0 {
            return false;
        }
        if noticed != running {
            noticed = running;
            ui.notice(&format!("жду подагентов: работает {running}"));
        }
        if std::time::Instant::now() >= deadline {
            ui.notice("подагенты ещё работают — отпускаю родителя, они отсоединены");
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
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
    let mut stream = crate::render::Stream::new(ui.color, ui.sink());
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
        reason_tok: reply.usage.reason_tok,
        encrypted_reasoning: reply.usage.encrypted,
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
        .map(|l| format!("  {}", crate::reduce::truncate(l, 300)))
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
    use crate::llm::{Fake, Usage};
    use crate::log::Call;

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
