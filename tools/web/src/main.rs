// web — браузер машины пользователя: живой Chrome по согласию (CDP), дерево
// доступности вместо пикселей, действия по адресам и батчем.
//
// Почему так, а не «скриншот → клик по координатам»: полное AX-дерево страницы —
// 500–600 КБ JSON, сжатое по интерактивным узлам — 10–16 КБ; скриншот стоит
// 1000–1800 токенов на кадр и не даёт адреса, по которому можно нажать.
// Поэтому снимок дерева уходит в файл, в контекст едет путь и счётчики, а
// действия идут по `e12` из последнего снимка.
//
// Соединение: `web connect` находит Chrome, включённый для отладки по согласию
// (chrome://inspect/#remote-debugging, Chrome 144+), и запоминает порт/вкладку.
// Дальше каждый вызов — отдельный процесс (важно для работы через ssh): адреса
// лежат в ~/.local/state/snens/web/<tab>/refs.json.
use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{json, Value};

pub mod act;
pub mod cdp;
pub mod snap;
pub mod state;

const USAGE: &str = "web <команда> … — браузер машины пользователя через CDP: снимок дерева с адресами, действия по адресам и батчем, скриншот как fallback";

const HELP: &str = "\
web — браузер машины пользователя через CDP: дерево с адресами, действия батчем, скриншот как fallback.
web connect|status|tabs|use|open|snap|act|shot|eval|text|probe|close [аргументы]

