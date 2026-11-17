// ssh-туннель к Chrome на машине пользователя. Смысл: чтобы проверить работу,
// на его машину не нужно ставить наш бинарник — достаточно, что там открыт Chrome
// с согласием на отладку. Мы поднимаем `ssh -N -L <локальный>:127.0.0.1:<порт>`
// и говорим с браузером как с локальным.
//
// Тунель живёт между вызовами (каждый вызов — свой процесс, адреса в состоянии),
// поэтому процесс отвязан от терминала и записан pid: иначе после `web connect`
// туннель умирал бы вместе с ним.
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::cdp::Endpoint;
use crate::state;

/// Куда ssh туннеля пишет stderr: файл в каталоге состояния рядом с tunnel.json.
fn stderr_path() -> std::path::PathBuf {
    state::dir().join("tunnel.err")
}

/// Свободный порт на локальной стороне: просим ОС выдать и закрываем.
fn free_port() -> Result<u16, String> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("порт: {e}"))?;
    let p = l.local_addr().map_err(|e| format!("порт: {e}"))?.port();
    Ok(p)
}

pub struct Tunnel {
    pub host: String,
    pub local_port: u16,
    pub remote_port: u16,
    pub pid: u32,
}

impl Tunnel {
    pub fn endpoint(&self) -> Endpoint {
        Endpoint {
            host: "127.0.0.1".into(),
            port: self.local_port,
            remote_port: self.remote_port,
            browser_path: None,
        }
    }
}

pub fn load() -> Option<Tunnel> {
    state::load_tunnel()
}

/// Поднять туннель: `user@machine` и порт CDP на той стороне (по умолчанию 9222).
/// Адрес машины уходит в argv ssh: значение, начинающееся с `-`, OpenSSH
/// разберёт как свою опцию (`-oProxyCommand=…` — это выполнение локальной
/// команды). Проверяем форму и ставим `--` перед адресом.
pub fn check_host(host: &str) -> Result<(), String> {
    let bad = host.is_empty()
        || host.starts_with('-')
        || host.chars().any(|c| c.is_whitespace() || c.is_control())
        || !host.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | ':' | '[' | ']')
        });
    if bad {
        return Err(format!(
            "web: недопустимый адрес машины `{host}` — нужен вид user@host"
        ));
    }
    Ok(())
}

/// Поднять туннель. Порт на локальной стороне занимается нашим же листенером и
/// сразу освобождается, поэтому теоретически его может забрать другой процесс;
/// тогда `is_open` покажет живой порт, которого мы не поднимали. Это ловится
/// первым же CDP-запросом, а не молчаливой работой с чужим браузером.
pub fn start(host: &str, remote_port: u16) -> Result<Tunnel, String> {
    check_host(host)?;
    // Уже поднят к тому же адресу — переиспользуем, а не плодим процессы.
    if let Some(t) = load() {
        if t.host == host && t.remote_port == remote_port && is_open(t.local_port) {
            return Ok(t);
        }
        stop(&t);
    }
    let local = free_port()?;
    let spec = format!("{local}:127.0.0.1:{remote_port}");
    let mut cmd = Command::new("ssh");
    cmd.arg("-N")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("ServerAliveInterval=20")
        .arg("-L")
        .arg(&spec)
        .arg("--")
        .arg(host)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // stderr в файл, а не в пайп: невычитанный пайп заполняется и вешает ssh
        // на write(2), а закрытие read-конца после нашего выхода убивает туннель
        // SIGPIPE'ом. Файл читаем только в ветке отказа.
        .stderr(Stdio::from(
            std::fs::File::create(stderr_path()).map_err(|e| format!("stderr туннеля: {e}"))?,
        ));
    // Отвязать от терминала: туннель должен переживать наш процесс, иначе
    // следующий вызов останется без соединения.
    #[cfg(unix)]
    // SAFETY: pre_exec исполняется в форкнутом ребёнке до exec, где допустимы
    // только async-signal-safe операции. `setsid(2)` такая и есть (ни аллокаций,
    // ни блокировок, ни логирования — не добавлять!). Возврат игнорируем
    // осознанно: своя сессия нужна лишь для живучести туннеля, а при EPERM
    // процесс всё равно запускается.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("ssh не запустился: {e}"))?;
    let pid = child.id();

    // Ждём, пока локальный порт начнёт отвечать по CDP. stderr читаем тут же:
    // при отказе в доступе ssh пишет причину, и ждать 10 секунд впустую незачем.
    let until = Instant::now() + Duration::from_secs(12);
    loop {
        if is_open(local) {
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            let msg = std::fs::read_to_string(stderr_path()).unwrap_or_default();
            return Err(format!(
                "ssh-туннель не поднялся (код {:?}): {}",
                status.code(),
                msg.trim().lines().last().unwrap_or("причина не сказана")
            ));
        }
        if Instant::now() >= until {
            let _ = child.kill();
            return Err(format!(
                "ssh-туннель на {host} не открыл порт {local} за 12 с: \
                 проверь `ssh {host} true` и что Chrome там слушает {remote_port}"
            ));
        }
        std::thread::sleep(Duration::from_millis(120));
    }
    let t = Tunnel {
        host: host.to_string(),
        local_port: local,
        remote_port,
        pid,
    };
    // Если состояние не сохранилось, туннель остался бы сиротой без pid-файла:
    // следующий `start` поднял бы второй. Гасим и забираем ребёнка.
    if let Err(e) = state::save_tunnel(&t) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    Ok(t)
}

