// desk — не браузер: окна и приложения машины пользователя через дерево
// доступности (Windows UI Automation сегодня, AT-SPI на Linux — следующий шаг).
//
// Устройство: на стороне пользователя живёт долгий процесс-агент в его
// интерактивной сессии (из ssh UIA видит НОЛЬ окон — проверено), а мы говорим с
// ним по одной JSON-строке на запрос через ssh-туннель. Это тот же контракт, что
// у `web`: снимок дерева с адресами, действия по адресам батчем, пиксели как
// fallback.
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::{json, Value};

mod tunnel;

const USAGE: &str = "desk — окна и приложения машины пользователя через дерево доступности: снимок с адресами, действия по адресам батчем, скриншот как fallback";

const HELP: &str = "\
desk — не браузер: окна и приложения машины пользователя через дерево доступности.
desk <команда> … [--tunnel user@machine]

desk launch [--port N]                 поднять агента на той машине в сессии пользователя
desk stop                              остановить агента
desk apps                              окна: индекс, заголовок, класс, pid, рамка
desk snap [--window W] [--full]         дерево окна с адресами e1…eN (файл + счётчики)
desk act --json '<действия>' [--window W]  действия по адресам: click/set/toggle/focus/type/keys
desk shot [--window W] [--out файл]     скриншот окна (почти всегда нужен для canvas)
desk focus [--window W]                 вывести окно на передний план
desk status                             что запущено на той стороне

  --window W   выбор окна: индекс из `apps` (0,1,2…), подстрока заголовка, или пусто = в фокусе
  --tunnel     машина, где живёт агент (ssh). Агент запускается `desk launch`

Устройство: на машине пользователя работает долгий процесс в ЕГО интерактивной
сессии (из ssh система видит ноль окон — проверено), мы говорим с ним по
JSON-строке через ssh-туннель. Снимок дерева уходит в файл, в контекст — путь,
счётчики и первые строки. Адреса e1…eN живут до следующего `snap`.

Приоритет: если задача в браузере — это `web` (CDP дешевле и точнее). `desk` — для
всего, что вне браузера: мессенджеры, файловые менеджеры, диалоги ОС, canvas.

Коды: 0 — успех, 2 — неверные аргументы, 1 — ошибка выполнения.";

fn die(msg: &str, code: i32) -> ! {
    eprintln!("{msg}");
    std::process::exit(code);
}

#[derive(Default)]
pub struct Args {
    pub positional: Vec<String>,
    pub flags: BTreeMap<String, String>,
    /// Все значения повторяющегося флага: `--json` может прийти несколько раз
    /// (ядро разворачивает массив из схемы в повторение флага).
    pub multi: BTreeMap<String, Vec<String>>,
    pub has: Vec<String>,
}

