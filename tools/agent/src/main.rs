// agent — подагенты snens: запустить, ждать, спросить, ответить, посмотреть.
//
// Тул, а не часть ядра: он говорит с сессиями через расклад на диске (лог сессии
// и её ящик), а не через внутренности харнесса. Надёжность конца ребёнка — по
// записи `Done` в его логе; её пишет харнесс, поэтому «модель забыла отчитаться»
// тут ничего не ломает.
//
// Слои: cfg — окружение и конфиг, mail — ящик и дерево (дети/родитель/братья),
// log — чтение чужого лога, здесь — команды и разбор аргументов.
mod cfg;
mod log;
mod mail;

use std::io::Read;
use std::process::Command;
use std::time::{Duration, Instant};

use cfg::Cfg;

const HELP: &str = "\
agent — подагенты snens: запустить, ждать, спросить, ответить, посмотреть.
agent start \"задача\" | wait <id> [--timeout S] [--progress S] | status <id> | result <id>
      ask [<id>|--parent] [--timeout S] \"вопрос\" | send <id> \"текст\" | peers
      broadcast \"текст\" | list | kill <id>

Подагент — отдельная сессия snens со своим логом. start возвращает её id (печатает
в stdout). Надёжный признак конца — запись Done в логе ребёнка, её пишет харнесс:
даже если ребёнок не отчитался, wait её увидит. Контекст (SNENS_SESSION,
SNENS_MAIL_DIR, SNENS_PARENT) харнесс передаёт переменными окружения.

status — неблокирующий снимок: жив/готов, число ходов, последний вызов инструмента,
сколько секунд назад была запись в логе. wait печатает прогресс раз в 30 с
(--progress S). Коды выхода wait: 0 — Done, 3 — истёк таймаут («ещё работает»),
4 — в моём ящике вопрос (kind=ask): он напечатан в stdout, на него отвечают
`agent send <id> \"...\"`, вопрос остаётся в ящике и доедет до хода сам.

list — дерево: родитель (^), братья (=), я (>), дети (-) с состоянием, числом
ходов и временем последней записи. peers — id братьев построчно. ask без id
спрашивает родителя (обратная совместимость); ответ ждём только от адресата.
broadcast шлёт steer всем братьям и родителю.";

pub fn die(msg: String) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