pub fn stop(t: &Tunnel) {
    #[cfg(unix)]
    {
        // PID мог быть переиспользован: перед сигналом убеждаемся, что это наш ssh.
        if owns(t.pid, &t.host) {
            let _ = Command::new("kill").arg(t.pid.to_string()).status();
        }
    }
    state::clear_tunnel();
}

/// Наш ли это ssh-процесс: командная строка должна содержать и порт-форвард
/// (`-L`), и адрес машины. На Linux читаем /proc, иначе доверяем pid.
#[cfg(unix)]
fn owns(pid: u32, host: &str) -> bool {
    let path = format!("/proc/{pid}/cmdline");
    match std::fs::read(&path) {
        Ok(raw) => {
            let cmd = String::from_utf8_lossy(&raw).replace('\0', " ");
            cmd.contains("ssh") && cmd.contains(host) && cmd.contains("-L")
        }
        // /proc нет (macOS): проверить нечем, ведём себя как раньше.
        Err(_) => true,
    }
}

/// Порт локального конца отвечает? Именно «порт открыт», без запроса к CDP:
/// Chrome ≥144 в режиме одобрения не отдаёт `/json/*` (404), и проверка через
/// HTTP отбрасывала бы живой туннель.
fn is_open(port: u16) -> bool {
    let addr = format!("127.0.0.1:{port}");
    let Ok(mut sa) = addr.to_socket_addrs() else {
        return false;
    };
    let Some(sa) = sa.next() else { return false };
    TcpStream::connect_timeout(&sa, Duration::from_millis(400)).is_ok()
}

/// ws-путь браузера с той стороны (только путь, без порта): быстрый способ —
/// `/json/version`, иначе файл `DevToolsActivePort`. Порт наружу не отдаётся —
/// у вызывающего он уже есть как `remote_port`.
/// В режиме одобрения (Chrome 144+) HTTP-эндпоинтов `/json/*` нет, но файл
/// `DevToolsActivePort` в каталоге профиля есть всегда — из него и берём путь.
pub fn remote_browser_path(host: &str, local_port: u16, remote_port: u16) -> Option<String> {
    // Сначала обычный путь: /json/version (старые сборки и отдельные профили).
    // Host — с портом той стороны: Chrome сверяет его со своим и на чужой
    // отвечает 403, из-за чего быстрый путь молча не работал.
    let host_header = format!("127.0.0.1:{remote_port}");
    if let Ok(raw) = crate::cdp::http_get_host(
        "127.0.0.1",
        local_port,
        &host_header,
        "/json/version",
    ) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(u) = v.get("webSocketDebuggerUrl").and_then(|x| x.as_str()) {
                if let Some(i) = u.find("/devtools/") {
                    return Some(u[i..].to_string());
                }
            }
        }
    }
    check_host(host).ok()?;
    for cmd in remote_candidates() {
        // Не `.ok()?`: сбой первого кандидата (Windows-команда на unix) не должен
        // прекращать поиск — следующий ещё может сработать. И общий таймаут:
        // ConnectTimeout покрывает только рукопожатие, а зависшая удалённая
        // команда держала бы вызывающего бесконечно.
        let Ok(out) = ssh_with_timeout(host, cmd, Duration::from_secs(10)) else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(p) = parse_devtools_active_port(&text) {
            return Some(p);
        }
    }
    None
}

/// ssh с общим ограничением времени: ждём не дольше `budget`, иначе гасим.
fn ssh_with_timeout(
    host: &str,
    cmd: &str,
    budget: Duration,
) -> Result<std::process::Output, String> {
    check_host(host)?;
    let mut child = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=5")
        .arg("--")
        .arg(host)
        .arg(cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("ssh {host}: {e}"))?;
    let until = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().map_err(|e| e.to_string()),
            Ok(None) => {}
            Err(e) => return Err(format!("ssh {host}: {e}")),
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("ssh {host}: не ответил за {budget:?}"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Команда для той стороны. ОС той машины наша сборка не знает, поэтому
/// пробуем обе формы: powershell (Windows) и sh (Linux/macOS). Лишняя попытка
/// стоит один ssh-вызов и только тогда, когда `/json/version` уже не ответил.
fn remote_candidates() -> [&'static str; 2] {
    [
        "powershell -NoProfile -Command \"Get-Content (Join-Path $env:LOCALAPPDATA 'Google\\Chrome\\User Data\\DevToolsActivePort') -ErrorAction SilentlyContinue\"",
        "sh -c 'cat \"$HOME/.config/google-chrome/DevToolsActivePort\" 2>/dev/null || cat \"$HOME/Library/Application Support/Google/Chrome/DevToolsActivePort\" 2>/dev/null'",
    ]
}

/// Разбор файла: первая строка — порт, вторая — путь (`/devtools/browser/<uuid>`).
pub fn parse_devtools_active_port(text: &str) -> Option<String> {
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("/devtools/") {
            return Some(l.to_string());
        }
    }
    None
}