/// Флаги, которые команда вообще понимает: опечатка (`--windw`) должна стать
/// ошибкой аргументов, а не тихо проигнорированным флагом.
const FLAGS: [&str; 8] = [
    "tunnel", "port", "window", "full", "json", "out", "timeout", "progress",
];

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
                if !FLAGS.contains(&k.as_str()) {
                    return Err(format!(
                        "desk: неизвестный флаг --{k} (есть: {})",
                        FLAGS.join(", ")
                    ));
                }
                if let Some(v) = v {
                    a.multi.entry(k.clone()).or_default().push(v.clone());
                    a.flags.insert(k, v);
                } else if let Some(next) = it.peek() {
                    if !next.starts_with("--") {
                        let v = it.next().unwrap().clone();
                        a.multi.entry(k.clone()).or_default().push(v.clone());
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

    /// Все значения повторяющегося флага (например `--json`).
    pub fn values(&self, name: &str) -> Vec<&str> {
        self.multi
            .get(name)
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    pub fn on(&self, name: &str) -> bool {
        self.has.iter().any(|h| h == name) || self.flags.contains_key(name)
    }

    /// Порт: строгий разбор без тихого отката на дефолт и без насыщения
    /// (`--port 70000` раньше молча становился 65535).
    pub fn port(&self) -> Result<u16, String> {
        match self.flag("port") {
            None => Ok(9227),
            Some(v) => v
                .parse::<u16>()
                .map_err(|e| format!("desk: --port ждёт число 1..65535, получено {v} ({e})")),
        }
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

fn main() {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
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
        "launch" => cmd_launch(&args),
        "stop" => cmd_stop(&args),
        "apps" => cmd_apps(&args),
        "snap" => cmd_snap(&args),
        "act" => cmd_act(&args),
        "shot" => cmd_shot(&args),
        "focus" => cmd_focus(&args),
        "status" => cmd_status(&args),
        other => {
            eprintln!("desk: неизвестная команда {other}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        die(&e, 1);
    }
}

// ---------------------------------------------------------------- транспорт

/// Запрос к агенту на той стороне: одна JSON-строка туда, одна обратно.
fn call(args: &Args, req: Value) -> Result<Value, String> {
    let host = tunnel::host(args)?;
    // Агент без токена не отвечает: токен лежит в каталоге состояния рядом с
    // туннелем и уезжает в каждом запросе.
    let token = tunnel::token()?;
    let mut req = req;
    req["token"] = Value::String(token);
    let port = tunnel::ensure(&host, args.port()?)?;
    let addr = format!("127.0.0.1:{port}");
    let sa = addr
        .to_socket_addrs()
        .map_err(|e| format!("{addr}: {e}"))?
        .next()
        .ok_or_else(|| format!("{addr}: не разрешился"))?;
    let mut s = TcpStream::connect_timeout(&sa, Duration::from_secs(10))
        .map_err(|e| format!("агент не отвечает на {addr} ({e}): подними его `desk launch`"))?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(60)));
    // Сериализация может отказать: пустая строка вместо запроса давала бы
    // «ответ не пришёл»/«не JSON» вместо настоящей причины.
    let body = serde_json::to_string(&req).map_err(|e| format!("запрос не сериализуется: {e}"))?;
    s.write_all(format!("{body}\n").as_bytes())
        .map_err(|e| format!("{addr}: запрос не отправлен: {e}"))?;
    let mut reader = BufReader::new(s);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("{addr}: ответ не пришёл: {e}"))?;
    if line.trim().is_empty() {
        return Err(format!(
            "{addr}: агент закрыл соединение без ответа. Скорее всего он старой версии — \
             перезапусти: `desk launch`"
        ));
    }
    let v: Value =
        serde_json::from_str(&line).map_err(|e| format!("агент ответил не JSON: {e}: {line}"))?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        let err = v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("без причины");
        return Err(format!("агент: {err}"));
    }
    Ok(v)
}

fn window_flag(args: &Args) -> Value {
    match args.flag("window") {
        Some(w) => Value::String(w.to_string()),
        None => Value::Null,
    }
}

// ---------------------------------------------------------------- команды

fn cmd_launch(args: &Args) -> Result<(), String> {
    let host = tunnel::host(args)?;
    let port = args.port()?;
    tunnel::start_agent(&host, port)?;
    println!("агент запущен на {host}, порт {port} (в сессии пользователя)");
    // Проверяем, что он действительно отвечает, а не просто «команда прошла»:
    // call сам поднимет туннель и спросит ping.
    let v = call(args, json!({"cmd": "ping"}))?;
    println!(
        "готов: сессия {}, pid {}",
        v.get("session").map(|x| x.to_string()).unwrap_or_default(),
        v.get("pid").map(|x| x.to_string()).unwrap_or_default()
    );
    Ok(())
}

fn cmd_stop(args: &Args) -> Result<(), String> {
    let host = tunnel::host(args)?;
    tunnel::stop_agent(&host)?;
    println!("агент остановлен на {host}");
    Ok(())
}

fn cmd_status(args: &Args) -> Result<(), String> {
    let host = tunnel::host(args)?;
    let port = args.port()?;
    match tunnel::ensure(&host, port) {
        Ok(_) => {
            let v = call(args, json!({"cmd": "ping"}))?;
            println!(
                "агент на {host} жив: сессия {}, pid {}",
                v.get("session").map(|x| x.to_string()).unwrap_or_default(),
                v.get("pid").map(|x| x.to_string()).unwrap_or_default()
            );
        }
        // Недоступный агент — это провал: по коду 0 скрипты считали бы, что он живёт.
        Err(e) => return Err(format!("агент на {host} недоступен: {e}")),
    }
    Ok(())
}

fn cmd_apps(args: &Args) -> Result<(), String> {
    let v = call(args, json!({"cmd": "apps"}))?;
    let wins = v
        .get("windows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if wins.is_empty() {
        return Err(
            "агент не видит ни одного окна. Он точно в сессии пользователя? \
             `desk launch` перезапускает его правильно"
                .into(),
        );
    }
    for w in &wins {
        let i = w.get("i").and_then(Value::as_i64).unwrap_or(0);
        let name = w.get("name").and_then(Value::as_str).unwrap_or("");
        let class = w.get("class").and_then(Value::as_str).unwrap_or("");
        let pid = w.get("pid").and_then(Value::as_i64).unwrap_or(0);
        let (x, y, ww, hh) = (
            w.get("x").and_then(Value::as_i64).unwrap_or(0),
            w.get("y").and_then(Value::as_i64).unwrap_or(0),
            w.get("w").and_then(Value::as_i64).unwrap_or(0),
            w.get("h").and_then(Value::as_i64).unwrap_or(0),
        );
        println!(
            "{i:>2}. {} [{class}] pid={pid} {x},{y} {ww}x{hh}",
            trunc(name, 70)
        );
    }
    Ok(())
}

fn refs_path(host: &str) -> std::path::PathBuf {
    let base = std::env::var("SNENS_DESK_STATE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
                .join(".local/state/snens/desk")
        });
    let safe: String = host
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    base.join(safe)
}

