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

use cdp::Endpoint;
use serde_json::{json, Value};

pub mod act;
pub mod cdp;
pub mod launch;
pub mod snap;
pub mod state;
pub mod tunnel;

const USAGE: &str = "web <команда> … — браузер машины пользователя через CDP: снимок дерева с адресами, действия по адресам и батчем, скриншот как fallback";

const HELP: &str = "\
web — браузер машины пользователя через CDP: дерево с адресами, действия батчем, скриншот как fallback.
web connect|status|tabs|use|open|snap|act|shot|eval|text|probe|disconnect|launch|show|close [аргументы]

Соединение — к Chrome, разрешённому для отладки по согласию
(chrome://inspect/#remote-debugging, Chrome 144+), или к отдельному профилю агента.

web connect [--port N]                 найти Chrome и запомнить порт (env SNENS_CDP_PORT)
web connect --tunnel user@machine       то же к Chrome на другой машине через ssh
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
web disconnect                         закрыть ssh-туннель (Chrome и вкладки не трогаем)
web launch [--profile путь] [--port N] [--visible]  запустить ОТДЕЛЬНЫЙ профиль Chrome
                                       на машине пользователя: без подтверждений, переживает ssh.
                                       --visible — в сессии пользователя, чтобы окно было видно
web show                               вывести окно агентского Chrome на экран (Windows)

--tunnel user@machine                  говорить с Chrome на той машине через ssh-туннель:
                                       на той стороне web НЕ нужен, только Chrome с согласием
--host user@machine                    выполнить ту же команду на другой машине по ssh
                                       (там тоже должен стоять web; вывод — как локально)

Проверка с машины пользователя, где ничего не установлено:
  web connect --tunnel user@machine        # ssh -L к его Chrome, порт 9222
  web snap / web act …                        # дальше как обычно, туннель уже поднят
  web disconnect                              # закрыть туннель

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
                    // Значение флага — только если следующее слово не флаг и не
                    // подкоманда в известной позиции; иначе это голый флаг.
                    // Два условия в одном: «позиционные уже были» (значит это
                    // аргументы, а не подкоманда) или «слово не команда».
                    // Проверка на `--` общая: значение самого флага флагом быть
                    // не может.
                    // У булевых флагов значения не бывает: иначе `web open --new
                    // https://…` съедал URL как значение --new.
                    let takes_value = !next.starts_with("--")
                        && !BOOL_FLAGS.contains(&k.as_str())
                        && (!a.positional.is_empty() || !is_command(next));
                    if takes_value {
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
        self.flag(name).and_then(|v| v.parse().ok()).unwrap_or(def)
    }

    /// Число с проверкой: отрицательное, NaN и inf — ошибка аргументов, а не
    /// тихое 0/усечение (`--timeout 0.5` раньше давало нулевой таймаут).
    pub fn num_checked(&self, name: &str, def: f64, lo: f64, hi: f64) -> Result<f64, String> {
        let v = self.num(name, def);
        if !v.is_finite() || v < lo || v > hi {
            return Err(format!(
                "web: --{name} ждёт число {lo}..{hi}, получено {v}"
            ));
        }
        Ok(v)
    }
}

/// Флаги-переключатели: значения не принимают.
const BOOL_FLAGS: [&str; 8] = [
    "new",
    "visible",
    "headless",
    "full",
    "all",
    "no-snap",
    "no-wait",
    "fresh",
];

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
            | "disconnect"
            | "launch"
            | "show"
            | "close"
    )
}

// ---------------------------------------------------------------- соединение

/// Порт CDP. Явно заданный и кривой — это ошибка аргументов, а не тихий откат
/// на дефолт: иначе `--port 92a22` подключается «куда-то» и падает позже.
fn port_from(args: &Args) -> Result<u16, String> {
    if let Some(p) = args.flag("port") {
        return p
            .parse::<u16>()
            .map_err(|e| format!("web: --port ждёт число 1..65535, получено {p} ({e})"));
    }
    if let Ok(p) = std::env::var("SNENS_CDP_PORT") {
        if !p.trim().is_empty() {
            return p
                .trim()
                .parse::<u16>()
                .map_err(|e| format!("web: SNENS_CDP_PORT={p}: {e}"));
        }
    }
    Ok(9222)
}

