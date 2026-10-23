// shot — скриншот страницы через headless Chrome по CDP: элемент по CSS-селектору
// (рамка считается в браузере, а не подбирается глазами), явный кроп, несколько
// размеров за один запуск и JS-проба вместо probe-HTML + `chrome --dump-dom`.
//
// Два правила, из-за которых этот тул вообще существует:
//  1. `--selector` берёт рамку у самого элемента: слепой подбор координат
//     (пять `ffmpeg crop` подряд) — не работа.
//  2. у headless-хрома нет своего таймаута: страница с открытым SSE/WebSocket
//     держит загрузку вечно. Внутренний дедлайн обязателен, иначе тул висит.
use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const USAGE: &str = "shot <url> [--size WxH[,WxH...]] [--selector CSS] [--crop X,Y,W,H] [--script файл.js] [--wait-ms N] [--timeout S] [--chrome путь] [--out-dir каталог]";

const HELP: &str = "\
shot — снять скриншот страницы headless-хромом: элемент по селектору, кроп, JS-проба.
shot <url> [--size WxH[,WxH...]] [--selector CSS] [--crop X,Y,W,H] [--script файл.js] [--no-shot] [--wait-ms N] [--timeout S] [--chrome путь] [--out-dir каталог]

Открывает URL в headless Chrome (CDP по ws://127.0.0.1), снимает PNG и кладёт файл
в канал вложений харнесса (SNENS_ATTACH_DIR) — оттуда картинка уезжает в контекст.

  --size WxH         размер окна; несколько через запятую (дефолт 1440x900):
                     `--size 1440x900,390x844` — десктоп и мобильный кадр за запуск
  --selector CSS     снять рамку элемента через CDP и отдать кроп именно его
  --crop X,Y,W,H     явный кроп (CSS-пиксели документа)
  --script файл.js   выполнить JS на странице и напечатать JSON результата.
                     Содержимое файла — тело async-функции: значение возвращается
                     через return, напр. `return {title: document.title}`.
  --no-shot          только JS-проба: картинку не снимать (экономит вложение)
  --wait-ms N        пауза после загрузки, мс (дефолт 300)
  --timeout S        внутренний таймаут всего запуска, секунды (дефолт 20)
  --out-dir каталог  куда класть PNG (дефолт — временный каталог, удаляется)

С `--script` без снимка действует как DOM-проба: probe-HTML в проекте не нужен.
Chrome берётся из --chrome, SNENS_CHROME, ~/.cache/ms-playwright/chromium-*/chrome-linux64/chrome
или PATH (chrome, chrome-headless-shell, chromium).

Коды: 0 — успех, 2 — неверные аргументы, 1 — ошибка выполнения (включая таймаут).";

fn die(msg: &str, code: i32) -> ! {
    eprintln!("{msg}");
    std::process::exit(code);
}

// ---------------------------------------------------------------- аргументы

#[derive(Clone)]
struct Size {
    w: u32,
    h: u32,
}

struct Args {
    url: String,
    sizes: Vec<Size>,
    selector: Option<String>,
    crop: Option<(u32, u32, u32, u32)>,
    script: Option<PathBuf>,
    no_shot: bool,
    wait_ms: u64,
    timeout: f64,
    chrome: Option<String>,
    out_dir: Option<PathBuf>,
}

/// `--size=1440x900` → `["--size", "1440x900"]`: один разбор вместо двух.
fn expand(argv: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for a in argv {
        match a.strip_prefix("--").and_then(|r| r.split_once('=')) {
            Some((k, v)) => {
                out.push(format!("--{k}"));
                out.push(v.to_string());
            }
            None => out.push(a.clone()),
        }
    }
    out
}

fn num<T: std::str::FromStr>(argv: &[String], i: usize, name: &str) -> T {
    let v = argv.get(i + 1).unwrap_or_else(|| {
        die(&format!("shot: {name} без значения\n{USAGE}"), 2);
    });
    v.parse().unwrap_or_else(|_| {
        die(
            &format!("shot: {name} ждёт число, получено {v}\n{USAGE}"),
            2,
        )
    })
}

fn parse_size(s: &str) -> Size {
    let (w, h) = s
        .split_once(['x', 'X', '×'])
        .unwrap_or_else(|| die(&format!("shot: --size ждёт WxH, получено {s}\n{USAGE}"), 2));
    let w: u32 = w
        .parse()
        .unwrap_or_else(|_| die(&format!("shot: --size: ширина не число: {w}\n{USAGE}"), 2));
    let h: u32 = h
        .parse()
        .unwrap_or_else(|_| die(&format!("shot: --size: высота не число: {h}\n{USAGE}"), 2));
    if w == 0 || h == 0 || w > 10_000 || h > 10_000 {
        die(
            &format!("shot: --size: {s} вне разумного (1..10000)\n{USAGE}"),
            2,
        );
    }
    Size { w, h }
}

fn parse_crop(s: &str) -> (u32, u32, u32, u32) {
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        die(
            &format!("shot: --crop ждёт X,Y,W,H, получено {s}\n{USAGE}"),
            2,
        );
    }
    let mut n = [0u32; 4];
    for (i, p) in parts.iter().enumerate() {
        n[i] = p
            .parse()
            .unwrap_or_else(|_| die(&format!("shot: --crop: не число: {p}\n{USAGE}"), 2));
    }
    if n[2] == 0 || n[3] == 0 {
        die(&format!("shot: --crop: нулевые W/H\n{USAGE}"), 2);
    }
    (n[0], n[1], n[2], n[3])
}