fn cmd_snap(args: &Args) -> Result<(), String> {
    let host = tunnel::host(args)?;
    let v = call(
        args,
        json!({"cmd": "snap", "window": window_flag(args), "full": args.on("full")}),
    )?;
    let tree = v.get("tree").and_then(Value::as_str).unwrap_or("");
    let refs = v.get("refs").cloned().unwrap_or(Value::Array(vec![]));
    let dir = refs_path(&host);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let page = dir.join("tree.txt");
    std::fs::write(&page, tree).map_err(|e| format!("{}: {e}", page.display()))?;
    let rf = dir.join("refs.json");
    std::fs::write(&rf, refs.to_string()).map_err(|e| format!("{}: {e}", rf.display()))?;

    println!(
        "{} — {} узлов, адресов {} ({} мс)",
        trunc(v.get("window").and_then(Value::as_str).unwrap_or(""), 60),
        v.get("total").and_then(Value::as_u64).unwrap_or(0),
        v.get("kept").and_then(Value::as_u64).unwrap_or(0),
        v.get("ms").and_then(Value::as_u64).unwrap_or(0)
    );
    println!("дерево → {}", page.display());
    println!("адреса → {}", rf.display());
    for line in tree.lines().take(12) {
        println!("{line}");
    }
    let n = tree.lines().count();
    if n > 12 {
        println!(
            "… ещё {} строк, читать: grep -n <слово> {}",
            n - 12,
            page.display()
        );
    }
    Ok(())
}

fn cmd_act(args: &Args) -> Result<(), String> {
    let host = tunnel::host(args)?;
    // Ядро собирает argv по схеме: массив из поля `json` превращается в
    // повторение флага, поэтому значений может быть несколько. Все части —
    // одна логическая пачка действий.
    let mut raws: Vec<String> = args
        .values("json")
        .into_iter()
        .map(str::to_string)
        .collect();
    if raws.is_empty() {
        // Позиционные разбираются из argv[1..]: первый из них — это [0], а не [1].
        if let Some(p) = args.positional.first() {
            raws.push(p.clone());
        }
    }
    if raws.is_empty() {
        return Err("desk act: нужен --json '<массив действий>'".into());
    }
    let parsed: Vec<Value> = raws
        .iter()
        .map(|raw| serde_json::from_str(raw).map_err(|e| format!("desk act: не JSON: {e}")))
        .collect::<Result<_, _>>()?;
    let acts: Vec<Value> = match parsed.as_slice() {
        // Один аргумент-массив — обычная форма `--json '[{...}]'`.
        [Value::Array(items)] => items.clone(),
        // Один не-массив — ошибка формы: одиночное действие путает с пустым набором.
        [other] => {
            return Err(format!(
            "desk act: ожидается массив действий, напр. [{{\"click\":\"e3\"}}], получено {other}"
        ))
        }
        // Несколько аргументов (ядро развернуло массив схемы в повторение
        // флага) — каждый обязан быть объектом-действием.
        many => {
            if let Some(bad) = many.iter().find(|v| !v.is_object()) {
                return Err(format!(
                    "desk act: ожидается массив действий, напр. [{{\"click\":\"e3\"}}], получено {bad}"
                ));
            }
            many.to_vec()
        }
    };
    let actions = Value::Array(acts);
    if actions.as_array().map(|a| a.is_empty()).unwrap_or(true) {
        return Err("desk act: ожидается массив действий, напр. [{\"click\":\"e3\"}]".into());
    }
    // Пути элементов берём из последнего снимка: AutomationElement между
    // запросами не сохранить, а путь «индексы детей от окна» стабилен.
    let rf = refs_path(&host).join("refs.json");
    // Пустая таблица адресов = все действия упадут в агенте с невнятной ошибкой.
    // Поэтому «снимка нет» и «снимок битый» различаем и говорим, что делать.
    let refs: Value = match std::fs::read_to_string(&rf) {
        Ok(t) => serde_json::from_str(&t).map_err(|e| {
            format!(
                "desk act: {} испорчен ({e}) — сделай `desk snap` заново",
                rf.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "desk act: нет снимка {} — сначала `desk snap`, иначе адреса некуда сопоставить",
                rf.display()
            ))
        }
        Err(e) => return Err(format!("desk act: {} не читается: {e}", rf.display())),
    };
    let v = call(
        args,
        json!({
            "cmd": "act",
            "window": window_flag(args),
            "actions": actions,
            "refs": refs,
            "focus_window": true,
        }),
    )?;
    for r in v
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let s = r.as_str().unwrap_or("");
        if s.starts_with("ошибка") {
            println!("✗ {s}");
        } else {
            println!("✓ {s}");
        }
    }
    Ok(())
}