Соединение — к Chrome, разрешённому для отладки по согласию
(chrome://inspect/#remote-debugging, Chrome 144+), или к отдельному профилю агента.

web connect [--port N]                 найти Chrome и запомнить порт (env SNENS_CDP_PORT)
web status                             что сейчас подключено и какие вкладки открыты
web open <url> [--new]                 перейти в текущей вкладке (или открыть новую)
web tabs [--all]                       список вкладок; --all — включая чужие
web use <tabId|N>                      переключиться на вкладку по id или номеру из tabs
web snap [--depth N] [--full]          снимок дерева доступности: файл + счётчики, адреса e1…eN
web act --json '<действия>' [--no-snap]  действия по адресам батчем: click/type/set/press/scroll/hover/select/navigate/wait
web shot [--full] [--out файл]         скриншот вкладки (fallback и верификация)
web eval <выражение>                   выполнить JS на странице (проверка, извлечение)
web text [--limit N]                   видимый текст страницы (дешёвое чтение)
web probe                              проверить, что ввод реально доставляется (клик/клавиша/вставка)
web close                              закрыть текущую вкладку и снять привязку

Снимок дерева НЕ уезжает в вывод целиком: печатаются путь к файлу, число узлов
и первые строки — остальное читается через grep/read. Действие возвращает
дельту состояния, а не всё дерево заново. Адреса живут до следующего `snap`.

Порядок предпочтения: CDP-действие (дёшево, без пикселей) → дерево → скриншот
и координаты только там, где дерева нет (canvas, чужое приложение).

Порты/соединение: --port, SNENS_CDP_PORT, иначе 9222. Chrome 136+ игнорирует
--remote-debugging-port на дефолтном профиле: нужен тумблер в chrome://inspect
или отдельный профиль агента.

Коды: 0 — успех, 2 — неверные аргументы, 1 — ошибка выполнения (включая таймаут).";

fn die(msg: &str, code: i32) -> ! {
    eprintln!("{msg}");
    std::process::exit(code);
}

// ---------------------------------------------------------------- аргументы

#[derive(Default)]
pub struct Args {
    pub positional: Vec<String>,
    pub flags: BTreeMap<String, String>,
    pub has: Vec<String>,
}

impl Args {
    fn parse(argv: &[String]) -> Result<Self, String> {
        let mut a = Args::default();
        let mut it = argv.iter().peekable();
        while let Some(x) = it.next() {
            if let Some(name) = x.strip_prefix("--") {
                let (k, v) = match name.split_once('=') {
                    Some((k, v)) => (k.to_string(), Some(v.to_string())),
                    None => (name.to_string(), None),
                };
                if let Some(v) = v {
                    a.flags.insert(k, v);
                } else if let Some(next) = it.peek() {
                    // Значение флага — только если это не следующий флаг и не
                    // подкоманда в известной позиции; иначе голый флаг.
                    if !next.starts_with("--") && !a.positional.is_empty() {
                        let v = it.next().unwrap().clone();
                        a.flags.insert(k, v);
                    } else if !next.starts_with("--") && !is_command(next) {
                        let v = it.next().unwrap().clone();
                        a.flags.insert(k, v);
                    } else {
                        a.has.push(k);
                    }
                } else {
                    a.has.push(k);
                }
            } else {
                a.positional.push(x.clone());
            }
        }
        Ok(a)
    }

    pub fn flag(&self, name: &str) -> Option<&str> {
        self.flags.get(name).map(String::as_str)
    }

    pub fn on(&self, name: &str) -> bool {
        self.has.iter().any(|h| h == name) || self.flags.contains_key(name)
    }

    pub fn num(&self, name: &str, def: f64) -> f64 {
        self.flag(name)
            .and_then(|v| v.parse().ok())
            .unwrap_or(def)
    }
}

fn is_command(s: &str) -> bool {
    matches!(
        s,
        "connect"
            | "status"
            | "open"
            | "tabs"
            | "use"
            | "snap"
            | "act"
            | "shot"
            | "eval"
            | "text"
            | "probe"
            | "close"
    )
}

// ---------------------------------------------------------------- соединение

fn port_from(args: &Args) -> u16 {
    args.flag("port")
        .and_then(|p| p.parse().ok())
        .or_else(|| std::env::var("SNENS_CDP_PORT").ok().and_then(|p| p.parse().ok()))
        .unwrap_or(9222)
}

pub struct Live {
    pub cdp: cdp::Cdp,
    pub tab: String,
    pub port: u16,
}

/// Открывает CDP текущей вкладки, поднимая привязку из состояния.
pub fn live(args: &Args) -> Result<Live, String> {
    let (target, tab) = match state::load_target() {
        Some(t) => t,
        None => {
            return Err(
                "не подключено: сначала `web connect` (Chrome с тумблером в chrome://inspect/#remote-debugging)"
                    .into(),
            )
        }
    };
    let port = args
        .flag("port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(target.port);
    let url = cdp::ws_url(port, Some(&tab)).map_err(|e| {
        format!("{e}\nвкладка {tab} пропала — `web status`, чтобы увидеть живые, `web connect` заново")
    })?;
    let cdp = cdp::Cdp::connect(&url, args.num("timeout", 30.0))?;
    Ok(Live { cdp, tab, port })
}

fn page_targets(port: u16) -> Result<Vec<Value>, String> {
    let raw = cdp::http_get(port, "/json/list")?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| format!("CDP /json/list: {e}"))?;
    let mut out = Vec::new();
    for t in v.as_array().cloned().unwrap_or_default() {
        if t.get("type").and_then(Value::as_str) == Some("page") {
            out.push(t);
        }
    }
    Ok(out)
}

fn tab_line(i: usize, t: &Value) -> String {
    let id = t.get("id").and_then(Value::as_str).unwrap_or("");
    let short_id: String = id.chars().take(8).collect();
    let url = t.get("url").and_then(Value::as_str).unwrap_or("");
    let title = t.get("title").and_then(Value::as_str).unwrap_or("");
    format!(
        "{:>2}. {short_id}  {}  —  {}",
        i + 1,
        trunc(title, 60),
        trunc(url, 90)
    )
}

pub fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

// ---------------------------------------------------------------- команды

fn cmd_connect(args: &Args) -> Result<(), String> {
    let port = port_from(args);
    // Проверяем не порт, а живой CDP: без согласия порт открыт, а /json/list пуст
    // или отдаёт 403 — тул должен сказать об этом прямо, а не падать на WS.
    let targets = page_targets(port).map_err(|e| {
        format!(
            "{e}\n\nПодсказка: в Chrome открой chrome://inspect/#remote-debugging и включи тумблер \
             (Chrome 144+, потребуется Allow в диалоге). Отдельный профиль агента:\n  \
             chrome --remote-debugging-port={port} --user-data-dir=~/.local/share/snens/web-profile &"
        )
    })?;
    if targets.is_empty() {
        return Err("в этом Chrome нет ни одной вкладки — открой страницу и повтори".into());
    }
    let tab = targets[0]
        .get("id")
        .and_then(Value::as_str)
        .ok_or("в /json/list нет id вкладки")?
        .to_string();
    state::save_target(state::Target { port }, &tab)?;
    println!(
        "подключено: 127.0.0.1:{port}, вкладка {} ({})",
        trunc(&tab, 8),
        trunc(targets[0].get("title").and_then(Value::as_str).unwrap_or(""), 60)
    );
    for (i, t) in targets.iter().enumerate().take(10) {
        println!("{}", tab_line(i, t));
    }
    Ok(())
}

fn cmd_status(args: &Args) -> Result<(), String> {
    let port = match state::load_target() {
        Some((t, tab)) => {
            println!("привязка: 127.0.0.1:{}, вкладка {}", t.port, trunc(&tab, 8));
            t.port
        }
        None => {
            println!("привязки нет (`web connect`)");
            port_from(args)
        }
    };
    let targets = page_targets(port)?;
    println!("вкладок: {}", targets.len());
    for (i, t) in targets.iter().enumerate().take(30) {
        println!("{}", tab_line(i, t));
    }
    Ok(())
}

fn cmd_tabs(args: &Args) -> Result<(), String> {
    let port = match state::load_target() {
        Some((t, _)) => t.port,
        None => port_from(args),
    };
    let targets = page_targets(port)?;
    for (i, t) in targets.iter().enumerate() {
        println!("{}", tab_line(i, t));
    }
    Ok(())
}

fn cmd_use(args: &Args) -> Result<(), String> {
    let want = args
        .positional
        .get(0)
        .ok_or("web use: нужен id вкладки или её номер из `web tabs`")?;
    let (port, _) = match state::load_target() {
        Some((t, _)) => (t.port, String::new()),
        None => {
            return Err("не подключено: сначала `web connect`".into());
        }
    };
    let targets = page_targets(port)?;
    let tab = if let Ok(n) = want.parse::<usize>() {
        targets
            .get(n.saturating_sub(1))
            .and_then(|t| t.get("id").and_then(Value::as_str))
            .ok_or_else(|| format!("нет вкладки №{n}"))?
            .to_string()
    } else {
        targets
            .iter()
            .find(|t| {
                t.get("id")
                    .and_then(Value::as_str)
                    .map(|id| id.starts_with(want.as_str()))
                    .unwrap_or(false)
            })
            .and_then(|t| t.get("id").and_then(Value::as_str))
            .ok_or_else(|| format!("нет вкладки с id, начинающимся на {want}"))?
            .to_string()
    };
    state::save_target(state::Target { port }, &tab)?;
    println!("вкладка: {}", trunc(&tab, 8));
    Ok(())
}

fn cmd_open(args: &Args) -> Result<(), String> {
    let url = args
        .positional
        .get(0)
        .ok_or("web open: нужен URL")?
        .clone();
    if !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("file://")) {
        return Err(format!("web open: только http/https/file, а не {url}"));
    }
    let mut l = live(args)?;
    if args.on("new") {
        // Новая вкладка создаётся браузерным endpoint'ом, а не вкладочным:
        // Target.createTarget доступен только там.
        let port = l.port;
        let ws = cdp::ws_url(port, None)?;
        let mut b = cdp::Cdp::connect(&ws, args.num("timeout", 30.0))?;
        let r = b.call("Target.createTarget", json!({"url": url}))?;
        let id = r
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or("CDP: Target.createTarget не вернул targetId")?;
        let tab = id.to_string();
        state::save_target(state::Target { port }, &tab)?;
        l = live(args)?;
    } else {
        let r = l.cdp.call("Page.navigate", json!({"url": url, "transitionType": "typed"}))?;
        if let Some(err) = r.get("errorText").and_then(Value::as_str) {
            if !err.is_empty() {
                return Err(format!("страница не открылась: {url}: {err}"));
            }
        }
    }
    l.cdp.wait_ready(Duration::from_secs(args.num("wait", 15.0) as u64))?;
    report_page(&mut l.cdp, "открыто")
}