/// Rust игнорирует SIGPIPE, поэтому `agent list | head` падал бы паникой
/// «failed printing to stdout». Возвращаем дефолтное поведение обычного CLI.
#[cfg(unix)]
fn restore_sigpipe() {
    extern "C" {
        fn signal(sig: i32, handler: usize) -> usize;
    }
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe() {}

fn main() {
    restore_sigpipe();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "-h" || argv[0] == "--help" {
        println!("{HELP}");
        std::process::exit(if argv.is_empty() { 1 } else { 0 });
    }
    let cfg = Cfg::load();
    match argv[0].as_str() {
        "start" => cmd_start(&cfg, &argv[1..]),
        "wait" => cmd_wait(&cfg, &argv[1..]),
        "status" => cmd_status(&cfg, &argv[1..]),
        "result" => cmd_result(&cfg, &argv[1..]),
        "ask" => cmd_ask(&cfg, &argv[1..]),
        "send" => cmd_send(&cfg, &argv[1..]),
        "peers" => cmd_peers(&cfg),
        "broadcast" => cmd_broadcast(&cfg, &argv[1..]),
        "list" => cmd_list(&cfg),
        "kill" => cmd_kill(&cfg, &argv[1..]),
        other => die(format!("agent: неизвестная команда {other}")),
    }
}

/// Короткий след вызова: command для run, args для тула — как в TUI.
pub fn hint_of(call: &serde_json::Value) -> String {
    let raw = call
        .get("function")
        .and_then(|f| f.get("arguments"))
        .and_then(|a| a.as_str())
        .map(str::to_string)
        // Формат лога ядра: {id, name, args} — args это строка с JSON аргументов.
        .or_else(|| {
            call.get("args")
                .and_then(|a| a.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    if let Some(c) = v.get("command").and_then(|c| c.as_str()) {
        return c.to_string();
    }
    match v.get("args") {
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| x.to_string())
            })
            .collect::<Vec<_>>()
            .join(" "),
        Some(serde_json::Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

// --- разбор аргументов ---

/// Разобранные аргументы: позиционные по порядку + известные флаги. Флаги и их
/// значения вырезаются, чтобы «--timeout 30» не попал в текст вопроса.
struct Args {
    pos: Vec<String>,
    timeout: Option<u64>,
    progress: Option<u64>,
    parent_flag: bool,
}

fn parse_args(args: &[String]) -> Args {
    let mut out = Args {
        pos: Vec::new(),
        timeout: None,
        progress: None,
        parent_flag: false,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--timeout" => out.timeout = it.next().and_then(|v| v.parse().ok()),
            "--progress" => out.progress = it.next().and_then(|v| v.parse().ok()),
            "--parent" => out.parent_flag = true,
            _ => out.pos.push(a.clone()),
        }
    }
    out
}

/// Текст: позиционные строки, а если их нет — stdin.
fn take_text(rest: &[String]) -> String {
    let joined = rest.join(" ");
    if !joined.trim().is_empty() {
        return joined;
    }
    let mut s = String::new();
    let _ = std::io::stdin().read_to_string(&mut s);
    s.trim().to_string()
}

fn need_id(pos: &[String], what: &str) -> String {
    pos.first()
        .cloned()
        .unwrap_or_else(|| die(format!("agent {what}: нужен id подагента")))
}

// --- команды ---

fn cmd_start(cfg: &Cfg, args: &[String]) {
    let task = take_text(&parse_args(args).pos);
    if task.trim().is_empty() {
        die("agent start: нужна задача".into());
    }
    let mut cmd = Command::new("snens");
    cmd.arg("run").arg("--detach").arg("--new");
    if !cfg.me.is_empty() {
        cmd.arg("--parent").arg(&cfg.me);
    }
    cmd.arg(&task);
    let out = cmd
        .output()
        .unwrap_or_else(|e| die(format!("agent start: {e}")));
    if !out.status.success() {
        die(format!(
            "agent start: snens вернул код {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if id.is_empty() {
        die("agent start: snens не вернул id подагента".into());
    }
    println!("{id}");
}

fn cmd_status(cfg: &Cfg, args: &[String]) {
    let id = need_id(&parse_args(args).pos, "status");
    if !cfg.sessions_dir.join(format!("{id}.jsonl")).is_file() {
        die(format!("agent status: нет лога подагента {id}"));
    }
    println!("{}", log::snapshot(cfg, &id));
}

/// Вопрос, лежащий в ящике: только смотрим, не забираем. Заберёт его ход сессии
/// на ближайшей границе — тогда вопрос попадёт в контекст и на него ответят.
fn pending_question(cfg: &Cfg, me: &str) -> Option<mail::Msg> {
    mail::peek(&cfg.mail_dir, me)
        .into_iter()
        .find(|m| m.kind == "ask")
}

fn cmd_wait(cfg: &Cfg, args: &[String]) {
    let a = parse_args(args);
    let id = need_id(&a.pos, "wait");
    let timeout = a.timeout.unwrap_or(3600);
    // Прогресс идёт в stderr: stdout остаётся чистым (его читают как результат),
    // а тишина на 20 минут перестаёт быть нормой.
    let every = a.progress.unwrap_or(30).max(5);
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let mut last_report = Instant::now();
    loop {
        if let Some(s) = log::done_status(cfg, &id) {
            let text = log::final_text(cfg, &id).unwrap_or_default();
            println!("подагент {id}: {s}");
            if !text.trim().is_empty() {
                println!("{text}");
            }
            return;
        }
        // Пока ждём ребёнка, нам мог прийти вопрос. Ждать дальше — гарантированный
        // дедлок «родитель ждёт ребёнка, ребёнок ждёт родителя»: выходим кодом 4 и
        // отдаём вопрос вызывающему. Из ящика не забираем — он доедет до хода.
        if !cfg.me.is_empty() {
            if let Some(q) = pending_question(cfg, &cfg.me) {
                println!("в ящике вопрос от {}: {}", q.from, q.text);
                eprintln!(
                    "agent wait: вышел по вопросу (код 4). Ответить: agent send {} \"...\"; \
                     вопрос остался в ящике и доедет до хода сам",
                    q.from
                );
                std::process::exit(4);
            }
        }
        // Нет Done, а процесс мёртв — его сняли (OOM/SIGKILL), а не «ещё думает».
        if !log::is_alive(cfg, &id) {
            die(format!(
                "agent wait: процесс подагента {id} мёртв, а Done в логе нет — он был снят"
            ));
        }
        if last_report.elapsed() >= Duration::from_secs(every) {
            eprintln!("{}", log::snapshot(cfg, &id));
            last_report = Instant::now();
        }
        if Instant::now() >= deadline {
            // Таймаут — не сбой, а «ещё работает»: код 3, чтобы вызывающий отличал
            // от падения (1) и понимал, что повторять бессмысленно только если жив.
            eprintln!(
                "agent wait: {id} ещё работает, истёк таймаут {timeout}s (код 3 — не ошибка)\n{}",
                log::snapshot(cfg, &id)
            );
            std::process::exit(3);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn cmd_result(cfg: &Cfg, args: &[String]) {
    let id = need_id(&parse_args(args).pos, "result");
    match log::final_text(cfg, &id) {
        Some(t) => println!("{t}"),
        None => die(format!("agent result: у {id} нет финального текста")),
    }
}

/// Адресат ask: `--parent` или первый позиционный, если это существующий ящик
/// либо за ним есть ещё текст. Иначе весь первый позиционный — сам вопрос. Без
/// явного адресата спрашиваем родителя: SNENS_PARENT, а если её нет — обратным
/// поиском по `children/`.
fn ask_target(cfg: &Cfg, a: &Args) -> (String, Vec<String>) {
    let parent = || {
        if cfg.parent.is_empty() {
            mail::parent_of(&cfg.mail_dir, &cfg.me).unwrap_or_default()
        } else {
            cfg.parent.clone()
        }
    };
    if a.parent_flag {
        return (parent(), a.pos.clone());
    }
    let Some(first) = a.pos.first() else {
        return (parent(), Vec::new());
    };
    if mail::box_dir(&cfg.mail_dir, first).is_dir() || a.pos.len() > 1 {
        (first.clone(), a.pos[1..].to_vec())
    } else {
        (parent(), a.pos.clone())
    }
}

/// Жив ли адресат. pid регистрирует тот, кто запустил сессию, поэтому: свой
/// ребёнок — pid у меня, брат — у общего родителя, а родитель нигде (его pid
/// знает только его родитель) — про такого честно `None`, «не знаю».
fn target_alive(cfg: &Cfg, target: &str) -> Option<bool> {
    log::child_pid(cfg, &cfg.me, target)
        .or_else(|| log::child_pid(cfg, &cfg.parent_or_me(), target))
        .map(log::alive)
}

fn cmd_ask(cfg: &Cfg, raw: &[String]) {
    let a = parse_args(raw);
    let (target, rest) = ask_target(cfg, &a);
    if target.is_empty() || target == cfg.me {
        die("agent ask: некого спрашивать (нет адресата и нет SNENS_PARENT)".into());
    }
    let question = take_text(&rest);
    if question.trim().is_empty() {
        die("agent ask: пустой вопрос".into());
    }
    let me = cfg.me_or_die("ask");
    let asked_at = log::now();
    mail::send(&cfg.mail_dir, &target, &me, "ask", &question);
    let timeout = a.timeout.unwrap_or(900);
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        // Фильтр по адресату и по времени: остальное (итоги других детей, steer
        // человека, чужие вопросы) не выедаем — оно должно доехать до хода сессии,
        // а старое слово того же адресата — не ответ на этот вопрос. Именно
        // поэтому здесь фильтр, а не «забрать всё и вернуть обратное»: возврат
        // переупорядочивал сообщения и мог ходить по кругу.
        let got = mail::drain_filter(&cfg.mail_dir, &me, |m| {
            m.kind == "steer" && m.from == target && m.ts >= asked_at
        });
        if let Some(m) = got.into_iter().next() {
            println!("{}", m.text);
            return;
        }
        // Адресат умер, пока мы ждали, — вопрос читать некому. Не висим: говорим
        // об этом и отдаём ход, чтобы ребёнок действовал сам.
        if target_alive(cfg, &target) == Some(false) {
            die(format!(
                "agent ask: {target} больше не работает — ответа нет, действуй сам"
            ));
        }
        if Instant::now() >= deadline {
            let left = mail::peek(&cfg.mail_dir, &me).len();
            die(format!(
                "agent ask: {target} не ответил за {timeout}s — ответа нет, действуй сам{}",
                if left > 0 {
                    format!(" (в ящике ждёт {left} сообщ.)")
                } else {
                    String::new()
                }
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn cmd_send(cfg: &Cfg, args: &[String]) {
    let pos = parse_args(args).pos;
    let id = need_id(&pos, "send");
    let text = take_text(&pos[1..]);
    if text.trim().is_empty() {
        die("agent send: пустой текст".into());
    }
    let from = if cfg.me.is_empty() { "human" } else { &cfg.me };
    mail::send(&cfg.mail_dir, &id, from, "steer", &text);
    println!("отправлено {id}");
}

/// Братья: другие дети того же родителя. Родитель — из SNENS_PARENT, а если её
/// нет — обратным поиском по `children/`.
fn peers_of(cfg: &Cfg, me: &str) -> (Option<String>, Vec<String>) {
    let parent = if cfg.parent.is_empty() {
        mail::parent_of(&cfg.mail_dir, me)
    } else {
        Some(cfg.parent.clone())
    };
    let sibs = parent
        .as_deref()
        .map(|p| mail::siblings(&cfg.mail_dir, p, me))
        .unwrap_or_default();
    (parent, sibs)
}

fn cmd_peers(cfg: &Cfg) {
    let me = cfg.me_or_die("peers");
    let (parent, sibs) = peers_of(cfg, &me);
    match parent {
        None => eprintln!("agent peers: родитель не найден, братьев не определить"),
        Some(p) if sibs.is_empty() => eprintln!("agent peers: братьев нет (родитель {p})"),
        Some(p) => eprintln!("agent peers: родитель {p}, братьев {}", sibs.len()),
    }
    // stdout — только id: его читают и подставляют в другую команду.
    for s in sibs {
        println!("{s}");
    }
}

fn cmd_broadcast(cfg: &Cfg, args: &[String]) {
    let text = take_text(&parse_args(args).pos);
    if text.trim().is_empty() {
        die("agent broadcast: пустой текст".into());
    }
    let me = cfg.me_or_die("broadcast");
    let (parent, sibs) = peers_of(cfg, &me);
    let mut sent = Vec::new();
    for s in &sibs {
        mail::send(&cfg.mail_dir, s, &me, "steer", &text);
        sent.push(s.clone());
    }
    // Родителю тоже: он держит контекст дерева, и один и тот же текст ему полезен.
    if let Some(p) = parent {
        if p != me {
            mail::send(&cfg.mail_dir, &p, &me, "steer", &text);
            sent.push(p);
        }
    }
    if sent.is_empty() {
        println!("отправлено 0: братьев и родителя нет");
        return;
    }
    println!("отправлено {}: {}", sent.len(), sent.join(" "));
}

fn ago_human(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs} с назад"),
        60..=3599 => format!("{} мин назад", secs / 60),
        _ => format!("{} ч назад", secs / 3600),
    }
}

/// Одна строка дерева: метка, id, состояние, ходы, время последней записи.
fn node_line(cfg: &Cfg, id: &str, mark: &str, indent: &str, tail: &str) -> String {
    let (steps, ago) = log::stats(cfg, id);
    format!(
        "{indent}{mark} {id}: {}, ходов {steps}, запись {}{tail}",
        log::state(cfg, id),
        ago_human(ago)
    )
}

fn cmd_list(cfg: &Cfg) {
    let me = cfg.me.clone();
    let parent = if me.is_empty() {
        None
    } else if cfg.parent.is_empty() {
        mail::parent_of(&cfg.mail_dir, &me)
    } else {
        Some(cfg.parent.clone())
    };
    let sibs = parent
        .as_deref()
        .map(|p| mail::siblings(&cfg.mail_dir, p, &me))
        .unwrap_or_default();
    let kids = mail::children(&cfg.mail_dir, &me);

    println!("дерево (> я, ^ родитель, = брат, - ребёнок)");
    match parent.as_deref() {
        Some(p) => println!("{}", node_line(cfg, p, "^", "", "")),
        None => println!("  (родителя нет — корневая сессия)"),
    }
    if sibs.is_empty() {
        println!("  (братьев нет)");
    }
    for s in &sibs {
        println!("{}", node_line(cfg, s, "=", "  ", ""));
    }
    if me.is_empty() {
        println!("  (я — без SNENS_SESSION, себя в дереве не знаю)");
    } else {
        println!("{}", node_line(cfg, &me, ">", "", ""));
    }
    if kids.is_empty() {
        println!("  (детей нет)");
        return;
    }
    for id in kids {
        let tail = log::final_text(cfg, &id)
            .map(|t| {
                let one = t.split_whitespace().collect::<Vec<_>>().join(" ");
                let s: String = one.chars().take(100).collect();
                format!(" — {s}")
            })
            .unwrap_or_default();
        println!("{}", node_line(cfg, &id, "-", "  ", &tail));
    }
}

fn cmd_kill(cfg: &Cfg, args: &[String]) {
    let id = need_id(&parse_args(args).pos, "kill");
    let pid = log::child_pid(cfg, &cfg.me, &id).or_else(|| log::child_pid(cfg, &cfg.parent, &id));
    let Some(pid) = pid else {
        die(format!("agent kill: не знаю pid подагента {id}"));
    };
    #[cfg(unix)]
    {
        let code = Command::new("kill").arg(pid.to_string()).status();
        match code {
            Ok(s) if s.success() => println!("послан SIGTERM {id} ({pid})"),
            _ => die(format!("agent kill: не смог послать сигнал {pid}")),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        die("agent kill: пока только unix".into());
    }
}
