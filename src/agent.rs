// Цикл: запрос → стрим → диспатч → append. Состояние — лог, поэтому resume бесплатен:
// незавершённый ход = User без Assistant, продолжение с того же места.
use serde_json::Value;
use tokio::sync::watch;

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
/// События цикла для surface-слоёв; поля события несут то, что клиенту
/// видно, даже если ядро их не читает.
#[allow(dead_code)]
pub enum SurfaceSignal {
    Delta {
        text: String,
        kind: &'static str,
    },
    ToolStarted {
        name: String,
        call_id: String,
        args: String,
    },
    ToolCompleted {
        name: String,
        call_id: String,
        exit: i32,
        preview: String,
    },
    Notice(String),
}

pub async fn run_agent<L: Llm>(
    sess: &Session,
    cfg: &Config,
    llm: &L,
    ui: &crate::render::Ui,
) -> R<()> {
    run_agent_surface(sess, cfg, llm, ui, SurfaceRunOptions::default()).await
}

/// Optional controls and metadata for running an agent on a surface.
#[derive(Clone)]
pub struct QueuedInput {
    pub text: String,
    pub parts: Vec<crate::log::Part>,
}

pub struct SurfaceRunOptions<'a> {
    pub observe: &'a (dyn Fn(SurfaceSignal) + Send + Sync),
    pub cancel: Option<watch::Receiver<bool>>,
    pub queued: Option<std::sync::Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<QueuedInput>>>>,
    pub surface: Option<&'a str>,
}

fn ignore_surface_signal(_: SurfaceSignal) {}

impl Default for SurfaceRunOptions<'static> {
    fn default() -> Self {
        Self {
            observe: &ignore_surface_signal,
            cancel: None,
            queued: None,
            surface: None,
        }
    }
}