/// Короткая сводка страницы вместо дампа: что открылось и сколько узлов.
fn report_page(cdp: &mut cdp::Cdp, verb: &str) -> Result<(), String> {
    let title = cdp
        .eval("document.title")
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let url = cdp
        .eval("location.href")
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    println!("{verb}: {} — {}", trunc(&url, 120), trunc(&title, 80));
    println!("дальше: `web snap` — дерево с адресами, `web text` — видимый текст");
    Ok(())
}

fn cmd_snap(args: &Args) -> Result<(), String> {
    let mut l = live(args)?;
    let depth = args.num("depth", 30.0) as usize;
    let s = snap::take_stable(&mut l.cdp, depth)?;
    let full = if args.on("full") {
        s.text.clone()
    } else {
        s.text
    };
    let path = state::save_page(&l.tab, &full)?;
    state::save_url(&l.tab, &s.url);
    let refs_path = state::save_refs(&l.tab, &s.refs)?;
    println!(
        "{} — {}",
        trunc(&s.url, 120),
        trunc(&s.title, 80)
    );
    println!(
        "дерево: {} узлов, интерактивных {} → {}",
        s.total,
        s.refs.len(),
        path.display()
    );
    println!("адреса → {}", refs_path.display());
    // 12 строк примера: чтобы модель поняла форму, но не платила за весь экран.
    for line in full.lines().take(12) {
        println!("{line}");
    }
    if full.lines().count() > 12 {
        println!(
            "… ещё {} строк, читать: grep -n <слово> {}",
            full.lines().count() - 12,
            path.display()
        );
    }
    Ok(())
}

