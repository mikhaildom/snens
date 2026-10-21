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
    let addr = format!("127.0.0.1:{port}");
    let sa = addr
        .to_socket_addrs()
        .map_err(|e| format!("{addr}: {e}"))?
        .next()
        .ok_or_else(|| format!("{addr}: адрес не разрешился"))?;
    let mut s = TcpStream::connect_timeout(&sa, Duration::from_secs(3)).map_err(|e| {
        format!("Chrome не слушает {addr} ({e}): не включён ли тумблер в chrome://inspect/#remote-debugging?")
    })?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
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
    Ok(String::from_utf8_lossy(&buf[h..]).to_string())
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// WebSocket-адрес вкладки (или браузера, если id не задан).
pub fn ws_url(port: u16, target: Option<&str>) -> Result<String, String> {
    if let Some(id) = target {
        let list = http_get(port, "/json/list")?;
        let v: Value =
            serde_json::from_str(&list).map_err(|e| format!("CDP /json/list: {e}"))?;
        for t in v.as_array().cloned().unwrap_or_default() {
            if t.get("id").and_then(Value::as_str) == Some(id) {
                if let Some(u) = t.get("webSocketDebuggerUrl").and_then(Value::as_str) {
                    return Ok(u.to_string());
                }
            }
        }
        return Err(format!("CDP: вкладки {id} больше нет"));
    }
    let v = http_get(port, "/json/version")?;
    let v: Value = serde_json::from_str(&v).map_err(|e| format!("CDP /json/version: {e}"))?;
    v.get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "CDP: в /json/version нет webSocketDebuggerUrl".to_string())
}

impl Cdp {
    pub fn connect(url: &str, timeout_s: f64) -> Result<Self, String> {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout_s.max(1.0));
        let (mut ws, _res) = tungstenite::connect(url).map_err(|e| format!("CDP: {e}"))?;
        if let MaybeTlsStream::Plain(s) = ws.get_mut() {
            let left = deadline.saturating_duration_since(Instant::now());
            let _ = s.set_read_timeout(Some(left));
            let _ = s.set_write_timeout(Some(left));
        }
        Ok(Cdp { ws, id: 0, deadline })
    }

    fn left(&self) -> Result<Duration, String> {
        let now = Instant::now();
        if now >= self.deadline {
            Err("таймаут: Chrome не довёл работу до конца (страница держит поток?)".into())
        } else {
            Ok(self.deadline - now)
        }
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.id += 1;
        let id = self.id;
        let left = self.left()?;
        if let MaybeTlsStream::Plain(s) = self.ws.get_mut() {
            let _ = s.set_write_timeout(Some(left));
        }
        let msg = json!({"id": id, "method": method, "params": params});
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
                    let v: Value =
                        serde_json::from_str(t.as_str()).map_err(|e| format!("CDP: не JSON: {e}"))?;
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
                    let v: Value =
                        serde_json::from_slice(&b).map_err(|e| format!("CDP: {e}"))?;
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
            let state = self
                .eval("document.readyState")
                .unwrap_or(Value::String(String::new()));
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