pub async fn run_agent_surface<L: Llm>(
    sess: &Session,
    cfg: &Config,
    llm: &L,
    ui: &crate::render::Ui,
    options: SurfaceRunOptions<'_>,
) -> R<()> {
    let r = run_loop(sess, cfg, llm, ui, options).await;
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
    options: SurfaceRunOptions<'_>,
) -> R<Finish> {
    let SurfaceRunOptions {
        observe,
        mut cancel,
        queued,
        surface,
    } = options;
    // Промпт сессии фиксируется в логе один раз и дальше читается оттуда: файлы
    // инстанса (soul/user/memory, скиллы) правятся по ходу, и пересборка на каждом
    // ходу промахивалась бы мимо префикс-кеша. Здесь — единственная точка, через
    // которую проходят все сессии, поэтому и запись здесь.
    if !sess.read()?.iter().any(|r| matches!(r, Rec::System { .. })) {
        sess.append(Rec::System {
            ts: log::now(),
            text: crate::prompt::system_prompt_for_surface(cfg, surface),
        })?;
    }
    let (defs, reg, path) = dispatch::prepare(cfg).await?;
    // Справка памяти впрыскивается ровно один раз, перед первым ходом сессии, и
    // записью в лог — проекция дальше читает её оттуда, как всё остальное.
    memory::recall_note(sess, cfg, &path).await;
    // Полные выводы обрезанных команд: модель получает путь и может дочитать.
    let out_dir = sess.path().with_extension("out");
    let mail_dir = cfg.mail_path();
    let mut mirror = crate::context::Mirror::open(sess, cfg)?;
    let mut pressure_next = cfg.context_budget();
    // Persist reminder hysteresis across resume; successful edits can re-arm it below target.
    for rec in sess.read()? {
        if let Rec::User { source, .. } = rec {
            if let Some(n) = source
                .strip_prefix("context-cleanup:")
                .and_then(|n| n.parse::<u64>().ok())
            {
                pressure_next = n;
            }
        }
    }
    let mut turn = 0u32;
    // Пустой ответ — не конец работы, а сбой (обрыв стрима, непарсимый формат
    // вызова). Считаем подряд: первый повторяем запросом, второй — выходим явной
    // ошибкой, чтобы прогон не закрылся «успешно», ничего не сделав.
    let mut empty_replies = 0u32;
    loop {
        if cancel.as_ref().is_some_and(|c| *c.borrow()) {
            return Err("cancelled".into());
        }
        turn += 1;
        if let Some(rx) = queued.as_ref() {
            let mut rx = rx.lock().await;
            while let Ok(input) = rx.try_recv() {
                sess.append(Rec::User {
                    ts: log::now(),
                    text: input.text,
                    source: "surface-steer".into(),
                    parts: input.parts,
                })?;
            }
        }
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
        let raw = sess.read()?;
        let mut msgs = mirror.prepare(&raw, cfg)?;
        let schema_tokens = serde_json::to_vec(&defs)?.len() as u64 / 3;
        let approx = project::estimate_tokens(&msgs) + schema_tokens;
        if approx < cfg.context_target() {
            pressure_next = cfg.context_budget();
        }
        if approx >= pressure_next {
            pressure_next = approx
                .saturating_add(cfg.context_budget() / 4)
                .max(approx.saturating_add(1));
            sess.append(Rec::User {
                ts: log::now(),
                source: format!("context-cleanup:{pressure_next}"),
                text: format!("[Уборка рабочего контекста] Сейчас ~{approx} токенов, экономический бюджет {}. Перед продолжением задачи отредактируй {} и вызови ctx commit. Ориентир после уборки — {} токенов. Удали переваренные выводы и законченные ветки, сохрани полезные точные фрагменты и обнови записку для продолжения: цель, ограничения, решения, точные данные, открытые проверки, следующие действия. Не пересказывай историю ради пересказа. Стратегию и объём правок выбираешь ты.", cfg.context_budget(), mirror.path().display(), cfg.context_target()),
                    parts: Vec::new(),
            })?;
            msgs = mirror.prepare(&sess.read()?, cfg)?;
            ui.notice(&format!(
                "контекст ~{approx}: запрос уборки (бюджет {})",
                cfg.context_budget()
            ));
        }
        // Protocol is constant across requests; outside the editable mirror.
        msgs.insert(1, serde_json::json!({"role":"system", "content": format!(
            "Ты управляешь своим рабочим контекстом. Зеркало следующего ввода: {} (также SNENS_CONTEXT_PATH в окружении инструментов). Читай его окнами и редактируй обычными read/edit/write/run, затем ctx commit. JSON blocks содержат messages; вызовы и ответы инструмента связаны одним блоком: нельзя оставлять сироты. Можно сокращать аргументы (валидный JSON), ответы, удалять целые пары/блоки, добавлять текстовые user notes с id new-*. Системные сообщения вне зеркала. Правка действует после результатов шага; исходный журнал не меняется. Держи рабочую записку и proactively удаляй ненужное. Бюджет {} токенов, цель уборки {}. Успешные прямые read/edit/write зеркала и ctx commit не остаются в рабочей истории; shell-команды остаются. Не дублируй всё зеркало в ответе.", mirror.path().display(), cfg.context_budget(), cfg.context_target())}));

        let physical_limit = cfg
            .ctx_window
            .saturating_sub(cfg.context_reserve.min(cfg.ctx_window / 4));
        if project::estimate_tokens(&msgs) + schema_tokens >= physical_limit {
            return Err("рабочий контекст достиг физического предела; запрос не отправлен. Нужна уборка LIVE_CONTEXT.json или больший размер окна".into());
        }

        let reply: Reply = if let Some(c) = cancel.as_mut() {
            tokio::select! { biased; _ = c.changed() => return Err("cancelled".into()), r = stream_reply(cfg, msgs, defs.clone(), llm, ui, observe) => r? }
        } else {
            stream_reply(cfg, msgs, defs.clone(), llm, ui, observe).await?
        };

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
                    if reply.usage.encrypted {
                        " зашифровано"
                    } else {
                        ""
                    }
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
            if let Some(rx) = queued.as_ref() {
                let mut rx = rx.lock().await;
                let mut accepted = Vec::new();
                while let Ok(input) = rx.try_recv() {
                    accepted.push(input);
                }
                drop(rx);
                if !accepted.is_empty() {
                    for input in accepted {
                        sess.append(Rec::User {
                            ts: log::now(),
                            text: input.text,
                            source: "surface-steer".into(),
                            parts: input.parts,
                        })?;
                    }
                    continue;
                }
            }
            return Ok(Finish::Ok);
        }

        for call in &reply.calls {
            observe(SurfaceSignal::ToolStarted {
                name: call.name.clone(),
                call_id: call.id.clone(),
                args: call.args.clone(),
            });
            let out = if let Some(c) = cancel.as_mut() {
                tokio::select! { biased; _ = c.changed() => return Err("cancelled".into()), o = dispatch::dispatch(call, cfg, &reg, &path, &out_dir, sess) => o }
            } else {
                dispatch::dispatch(call, cfg, &reg, &path, &out_dir, sess).await
            };
            observe(SurfaceSignal::ToolCompleted {
                name: call.name.clone(),
                call_id: call.id.clone(),
                exit: out.exit,
                preview: out.text.chars().take(500).collect(),
            });
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

        let committing = reply.calls.iter().any(|c| {
            c.name == "ctx"
                && serde_json::from_str::<Value>(&c.args)
                    .ok()
                    .is_some_and(|a| {
                        a.get("action").and_then(Value::as_str).unwrap_or("commit") == "commit"
                    })
        });
        let outcome = if committing {
            Some(
                mirror
                    .apply(sess, cfg)?
                    .unwrap_or_else(|| "context unchanged: no edit to apply".to_string()),
            )
        } else {
            None
        };
        if let Some(outcome) = outcome {
            ui.notice(&outcome);
            sess.append(Rec::ContextNotice {
                ts: log::now(),
                text: format!("[Harness context status] {outcome}"),
            })?;
        }

        // Лимит исчерпан — не падаем и не закрываем поверхность: просим
        // промежуточный итог и на этом заканчиваем запуск (см. final_answer).
        if turn >= cfg.max_turns {
            final_answer(sess, cfg, llm, ui, &path, turn, observe).await?;
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
    observe: &(dyn Fn(SurfaceSignal) + Send + Sync),
) -> R<()> {
    sess.append(Rec::User {
        ts: log::now(),
        text: LIMIT_NOTE.into(),
        source: "limit".into(),
            parts: Vec::new(),
    })?;
    let log = sess.read()?;
    let msgs = crate::context::effective(&log, cfg)?;
    let limit = cfg
        .ctx_window
        .saturating_sub(cfg.context_reserve.min(cfg.ctx_window / 4));
    if project::estimate_tokens(&msgs) >= limit {
        return Err("final answer context exceeds physical window; compact before resuming".into());
    }
    let reply = stream_reply(cfg, msgs, Vec::new(), llm, ui, observe).await?;
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
    observe: &(dyn Fn(SurfaceSignal) + Send + Sync),
) -> R<Reply> {
    let mut stream = crate::render::Stream::new(ui.color, ui.sink());
    let show = ui.show_reasoning();
    let mut reason_on = false;
    let reply = llm
        .reply(cfg, msgs, tools, &mut |d| match d {
            crate::llm::Delta::Reasoning(r) => {
                observe(SurfaceSignal::Delta {
                    text: r.to_string(),
                    kind: "reasoning",
                });
                if !reason_on {
                    reason_on = true;
                    stream.reasoning(show);
                }
                if show {
                    stream.reasoning_chunk(r);
                }
            }
            crate::llm::Delta::Content(c) => {
                observe(SurfaceSignal::Delta {
                    text: c.to_string(),
                    kind: "content",
                });
                stream.content(c)
            }
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

    #[tokio::test]
    async fn surface_prompt_is_logged_once_and_survives_resume() -> R<()> {
        let dir = std::env::temp_dir().join(format!("snens-surface-prompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let cfg = cfg_tmp(&dir);
        let sess = Session::open(&dir, "surface")?;
        sess.append(Rec::User {
            ts: log::now(),
            text: "hello".into(),
            source: "surface".into(),
                parts: Vec::new(),
        })?;
        let observe = |_| {};

        run_agent_surface(
            &sess,
            &cfg,
            &Fake::new(vec![text_reply("first")]),
            &crate::render::Ui::plain(),
            SurfaceRunOptions {
                observe: &observe,
                surface: Some("telegram"),
                ..SurfaceRunOptions::default()
            },
        )
        .await?;
        let first = sess.read()?;
        let prompt = first
            .iter()
            .find_map(|r| match r {
                Rec::System { text, .. } => Some(text.clone()),
                _ => None,
            })
            .expect("system prompt must be logged");
        assert!(prompt.contains("Текущая поверхность: Telegram, личный чат."));

        run_agent_surface(
            &sess,
            &cfg,
            &Fake::new(vec![text_reply("resumed")]),
            &crate::render::Ui::plain(),
            SurfaceRunOptions {
                observe: &observe,
                surface: Some("untrusted\ninstructions"),
                ..SurfaceRunOptions::default()
            },
        )
        .await?;
        let second = sess.read()?;
        let prompts = second
            .iter()
            .filter(|r| matches!(r, Rec::System { .. }))
            .count();
        assert_eq!(prompts, 1, "resume must retain the original system prompt");
        let resumed_prompt = second
            .iter()
            .find_map(|r| match r {
                Rec::System { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .unwrap();
        assert_eq!(prompt, resumed_prompt);
        assert!(!resumed_prompt.contains("untrusted"));
        std::fs::remove_dir_all(&dir).ok();
        Ok(())
    }

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
                parts: Vec::new(),
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
                parts: Vec::new(),
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
                parts: Vec::new(),
        })?;
        let fake = Fake::new(vec![text_reply(""), text_reply("")]);
        let err = run_agent(&sess, &cfg, &fake, &crate::render::Ui::plain()).await;
        assert!(err.is_err(), "два пустых ответа не дали ошибку");
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
