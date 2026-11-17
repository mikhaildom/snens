// CDP-клиент: соединение по WebSocket, вызовы с ответом по id, дедлайн на весь
// запуск. Копия подхода из `shot` (там он проверен на живом Chrome), но без
// сессий: подключаемся прямо к WebSocket вкладки, а браузерный endpoint нужен
// только чтобы открыть новую.
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

pub struct Cdp {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    id: u64,
    deadline: Instant,
    /// Сессия вложенной вкладки. Chrome ≥144 в режиме одобрения не пускает по
    /// прямому ws вкладки (403 Forbidden), поэтому единственный рабочий путь —
    /// один ws браузера и `Target.attachToTarget{flatten:true}`; все вызовы
    /// дальше идут с этим sessionId.
    session: Option<String>,
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

/// Минимальный HTTP GET по loopback: DevTools отдаёт /json/version и /json/list,
/// а тянуть ради этого http-клиент в тул незачем.
///
/// Читаем по Content-Length, а не «до закрытия»: сервер DevTools держит
/// соединение открытым (keep-alive), и чтение до EOF упирается в таймаут.
pub fn http_get(port: u16, path: &str) -> Result<String, String> {
    http_get_host("127.0.0.1", port, "127.0.0.1", path)
}

/// Тот же GET, но с произвольным адресом подключения и `Host`: через ssh-туннель
/// мы стучимся в локальный порт, а Chrome на той стороне ждёт `Host: 127.0.0.1:<его
/// порт>` — с чужим Host он отвечает 403 «Host header is specified and is not an
/// IP address or localhost».
pub fn http_get_host(
    host: &str,
    port: u16,
    host_header: &str,
    path: &str,
) -> Result<String, String> {
    let addr = format!("{host}:{port}");
    let sa = addr
        .to_socket_addrs()
        .map_err(|e| format!("{addr}: {e}"))?
        .next()
        .ok_or_else(|| format!("{addr}: адрес не разрешился"))?;
    let mut s = TcpStream::connect_timeout(&sa, Duration::from_secs(3)).map_err(|e| {
        format!("Chrome не слушает {addr} ({e}): не включён ли тумблер в chrome://inspect/#remote-debugging?")
    })?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes())
        .map_err(|e| format!("{addr}: {e}"))?;
    let mut buf: Vec<u8> = Vec::new();
    let mut head_end = None;
    let mut want: Option<usize> = None;
    loop {
        let mut chunk = [0u8; 8192];
        let n = match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                if head_end.is_some() {
                    break; // тело уже началось — довольствуемся тем, что есть
                }
                return Err(format!("{addr}{path}: {e}"));
            }
        };
        buf.extend_from_slice(&chunk[..n]);
        if head_end.is_none() {
            if let Some(i) = find(&buf, b"\r\n\r\n") {
                head_end = Some(i + 4);
                let head = String::from_utf8_lossy(&buf[..i]).to_string();
                let first = head.lines().next().unwrap_or("?");
                if !(first.contains(" 200") || first.contains(" 101")) {
                    return Err(format!("{addr}{path}: {first}"));
                }
                for line in head.lines().skip(1) {
                    if let Some((k, v)) = line.split_once(':') {
                        if k.eq_ignore_ascii_case("content-length") {
                            want = v.trim().parse().ok();
                        }
                    }
                }
            }
        }
        if let (Some(h), Some(w)) = (head_end, want) {
            if buf.len() >= h + w {
                break;
            }
        }
    }
    let h = head_end.ok_or_else(|| format!("{addr}{path}: не HTTP-ответ"))?;
    // Оборванное тело не выдаём за целое: обрезанный `/json/list` давал либо
    // «не JSON», либо, хуже, неполный список вкладок под видом полного.
    if let Some(w) = want {
        if buf.len() < h + w {
            return Err(format!(
                "{addr}{path}: тело оборвано ({} из {w} Б)",
                buf.len() - h
            ));
        }
    }
    Ok(String::from_utf8_lossy(&buf[h..]).to_string())
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Куда и как стучаться: прямой порт либо ssh-туннель к машине пользователя.
#[derive(Clone)]
pub struct Endpoint {
    /// адрес, куда подключаемся (обычно 127.0.0.1 — туннель уже поднят)
    pub host: String,
    /// порт на этом конце
    pub port: u16,
    /// порт на той стороне — он же в `Host` и его надо подставить в ws-URL
    pub remote_port: u16,
    /// ws-путь браузера, если `/json/version` недоступен (режим одобрения Chrome)
    pub browser_path: Option<String>,
}

impl Endpoint {
    pub fn direct(port: u16) -> Self {
        Endpoint {
            host: "127.0.0.1".into(),
            port,
            remote_port: port,
            browser_path: None,
        }
    }

    fn host_header(&self) -> String {
        format!("127.0.0.1:{}", self.remote_port)
    }

