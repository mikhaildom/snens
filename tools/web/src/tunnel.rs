// ssh-туннель к Chrome на машине пользователя. Смысл: чтобы проверить работу,
// на его машину не нужно ставить наш бинарник — достаточно, что там открыт Chrome
// с согласием на отладку. Мы поднимаем `ssh -N -L <локальный>:127.0.0.1:<порт>`
// и говорим с браузером как с локальным.
//
// Тунель живёт между вызовами (каждый вызов — свой процесс, адреса в состоянии),
// поэтому процесс отвязан от терминала и записан pid: иначе после `web connect`
// туннель умирал бы вместе с ним.
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::cdp::Endpoint;
use crate::state;

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
        }
    }
}

pub fn load() -> Option<Tunnel> {
    state::load_tunnel()
}

/// Поднять туннель: `user@machine` и порт CDP на той стороне (по умолчанию 9222).
pub fn start(host: &str, remote_port: u16) -> Result<Tunnel, String> {
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
        .arg(host)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // Отвязать от терминала: туннель должен переживать наш процесс, иначе
    // следующий вызов останется без соединения.
    #[cfg(unix)]
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
            let mut msg = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut msg);
            }
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
    state::save_tunnel(&t)?;
    Ok(t)
}

pub fn stop(t: &Tunnel) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill").arg(t.pid.to_string()).status();
    }
    state::clear_tunnel();
}

fn is_open(port: u16) -> bool {
    let addr = format!("127.0.0.1:{port}");
    let Ok(mut sa) = addr.to_socket_addrs() else {
        return false;
    };
    let Some(sa) = sa.next() else { return false };
    let Ok(mut s) = TcpStream::connect_timeout(&sa, Duration::from_millis(400)) else {
        return false;
    };
    // Порт открыт ещё не значит «CDP живой»: проверяем настоящим запросом.
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let req = format!("GET /json/version HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    matches!(s.read(&mut buf), Ok(n) if n > 0)
}
