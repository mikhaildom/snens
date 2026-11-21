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
        /// Вложения вызова (картинки и прочее из канала `SNENS_ATTACH_DIR`).
        /// Без них стрим показывал тул без картинки, а после перечитывания
        /// истории она появлялась — расхождение стрима и истории.
        parts: Vec<crate::log::Part>,
        /// Сколько занял вызов, мс: в журнале это поле есть, в стриме не было.
        ms: u64,
    },
    /// Расход шага (ответа модели): токены и цена. Поверхность показывает его
    /// в шапке шага — в стриме цифр не было, они появлялись только после
    /// перечитывания истории.
    Usage {
        in_tok: u64,
        cached_tok: u64,
        out_tok: u64,
        reason_tok: u64,
        cost_micro: u64,
    },
    Notice(String),
    /// Давление рабочего контекста: оценка в токенах и экономический бюджет.
    /// Отдаётся, когда уборка запрошена. Поверхности с долгоживущей сессией
    /// (групповой чат) по нему решают ротацию; в чат это не показывается.
    ContextPressure {
        approx: u64,
        budget: u64,
    },
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
    /// Ответ на вопрос артефакта: едет в журнал вместе с текстом, чтобы
    /// одноразовость пережила очередь и границу хода.
    pub ask: Option<crate::log::AskRef>,
}

pub struct SurfaceRunOptions<'a> {
    pub observe: &'a (dyn Fn(SurfaceSignal) + Send + Sync),
    pub cancel: Option<watch::Receiver<bool>>,
    pub queued: Option<
        std::sync::Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<QueuedInput>>>,
    >,
    pub surface: Option<&'a str>,
    pub surface_tools: Vec<Value>,
    pub tool_tx: Option<tokio::sync::mpsc::UnboundedSender<SurfaceToolRequest>>,
    /// Кто-то разбудит сессию по входящему (ящик, фоновое задание): поверхность
    /// или TUI. Тогда цикл не держится живым ради детей — он заканчивается, а
    /// итог ребёнка поднимает новый ход. Без будильника (одноразовый `snens run`)
    /// ждём детей сами, иначе их итог было бы некому прочитать.
    pub wake: bool,
}

pub struct SurfaceToolRequest {
    pub call_id: String,
    pub name: String,
    pub args: String,
    pub result: tokio::sync::oneshot::Sender<SurfaceToolResult>,
}
#[derive(Clone)]
pub struct SurfaceToolResult {
    pub text: String,
    pub exit: i32,
}

#[cfg(test)]
fn surface_tool_timeout_secs() -> u64 {
    1
}

#[cfg(test)]
mod surface_tool_timeout_tests {
    use super::*;
    #[test]
    fn timeout_is_short_in_tests_and_finite_in_production() {
        assert_eq!(surface_tool_timeout_secs(), 1);
    }
}
#[cfg(not(test))]
fn surface_tool_timeout_secs() -> u64 {
    60
}

/// Ждать отмены хода.
///
/// Закрытый канал — не отмена: он значит, что контрол снят и отменять больше
/// некому. `watch::Receiver::changed()` на закрытом канале возвращает `Err`
/// сразу, и раньше селект срабатывал на этом `Err`, как на отмене: ход умирал
/// с `cancelled` (и это видел человек в ленте), хотя никто его не отменял. Ждём
/// вечно: без отправляющего конца отменить нечего.
async fn wait_cancel(c: &mut watch::Receiver<bool>) {
    loop {
        match c.changed().await {
            Ok(()) => {
                if *c.borrow() {
                    return;
                }
            }
            Err(_) => std::future::pending::<()>().await,
        }
    }
}

fn ignore_surface_signal(_: SurfaceSignal) {}

