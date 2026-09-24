// agent — подагенты snens: запустить, ждать, спросить, ответить, посмотреть.
//
// Тул, а не часть ядра: он говорит с сессиями через расклад на диске (лог ребёнка
// и его ящик), а не через внутренности харнесса. Надёжность конца ребёнка — по
// записи `Done` в его логе; её пишет харнесс, поэтому «модель забыла отчитаться»
// тут ничего не ломает.
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const HELP: &str = "\
agent — подагенты snens: запустить, ждать, спросить, ответить, посмотреть.
agent start \"задача\" | wait <id> [--timeout S] | result <id> | ask \"вопрос\" | send <id> \"текст\" | list | kill <id>

Подагент — отдельная сессия snens со своим логом. start возвращает её id (печатает
в stdout). Надёжный признак конца — запись Done в логе ребёнка, её пишет харнесс:
даже если ребёнок не отчитался, wait её увидит. ask ждёт ответа родителя из ящика;
send отвечает/пишет ребёнку. Контекст (SNENS_SESSION, SNENS_MAIL_DIR, SNENS_PARENT)
харнесс передаёт переменными окружения.";

fn die(msg: String) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "-h" || argv[0] == "--help" {
        println!("{HELP}");
        std::process::exit(if argv.is_empty() { 1 } else { 0 });
    }
    let cfg = Cfg::load();
    match argv[0].as_str() {
        "start" => cmd_start(&cfg, &argv[1..]),
        "wait" => cmd_wait(&cfg, &argv[1..]),
        "result" => cmd_result(&cfg, &argv[1..]),
        "ask" => cmd_ask(&cfg, &argv[1..]),
        "send" => cmd_send(&cfg, &argv[1..]),
        "list" => cmd_list(&cfg, &argv[1..]),
        "kill" => cmd_kill(&cfg, &argv[1..]),
        other => die(format!("agent: неизвестная команда {other}")),
    }
}

/// Конфигурация: ящик и сессии. Переменные окружения приоритетнее файла — харнесс
/// передаёт их вызываемому тулу; запущенный человеком тул читает config.toml.
struct Cfg {
    mail_dir: PathBuf,
    sessions_dir: PathBuf,
    me: String,
    parent: String,
}

#[derive(Deserialize, Default)]
struct File {
    #[serde(default)]
    mail_dir: String,
    #[serde(default)]
    sessions_dir: String,
}

impl Cfg {
    fn load() -> Cfg {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default();
        let f: File = std::env::var("SNENS_CONFIG")
            .ok()
            .or_else(|| Some(format!("{home}/.config/snens/config.toml")))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default();
        let mail = std::env::var("SNENS_MAIL_DIR").unwrap_or_else(|_| {
            if f.mail_dir.is_empty() {
                format!("{home}/.local/share/snens/mail")
            } else {
                f.mail_dir.replace('~', &home)
            }
        });
        let sessions = if f.sessions_dir.is_empty() {
            format!("{home}/.local/share/snens/sessions")
        } else {
            f.sessions_dir.replace('~', &home)
        };
        Cfg {
            mail_dir: PathBuf::from(mail),
            sessions_dir: PathBuf::from(sessions),
            me: std::env::var("SNENS_SESSION").unwrap_or_default(),
            parent: std::env::var("SNENS_PARENT").unwrap_or_default(),
        }
    }
}

// --- сообщение в ящике: формат общий с ядром (mail.rs) ---

#[derive(Serialize, Deserialize, Clone)]
struct Msg {
    ts: u64,
    from: String,
    kind: String,
    text: String,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn new_dir(cfg: &Cfg, id: &str) -> PathBuf {
    cfg.mail_dir.join(id).join("new")
}

fn send(cfg: &Cfg, to: &str, from: &str, kind: &str, text: &str) {
    let dir = new_dir(cfg, to);
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| die(format!("agent: ящик {to}: {e}")));
    let msg = Msg {
        ts: now(),
        from: from.to_string(),
        kind: kind.to_string(),
        text: text.to_string(),
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = format!("{nanos:020}-{}-{}.json", sanitize(from), std::process::id());
    let path = dir.join(&name);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string(&msg).unwrap_or_default())
        .unwrap_or_else(|e| die(format!("agent: запись сообщения: {e}")));
    let _ = std::fs::rename(&tmp, &path);
}