fn cmd_shot(args: &Args) -> Result<(), String> {
    let v = call(
        args,
        json!({"cmd": "shot", "window": window_flag(args), "focus_window": true}),
    )?;
    let b64 = v
        .get("png_base64")
        .and_then(Value::as_str)
        .ok_or("агент не вернул картинку")?;
    let bytes = base64_decode(b64)?;
    let d = std::env::var("SNENS_ATTACH_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| refs_path("shots"));
    let _ = std::fs::create_dir_all(&d);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|x| x.as_secs())
        .unwrap_or(0);
    let out = args
        .flag("out")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| d.join(format!("desk-{stamp}.png")));
    std::fs::write(&out, &bytes).map_err(|e| format!("{}: {e}", out.display()))?;
    println!(
        "{}: image/png {}x{} {} Б",
        out.display(),
        v.get("w").and_then(Value::as_u64).unwrap_or(0),
        v.get("h").and_then(Value::as_u64).unwrap_or(0),
        bytes.len()
    );
    Ok(())
}

fn cmd_focus(args: &Args) -> Result<(), String> {
    let v = call(args, json!({"cmd": "focus", "window": window_flag(args)}))?;
    println!(
        "в фокусе: {}",
        trunc(v.get("window").and_then(Value::as_str).unwrap_or(""), 60)
    );
    Ok(())
}

/// Base64 с проверкой формы: `=` только в хвосте, длина кратна 4, остаточные
/// биты нулевые. Иначе искажённый ответ агента декодировался бы в мусорный PNG,
/// а не в ошибку.
fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    const NL: u8 = 10;
    const CR: u8 = 13;
    let bad = || "агент: не base64 в картинке".to_string();
    let body: Vec<u8> = s.bytes().filter(|c| *c != NL && *c != CR).collect();
    if body.is_empty() || !body.len().is_multiple_of(4) {
        return Err(bad());
    }
    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    let mut pad_seen = false;
    for (i, c) in body.iter().copied().enumerate() {
        if c == b'=' {
            // Допустимы только два последних байта, и после них — ничего.
            if i < body.len() - 2 {
                return Err(bad());
            }
            pad_seen = true;
            continue;
        }
        if pad_seen {
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
        }
    }
    // Мусорные младшие биты в хвосте — признак неполного/искажённого base64.
    if bits > 0 && (acc & ((1 << bits) - 1)) != 0 {
        return Err(bad());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::base64_decode;

    #[test]
    fn base64_rejects_malformed_input() {
        assert_eq!(base64_decode("aGk=").unwrap(), b"hi");
        assert!(base64_decode("aGk").is_err(), "длина не кратна 4");
        assert!(base64_decode("aG=k").is_err(), "символ после паддинга");
        assert!(base64_decode("a=lk").is_err(), "паддинг не в хвосте");
        assert!(base64_decode("aGl*").is_err(), "посторонний символ");
        assert!(base64_decode("aGk=aGk=").is_err(), "две группы с паддингом");
    }
}