fn parse(argv: &[String]) -> Args {
    let argv = expand(argv);
    let mut url: Option<String> = None;
    let mut a = Args {
        url: String::new(),
        sizes: Vec::new(),
        selector: None,
        crop: None,
        script: None,
        no_shot: false,
        wait_ms: 300,
        timeout: 20.0,
        chrome: None,
        out_dir: None,
    };
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        match arg {
            "--no-shot" => {
                a.no_shot = true;
                i += 1;
                continue;
            }
            "--size" | "--selector" | "--crop" | "--script" | "--wait-ms" | "--timeout"
            | "--chrome" | "--out-dir" => {
                // Значение проверяем сразу: пустой --size молчаливого дефолта не заслуживает.
                match arg {
                    "--size" => {
                        let v = argv.get(i + 1).unwrap_or_else(|| {
                            die(&format!("shot: --size без значения\n{USAGE}"), 2)
                        });
                        for s in v.split(',') {
                            if !s.trim().is_empty() {
                                a.sizes.push(parse_size(s.trim()));
                            }
                        }
                        if a.sizes.is_empty() {
                            die(&format!("shot: --size пуст\n{USAGE}"), 2);
                        }
                    }
                    "--selector" => {
                        let v = argv.get(i + 1).unwrap_or_else(|| {
                            die(&format!("shot: --selector без значения\n{USAGE}"), 2)
                        });
                        if v.trim().is_empty() {
                            die(&format!("shot: --selector пуст\n{USAGE}"), 2);
                        }
                        a.selector = Some(v.clone());
                    }
                    "--crop" => {
                        let v = argv.get(i + 1).unwrap_or_else(|| {
                            die(&format!("shot: --crop без значения\n{USAGE}"), 2)
                        });
                        a.crop = Some(parse_crop(v));
                    }
                    "--script" => {
                        let v = argv.get(i + 1).unwrap_or_else(|| {
                            die(&format!("shot: --script без значения\n{USAGE}"), 2)
                        });
                        a.script = Some(PathBuf::from(v));
                    }
                    "--wait-ms" => a.wait_ms = num(&argv, i, "--wait-ms"),
                    "--timeout" => {
                        let t: f64 = num(&argv, i, "--timeout");
                        if t <= 0.0 || t > 600.0 {
                            die(&format!("shot: --timeout вне 0..600 с: {t}\n{USAGE}"), 2);
                        }
                        a.timeout = t;
                    }
                    "--chrome" => a.chrome = Some(argv[i + 1].clone()),
                    _ => a.out_dir = Some(PathBuf::from(&argv[i + 1])),
                }
                i += 2;
            }
            _ if arg.starts_with('-') && arg != "-" => {
                die(&format!("shot: неизвестный аргумент {arg}\n{USAGE}"), 2)
            }
            _ => {
                if url.is_some() {
                    die(&format!("shot: URL указан дважды\n{USAGE}"), 2);
                }
                url = Some(arg.to_string());
                i += 1;
            }
        }
    }
    a.url = url.unwrap_or_else(|| die(&format!("shot: не указан URL\n{USAGE}"), 2));
    if !(a.url.starts_with("http://") || a.url.starts_with("https://")) {
        die(
            &format!("shot: только http/https, получено {}\n{USAGE}", a.url),
            2,
        );
    }
    if a.selector.is_some() && a.crop.is_some() {
        die(
            &format!("shot: --selector и --crop взаимоисключающие — рамку элемента задаёт селектор\n{USAGE}"),
            2,
        );
    }
    if a.no_shot && a.script.is_none() {
        die(
            &format!("shot: --no-shot без --script — снимать нечего\n{USAGE}"),
            2,
        );
    }
    if a.sizes.is_empty() {
        a.sizes.push(Size { w: 1440, h: 900 });
    }
    if let Some(p) = &a.script {
        if !p.is_file() {
            die(
                &format!("shot: --script: нет файла {}\n{USAGE}", p.display()),
                2,
            );
        }
    }
    a
}