/// Забрать входящие в `read/` (как ядро): ровно один потребитель на сообщение.
fn drain(cfg: &Cfg, id: &str) -> Vec<Msg> {
    let dir = new_dir(cfg, id);
    let done = cfg.mail_dir.join(id).join("read");
    let mut names: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    names.sort();
    let mut out = Vec::new();
    for p in names {
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(m) = serde_json::from_str::<Msg>(&text) else {
            let _ = std::fs::remove_file(&p);
            continue;
        };
        let _ = std::fs::create_dir_all(&done);
        let _ = std::fs::rename(&p, done.join(p.file_name().unwrap_or_default()));
        out.push(m);
    }
    out
}

// --- лог ребёнка: конец и финальный текст ---

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Rec {
    Meta {},
    Assistant {
        text: Option<String>,
        #[serde(default)]
        tool_calls: Vec<serde_json::Value>,
    },
    Done {
        status: String,
    },
    #[serde(other)]
    Other,
}

fn read_log(cfg: &Cfg, id: &str) -> Vec<Rec> {
    let path = cfg.sessions_dir.join(format!("{id}.jsonl"));
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Rec>(l).ok())
        .collect()
}

fn done_status(cfg: &Cfg, id: &str) -> Option<String> {
    read_log(cfg, id).into_iter().rev().find_map(|r| match r {
        Rec::Done { status } => Some(status),
        _ => None,
    })
}

fn final_text(cfg: &Cfg, id: &str) -> Option<String> {
    read_log(cfg, id).into_iter().rev().find_map(|r| match r {
        Rec::Assistant {
            text: Some(t),
            tool_calls,
            ..
        } if tool_calls.is_empty() && !t.trim().is_empty() => Some(t),
        _ => None,
    })
}

fn child_pid(cfg: &Cfg, parent: &str, id: &str) -> Option<u32> {
    std::fs::read_to_string(cfg.mail_dir.join(parent).join("children").join(id))
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    unsafe { libc_kill(pid) }
}
#[cfg(not(unix))]
fn alive(_pid: u32) -> bool {
    true
}

#[cfg(unix)]
unsafe fn libc_kill(pid: u32) -> bool {
    // kill(pid, 0): сигнал 0 не шлётся, только проверка существования.
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid as i32, 0) == 0
}

fn state(cfg: &Cfg, id: &str) -> String {
    match done_status(cfg, id) {
        Some(s) if s == "ok" => "готов".into(),
        Some(s) => format!("неудача ({s})"),
        None => {
            let live = child_pid(cfg, &cfg.parent_or_me(), id)
                .or_else(|| child_pid(cfg, &cfg.me, id))
                .map(alive)
                .unwrap_or(false);
            if live {
                "работает".into()
            } else {
                "убит".into()
            }
        }
    }
}

impl Cfg {
    fn parent_or_me(&self) -> String {
        if self.parent.is_empty() {
            self.me.clone()
        } else {
            self.parent.clone()
        }
    }
}

// --- команды ---

fn take_text(args: &[String]) -> String {
    let joined = args.join(" ");
    if !joined.trim().is_empty() {
        return joined;
    }
    let mut s = String::new();
    let _ = std::io::stdin().read_to_string(&mut s);
    s.trim().to_string()
}

fn cmd_start(cfg: &Cfg, args: &[String]) {
    let task = take_text(args);
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

fn parse_timeout(args: &[String], default: u64) -> u64 {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--timeout" {
            if let Some(v) = it.next() {
                return v.parse().unwrap_or(default);
            }
        }
    }
    default
}

fn first_arg(args: &[String]) -> String {
    args.iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| die("agent: нужен id подагента".into()))
}