impl Default for SurfaceRunOptions<'static> {
    fn default() -> Self {
        Self {
            observe: &ignore_surface_signal,
            cancel: None,
            queued: None,
            surface: None,
            surface_tools: Vec::new(),
            tool_tx: None,
            wake: false,
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
        surface_tools,
        tool_tx,
        wake,
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
    let (mut defs, reg, path) = dispatch::prepare(cfg).await?;
    defs.extend(surface_tools.clone());
    // Требование уборки ссылается на `ctx commit`. Если тула в схеме нет,
    // инструктировать им нельзя: модель либо поверит, что тул есть, и потратит
    // ходы на несуществующий вызов, либо бросит уборку целиком. Проверяем факт.
    let ctx_tool = defs
        .iter()
        .any(|d| d.pointer("/function/name").and_then(Value::as_str) == Some("ctx"));
    // Справка памяти впрыскивается по режиму `memory.recall`, записью в лог —
    // проекция дальше читает её оттуда, как всё остальное. Режим берётся
    // перечитанным из файла: поверхность живёт долго, тумблер действует сразу.
    memory::recall_note(sess, cfg, &path).await;
    // Полные выводы обрезанных команд: модель получает путь и может дочитать.
    let out_dir = sess.path().with_extension("out");
    let mail_dir = cfg.mail_path();
    let mut mirror = crate::context::Mirror::open(sess, cfg)?;
    // Ступени давления: 50% и 75% бюджета по умолчанию. Гистерезис переживает
    // resume — на нём держится `Rec::User { source: "context-cleanup:<порог>" }`.
    let mut pressure = {
        let mut persisted = None;
        for rec in sess.read()? {
            if let Rec::User { source, .. } = rec {
                if let Some(n) = source
                    .strip_prefix("context-cleanup:")
                    .and_then(|n| n.parse::<u64>().ok())
                {
                    persisted = Some(n);
                }
            }
        }
        crate::context::Pressure::resumed(cfg.context_pressure_tiers(), persisted)
    };
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
        // Конфиг на этот ход — из файла: меню настроек, `snens config` и человек
        // в редакторе правят файл, а поверхность живёт днями и снимком старта
        // ничего бы не увидела до рестарта сервиса. Файл сломан — остаётся
        // снимок старта, ход из-за чужой правки не падает.
        let live = cfg.live();
        let cfg = &live;
        if let Some(rx) = queued.as_ref() {
            let mut rx = rx.lock().await;
            while let Ok(input) = rx.try_recv() {
                sess.append(Rec::User {
                    ts: log::now(),
                    text: input.text,
                    source: "surface-steer".into(),
                    parts: input.parts,
                    ask: input.ask,
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
        // Готовые фоновые задания забираются ЗДЕСЬ же, наравне с ящиком, и ход
        // не заканчивается, не отчитавшись за них. Раньше их забирал только
        // будильник поверхности по концу хода: результат, пришедший посреди
        // работы, ждал её конца, и человек видел отчёт спустя минуты, хотя
        // границ, на которых его можно было отдать, было много.
        //
        // Запись — `User { source: "wake" }`, а не `Steer`: задание запущено в
        // этой же сессии, и для человека это часть её работы (лента кладёт его в
        // пачку), а не чужое слово в ход (steer она рисует репликой).
        let finished = crate::jobs::collect_finished(sess.path());
        for text in finished {
            let _ = sess.append(Rec::User {
                ts: log::now(),
                text,
                source: "wake".into(),
                parts: Vec::new(),
                ask: None,
            });
        }
        let raw = sess.read()?;
        let mut msgs = mirror.prepare(&raw, cfg)?;
        // Автоэлизия: детерминированный пред-шаг, один раз на переходе через
        // бюджет. Результаты схлопываются в пометки, диалог не трогается;
        // решение живёт записью Rec::Ctx и не пересчитывается каждый ход.
        let cleared = project::plan_elision(&raw, cfg);
        if !cleared.is_empty() {
            let elided = cleared.len();
            sess.append(Rec::Ctx {
                ts: log::now(),
                action: "auto".into(),
                elided,
                freed_bytes: 0,
                cleared: cleared.clone(),
            })?;
            project::apply_cleared(&mut msgs, &cleared);
            ui.notice(&format!("контекст: сжал {elided} результатов тулов"));
        } else {
            project::apply_cleared(&mut msgs, &project::cleared_map(&raw));
        }
        let schema_tokens = serde_json::to_vec(&defs)?.len() as u64 / 3;
        // Порог — по токенам провайдера: сырая оценка, помноженная на множитель
        // из последних usage этой сессии (см. project::calibration).
        let est_raw = project::estimate_tokens(&msgs) + schema_tokens;
        let approx = (est_raw as f64 * project::calibration(&raw)).round() as u64;
        // Порог мог смениться из файла (бюджет, профиль модели, ступени) —
        // пересчитываем точки взвода, уже пройденное не откатываем.
        pressure.retune(cfg.context_pressure_tiers());
        // Абзац про управление зеркалом — только когда уборка запрошена:
        // постоянное напоминание на каждом ходу отвлекало модель на размышления
        // о контексте вместо задачи.
        let mut cleanup_requested = false;
        if let Some(next) = pressure.observe(approx, cfg.context_budget()) {
            // Даже без `ctx` в схеме автоэлизия уже сработала выше — часть
            // давления снята. Требование правки зеркала шлём только когда
            // применить её действительно чем: иначе это приглашение в тупик.
            cleanup_requested = ctx_tool;
            let text = if ctx_tool {
                format!("[Уборка рабочего контекста] Сейчас ~{approx} токенов, экономический бюджет {}. Перед продолжением задачи отредактируй {} и вызови ctx commit. Ориентир после уборки — {} токенов. Удали переваренные выводы и законченные ветки, сохрани полезные точные фрагменты и обнови записку для продолжения: цель, ограничения, решения, точные данные, открытые проверки, следующие действия. Не пересказывай историю ради пересказа. Стратегию и объём правок выбираешь ты.", cfg.context_budget(), mirror.path().display(), cfg.context_target())
            } else {
                format!("[Уборка рабочего контекста] Сейчас ~{approx} токенов, экономический бюджет {}. Инструмента ctx в этой схеме нет, поэтому правка зеркала не применяется: старые результаты уже схлопнуты автоэлизией. Работай дальше; если контекста не хватит, скажи об этом прямо — уборку сделает человек.", cfg.context_budget())
            };
            // Поверхность узнаёт о давлении тем же сигналом, на котором
            // завязана уборка: молчание здесь скрыло бы подступающий потолок от
            // группового чата, который живёт долго и решает по нему ротацию.
            observe(SurfaceSignal::ContextPressure {
                approx,
                budget: cfg.context_budget(),
            });
            sess.append(Rec::User {
                ts: log::now(),
                source: format!("context-cleanup:{next}"),
                text,
                parts: Vec::new(),
                ask: None,
            })?;
            msgs = mirror.prepare(&sess.read()?, cfg)?;
            project::apply_cleared(&mut msgs, &project::cleared_map(&sess.read()?));
            ui.notice(&format!(
                "контекст ~{approx} из {} — прошу агента убрать лишнее",
                cfg.context_budget()
            ));
        }
        // Protocol is constant across requests; outside the editable mirror.
        // Едет только когда запрошена уборка: постоянное напоминание отвлекало
        // модель на размышления о контексте.
        if cleanup_requested {
            msgs.insert(1, serde_json::json!({"role":"system", "content": format!(
            "Ты управляешь своим рабочим контекстом. Зеркало следующего ввода: {} (также SNENS_CONTEXT_PATH в окружении инструментов). Читай его окнами, правь обычными read/edit/write/run и примени правку одним вызовом ctx commit. Защищены первый и последний запрос человека; всё остальное — ответы модели, середина диалога, результаты тулов — можно сокращать, удалять и переставлять. Одна батчевая запись за ход. Сохраняй порядок и структуру диалога: устаревший анализ заменяй локальным саммари на том же месте. Сохраняй решения, ограничения, открытые вопросы, пути, команды, результаты проверок и следующий шаг. Снимай в первую очередь крупные результаты тулов и отработанные ветки. Вызов и результат тула — пара: правка с сиротой не пройдёт. Заметки — блоки с id new-*; системные сообщения вне зеркала. Правка проверяется на сжатие: если снято мало, придёт отказ с числами и списком крупнейших блоков. Бюджет {} токенов, цель уборки {}. Успешные прямые read/edit/write зеркала и ctx commit не остаются в рабочей истории; shell-команды остаются. Не печатай зеркало целиком.", mirror.path().display(), cfg.context_budget(), cfg.context_target())}));
        }

        let physical_limit = cfg
            .ctx_window
            .saturating_sub(cfg.context_reserve.min(cfg.ctx_window / 4));
        if project::estimate_tokens(&msgs) + schema_tokens >= physical_limit {
            return Err("рабочий контекст достиг физического предела; запрос не отправлен. Нужна уборка LIVE_CONTEXT.json или больший размер окна".into());
        }

        // `snens model <slug>` правит файл, а поверхность живёт долго: без
        // перечитки новый ход уходил бы моделью, снятой при старте процесса.
        // Подменяем только модель — остальные поля конфига остаются снимком старта.
        // Модель уже подставлена `live()`; остальные поля — тоже из файла.
        let mut turn_cfg = cfg.clone();
        // Пришло ли от модели хоть что-то: после первых данных повтор через
        // другой профиль напечатал бы ответ второй раз.
        let streamed = std::sync::atomic::AtomicBool::new(false);
        let watch = |signal: SurfaceSignal| {
            if matches!(signal, SurfaceSignal::Delta { .. }) {
                streamed.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            observe(signal)
        };
        let mut reply = ask_turn(&turn_cfg, msgs.clone(), defs.clone(), llm, ui, &watch, &mut cancel).await;
        // Ключ активного профиля не работает — запасной профиль (второй ключ).
        // Ключ мёртв (401/402/403) и запасной ответил — запасной становится
        // активным, чтобы следующие ходы не стучались в мёртвый ключ. Временный
        // сбой (лимит, 5xx, сеть) — запасной только на этот ход: переводить
        // человека насовсем на другой, возможно платный, профиль из-за минутного
        // обрыва нельзя. Человек видит оба случая; вернуть — в настройках.
        if let Err(e) = &reply {
            if crate::llm::is_provider_down(e.as_ref())
                && !streamed.load(std::sync::atomic::Ordering::Relaxed)
            {
                if let Some((name, fallback)) = turn_cfg.fallback() {
                    let from = turn_cfg.active_provider.trim().to_string();
                    let dead = crate::llm::is_key_dead(e.as_ref());
                    let note = format!(
                        "провайдер «{from}» не отвечает ({}) — пробую запасной «{name}»",
                        crate::reduce::truncate(&e.to_string(), 160)
                    );
                    ui.notice(&note);
                    observe(SurfaceSignal::Notice(note));
                    reply = ask_turn(&fallback, msgs.clone(), defs.clone(), llm, ui, observe, &mut cancel).await;
                    if reply.is_ok() {
                        let note = if !dead {
                            format!(
                                "этот ход сделан через запасной «{name}»: «{from}» временно \
                                 не отвечает, следующий ход снова пойдёт через него"
                            )
                        } else {
                            match crate::providers::activate(cfg, &name) {
                                Ok(()) => format!(
                                    "переключено на запасной профиль «{name}»: ключ «{from}» \
                                     не работает. Вернуть — в настройках, раздел провайдеров"
                                ),
                                Err(err) => format!(
                                    "этот ход сделан через запасной «{name}», но сделать его \
                                     активным не вышло ({err}) — следующий ход снова пойдёт через «{from}»"
                                ),
                            }
                        };
                        ui.notice(&note);
                        observe(SurfaceSignal::Notice(note));
                    }
                    // Дальше ход (повтор без картинок) — через запасной при любом
                    // исходе: основной только что отказал.
                    turn_cfg = fallback;
                }
            }
        }
        // Отказ по размеру тела. Бюджет в `stream_reply` уже снял лишнее, но
        // провайдер может считать иначе (у Nous свой счёт и своё «30MB»). Тогда
        // один повтор вообще без картинок: лучше текстовый ход, чем сессия,
        // которую нельзя продолжить — каждый следующий запрос упирался бы в тот
        // же 413, потому что проекция собирается из тех же файлов.
        if let Err(e) = &reply {
            if crate::llm::is_payload_too_large(e.as_ref()) {
                let dropped = project::fit_media(&mut msgs, 0);
                let note = format!(
                    "провайдер отверг запрос по размеру тела — вложения ({dropped} шт.) сняты, \
                     ход повторяется текстом"
                );
                ui.notice(&note);
                observe(SurfaceSignal::Notice(note));
                reply = ask_turn(&turn_cfg, msgs, defs.clone(), llm, ui, observe, &mut cancel).await;
            }
        }
        let reply = reply?;

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
                "ход {turn}: вход {} · кеш {} · выход {}{reasoning}{cost}",
                reply.usage.in_tok, reply.usage.cached_tok, reply.usage.out_tok
            ));
        }

        // Пусто (ни текста, ни вызовов) — это сбой, а не «работа кончилась»:
        // провайдер мог оборвать стрим или прислать вызов в непарсимом формате.
        // Ход пишем только записью расхода (пустая assistant-запись сбила бы
        // проекцию), делаем один повтор; второй пустой подряд — явная ошибка,
        // иначе прогон закрывается «успешно», ничего не сделав.
        if reply.calls.is_empty() && reply.text.trim().is_empty() {
            sess.append(usage_rec(&reply, est_raw, &cfg.model_now()))?;
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
        sess.append(usage_rec(&reply, est_raw, &cfg.model_now()))?;
        observe(SurfaceSignal::Usage {
            in_tok: reply.usage.in_tok,
            cached_tok: reply.usage.cached_tok,
            out_tok: reply.usage.out_tok,
            reason_tok: reply.usage.reason_tok,
            cost_micro: reply.usage.cost_micro,
        });

        if reply.calls.is_empty() {
            memory::retain_turn(sess, cfg, &path, &reply.text).await;
            // Подагенты: пока есть живые дети, не выходим — ждём их вопросы и
            // итоги. Но только там, где нет будильника: если сессию разбудит
            // входящее (поверхность, TUI), ход заканчивается сразу, а итог
            // ребёнка поднимет новый. Иначе выход держится лишние минуты, и слово
            // человека стоит в очереди.
            if !wake && supervisor(sess, cfg, ui, queued.as_ref()).await {
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
                            ask: input.ask,
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
            let out = if surface_tools.iter().any(|d| {
                d.pointer("/function/name").and_then(Value::as_str) == Some(call.name.as_str())
            }) {
                let fail = |text: String| crate::exec::Out {
                    exit: -1,
                    text,
                    bytes: 0,
                    ms: 0,
                    timed_out: false,
                    parts: Vec::new(),
                };
                if let Some(tx) = tool_tx.as_ref() {
                    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                    let req = SurfaceToolRequest {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        args: call.args.clone(),
                        result: result_tx,
                    };
                    if tx.send(req).is_err() {
                        fail("surface tool broker unavailable".into())
                    } else {
                        let wait = tokio::time::timeout(
                            std::time::Duration::from_secs(surface_tool_timeout_secs()),
                            result_rx,
                        );
                        let got = if let Some(c) = cancel.as_mut() {
                            tokio::select! { biased; _ = wait_cancel(c) => { let _=sess.append(Rec::ToolResult{ts:log::now(),call_id:call.id.clone(),tool:call.name.clone(),argv:Vec::new(),exit:-1,bytes:0,ms:0,body:Body::Inline{ text:"cancelled".into() },parts:Vec::new()}); return Err("cancelled".into()) }, r = wait => r }
                        } else {
                            wait.await
                        };
                        match got {
                            Ok(Ok(r)) => crate::exec::Out {
                                exit: r.exit,
                                text: r.text,
                                bytes: 0,
                                ms: 0,
                                timed_out: false,
                                parts: Vec::new(),
                            },
                            Ok(Err(_)) => fail("surface tool response expired".into()),
                            Err(_) => fail("surface tool timed out after 60s".into()),
                        }
                    }
                } else {
                    fail("surface tool executor unavailable".into())
                }
            } else if let Some(c) = cancel.as_mut() {
                tokio::select! { biased; _ = wait_cancel(c) => return Err("cancelled".into()), o = dispatch::dispatch(call, cfg, &reg, &path, &out_dir, sess) => o }
            } else {
                dispatch::dispatch(call, cfg, &reg, &path, &out_dir, sess).await
            };
            observe(SurfaceSignal::ToolCompleted {
                name: call.name.clone(),
                call_id: call.id.clone(),
                exit: out.exit,
                preview: out.text.chars().take(500).collect(),
                parts: out.parts.clone(),
                ms: out.ms,
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
            match mirror.apply(sess, cfg)? {
                Some(outcome) => Some(outcome),
                None => Some(crate::context::Mirror::noop_outcome(sess)?),
            }
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
        // `0` — лимита нет: остаются бюджет контекста и поведение провайдера.
        if cfg.max_turns > 0 && turn >= cfg.max_turns {
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
async fn supervisor(
    sess: &Session,
    cfg: &Config,
    ui: &crate::render::Ui,
    queued: Option<
        &std::sync::Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<QueuedInput>>>,
    >,
) -> bool {
    let parent = sess.id();
    let mail_dir = cfg.mail_path();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(SUPERVISE_LIMIT_SECS);
    let mut noticed = 0;
    loop {
        // Сообщение уже ждёт — не нужно ждать детей, отдаём ход.
        if mail::has_pending(&mail_dir, parent) {
            return true;
        }
        // Слово человека в очереди тоже важнее ожидания детей: иначе steer лежит
        // невидимым до конца подагентов, хотя поверхность уже приняла его и
        // показала отправленным. `try_lock`, а не `lock`: очередь в этот момент
        // не держит никто, а блокировать цикл ожидания из-за неё нельзя.
        if let Some(rx) = queued {
            if let Ok(rx) = rx.try_lock() {
                if !rx.is_empty() {
                    return true;
                }
            }
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
        ask: None,
    })?;
    let log = sess.read()?;
    // `project::msgs`, а не `context::effective`: последний — база зеркала, а на
    // провод справка памяти дописывается в конце хода (`append_turn_recall`).
    let msgs = project::msgs(&log, cfg);
    let est_raw = project::estimate_tokens(&msgs);
    let limit = cfg
        .ctx_window
        .saturating_sub(cfg.context_reserve.min(cfg.ctx_window / 4));
    if project::calibrated_tokens(&log, &msgs) >= limit {
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
    sess.append(usage_rec(&reply, est_raw, &cfg.model_now()))?;
    ui.notice(&format!("лимит {turn} ходов — прошу подвести итог"));
    memory::retain_turn(sess, cfg, path, &reply.text).await;
    Ok(())
}

/// Один запрос хода с учётом отмены: селект живёт здесь, чтобы повтор после
/// отказа провайдера по размеру тела шёл через ту же отмену, а не мимо неё.
async fn ask_turn<L: Llm>(
    cfg: &Config,
    msgs: Vec<Value>,
    tools: Vec<Value>,
    llm: &L,
    ui: &crate::render::Ui,
    observe: &(dyn Fn(SurfaceSignal) + Send + Sync),
    cancel: &mut Option<watch::Receiver<bool>>,
) -> R<Reply> {
    match cancel.as_mut() {
        Some(c) => tokio::select! { biased; _ = wait_cancel(c) => Err("cancelled".into()), r = stream_reply(cfg, msgs, tools, llm, ui, observe) => r },
        None => stream_reply(cfg, msgs, tools, llm, ui, observe).await,
    }
}

/// Один запрос к модели с живым рендером: ответ и рассуждение стримятся через
/// `Stream`, таблицы склеиваются в рамку. Здесь же — единственное место, где
/// решение «показывать ли рассуждение» спрашивается у `Ui`.
async fn stream_reply<L: Llm>(
    cfg: &Config,
    mut msgs: Vec<Value>,
    tools: Vec<Value>,
    llm: &L,
    ui: &crate::render::Ui,
    observe: &(dyn Fn(SurfaceSignal) + Send + Sync),
) -> R<Reply> {
    // Бюджет вложений на запрос. Проверяется здесь, а не на уровне тула: файлы
    // не удаляются и журнал не портится, но старые картинки не едут, а на их
    // месте остаётся пометка (файл на диске, смотри текстом). Иначе сессия,
    // набравшая скриншотов, упиралась бы в 413 на каждом ходу до конца жизни.
    let dropped = project::fit_media(&mut msgs, cfg.media_budget_bytes);
    if dropped > 0 {
        ui.notice(&format!(
            "вложения: {dropped} старых картинок не едут — бюджет {} Б на запрос",
            cfg.media_budget_bytes
        ));
        observe(SurfaceSignal::Notice(format!(
            "вложения не отправлены: {dropped} шт. сверх бюджета {} Б на запрос",
            cfg.media_budget_bytes
        )));
    }
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
fn usage_rec(reply: &Reply, est_tok: u64, model: &str) -> Rec {
    Rec::Usage {
        ts: log::now(),
        model: model.to_string(),
        in_tok: reply.usage.in_tok,
        cached_tok: reply.usage.cached_tok,
        out_tok: reply.usage.out_tok,
        cost_micro: reply.usage.cost_micro,
        upstream_id: reply.upstream.id.clone(),
        fingerprint: reply.upstream.fingerprint.clone(),
        finish_reason: reply.finish_reason.clone(),
        reason_tok: reply.usage.reason_tok,
        encrypted_reasoning: reply.usage.encrypted,
        est_tok,
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
    use crate::llm::{Delta, Fake, Usage};
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
            ask: None,
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
        assert!(prompt.contains("Поверхность: Telegram, личный чат."));

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
            ask: None,
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

    /// 413 от провайдера — не конец сессии: ход повторяется без картинок, и
    /// человек видит, почему. Раньше такой отказ рубил сессию навсегда: проекция
    /// собиралась из тех же файлов и снова не влезала.
    #[tokio::test]
    async fn payload_too_large_retries_without_attachments() -> R<()> {
        use std::sync::Mutex;
        struct OversizedImage {
            seen: Mutex<Vec<bool>>,
        }
        impl Llm for OversizedImage {
            async fn reply(
                &self,
                _cfg: &Config,
                msgs: Vec<Value>,
                _tools: Vec<Value>,
                on_delta: &mut (dyn FnMut(Delta) + Send),
            ) -> R<Reply> {
                let images = msgs
                    .iter()
                    .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
                    .filter(|p| p["type"] == "image_url")
                    .count();
                self.seen.lock().unwrap().push(images > 0);
                if images > 0 {
                    return Err("HTTP 413 Payload Too Large: {\"error\":{\"message\":\"Downloaded image content cannot exceed 30MB\"}}".into());
                }
                on_delta(Delta::Content("посмотрел текстом"));
                Ok(text_reply("посмотрел текстом"))
            }
        }
        let dir = std::env::temp_dir().join(format!("snens-413-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let png = dir.join("shot.png");
        std::fs::write(&png, [0x89, b'P', b'N', b'G', 1, 2, 3, 4])?;
        // Бюджет не ограничивает: проверяем именно дорогу 413 → повтор.
        let cfg = Config {
            media_budget_bytes: 1 << 30,
            ..cfg_tmp(&dir)
        };
        let sess = Session::open(&dir, "t")?;
        sess.append(Rec::User {
            ts: log::now(),
            text: "что на скриншоте?".into(),
            source: "test".into(),
            parts: vec![crate::log::Part {
                kind: "image".into(),
                path: png.to_string_lossy().into_owned(),
                mime: "image/png".into(),
                bytes: 8,
                width: 1,
                height: 1,
            }],
            ask: None,
        })?;
        let llm = OversizedImage { seen: Mutex::new(vec![]) };
        run_agent(&sess, &cfg, &llm, &crate::render::Ui::plain()).await?;
        let seen = llm.seen.lock().unwrap().clone();
        assert_eq!(seen, vec![true, false], "должен быть ровно один повтор без картинок: {seen:?}");
        let recs = sess.read()?;
        assert!(
            recs.iter().rev().any(|r| matches!(
                r,
                Rec::Assistant { text: Some(t), .. } if t.contains("посмотрел текстом")
            )),
            "ход не доведён до ответа"
        );
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Ключ активного профиля не работает (401) — ход доводится через запасной
    /// профиль, тот становится активным, и человек видит, что случилось.
    #[tokio::test]
    async fn dead_key_switches_to_fallback_profile() -> R<()> {
        let (active, notes) = fallback_run("dead", "dead").await?;
        assert_eq!(active, "spare", "запасной не стал активным");
        assert!(notes.iter().any(|n| n.contains("переключено на запасной профиль «spare»")), "{notes:?}");
        Ok(())
    }

    /// Временный сбой (529 «перегружен») — запасной только на этот ход.
    #[tokio::test]
    async fn busy_provider_uses_fallback_for_one_turn() -> R<()> {
        let (active, notes) = fallback_run("busy", "busy").await?;
        assert_eq!(active, "main", "из-за временного сбоя профиль не меняется");
        assert!(notes.iter().any(|n| n.contains("временно")), "{notes:?}");
        Ok(())
    }

    /// Ход с основным ключом `main_key` («dead» → 401, «busy» → 529) и
    /// запасным «alive». Возвращает активный профиль после хода и уведомления.
    async fn fallback_run(main_key: &str, tag: &str) -> R<(String, Vec<String>)> {
        use std::sync::Mutex;
        struct DeadMainKey {
            asked: Mutex<Vec<String>>,
        }
        impl Llm for DeadMainKey {
            async fn reply(
                &self,
                cfg: &Config,
                _msgs: Vec<Value>,
                _tools: Vec<Value>,
                on_delta: &mut (dyn FnMut(Delta) + Send),
            ) -> R<Reply> {
                let key = cfg.key_value(&cfg.key_env).unwrap_or_default();
                self.asked.lock().unwrap().push(format!("{}:{key}", cfg.base_url));
                if key == "dead" {
                    return Err("HTTP 401 Unauthorized: invalid x-api-key".into());
                }
                if key == "busy" {
                    return Err("HTTP 529 <unknown status code>: overloaded".into());
                }
                on_delta(Delta::Content("ответ через запасной"));
                Ok(text_reply("ответ через запасной"))
            }
        }
        let dir = std::env::temp_dir().join(format!("snens-fallback-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            format!(
                "sessions_dir = {d:?}\ntools_dir = {t:?}\nactive_provider = \"main\"\n\n\
                 [providers.main]\nbase_url = \"https://main/v1\"\nmodel = \"m\"\nkey_env = \"FB3_MAIN\"\n\n\
                 [providers.spare]\nbase_url = \"https://spare/v1\"\nmodel = \"s\"\nkey_env = \"FB3_SPARE\"\n",
                d = dir.to_string_lossy(),
                t = dir.join("tools").to_string_lossy(),
            ),
        )?;
        std::fs::write(dir.join(".env"), format!("FB3_MAIN={main_key}\nFB3_SPARE=alive\n"))?;
        let cfg = Config::load(Some(&path.display().to_string()))?;
        let sess = Session::open(&dir, "t")?;
        sess.append(Rec::User {
            ts: log::now(),
            text: "привет".into(),
            source: "test".into(),
            parts: vec![],
            ask: None,
        })?;
        let llm = DeadMainKey { asked: Mutex::new(vec![]) };
        let notes = Mutex::new(Vec::<String>::new());
        let observe = |signal: SurfaceSignal| {
            if let SurfaceSignal::Notice(text) = signal {
                notes.lock().unwrap().push(text);
            }
        };
        let options = SurfaceRunOptions {
            observe: &observe,
            ..Default::default()
        };
        run_agent_surface(&sess, &cfg, &llm, &crate::render::Ui::plain(), options).await?;
        let asked = llm.asked.lock().unwrap().clone();
        assert_eq!(
            asked,
            vec![format!("https://main/v1:{main_key}"), "https://spare/v1:alive".to_string()],
            "{asked:?}"
        );
        let active = cfg.live().active_provider;
        let notes = notes.lock().unwrap().clone();
        let _ = std::fs::remove_dir_all(&dir);
        Ok((active, notes))
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
            ask: None,
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
            ask: None,
        })?;
        let fake = Fake::new(vec![text_reply(""), text_reply("")]);
        let err = run_agent(&sess, &cfg, &fake, &crate::render::Ui::plain()).await;
        assert!(err.is_err(), "два пустых ответа не дали ошибку");
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Готовое фоновое задание отдаётся на границе хода, а не только по его
    /// концу. Раньше его забирал только будильник поверхности, и результат,
    /// пришедший посреди работы, ждал конца хода — а границ за это время много.
    #[tokio::test]
    async fn finished_job_is_delivered_at_turn_boundary() -> R<()> {
        let dir = std::env::temp_dir().join(format!("snens-jobwake-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let cfg = cfg_tmp(&dir);
        let sess = Session::open(&dir, "j")?;
        sess.append(Rec::User {
            ts: log::now(),
            text: "запусти в фоне".into(),
            source: "test".into(),
            parts: Vec::new(),
            ask: None,
        })?;
        // Задание уже готово к первому шагу: `done` лежит рядом с журналом.
        let jobs = crate::jobs::dir(sess.path());
        std::fs::create_dir_all(&jobs)?;
        std::fs::write(jobs.join("7-1.log"), "сборка прошла\n")?;
        std::fs::write(
            jobs.join("7-1.json"),
            serde_json::to_vec(&crate::jobs::Job {
                id: "7-1".into(),
                command: "cargo build".into(),
                cwd: dir.display().to_string(),
                timeout_s: 60,
                log: jobs.join("7-1.log").display().to_string(),
                pid: 0,
                started: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            path: None,
            })?,
        )?;
        std::fs::write(
            jobs.join("7-1.done"),
            serde_json::to_vec(&crate::jobs::Done {
                exit: 0,
                ms: 5,
                timed_out: false,
                bytes: 16,
            })?,
        )?;
        let fake = Fake::new(vec![
            Reply {
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
            },
            text_reply("вижу отчёт"),
        ]);
        run_agent(&sess, &cfg, &fake, &crate::render::Ui::plain()).await?;
        let recs = sess.read()?;
        let at = recs
            .iter()
            .position(|r| matches!(r, Rec::User { source, .. } if source == "wake"))
            .expect("итог задания не попал в журнал");
        let answer_at = recs
            .iter()
            .position(|r| matches!(r, Rec::Assistant { text: Some(t), .. } if t == "вижу отчёт"))
            .expect("ответ модели не записан");
        assert!(
            at < answer_at,
            "отчёт лёг после ответа модели — значит ждал конца хода"
        );
        // Результат забран из очереди: второй раз его не отдадут.
        assert!(crate::jobs::collect_finished(sess.path()).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