fn cmd_text(args: &Args) -> Result<(), String> {
    let mut l = live(args)?;
    let limit = args.num("limit", 4000.0) as usize;
    let v = l.cdp.eval(
        "(() => { const b = document.body; if (!b) return ''; \
         const t = b.innerText || b.textContent || ''; return t.replace(/\\n{3,}/g, '\\n\\n').trim(); })()",
    )?;
    let text = v.as_str().unwrap_or("").to_string();
    let cut: String = text.chars().take(limit).collect();
    println!("{cut}");
    if text.chars().count() > limit {
        println!("… обрезано: {} символов всего, --limit чтобы поднять", text.chars().count());
    }
    Ok(())
}

fn cmd_act(args: &Args) -> Result<(), String> {
    act::run(args)
}

fn cmd_eval(args: &Args) -> Result<(), String> {
    let expr = args
        .flag("expr")
        .map(str::to_string)
        .or_else(|| args.positional.get(0).cloned())
        .ok_or("web eval: нужно JS-выражение (аргументом или --expr)")?;
    let mut l = live(args)?;
    let v = l.cdp.eval(&expr)?;
    match v {
        Value::String(s) => println!("{s}"),
        other => println!("{}", serde_json::to_string_pretty(&other).unwrap_or_default()),
    }
    Ok(())
}