fn cmd_wait(cfg: &Cfg, args: &[String]) {
    let id = first_arg(args);
    let timeout = parse_timeout(args, 3600);
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        if let Some(s) = done_status(cfg, &id) {
            let text = final_text(cfg, &id).unwrap_or_default();
            println!("подагент {id}: {s}");
            if !text.trim().is_empty() {
                println!("{text}");
            }
            return;
        }
        // Нет Done, а процесс мёртв — его сняли (OOM/SIGKILL), а не «ещё думает».
        let live = child_pid(cfg, &cfg.me, &id)
            .or_else(|| child_pid(cfg, &cfg.parent, &id))
            .map(alive)
            .unwrap_or(false);
        if !live {
            die(format!(
                "agent wait: процесс подагента {id} мёртв, а Done в логе нет — он был снят"
            ));
        }
        if Instant::now() >= deadline {
            die(format!(
                "agent wait: {id} ещё работает, истёк таймаут {timeout}s"
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn cmd_result(cfg: &Cfg, args: &[String]) {
    let id = first_arg(args);
    match final_text(cfg, &id) {
        Some(t) => println!("{t}"),
        None => die(format!("agent result: у {id} нет финального текста")),
    }
}

fn cmd_ask(cfg: &Cfg, args: &[String]) {
    if cfg.parent.is_empty() {
        die("agent ask: я не подагент (нет SNENS_PARENT) — спрашивать некого".into());
    }
    let question = take_text(args);
    if question.trim().is_empty() {
        die("agent ask: пустой вопрос".into());
    }
    send(cfg, &cfg.parent, &cfg.me, "ask", &question);
    // Ждём ответ родителя: он приходит тем же каналом, что steer человека.
    let timeout = parse_timeout(args, 900);
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        for m in drain(cfg, &cfg.me) {
            if m.kind == "steer" && m.from == cfg.parent {
                println!("{}", m.text);
                return;
            }
            // Не ответ (итог другого подагента, чей-то steer) — вернём в ящик:
            // его прочитает цикл сессии на ближайшей границе хода.
            send(cfg, &cfg.me, &m.from, &m.kind, &m.text);
        }
        // Родитель умер, пока мы ждали, — вопрос читать некому. Не висим: говорим
        // об этом и отдаём ход, чтобы ребёнок действовал сам.
        let plive = child_pid(cfg, &cfg.parent, &cfg.me)
            .map(alive)
            .or_else(|| {
                // pid родителя не зарегистрирован у нас — считаем живым, пока лог растёт
                let _ = &cfg.parent;
                None
            })
            .unwrap_or(true);
        if !plive {
            die("agent ask: родитель больше не работает — ответа нет, действуй сам".into());
        }
        if Instant::now() >= deadline {
            die(format!(
                "agent ask: родитель не ответил за {timeout}s — ответа нет, действуй сам"
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn cmd_send(cfg: &Cfg, args: &[String]) {
    let id = first_arg(args);
    let rest: Vec<String> = args.iter().filter(|a| *a != &id).cloned().collect();
    let text = take_text(&rest);
    if text.trim().is_empty() {
        die("agent send: пустой текст".into());
    }
    let from = if cfg.me.is_empty() { "human" } else { &cfg.me };
    send(cfg, &id, from, "steer", &text);
    println!("отправлено {id}");
}

fn cmd_list(cfg: &Cfg, _args: &[String]) {
    let me = if cfg.me.is_empty() {
        die("agent list: не знаю свою сессию (нет SNENS_SESSION)".into())
    } else {
        cfg.me.clone()
    };
    let dir = cfg.mail_dir.join(&me).join("children");
    let mut ids: Vec<String> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect(),
        Err(_) => Vec::new(),
    };
    ids.sort();
    if ids.is_empty() {
        println!("(детей нет)");
        return;
    }
    for id in ids {
        let st = state(cfg, &id);
        let tail = final_text(cfg, &id)
            .map(|t| {
                let one = t.split_whitespace().collect::<Vec<_>>().join(" ");
                let s: String = one.chars().take(100).collect();
                format!(" — {s}")
            })
            .unwrap_or_default();
        println!("{id}: {st}{tail}");
    }
}

fn cmd_kill(cfg: &Cfg, args: &[String]) {
    let id = first_arg(args);
    let pid = child_pid(cfg, &cfg.me, &id);
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