// ---------------------------------------------------------------- временный каталог

/// Всё временное — в одном каталоге под $TMPDIR, который убирается на выходе.
/// В каталог проекта тул не пишет ничего, даже при ошибке.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Result<Self, String> {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = base.join(format!("shot-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path)
            .map_err(|e| format!("временный каталог {}: {e}", path.display()))?;
        Ok(TempDir { path })
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Chrome убит, но его потомки (zygote, crashpad) успевают писать ещё чуть-чуть:
        // единственная попытка молча оставляла бы мусор в $TMPDIR после каждого
        // прогона. Пара секунд терпения дешевле, чем забытые профили.
        for _ in 0..20 {
            if fs::remove_dir_all(&self.path).is_ok() || !self.path.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        eprintln!(
            "shot: временный каталог {} не убрался — удали вручную",
            self.path.display()
        );
    }
}

// ---------------------------------------------------------------- chrome

/// Процесс хрома умирает вместе с тулом при любом выходе, включая ошибку:
/// иначе на каждом таймауте в системе остаётся висеть headless-браузер.
struct ChromeGuard {
    child: Child,
}

impl Drop for ChromeGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn is_exec(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match fs::metadata(p) {
            Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        if is_exec(&p) {
            return Some(p);
        }
    }
    None
}

/// Дефолт — Chrome for Testing из playwright: он на этой машине есть и работает.
/// Версию не прибиваем: берём старший `chromium-*`, если появится новее.
fn playwright_chrome() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let base = PathBuf::from(home).join(".cache/ms-playwright");
    let mut best: Option<(u64, PathBuf)> = None;
    for e in fs::read_dir(&base).ok()? {
        let e = e.ok()?;
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(ver) = name.strip_prefix("chromium-") else {
            continue;
        };
        let ver: u64 = ver.parse().unwrap_or(0);
        let bin = e.path().join("chrome-linux64/chrome");
        if is_exec(&bin) && best.as_ref().map(|(v, _)| ver > *v).unwrap_or(true) {
            best = Some((ver, bin));
        }
    }
    best.map(|(_, p)| p)
}

fn find_chrome(a: &Args) -> Result<PathBuf, String> {
    if let Some(c) = &a.chrome {
        let p = PathBuf::from(c);
        if !is_exec(&p) {
            return Err(format!("--chrome: нет исполняемого файла {c}"));
        }
        return Ok(p);
    }
    if let Ok(c) = std::env::var("SNENS_CHROME") {
        if !c.is_empty() {
            let p = PathBuf::from(&c);
            if !is_exec(&p) {
                return Err(format!("SNENS_CHROME: нет исполняемого файла {c}"));
            }
            return Ok(p);
        }
    }
    if let Some(p) = playwright_chrome() {
        return Ok(p);
    }
    for name in [
        "chrome",
        "chrome-headless-shell",
        "chromium",
        "google-chrome",
        "chromium-browser",
    ] {
        if let Some(p) = which(name) {
            return Ok(p);
        }
    }
    Err(
        "Chrome не найден: задай --chrome, SNENS_CHROME или поставь Google Chrome for Testing"
            .into(),
    )
}