/// Проба доставки ввода. Нужна потому, что отказ бывает молчаливым: вкладка
/// рапортует `visible`/`focused`, а события до страницы не доходят (окно не
/// получило фокус от WM, вкладка не активна в браузере). Тогда клик «успешен»
/// и ничего не делает — худший вид поломки. Проба отвечает фактом.
fn cmd_probe(args: &Args) -> Result<(), String> {
    let mut l = live(args)?;
    let _ = l.cdp.eval(
        "window.__snens_probe={mm:0,md:0,cl:0,kd:0,put:0};\
         ['mousemove','mousedown','click','keydown','input'].forEach(t=>\
           document.addEventListener(t,()=>window.__snens_probe[t==='mousemove'?'mm':t==='mousedown'?'md':t==='click'?'cl':t==='keydown'?'kd':'put']++,true)); 1",
    )?;
    let act = |l: &mut Live, json_actions: &str| -> Result<(), String> {
        let args2 = Args {
            positional: Vec::new(),
            flags: [("json".to_string(), json_actions.to_string())]
                .into_iter()
                .collect(),
            has: vec!["no-snap".to_string()],
        };
        act::run_with(l, &args2)
    };
    act(&mut l, r#"[{"click":[10,10]},{"press":"a"},{"eval":"1"}]"#)?;
    let r = l
        .cdp
        .eval("JSON.stringify(window.__snens_probe || {})")?
        .as_str()
        .unwrap_or("{}")
        .to_string();
    let v: Value = serde_json::from_str(&r).unwrap_or(Value::Null);
    let get = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let vis = l
        .cdp
        .eval("document.visibilityState")
        .ok()
        .and_then(|x| x.as_str().map(str::to_string))
        .unwrap_or_default();
    println!(
        "вкладка: {vis}; события страницы — mousemove {}, mousedown {}, click {}, keydown {}, input {}",
        get("mm"),
        get("md"),
        get("cl"),
        get("kd"),
        get("put")
    );
    if get("mm") + get("md") + get("cl") + get("kd") == 0 {
        return Err(format!(
            "ввод НЕ доставляется (событий нет). Причины по частоте: вкладка не активна в браузере — \
             `Target.activateTarget` не сработал, потому что активировать нечего (окно свёрнуто/без WM); \
             окно без фокуса X (нет менеджера окон); вкладка на другом рабочем столе.\
\nСейчас: visibilityState={vis}. Проверь `web tabs` (одна вкладка на сессию) и что WM жив."
        ));
    }
    println!("ввод доставляется");
    Ok(())
}

fn cmd_close(args: &Args) -> Result<(), String> {
    let l = live(args)?;
    let port = l.port;
    let ws = cdp::ws_url(port, None)?;
    let mut b = cdp::Cdp::connect(&ws, args.num("timeout", 30.0))?;
    b.call("Target.closeTarget", json!({"targetId": l.tab}))?;
    state::clear_target();
    println!("вкладка закрыта, привязка снята");
    Ok(())
}

fn cmd_shot(args: &Args) -> Result<(), String> {
    let mut l = live(args)?;
    let r = l.cdp.call(
        "Page.captureScreenshot",
        json!({"format": "png", "captureBeyondViewport": args.on("full")}),
    )?;
    let b64 = r
        .get("data")
        .and_then(Value::as_str)
        .ok_or("CDP: captureScreenshot без data")?;
    let bytes = base64_decode(b64)?;
    let out = args
        .flag("out")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let d = std::env::var("SNENS_ATTACH_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| state::dir());
            d.join(format!("web-{}.png", l.tab.chars().take(8).collect::<String>()))
        });
    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&out, &bytes).map_err(|e| format!("{}: {e}", out.display()))?;
    println!("{}: image/png {} Б", out.display(), bytes.len());
    Ok(())
}

/// Минимальный base64: тащить крейт ради скриншота не нужно.
fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    const NL: u8 = 10;
    const CR: u8 = 13;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => continue,
            NL | CR => continue,
            _ => return Err("CDP: не base64 в data".into()),
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

/// SIGPIPE в дефолтное состояние: Rust его глушит, и запись в закрытый пайп
/// (`web snap | head`) превращается в панику «failed printing to stdout».
/// Тихий выход по SIGPIPE — то, чего ждут от CLI.
#[cfg(unix)]
fn allow_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn allow_sigpipe() {}

fn main() {
    allow_sigpipe();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "--help" || argv[0] == "-h" {
        println!("{HELP}");
        return;
    }
    let cmd = argv[0].clone();
    let args = match Args::parse(&argv[1..]) {
        Ok(a) => a,
        Err(e) => die(&e, 2),
    };
    let r = match cmd.as_str() {
        "connect" => cmd_connect(&args),
        "status" => cmd_status(&args),
        "tabs" => cmd_tabs(&args),
        "use" => cmd_use(&args),
        "open" => cmd_open(&args),
        "snap" => cmd_snap(&args),
        "act" => cmd_act(&args),
        "shot" => cmd_shot(&args),
        "eval" => cmd_eval(&args),
        "text" => cmd_text(&args),
        "probe" => cmd_probe(&args),
        "close" => cmd_close(&args),
        other => {
            eprintln!("web: неизвестная команда {other}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        die(&e, 1);
    }
}