    pub fn get(&self, path: &str) -> Result<String, String> {
        http_get_host(&self.host, self.port, &self.host_header(), path)
    }

    /// ws-URL из ответа DevTools указывает на порт той стороны. При туннеле он
    /// неверен (там другой локальный порт), поэтому переписываем порт, оставляя
    /// путь и id.
    fn rewrite_ws(&self, url: &str) -> Result<String, String> {
        let rest = url
            .strip_prefix("ws://")
            .ok_or_else(|| format!("CDP: неожиданный ws-адрес {url}"))?;
        let slash = rest.find('/').unwrap_or(rest.len());
        let path = &rest[slash..];
        Ok(format!("ws://{}:{}{}", self.host, self.port, path))
    }

    /// ws браузера: из `/json/version`, а если его нет (режим одобрения) — из
    /// известного пути `/devtools/browser/<uuid>`.
    pub fn browser_ws(&self) -> Result<String, String> {
        if let Ok(ws) = self.ws_url(None) {
            return Ok(ws);
        }
        match &self.browser_path {
            Some(p) => Ok(format!("ws://{}:{}{}", self.host, self.port, p)),
            None => Err(
                "CDP: браузерный endpoint недоступен. Chrome ≥144 в режиме одобрения не отдаёт \
                 /json/*; путь берётся из файла DevToolsActivePort — проверь, что `web connect --tunnel` \
                 видит его на той стороне"
                    .into(),
            ),
        }
    }

    pub fn ws_url(&self, target: Option<&str>) -> Result<String, String> {
        if let Some(id) = target {
            // Обычный путь — /json/list с готовым ws-адресом.
            if let Ok(list) = self.get("/json/list") {
                if let Ok(v) = serde_json::from_str::<Value>(&list) {
                    for t in v.as_array().cloned().unwrap_or_default() {
                        if t.get("id").and_then(Value::as_str) == Some(id) {
                            if let Some(u) = t.get("webSocketDebuggerUrl").and_then(Value::as_str) {
                                return self.rewrite_ws(u);
                            }
                        }
                    }
                }
            }
            // Режим одобрения: ws-адрес вкладки строится по её targetId.
            // Существование проверяем через Target.getTargets — иначе опечатка в
            // id превращалась бы в непонятную ошибку соединения.
            let mut b = Cdp::connect(&self.browser_ws()?, 15.0)?;
            let r = b.call("Target.getTargets", Value::Null)?;
            let found = r
                .get("targetInfos")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter().any(|t| {
                        t.get("targetId").and_then(Value::as_str) == Some(id)
                            && t.get("type").and_then(Value::as_str) == Some("page")
                    })
                })
                .unwrap_or(false);
            if !found {
                return Err(format!("CDP: вкладки {id} больше нет"));
            }
            return Ok(format!(
                "ws://{}:{}/devtools/page/{id}",
                self.host, self.port
            ));
        }
        let raw = self.get("/json/version")?;
        let v: Value = serde_json::from_str(&raw).map_err(|e| format!("CDP /json/version: {e}"))?;
        let u = v
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| "CDP: в /json/version нет webSocketDebuggerUrl".to_string())?;
        self.rewrite_ws(u)
    }
}

/// `ws://host:port/path` → адрес для TCP-connect. `wss://` отвергаем явно:
/// сборка tungstenite без TLS, и раньше такая попытка падала непонятной ошибкой.
fn tcp_addr(url: &str) -> Result<std::net::SocketAddr, String> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| format!("CDP: только ws:// (без TLS): {url}"))?;
    let hostport = rest.split(['/', '?']).next().unwrap_or(rest);
    let hostport = if hostport.contains(':') {
        hostport.to_string()
    } else {
        format!("{hostport}:80")
    };
    use std::net::ToSocketAddrs;
    hostport
        .to_socket_addrs()
        .map_err(|e| format!("CDP: {hostport}: {e}"))?
        .next()
        .ok_or_else(|| format!("CDP: {hostport} не разрешился"))
}

impl Cdp {
    pub fn connect(url: &str, timeout_s: f64) -> Result<Self, String> {
        Self::connect_with_host(url, timeout_s, None)
    }