fn launch_chrome(bin: &Path, profile: &Path) -> Result<ChromeGuard, String> {
    let child = Command::new(bin)
        .arg("--headless=new")
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--no-first-run",
            "--no-sandbox",
            "--disable-gpu",
            "--hide-scrollbars",
            "--disable-extensions",
            "--disable-background-networking",
            "--disable-background-timer-throttling",
            "--disable-sync",
            "--disable-dev-shm-usage",
            "--mute-audio",
            "--no-default-browser-check",
            "--disable-features=Translate,MediaRouter",
            // Первая страница открывается оффскрин-таргетом: живой URL в профиле
            // не нужен, навигацию делает CDP.
            "about:blank",
        ])
        .env("CHROME_HEADLESS", "1")
        // Сокет singleton — вне профиля (длинные пути), в $TMPDIR. Свой TMPDIR
        // уводит его в наш временный каталог: иначе каждый прогон оставлял бы
        // в /tmp каталог org.chromium.Chromium.*.
        .env("TMPDIR", profile.parent().unwrap_or(profile))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("запуск {}: {e}", bin.display()))?;
    Ok(ChromeGuard { child })
}

/// Порт и путь браузерного сокета: Chrome пишет `<порт>\n/devtools/browser/<id>`.
fn wait_devtools(
    profile: &Path,
    guard: &mut ChromeGuard,
    until: Instant,
) -> Result<(u16, String), String> {
    let f = profile.join("DevToolsActivePort");
    loop {
        if let Ok(txt) = fs::read_to_string(&f) {
            let mut lines = txt.lines();
            let port = lines.next().unwrap_or("").trim().parse::<u16>();
            let path = lines.next().unwrap_or("").trim().to_string();
            if let (Ok(port), false) = (port, path.is_empty()) {
                return Ok((port, path));
            }
        }
        if let Ok(Some(st)) = guard.child.try_wait() {
            return Err(format!("Chrome завершился на старте ({st})"));
        }
        if Instant::now() >= until {
            return Err("Chrome не открыл порт отладки за отведённое время".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------- CDP

struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    sx: f64,
    sy: f64,
    vw: f64,
    vh: f64,
    dpr: f64,
    tag: String,
}

struct Cdp {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    id: u64,
    deadline: Instant,
    timeout_s: f64,
}

fn is_timeout(e: &tungstenite::Error) -> bool {
    match e {
        tungstenite::Error::Io(io) => matches!(
            io.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        _ => false,
    }
}

impl Cdp {
    fn connect(url: &str, deadline: Instant, timeout_s: f64) -> Result<Self, String> {
        let (mut ws, _res) = tungstenite::connect(url).map_err(|e| format!("CDP: {e}"))?;
        if let MaybeTlsStream::Plain(s) = ws.get_mut() {
            let left = deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(200));
            let _ = s.set_read_timeout(Some(left));
            let _ = s.set_write_timeout(Some(left));
        }
        Ok(Cdp {
            ws,
            id: 0,
            deadline,
            timeout_s,
        })
    }

    /// Дедлайн не «на один вызов», а на весь запуск: даже если Chrome бодро
    /// отвечает на каждый шаг, тул обязан вернуться в обещанное время.
    fn left(&self) -> Result<Duration, String> {
        let now = Instant::now();
        if now >= self.deadline {
            Err(format!(
                "таймаут {} с: Chrome не довёл работу до конца (страница держит открытый поток?)",
                self.timeout_s
            ))
        } else {
            Ok(self.deadline - now)
        }
    }

    fn send_json(&mut self, v: &Value) -> Result<(), String> {
        let left = self.left()?;
        if let MaybeTlsStream::Plain(s) = self.ws.get_mut() {
            let _ = s.set_write_timeout(Some(left));
        }
        self.ws
            .send(Message::Text(v.to_string().into()))
            .map_err(|e| format!("CDP send: {e}"))
    }

    fn read_json(&mut self) -> Result<Value, String> {
        loop {
            let left = self.left()?;
            if let MaybeTlsStream::Plain(s) = self.ws.get_mut() {
                let _ = s.set_read_timeout(Some(left));
            }
            match self.ws.read() {
                Ok(Message::Text(t)) => {
                    return serde_json::from_str(t.as_str())
                        .map_err(|e| format!("CDP: не JSON от Chrome: {e}"))
                }
                Ok(Message::Binary(b)) => {
                    return serde_json::from_slice(&b).map_err(|e| format!("CDP: {e}"))
                }
                Ok(Message::Ping(p)) => {
                    let _ = self.ws.send(Message::Pong(p));
                }
                Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => continue,
                Ok(Message::Close(_)) => return Err("CDP: Chrome закрыл соединение".into()),
                Err(e) if is_timeout(&e) => {
                    return Err(self.left().err().unwrap_or_else(|| {
                        "CDP: Chrome не ответил (страница держит открытый поток?)".into()
                    }))
                }
                Err(e) => return Err(format!("CDP: {e}")),
            }
        }
    }

    fn call(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        self.id += 1;
        let id = self.id;
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = Value::String(s.to_string());
        }
        self.send_json(&msg)?;
        loop {
            let v = self.read_json()?;
            // События (Page.*, Network.*) не нужны: ждём ответ на свой id.
            if v.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = v.get("error") {
                let text = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("ошибка CDP");
                return Err(format!("CDP {method}: {text}"));
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn eval(&mut self, session: &str, expr: &str) -> Result<Value, String> {
        let r = self.call(
            Some(session),
            "Runtime.evaluate",
            json!({
                "expression": expr,
                "returnByValue": true,
                "awaitPromise": true,
                "userGesture": true,
            }),
        )?;
        if let Some(ex) = r.get("exceptionDetails") {
            let text = ex
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .or_else(|| ex.get("text").and_then(Value::as_str))
                .unwrap_or("исключение в JS");
            return Err(format!("JS: {}", text.lines().next().unwrap_or(text)));
        }
        Ok(r.get("result")
            .and_then(|x| x.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }
}

// ---------------------------------------------------------------- страница

fn navigate(cdp: &mut Cdp, session: &str, url: &str) -> Result<(), String> {
    let r = cdp.call(
        Some(session),
        "Page.navigate",
        json!({"url": url, "transitionType": "typed"}),
    )?;
    // Chrome рисует свою страницу ошибки и отвечает успехом: без этой проверки
    // скриншот недоступного URL выглядел бы как нормальный кадр.
    if let Some(err) = r.get("errorText").and_then(Value::as_str) {
        if !err.is_empty() {
            return Err(format!("страница не открылась: {url}: {err}"));
        }
    }
    Ok(())
}

/// Готовность документа — опросом readyState, а не событиями загрузки: на
/// странице с открытым потоком `Page.loadEventFired` может не прийти никогда.
fn wait_ready(cdp: &mut Cdp, session: &str, budget: Duration) -> Result<(), String> {
    let until = Instant::now() + budget;
    #[allow(unused_assignments)]
    let mut last = String::new();
    loop {
        match cdp.eval(session, "document.readyState") {
            Ok(v) => {
                last = v.as_str().unwrap_or("?").to_string();
                if last == "complete" {
                    return Ok(());
                }
            }
            // Контекст исполнения пропадает при навигации — это не ошибка, ждём дальше.
            Err(e) if e.starts_with("таймаут") => return Err(e),
            Err(e) => last = e,
        }
        if Instant::now() >= until {
            return Err(format!(
                "страница не догрузилась за {} с (readyState: {last})",
                budget.as_secs_f32().round()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn measure(cdp: &mut Cdp, session: &str, selector: &str) -> Result<Option<Rect>, String> {
    let sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into());
    let js = format!(
        r#"(() => {{
  let el;
  try {{ el = document.querySelector({sel}); }}
  catch (e) {{ return {{ bad: String(e && e.message || e) }}; }}
  if (!el) return {{ missing: true }};
  const r = el.getBoundingClientRect();
  return {{
    x: r.left, y: r.top, w: r.width, h: r.height,
    sx: window.scrollX, sy: window.scrollY,
    vw: window.innerWidth, vh: window.innerHeight,
    dpr: window.devicePixelRatio || 1,
    tag: String(el.tagName || '').toLowerCase()
  }};
}})()"#
    );
    let v = cdp.eval(session, &js)?;
    if let Some(bad) = v.get("bad").and_then(Value::as_str) {
        return Err(format!("--selector: кривой CSS-селектор {selector}: {bad}"));
    }
    if v.get("missing").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(None);
    }
    let f = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    Ok(Some(Rect {
        x: f("x"),
        y: f("y"),
        w: f("w"),
        h: f("h"),
        sx: f("sx"),
        sy: f("sy"),
        vw: f("vw"),
        vh: f("vh"),
        dpr: if f("dpr") > 0.0 { f("dpr") } else { 1.0 },
        tag: v
            .get("tag")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string(),
    }))
}

/// Элемент может быть за пределами вьюпорта: сдвигаем страницу к нему, чтобы
/// кадр не оказался пустым. Координаты всё равно берём из getBoundingClientRect.
fn shift_to_view(cdp: &mut Cdp, session: &str, selector: &str) -> Result<Rect, String> {
    let mut last: Option<Rect> = None;
    for _ in 0..4 {
        let Some(r) = measure(cdp, session, selector)? else {
            return Err(format!(
                "--selector: элемента `{selector}` нет на странице (querySelector → null)"
            ));
        };
        let mut dx = 0.0;
        let mut dy = 0.0;
        if r.x < 0.0 {
            dx = r.x;
        } else if r.w <= r.vw && r.x + r.w > r.vw {
            dx = r.x + r.w - r.vw;
        }
        if r.y < 0.0 {
            dy = r.y;
        } else if r.h <= r.vh && r.y + r.h > r.vh {
            dy = r.y + r.h - r.vh;
        }
        let done = dx.abs() < 1.0 && dy.abs() < 1.0;
        last = Some(r);
        if done {
            break;
        }
        cdp.eval(session, &format!("window.scrollBy({dx:.1}, {dy:.1}); 0"))?;
        std::thread::sleep(Duration::from_millis(60));
    }
    last.ok_or_else(|| format!("--selector: не удалось измерить `{selector}`"))
}

/// Клип в пикселях изображения: координаты документа × devicePixelRatio.
fn clip_of(x: f64, y: f64, w: f64, h: f64, dpr: f64) -> Value {
    let x = x.max(0.0);
    let y = y.max(0.0);
    json!({
        "x": x * dpr,
        "y": y * dpr,
        "width": (w * dpr).max(1.0),
        "height": (h * dpr).max(1.0),
        "scale": 1,
    })
}

fn screenshot(cdp: &mut Cdp, session: &str, clip: Option<Value>) -> Result<Vec<u8>, String> {
    let mut params = json!({
        "format": "png",
        "fromSurface": true,
        // Рамка может быть выше вьюпорта: без этого CDP обрезал бы её по окну.
        "captureBeyondViewport": true,
    });
    if let Some(c) = clip {
        params["clip"] = c;
    }
    let r = cdp.call(Some(session), "Page.captureScreenshot", params)?;
    let data = r
        .get("data")
        .and_then(Value::as_str)
        .ok_or("Page.captureScreenshot: пустой ответ")?;
    base64_decode(data)
}

/// Base64 руками: зависимость ради одной функции в тул не тащим.
fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = val(c).ok_or_else(|| format!("base64: неожиданный байт {c}"))?;
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

fn png_dims(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() >= 24 && b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some((
            u32::from_be_bytes(b[16..20].try_into().ok()?),
            u32::from_be_bytes(b[20..24].try_into().ok()?),
        ))
    } else {
        None
    }
}

// ---------------------------------------------------------------- файлы

/// Имя кадра: хост + размер + чем снят. Так файлы различимы в канале вложений.
fn shot_name(url: &str, w: u32, h: u32, tag: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host: String = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("page")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let host = host.trim_matches('-').replace("--", "-");
    let host = if host.is_empty() { "page".into() } else { host };
    format!("{host}-{w}x{h}{tag}.png")
}

fn unique(dir: &Path, name: &str) -> PathBuf {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("shot");
    let mut p = dir.join(name);
    let mut i = 1;
    while p.exists() {
        p = dir.join(format!("{stem}-{i}.png"));
        i += 1;
    }
    p
}

/// Канал вложений: модель видит картинку только оттуда. Без переменной файл
/// остаётся на диске и о том, что он не уехал, сказано в stdout.
fn deliver(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let (w, h) = png_dims(bytes).unwrap_or((0, 0));
    let attach = std::env::var_os("SNENS_ATTACH_DIR").map(PathBuf::from);
    match attach {
        Some(dir) => {
            fs::create_dir_all(&dir)
                .map_err(|e| format!("канал вложений {}: {e}", dir.display()))?;
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("shot.png");
            let to = unique(&dir, name);
            fs::copy(path, &to).map_err(|e| format!("{}: {e}", to.display()))?;
            println!(
                "{}: image/png {}x{} {} Б → вложение {}",
                path.display(),
                w,
                h,
                bytes.len(),
                to.file_name().and_then(|n| n.to_str()).unwrap_or("?")
            );
        }
        None => println!(
            "{}: image/png {}x{} {} Б — канал вложений недоступен (нет SNENS_ATTACH_DIR), \
             файл на диске, картинка в контекст не уехала",
            path.display(),
            w,
            h,
            bytes.len()
        ),
    }
    Ok(())
}

// ---------------------------------------------------------------- прогон

fn run(a: &Args) -> Result<(), String> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs_f64(a.timeout);
    let bin = find_chrome(a)?;
    let tmp = TempDir::new()?;
    run_inner(a, &bin, &tmp, deadline)
}

fn run_inner(a: &Args, bin: &Path, tmp: &TempDir, deadline: Instant) -> Result<(), String> {
    let profile = tmp.path.join("profile");
    fs::create_dir_all(&profile).map_err(|e| format!("профиль {}: {e}", profile.display()))?;
    let out_dir = match &a.out_dir {
        Some(d) => {
            if d.is_file() {
                return Err(format!(
                    "--out-dir {} — это файл, а не каталог",
                    d.display()
                ));
            }
            fs::create_dir_all(d).map_err(|e| format!("--out-dir {}: {e}", d.display()))?;
            d.clone()
        }
        None => {
            let d = tmp.path.join("out");
            fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
            d
        }
    };
    let mut guard = launch_chrome(bin, &profile)?;
    // Порт ждём отдельным коротким бюджетом: старт хрома — не самая долгая часть,
    // и на нём не нужно съедать весь таймаут страницы.
    let boot_until = (Instant::now() + Duration::from_secs(10)).min(deadline);
    let (port, path) = wait_devtools(&profile, &mut guard, boot_until)?;
    let ws_url = format!("ws://127.0.0.1:{port}{path}");
    let mut cdp = Cdp::connect(&ws_url, deadline, a.timeout)?;

    let target = cdp.call(None, "Target.createTarget", json!({"url": "about:blank"}))?;
    let target_id = target
        .get("targetId")
        .and_then(Value::as_str)
        .ok_or("Target.createTarget: пустой targetId")?
        .to_string();
    let attached = cdp.call(
        None,
        "Target.attachToTarget",
        json!({"targetId": target_id, "flatten": true}),
    )?;
    let session = attached
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or("Target.attachToTarget: пустой sessionId")?
        .to_string();
    cdp.call(Some(&session), "Page.enable", json!({}))?;

    // Проба без снимка всё равно требует вьюпорта: устройство по умолчанию в
    // headless — 800x600, и любой layout-зависимый JS соврёт.
    let first = a.sizes[0].clone();
    cdp.call(
        Some(&session),
        "Emulation.setDeviceMetricsOverride",
        json!({"width": first.w, "height": first.h, "deviceScaleFactor": 1, "mobile": false}),
    )?;
    shots_and_probe(&mut cdp, &session, a, &out_dir)
}

/// Навигация + готовность + пауза: один шаг, общий для снимка и пробы.
fn open(cdp: &mut Cdp, session: &str, a: &Args) -> Result<(), String> {
    navigate(cdp, session, &a.url)?;
    let ready_budget = cdp.left()?;
    wait_ready(cdp, session, ready_budget)?;
    if a.wait_ms > 0 {
        std::thread::sleep(Duration::from_millis(a.wait_ms));
    }
    Ok(())
}

/// Кадры по размерам, затем JS-проба на последнем открытом документе: проба
/// видит ту же страницу, что и снимок, — иначе поведение проверялось бы на другой.
fn shots_and_probe(cdp: &mut Cdp, session: &str, a: &Args, out_dir: &Path) -> Result<(), String> {
    let sizes: &[Size] = if a.no_shot { &[] } else { &a.sizes };
    // Порядок шагов — часть контракта: страница открывается один раз, `--script`
    // выполняется сразу после готовности, и только потом снимается кадр. Иначе
    // сценарий «нажать и показать результат» снимал бы исходный вид: снимок
    // делался до пробы, а повторный `navigate` на второй размер стирал состояние.
    open(cdp, session, a)?;
    if a.script.is_some() {
        probe(cdp, session, a)?;
    }
    for size in sizes {
        cdp.call(
            Some(session),
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": size.w,
                "height": size.h,
                "deviceScaleFactor": 1,
                "mobile": false,
            }),
        )?;
        let (clip, tag) = if let Some(sel) = &a.selector {
            let r = shift_to_view(cdp, session, sel)?;
            println!(
                "рамка `{sel}` (<{}>): x={:.0} y={:.0} {}x{} CSS px, dpr {}",
                r.tag,
                r.x + r.sx,
                r.y + r.sy,
                r.w.round() as i64,
                r.h.round() as i64,
                r.dpr
            );
            (
                Some(clip_of(r.x + r.sx, r.y + r.sy, r.w, r.h, r.dpr)),
                "-sel",
            )
        } else if let Some((cx, cy, cw, ch)) = a.crop {
            let dpr = cdp
                .eval(session, "window.devicePixelRatio || 1")?
                .as_f64()
                .unwrap_or(1.0);
            let dpr = if dpr > 0.0 { dpr } else { 1.0 };
            (
                Some(clip_of(cx as f64, cy as f64, cw as f64, ch as f64, dpr)),
                "-crop",
            )
        } else {
            (None, "")
        };
        let png = screenshot(cdp, session, clip)?;
        let path = unique(out_dir, &shot_name(&a.url, size.w, size.h, tag));
        fs::write(&path, &png).map_err(|e| format!("{}: {e}", path.display()))?;
        deliver(&path, &png)?;
    }
    Ok(())
}

/// Выполнить `--script` как тело async-функции страницы и напечатать JSON.
fn probe(cdp: &mut Cdp, session: &str, a: &Args) -> Result<(), String> {
    let Some(script) = &a.script else {
        return Ok(());
    };
    let src =
        fs::read_to_string(script).map_err(|e| format!("--script {}: {e}", script.display()))?;
    // Тело async-функции: `return` определяет результат, await разрешён.
    let expr = format!("(async () => {{\n{src}\n}})()");
    let v = cdp.eval(session, &expr)?;
    println!(
        "{}",
        serde_json::to_string(&v).unwrap_or_else(|_| "null".into())
    );
    Ok(())
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        println!("{HELP}");
        return;
    }
    if argv.is_empty() {
        die(&format!("shot: не указан URL\n{USAGE}"), 2);
    }
    let a = parse(&argv);
    if let Err(e) = run(&a) {
        eprintln!("shot: {e}");
        std::process::exit(1);
    }
}