/// Куда стучаться: живой ssh-туннель (если поднят) или прямой порт.
/// `--tunnel user@host` поднимает туннель немедленно и дальше всё идёт через него.
fn endpoint(args: &Args) -> Result<Endpoint, String> {
    if let Some(host) = args.flag("tunnel") {
        // Порт на той стороне: --port (по умолчанию 9222).
        match tunnel::start(host, port_from(args)?) {
            Ok(t) => {
                let mut ep = t.endpoint();
                ep.browser_path = tunnel::remote_browser_path(host, ep.port, ep.remote_port);
                return Ok(ep);
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }
    // Явный --port/SNENS_CDP_PORT сильнее сохранённой привязки: иначе после
    // первого `connect` переключиться на другой Chrome без удаления состояния
    // невозможно, и флаг молча игнорируется.
    let explicit_port = args.flag("port").is_some() || std::env::var("SNENS_CDP_PORT").is_ok();
    if !explicit_port {
        if let Some((target, _)) = state::load_target() {
            return Ok(Endpoint {
                host: "127.0.0.1".into(),
                port: target.port,
                remote_port: target.remote_port,
                browser_path: target.browser_path.clone(),
            });
        }
    } else {
        // Порт задан руками — туннель к нему тоже надо поднять заново.
        if let Some(t) = tunnel::load() {
            if t.remote_port != port_from(args)? {
                tunnel::stop(&t);
            }
        }
    }
    if let Some(t) = tunnel::load() {
        // Порт туннеля должен совпадать с запрошенным: иначе молча работаем не
        // с тем Chrome.
        if !explicit_port || t.remote_port == port_from(args)? {
            let mut ep = t.endpoint();
            ep.browser_path = tunnel::remote_browser_path(&t.host, ep.port, ep.remote_port);
            return Ok(ep);
        }
    }
    Ok(Endpoint::direct(port_from(args)?))
}

pub struct Live {
    pub cdp: cdp::Cdp,
    pub tab: String,
    pub port: u16,
    pub ep: Endpoint,
}

/// Открывает CDP текущей вкладки, поднимая привязку из состояния.
pub fn live(args: &Args) -> Result<Live, String> {
    let (target, tab) = match state::load_target() {
        Some(t) => t,
        None => {
            return Err(
                "не подключено: сначала `web connect` (Chrome с тумблером в chrome://inspect/#remote-debugging) \
                 или `web connect --tunnel user@machine`"
                    .into(),
            )
        }
    };
    let ep = if args.flag("tunnel").is_some() {
        endpoint(args)?
    } else {
        Endpoint {
            host: "127.0.0.1".into(),
            port: target.port,
            remote_port: target.remote_port,
            browser_path: target.browser_path.clone(),
        }
    };
    let url = ep.ws_url(Some(&tab)).map_err(|e| {
        format!(
            "{e}\nвкладка {tab} пропала — `web status`, чтобы увидеть живые, `web connect` заново"
        )
    })?;
    let host_header = format!("127.0.0.1:{}", ep.remote_port);
    let timeout = args.num_checked("timeout", 30.0, 0.1, 3600.0)?;
    // Два пути к вкладке. Обычный Chrome пускает по прямому ws вкладки; Chrome
    // ≥144 в режиме одобрения отвечает на него 403 и пускает только через
    // сессию (`Target.attachToTarget` в ws браузера).
    let cdp = match cdp::Cdp::connect_with_host(&url, timeout, Some(&host_header)) {
        Ok(c) => c,
        Err(e) if e.contains("403") => {
            let bws = ep.browser_ws()?;
            let mut b = cdp::Cdp::connect_with_host(&bws, timeout, Some(&host_header))?;
            let sid = b.attach(&tab)?;
            let mut b = b.with_session(sid.clone());
            // Проверяем, что сессия живая, до того как отдать её вызывающему:
            // иначе первая же команда упадёт непонятной ошибкой.
            b.call(
                "Runtime.evaluate",
                json!({"expression": "1", "returnByValue": true}),
            )?;
            b
        }
        Err(e) => return Err(e),
    };
    Ok(Live {
        cdp,
        tab,
        port: ep.port,
        ep,
    })
}

/// Вкладки: сначала обычный `/json/list`, а если его нет (Chrome ≥144 в режиме
/// одобрения отдаёт на `/json/*` 404) — `Target.getTargets` по ws браузера.
/// Форма ответа приводится к одному виду: id/title/url.
fn page_targets(ep: &Endpoint) -> Result<Vec<Value>, String> {
    if let Ok(raw) = ep.get("/json/list") {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            // Не-массив — это не «вкладок нет»: пробуем путь через ws, а не
            // рапортуем «в этом Chrome нет вкладок».
            let Some(list) = v.as_array() else {
                return targets_via_ws(ep);
            };
            let out: Vec<Value> = list
                .clone()
                .into_iter()
                .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
                .collect();
            return Ok(out);
        }
    }
    targets_via_ws(ep)
}

fn targets_via_ws(ep: &Endpoint) -> Result<Vec<Value>, String> {
    let ws = ep.browser_ws().map_err(|e| {
        format!(
            "{e}\n\nПодсказка: в Chrome на той машине открой chrome://inspect/#remote-debugging \
             и включи тумблер (Chrome 144+, потребуется Allow в диалоге)."
        )
    })?;
    let mut cdp = cdp::Cdp::connect(&ws, 20.0)?;
    let r = cdp.call("Target.getTargets", Value::Null)?;
    let mut out = Vec::new();
    for t in r
        .get("targetInfos")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        if t.get("type").and_then(Value::as_str) != Some("page") {
            continue;
        }
        out.push(json!({
            "id": t.get("targetId").cloned().unwrap_or(Value::Null),
            "title": t.get("title").cloned().unwrap_or(Value::Null),
            "url": t.get("url").cloned().unwrap_or(Value::Null),
        }));
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
    let ep = endpoint(args)?;
    let port = ep.port;
    // Проверяем не порт, а живой CDP: без согласия порт открыт, а /json/list пуст
    // или отдаёт 403 — тул должен сказать об этом прямо, а не падать на WS.
    let targets = page_targets(&ep).map_err(|e| {
        format!(
            "{e}\n\nОтдельный профиль агента (если тумблер не подходит):\n  \
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
    state::save_target(
        state::Target {
            port,
            remote_port: ep.remote_port,
            browser_path: ep.browser_path.clone(),
        },
        &tab,
    )?;
    println!(
        "подключено: 127.0.0.1:{port}{}, вкладка {} ({})",
        match tunnel::load() {
            Some(t) if t.local_port == port => format!(
                " (туннель к {}:{} — на той машине web не нужен)",
                t.host, t.remote_port
            ),
            _ => String::new(),
        },
        trunc(&tab, 8),
        trunc(
            targets[0]
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(""),
            60
        )
    );
    for (i, t) in targets.iter().enumerate().take(10) {
        println!("{}", tab_line(i, t));
    }
    Ok(())
}

fn cmd_status(args: &Args) -> Result<(), String> {
    let ep = endpoint(args)?;
    match state::load_target() {
        Some((t, tab)) => {
            println!("привязка: 127.0.0.1:{}, вкладка {}", t.port, trunc(&tab, 8));
            if let Some(tn) = tunnel::load() {
                println!(
                    "туннель: {} (pid {}), порт там {}",
                    tn.host, tn.pid, tn.remote_port
                );
            }
        }
        None => println!("привязки нет (`web connect`)"),
    }
    let targets = page_targets(&ep)?;
    println!("вкладок: {}", targets.len());
    for (i, t) in targets.iter().enumerate().take(30) {
        println!("{}", tab_line(i, t));
    }
    Ok(())
}

fn cmd_tabs(args: &Args) -> Result<(), String> {
    let ep = endpoint(args)?;
    let targets = page_targets(&ep)?;
    // По умолчанию — только вкладки нашего профиля: чужие (личный Chrome
    // пользователя) в списке сбивают `web use` по номеру.
    let ours = state::load_target();
    for (i, t) in targets.iter().enumerate() {
        let id = t.get("id").and_then(Value::as_str).unwrap_or("");
        let mine = ours
            .as_ref()
            .map(|(_, tab)| tab == id)
            .unwrap_or(true);
        if !mine && !args.on("all") {
            continue;
        }
        println!("{}", tab_line(i, t));
    }
    if !args.on("all") {
        let hidden = targets.len();
        if hidden > 0 && ours.is_some() {
            println!(
                "(показаны вкладки текущей привязки; все — `web tabs --all`)"
            );
        }
    }
    Ok(())
}

fn cmd_use(args: &Args) -> Result<(), String> {
    let want = args
        .positional
        .first()
        .ok_or("web use: нужен id вкладки или её номер из `web tabs`")?;
    let ep = endpoint(args)?;
    if state::load_target().is_none() {
        return Err("не подключено: сначала `web connect`".into());
    }
    let targets = page_targets(&ep)?;
    // Число — номер из `web tabs` (нумерация с 1); `0` — ошибка, а не «первая».
    // Иначе ищем по префиксу id: hex-идентификаторы бывают из одних цифр.
    let as_number = want.parse::<usize>().ok().filter(|n| *n > 0);
    let tab = match as_number {
        Some(n) => targets
            .get(n - 1)
            .and_then(|t| t.get("id").and_then(Value::as_str))
            .ok_or_else(|| {
                format!(
                    "нет вкладки №{n} (в tabs их {}; нумерация с 1)",
                    targets.len()
                )
            })?
            .to_string(),
        None => {
            // Сначала id, потом номер: если число не совпало как номер, пробуем id.
            let by_id = targets
                .iter()
                .find(|t| {
                    t.get("id")
                        .and_then(Value::as_str)
                        .map(|id| id.starts_with(want.as_str()))
                        .unwrap_or(false)
                })
                .and_then(|t| t.get("id").and_then(Value::as_str));
            match by_id {
                Some(id) => id.to_string(),
                None if want == "0" => {
                    return Err("web use: нумерация вкладок начинается с 1".into())
                }
                None => return Err(format!("нет вкладки с id, начинающимся на {want}")),
            }
        }
    };
    state::save_target(
        state::Target {
            port: ep.port,
            remote_port: ep.remote_port,
            browser_path: ep.browser_path.clone(),
        },
        &tab,
    )?;
    println!("вкладка: {}", trunc(&tab, 8));
    Ok(())
}

fn cmd_open(args: &Args) -> Result<(), String> {
    let url = args
        .positional
        .first()
        .ok_or("web open: нужен URL")?
        .clone();
    if !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("file://")) {
        return Err(format!("web open: только http/https/file, а не {url}"));
    }
    let mut l = live(args)?;
    if args.on("new") {
        // Новая вкладка создаётся браузерным endpoint'ом, а не вкладочным:
        // Target.createTarget доступен только там.
        let ws = l.ep.browser_ws()?;
        let mut b = cdp::Cdp::connect(&ws, args.num_checked("timeout", 30.0, 0.1, 3600.0)?)?;
        let r = b.call("Target.createTarget", json!({"url": url}))?;
        let id = r
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or("CDP: Target.createTarget не вернул targetId")?;
        let tab = id.to_string();
        state::save_target(
            state::Target {
                port: l.ep.port,
                remote_port: l.ep.remote_port,
                browser_path: l.ep.browser_path.clone(),
            },
            &tab,
        )?;
        l = live(args)?;
    } else {
        let r = l.cdp.call(
            "Page.navigate",
            json!({"url": url, "transitionType": "typed"}),
        )?;
        if let Some(err) = r.get("errorText").and_then(Value::as_str) {
            if !err.is_empty() {
                return Err(format!("страница не открылась: {url}: {err}"));
            }
        }
    }
    l.cdp
        .wait_ready(Duration::from_secs(args.num("wait", 15.0) as u64))?;
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
    // --full: дерево без ограничения глубины. Иначе флаг из справки ничего не делал.
    // Отрицательное/дробное значение раньше молча становилось 0: проверяем.
    let depth_raw = args.num("depth", 30.0);
    if !depth_raw.is_finite() || depth_raw < 1.0 {
        return Err(format!("web: --depth ждёт положительное число, получено {depth_raw}"));
    }
    let depth = depth_raw as usize;
    let depth = if args.on("full") {
        usize::MAX
    } else {
        depth
    };
    let s = snap::take_stable(&mut l.cdp, depth)?;
    let path = state::save_page(&l.tab, &s.text)?;
    state::save_url(&l.tab, &s.url);
    let refs_path = state::save_refs(&l.tab, &s.refs)?;
    println!("{} — {}", trunc(&s.url, 120), trunc(&s.title, 80));
    println!(
        "дерево: {} узлов, интерактивных {} → {}",
        s.total,
        s.refs.len(),
        path.display()
    );
    println!("адреса → {}", refs_path.display());
    // 12 строк примера: чтобы модель поняла форму, но не платила за весь экран.
    for line in s.text.lines().take(12) {
        println!("{line}");
    }
    if s.text.lines().count() > 12 {
        println!(
            "… ещё {} строк, читать: grep -n <слово> {}",
            s.text.lines().count() - 12,
            path.display()
        );
    }
    Ok(())
}

fn cmd_text(args: &Args) -> Result<(), String> {
    let mut l = live(args)?;
    let limit = args.num_checked("limit", 4000.0, 1.0, 1e7)? as usize;
    let v = l.cdp.eval(
        "(() => { const b = document.body; if (!b) return ''; \
         const t = b.innerText || b.textContent || ''; return t.replace(/\\n{3,}/g, '\\n\\n').trim(); })()",
    )?;
    let text = v.as_str().unwrap_or("").to_string();
    let cut: String = text.chars().take(limit).collect();
    println!("{cut}");
    if text.chars().count() > limit {
        println!(
            "… обрезано: {} символов всего, --limit чтобы поднять",
            text.chars().count()
        );
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
        .or_else(|| args.positional.first().cloned())
        .ok_or("web eval: нужно JS-выражение (аргументом или --expr)")?;
    let mut l = live(args)?;
    let v = l.cdp.eval(&expr)?;
    match v {
        Value::String(s) => println!("{s}"),
        other => println!(
            "{}",
            serde_json::to_string_pretty(&other).unwrap_or_default()
        ),
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

/// Агентский профиль: запуск отдельного Chrome на машине пользователя и
/// подключение к нему. Это ответ на «подтверждать тумблер каждый раз»: с
/// собственным `--user-data-dir` Chrome отдаёт отладку без согласия.
fn cmd_launch(args: &Args) -> Result<(), String> {
    let host = args
        .flag("tunnel")
        .ok_or("web launch: нужен --tunnel user@machine (где поднимать Chrome)")?
        .to_string();
    // Порт подбираем сами: занятый порт означал бы, что Chrome прилип к уже
    // запущенному процессу с чужим профилем.
    let want = port_from(args)?;
    let port = if args.flag("port").is_some() {
        if !launch::port_free(&host, want)? {
            return Err(format!(
                "порт {want} на {host} занят — скорее всего там уже Chrome другого профиля. \
                 Возьми другой: --port {}",
                want.saturating_add(2)
            ));
        }
        want
    } else {
        launch::pick_port(&host, want)?
    };
    let profile = args.flag("profile");
    if args.flag("port").is_none() {
        if let Some((h, p)) = state::load_launch() {
            if h == host && launch::ours_on_port(&host, p).unwrap_or(false) {
                println!("Chrome агента уже работает на {host}, порт {p} — переиспользую");
                return connect_to(&host, p);
            }
        }
    }
    let visible = args.on("visible");
    let os = if visible {
        launch::start_visible(&host, port, profile)?;
        "windows/видимое окно".to_string()
    } else {
        launch::start_agent_chrome(&host, port, profile)?
    };
    state::save_launch(&host, port);
    println!(
        "Chrome запущен: {host} ({os}), профиль {}, порт {port}{}",
        profile.unwrap_or("по умолчанию"),
        if visible {
            " — окно на экране пользователя"
        } else {
            ""
        }
    );
    // Дальше — обычный путь: туннель + привязка к вкладке.
    connect_to(&host, port)
}

fn connect_to(host: &str, port: u16) -> Result<(), String> {
    let args2 = Args {
        positional: vec!["connect".to_string()],
        flags: [
            ("tunnel".to_string(), host.to_string()),
            ("port".to_string(), port.to_string()),
        ]
        .into_iter()
        .collect(),
        has: Vec::new(),
    };
    cmd_connect(&args2)
}

/// Показать окно агентского Chrome на экране пользователя.
fn cmd_show(args: &Args) -> Result<(), String> {
    let host = match args.flag("tunnel") {
        Some(h) => h.to_string(),
        None => match state::load_launch() {
            Some((h, _)) => h,
            None => {
                return Err("web show: нужен --tunnel user@machine или сначала `web launch`".into())
            }
        },
    };
    launch::show_window(&host)?;
    println!("окно показано на {host}");
    Ok(())
}

/// Закрыть ssh-туннель. Вкладки и Chrome на той стороне не трогаем: отключение
/// от отладки — это снятие согласия, а не убийство браузера человека.
fn cmd_disconnect(_args: &Args) -> Result<(), String> {
    match tunnel::load() {
        Some(t) => {
            tunnel::stop(&t);
            state::clear_target();
            println!("туннель к {} закрыт, привязка снята", t.host);
        }
        None => println!("туннеля нет"),
    }
    Ok(())
}

fn cmd_close(args: &Args) -> Result<(), String> {
    let l = live(args)?;
    let ws = l.ep.browser_ws()?;
    let mut b = cdp::Cdp::connect(&ws, args.num_checked("timeout", 30.0, 0.1, 3600.0)?)?;
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
            d.join(format!(
                "web-{}.png",
                l.tab.chars().take(8).collect::<String>()
            ))
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
    // `=` только в хвосте, длина кратна 4, остаточные биты нулевые: иначе
    // обрезанный/порченный base64 давал бы валидный на вид, но битый файл.
    let bad = || "CDP: не base64 в data".to_string();
    let body: Vec<u8> = s.bytes().filter(|c| *c != NL && *c != CR).collect();
    if body.is_empty() || !body.len().is_multiple_of(4) {
        return Err(bad());
    }
    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    let mut pad = false;
    for (i, c) in body.iter().copied().enumerate() {
        if c == b'=' {
            if i < body.len() - 2 {
                return Err(bad());
            }
            pad = true;
            continue;
        }
        if pad {
            return Err(bad());
        }
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err(bad()),
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    if bits > 0 && acc != 0 {
        return Err(bad());
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

/// `--host user@machine`: та же команда уезжает одним ssh-вызовом на машину
/// пользователя. Так ядро на сервере без графики работает с браузером на его
/// компьютере, не держа открытых портов и не требуя сети между демонами.
fn remote(argv: &[String]) -> Option<(String, Vec<String>)> {
    let mut host = None;
    let mut rest: Vec<String> = Vec::with_capacity(argv.len());
    let mut it = argv.iter().peekable();
    while let Some(a) = it.next() {
        if a == "--host" {
            host = it.next().cloned();
        } else if let Some(h) = a.strip_prefix("--host=") {
            host = Some(h.to_string());
        } else {
            rest.push(a.clone());
        }
    }
    host.map(|h| (h, rest))
}

/// Один ssh → один `web` на той машине. Кавычки для удалённого argv собираем
/// здесь: одинарные кавычки внутри аргумента экранируются по правилам sh.
fn run_remote(host: &str, argv: &[String]) -> i32 {
    let mut cmd = String::from("web");
    for a in argv {
        cmd.push(' ');
        cmd.push('\'');
        cmd.push_str(&a.replace('\'', "'\\''"));
        cmd.push('\'');
    }
    // Тот же адрес, что у туннеля: `-` в начале разбирался бы как опция ssh.
    if let Err(e) = tunnel::check_host(host) {
        eprintln!("{e}");
        return 2;
    }
    let status = std::process::Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("--")
        .arg(host)
        .arg(&cmd)
        .status();
    match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("web --host {host}: ssh не запустился: {e}");
            1
        }
    }
}

fn main() {
    allow_sigpipe();
    let argv_raw: Vec<String> = std::env::args().skip(1).collect();
    if let Some((host, rest)) = remote(&argv_raw) {
        std::process::exit(run_remote(&host, &rest));
    }
    let argv: Vec<String> = argv_raw;
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
        "disconnect" => cmd_disconnect(&args),
        "launch" => cmd_launch(&args),
        "show" => cmd_show(&args),
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
