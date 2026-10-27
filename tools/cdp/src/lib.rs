//! Headless Chrome по CDP для тулов snens.
//!
//! Два правила, из-за которых этот крейт существует:
//!
//! 1. у headless-хрома нет своего таймаута: страница с открытым SSE/WebSocket
//!    держит загрузку вечно, поэтому дедлайн на весь запуск обязателен;
//! 2. событийная часть CDP нужна не всем, но нужна: `shot` ждёт ответы на свои
//!    запросы, а `genui` обязан слышать `Runtime.consoleAPICalled` и
//!    `Runtime.exceptionThrown` из песочницы артефакта. Поэтому события не
//!    выбрасываются, а складываются в очередь, которую читает вызывающий.
//!
//! Всё остальное — запуск браузера, навигация, кадры, канал вложений — общее:
//! `shot` и `genui` обязаны видеть страницу одинаково, иначе проверка артефакта
//! врёт про то, что видит человек.

use std::collections::VecDeque;
use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// Каталог под `$TMPDIR`, который убирается на выходе. В каталог проекта тул не
/// пишет ничего, даже при ошибке.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> Result<Self, String> {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = base.join(format!("{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path)
            .map_err(|e| format!("временный каталог {}: {e}", path.display()))?;
        Ok(TempDir { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Chrome убит, но его потомки (zygote, crashpad) успевают писать ещё
        // чуть-чуть: единственная попытка молча оставляла бы мусор в $TMPDIR
        // после каждого прогона. Пара секунд терпения дешевле забытых профилей.
        for _ in 0..20 {
            if fs::remove_dir_all(&self.path).is_ok() || !self.path.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        eprintln!(
            "cdp: временный каталог {} не убрался — удали вручную",
            self.path.display()
        );
    }
}

// ---------------------------------------------------------------- chrome

/// Процесс хрома умирает вместе с тулом при любом выходе, включая ошибку:
/// иначе на каждом таймауте в системе остаётся висеть headless-браузер.
pub struct ChromeGuard {
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

/// Найти браузер: `--chrome`, `SNENS_CHROME`, playwright, PATH.
pub fn find_chrome(explicit: Option<&str>) -> Result<PathBuf, String> {
    if let Some(c) = explicit {
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

/// Есть ли браузер вообще: для деградации `genui check` до lint.
pub fn chrome_available(explicit: Option<&str>) -> bool {
    find_chrome(explicit).is_ok()
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

/// Соединение с браузером. Дедлайн — на весь запуск, а не на вызов: даже если
/// Chrome бодро отвечает на каждый шаг, тул обязан вернуться в обещанное время.
pub struct Cdp {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    id: u64,
    deadline: Instant,
    timeout_s: f64,
    /// События, пришедшие между ответами. Не выбрасываются: их читает тот, кому
    /// они нужны (`genui` — консоль и исключения песочницы).
    events: VecDeque<Value>,
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
    pub fn connect(url: &str, deadline: Instant, timeout_s: f64) -> Result<Self, String> {
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
            events: VecDeque::new(),
        })
    }

    pub fn left(&self) -> Result<Duration, String> {
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

    /// Вызов метода. События по пути складываются в очередь, а не выбрасываются.
    pub fn call(
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
            if v.get("id").and_then(Value::as_u64) != Some(id) {
                if v.get("method").is_some() {
                    self.events.push_back(v);
                }
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

    /// Забрать накопленные события (дренирует очередь).
    pub fn take_events(&mut self) -> Vec<Value> {
        self.events.drain(..).collect()
    }

    /// Подождать событие `method` (или любое, если `method` пусто), не дольше
    /// `budget`. `None` — не дождались. Ответы на чужие запросы не мешают.
    pub fn wait_event(&mut self, method: &str, budget: Duration) -> Result<Option<Value>, String> {
        if let Some(pos) = self.events.iter().position(|e| {
            method.is_empty() || e.get("method").and_then(Value::as_str) == Some(method)
        }) {
            return Ok(self.events.remove(pos));
        }
        let until = Instant::now() + budget;
        loop {
            if Instant::now() >= until {
                return Ok(None);
            }
            match self.read_json() {
                Ok(v) if v.get("method").is_some() => {
                    let hit = method.is_empty()
                        || v.get("method").and_then(Value::as_str) == Some(method);
                    if hit {
                        return Ok(Some(v));
                    }
                    self.events.push_back(v);
                }
                Ok(_) => {}
                Err(e) if e.starts_with("таймаут") => return Ok(None),
                // Закрытие страницы посреди ожидания — это конец потока, а не сбой.
                Err(e) => return Err(e),
            }
        }
    }

    pub fn eval(&mut self, session: &str, expr: &str) -> Result<Value, String> {
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

// ---------------------------------------------------------------- браузер

/// Живой браузер: процесс, соединение и открытая страница.
pub struct Browser {
    #[allow(dead_code)] // держит процесс живым до конца прогона
    guard: ChromeGuard,
    #[allow(dead_code)]
    tmp: TempDir,
    pub cdp: Cdp,
    pub session: String,
    pub out_dir: PathBuf,
}

/// Открыть браузер и пустую страницу. `out_dir` — куда класть кадры; `None` —
/// во временный каталог (он живёт столько же, сколько браузер).
/// `tag` — префикс временного каталога: тул называет его своим именем, чтобы
/// уборка мусора и контрактные тесты видели свои каталоги (`/tmp/shot-*`).
pub fn open(
    chrome: Option<&str>,
    deadline: Instant,
    timeout_s: f64,
    out_dir: Option<&Path>,
    tag: &str,
) -> Result<Browser, String> {
    let bin = find_chrome(chrome)?;
    let tmp = TempDir::new(tag)?;
    let profile = tmp.path().join("profile");
    fs::create_dir_all(&profile).map_err(|e| format!("профиль {}: {e}", profile.display()))?;
    let out = match out_dir {
        Some(d) => {
            if d.is_file() {
                return Err(format!(
                    "--out-dir {} — это файл, а не каталог",
                    d.display()
                ));
            }
            fs::create_dir_all(d).map_err(|e| format!("--out-dir {}: {e}", d.display()))?;
            d.to_path_buf()
        }
        None => {
            let d = tmp.path().join("out");
            fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
            d
        }
    };
    let mut guard = launch_chrome(&bin, &profile)?;
    // Порт ждём отдельным коротким бюджетом: старт хрома — не самая долгая часть,
    // и на нём не нужно съедать весь таймаут страницы.
    let boot_until = (Instant::now() + Duration::from_secs(10)).min(deadline);
    let (port, path) = wait_devtools(&profile, &mut guard, boot_until)?;
    let ws_url = format!("ws://127.0.0.1:{port}{path}");
    let mut cdp = Cdp::connect(&ws_url, deadline, timeout_s)?;

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

    Ok(Browser {
        guard,
        tmp,
        cdp,
        session,
        out_dir: out,
    })
}

impl Browser {
    /// Вьюпорт: без него headless-дефолт 800x600, и любой layout-зависимый JS соврёт.
    pub fn set_viewport(&mut self, w: u32, h: u32) -> Result<(), String> {
        self.cdp.call(
            Some(&self.session),
            "Emulation.setDeviceMetricsOverride",
            json!({"width": w, "height": h, "deviceScaleFactor": 1, "mobile": false}),
        )?;
        Ok(())
    }

    pub fn navigate(&mut self, url: &str) -> Result<(), String> {
        let r = self.cdp.call(
            Some(&self.session),
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
    pub fn wait_ready(&mut self, budget: Duration) -> Result<(), String> {
        let until = Instant::now() + budget;
        #[allow(unused_assignments)]
        let mut last = String::new();
        loop {
            match self.cdp.eval(&self.session, "document.readyState") {
                Ok(v) => {
                    last = v.as_str().unwrap_or("?").to_string();
                    if last == "complete" {
                        return Ok(());
                    }
                }
                // Контекст исполнения пропадает при навигации — это не ошибка.
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

    pub fn sleep(ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }

    /// Кадр страницы (или кропа). `clip` — в пикселях изображения.
    pub fn screenshot(&mut self, clip: Option<Value>) -> Result<Vec<u8>, String> {
        let mut params = json!({
            "format": "png",
            "fromSurface": true,
            // Рамка может быть выше вьюпорта: без этого CDP обрезал бы её по окну.
            "captureBeyondViewport": true,
        });
        if let Some(c) = clip {
            params["clip"] = c;
        }
        let r = self
            .cdp
            .call(Some(&self.session), "Page.captureScreenshot", params)?;
        let data = r
            .get("data")
            .and_then(Value::as_str)
            .ok_or("Page.captureScreenshot: пустой ответ")?;
        base64_decode(data)
    }
}

// ---------------------------------------------------------------- страница

/// Клип в пикселях изображения: координаты документа × devicePixelRatio.
pub fn clip_of(x: f64, y: f64, w: f64, h: f64, dpr: f64) -> Value {
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

/// Базовая64 руками: зависимость ради одной функции не тащим.
pub fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
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

pub fn png_dims(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() >= 24 && b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some((
            u32::from_be_bytes(b[16..20].try_into().ok()?),
            u32::from_be_bytes(b[20..24].try_into().ok()?),
        ))
    } else {
        None
    }
}

/// Имя кадра: хост + размер + чем снят. Так файлы различимы в канале вложений.
pub fn shot_name(url: &str, w: u32, h: u32, tag: &str) -> String {
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

/// Свободное имя в каталоге: одинаковые кадры не должны затирать друг друга.
pub fn unique(dir: &Path, name: &str) -> PathBuf {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("shot");
    let ext = Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("png");
    let mut p = dir.join(name);
    let mut i = 1;
    while p.exists() {
        p = dir.join(format!("{stem}-{i}.{ext}"));
        i += 1;
    }
    p
}

/// Канал вложений: модель видит картинку только оттуда. Без переменной файл
/// остаётся на диске и о том, что он не уехал, сказано в stdout.
pub fn deliver(path: &Path, bytes: &[u8]) -> Result<(), String> {
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