    /// Подключение с подменой `Host`. Через туннель мы стучимся в локальный порт,
    /// а Chrome сравнивает `Host` с тем портом, на котором слушает сам (9222), и
    /// на чужой отвечает 403 Forbidden ещё на рукопожатии.
    pub fn connect_with_host(
        url: &str,
        timeout_s: f64,
        host_header: Option<&str>,
    ) -> Result<Self, String> {
        // from_secs_f64 паникует на NaN/inf, а Instant + Duration — на переполнении:
        // `--timeout inf` из CLI не должен ронять процесс.
        let secs = if timeout_s.is_finite() {
            timeout_s.clamp(1.0, 3600.0)
        } else {
            1.0
        };
        let budget = Duration::from_secs_f64(secs);
        let deadline = Instant::now() + budget;
        // Таймаут ставим ДО рукопожатия: `tungstenite::connect` делает TCP-connect
        // и WebSocket-handshake без таймаута вовсе, и мёртвый (blackhole) туннель
        // висел бы минутами, обещая «жёсткий дедлайн».
        let stream = TcpStream::connect_timeout(&tcp_addr(url)?, budget)
            .map_err(|e| format!("CDP: {url}: {e}"))?;
        let _ = stream.set_read_timeout(Some(budget));
        let _ = stream.set_write_timeout(Some(budget));
        // Тот же тип, что у `tungstenite::connect`: структура держит MaybeTlsStream.
        let stream = MaybeTlsStream::Plain(stream);
        let (mut ws, _res) = match host_header {
            Some(h) => {
                use tungstenite::client::IntoClientRequest;
                let mut req = url
                    .into_client_request()
                    .map_err(|e| format!("CDP: адрес {url}: {e}"))?;
                req.headers_mut().insert(
                    "Host",
                    h.parse().map_err(|e| format!("CDP: Host {h}: {e}"))?,
                );
                tungstenite::client(req, stream).map_err(|e| format!("CDP: {e}"))?
            }
            None => {
                use tungstenite::client::IntoClientRequest;
                let req = url
                    .into_client_request()
                    .map_err(|e| format!("CDP: адрес {url}: {e}"))?;
                tungstenite::client(req, stream).map_err(|e| format!("CDP: {e}"))?
            }
        };
        if let MaybeTlsStream::Plain(s) = ws.get_mut() {
            let left = deadline.saturating_duration_since(Instant::now());
            let _ = s.set_read_timeout(Some(left));
            let _ = s.set_write_timeout(Some(left));
        }
        Ok(Cdp {
            ws,
            id: 0,
            deadline,
            session: None,
        })
    }

    fn left(&self) -> Result<Duration, String> {
        let now = Instant::now();
        if now >= self.deadline {
            Err("таймаут: Chrome не довёл работу до конца (страница держит поток?)".into())
        } else {
            Ok(self.deadline - now)
        }
    }

    /// Вложенная сессия вкладки — все вызовы дальше идут в неё.
    pub fn with_session(mut self, session: String) -> Self {
        self.session = Some(session);
        self
    }

    /// Вложиться в вкладку (режим одобрения Chrome). Возвращает sessionId.
    pub fn attach(&mut self, target: &str) -> Result<String, String> {
        let r = self.call(
            "Target.attachToTarget",
            json!({"targetId": target, "flatten": true}),
        )?;
        r.get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "CDP: attachToTarget не вернул sessionId".to_string())
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.id += 1;
        let id = self.id;
        let left = self.left()?;
        if let MaybeTlsStream::Plain(s) = self.ws.get_mut() {
            let _ = s.set_write_timeout(Some(left));
        }
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = &self.session {
            msg["sessionId"] = Value::String(s.clone());
        }
        self.ws
            .send(Message::Text(msg.to_string().into()))
            .map_err(|e| format!("CDP send {method}: {e}"))?;
        loop {
            let left = self.left()?;
            if let MaybeTlsStream::Plain(s) = self.ws.get_mut() {
                let _ = s.set_read_timeout(Some(left));
            }
            match self.ws.read() {
                Ok(Message::Text(t)) => {
                    let v: Value = serde_json::from_str(t.as_str())
                        .map_err(|e| format!("CDP: не JSON: {e}"))?;
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
                Ok(Message::Binary(b)) => {
                    let v: Value = serde_json::from_slice(&b).map_err(|e| format!("CDP: {e}"))?;
                    if v.get("id").and_then(Value::as_u64) != Some(id) {
                        continue;
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
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
                Err(e) => return Err(format!("CDP {method}: {e}")),
            }
        }
    }

    /// JS на странице; исключение — ошибка, значение — `returnByValue`.
    pub fn eval(&mut self, expr: &str) -> Result<Value, String> {
        let r = self.call(
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

    /// Документ ждём опросом readyState: на странице с открытым потоком
    /// `Page.loadEventFired` может не прийти никогда.
    pub fn wait_ready(&mut self, budget: Duration) -> Result<(), String> {
        let until = Instant::now() + budget;
        loop {
            let state = match self.eval("document.readyState") {
                Ok(v) => v,
                // Мёртвое соединение/закрытая вкладка — не «страница ещё грузится»:
                // иначе вызывающий узнает об этом позже и не там.
                Err(e) => return Err(format!("CDP: страница не отвечает: {e}")),
            };
            if state.as_str() == Some("complete") {
                return Ok(());
            }
            if Instant::now() >= until {
                return Ok(()); // недостроенная страница — не повод падать
            }
            std::thread::sleep(Duration::from_millis(120));
        }
    }
}
