// Туннель к агенту на машине пользователя + его запуск/остановка.
//
// Агент обязан жить в интерактивной сессии пользователя: из ssh мы попадаем в
// сессию 0, где UI Automation видит НОЛЬ окон (проверено на живой машине).
// Поэтому на Windows запуск идёт через `schtasks /IT`, а не напрямую.
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::Args;

/// Машина, на которой живёт агент: `--tunnel` или последний использованный.
pub fn host(args: &Args) -> Result<String, String> {
    if let Some(h) = args.flag("tunnel") {
        std::env::set_var("SNENS_DESK_HOST", h);
        return Ok(h.to_string());
    }
    if let Ok(h) = std::env::var("SNENS_DESK_HOST") {
        if !h.is_empty() {
            return Ok(h);
        }
    }
    Err("desk: нужен --tunnel user@machine (где живёт агент)".into())
}

fn ssh(host: &str, cmd: &str) -> Result<String, String> {
    let out = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg(host)
        .arg(cmd)
        .output()
        .map_err(|e| format!("ssh {host}: {e}"))?;
    if !out.status.success() {
        // Не «последняя строка stderr»: на Windows-ssh причина часто в stdout
        // (cmd пишет ошибки туда), а последняя строка бывает пустой.
        let err = String::from_utf8_lossy(&out.stderr);
        let so = String::from_utf8_lossy(&out.stdout);
        let text = format!("{err}\n{so}\n{cmd}");
        let brief: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .take(6)
            .collect();
        return Err(format!(
            "на {host} команда не прошла (код {:?}): {}",
            out.status.code(),
            brief.join(" | ")
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).replace('\r', ""))
}

/// PowerShell принимает скрипт как base64 от UTF-16LE. Это единственный способ
/// провести сложный скрипт через ssh → cmd → powershell: экранирование кавычек и
/// `$_` в аргументах ломается на каждом втором вызове.
fn ps_encoded(script: &str) -> String {
    let mut bytes = Vec::with_capacity(script.len() * 2);
    for u in script.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    base64(&bytes)
}

fn powershell(host: &str, script: &str) -> Result<String, String> {
    ssh(
        host,
        &format!(
            "powershell -NoProfile -EncodedCommand {}",
            ps_encoded(script)
        ),
    )
}

fn remote_os(host: &str) -> Result<&'static str, String> {
    match ssh(host, "cmd /c echo SNENS_WINDOWS") {
        Ok(s) if s.contains("SNENS_WINDOWS") => Ok("windows"),
        Ok(_) => Ok("unix"),
        Err(e) => Err(format!("ssh до {host} не работает: {e}")),
    }
}

fn free_port() -> Result<u16, String> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("порт: {e}"))?;
    Ok(l.local_addr().map_err(|e| format!("порт: {e}"))?.port())
}

/// Поднять (или переиспользовать) туннель к порту агента. Возвращает локальный порт.
pub fn ensure(host: &str, remote_port: u16) -> Result<u16, String> {
    if let Some((h, local, rem, pid)) = load_tunnel() {
        if h == host && rem == remote_port && alive(pid) && port_open(local) {
            return Ok(local);
        }
        // Старый туннель к другому порту (или к той же машине после смены порта)
        // закрываем: иначе процессы копятся и держат порты.
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    let local = free_port()?;
    let mut cmd = Command::new("ssh");
    cmd.arg("-N")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("ServerAliveInterval=20")
        .arg("-L")
        .arg(format!("{local}:127.0.0.1:{remote_port}"))
        .arg(host)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|e| format!("ssh: {e}"))?;
    let pid = child.id();
    let until = Instant::now() + Duration::from_secs(12);
    loop {
        if port_open(local) {
            save_tunnel(host, local, remote_port, pid);
            return Ok(local);
        }
        if !alive(pid) {
            return Err(format!(
                "ssh-туннель к {host}:{remote_port} не поднялся (проверь `ssh {host} true`)"
            ));
        }
        if Instant::now() >= until {
            let _ = Command::new("kill").arg(pid.to_string()).status();
            return Err(format!(
                "ssh-туннель к {host}:{remote_port} не открыл порт за 12 с"
            ));
        }
        std::thread::sleep(Duration::from_millis(120));
    }
}

fn port_open(port: u16) -> bool {
    let addr = format!("127.0.0.1:{port}");
    let Ok(mut it) = addr.to_socket_addrs() else {
        return false;
    };
    let Some(sa) = it.next() else { return false };
    TcpStream::connect_timeout(&sa, Duration::from_millis(400)).is_ok()
}

fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

fn state_dir() -> std::path::PathBuf {
    std::env::var("SNENS_DESK_STATE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
                .join(".local/state/snens/desk")
        })
}

fn save_tunnel(host: &str, local: u16, remote: u16, pid: u32) {
    let d = state_dir();
    let _ = std::fs::create_dir_all(&d);
    let v = serde_json::json!({"host": host, "local": local, "remote": remote, "pid": pid});
    let _ = std::fs::write(d.join("tunnel.json"), v.to_string());
}

fn load_tunnel() -> Option<(String, u16, u16, u32)> {
    let text = std::fs::read_to_string(state_dir().join("tunnel.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some((
        v.get("host")?.as_str()?.to_string(),
        v.get("local")?.as_u64()? as u16,
        v.get("remote")?.as_u64()? as u16,
        v.get("pid")?.as_u64()? as u32,
    ))
}

/// Запустить агента на той стороне. Windows — через schtasks /IT (сессия
/// пользователя), unix — systemd --user или nohup внутри своей сессии.
pub fn start_agent(host: &str, port: u16) -> Result<(), String> {
    let os = remote_os(host)?;
    match os {
        "windows" => {
            // Старый агент (другая версия скрипта) держит порт и отвечает старым
            // кодом — перезапускаем всегда, иначе ловим загадочные «EOF» и
            // «0 окон» от устаревшего процесса.
            stop_agent(host).ok();
            // Ждём, пока порт освободится по-настоящему. Проверять надо именно
            // LISTENING: `netstat | findstr :9227` ловит и TIME_WAIT от прошлых
            // соединений, и тогда мы решали «уже слушает», не запустив агента.
            let until = Instant::now() + Duration::from_secs(10);
            while listening(host, port).unwrap_or(false) {
                if Instant::now() >= until {
                    return Err(format!(
                        "порт {port} на {host} занят другим процессом: освободи его или возьми --port"
                    ));
                }
                std::thread::sleep(Duration::from_millis(300));
            }
            // Скрипт агента кладём через stdin (аргументы ssh ломают кавычки).
            put_file(
                host,
                "snens-desk-agent.ps1",
                include_str!("../agent/desk-agent.ps1"),
            )?;
            let task = "snens-desk-agent";
            let tr = format!(
                r"powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\%USERNAME%\snens-desk-agent.ps1 -Port {port}"
            );
            let _ = ssh(host, &format!("schtasks /Delete /TN {task} /F"));
            ssh(
                host,
                &format!(r#"schtasks /Create /TN {task} /TR "{tr}" /SC ONCE /ST 00:00 /IT /F"#),
            )?;
            ssh(host, &format!("schtasks /Run /TN {task}"))?;
            wait_agent(host, port, Duration::from_secs(25))
        }
        _ => Err(
            "агент для Linux ещё не написан (это следующий шаг: AT-SPI по D-Bus и systemd --user)"
                .into(),
        ),
    }
}

/// Положить текстовый файл на ту сторону.
///
/// Только base64 + декодирование на месте: `more`/`type` перекодируют поток в
/// ANSI, и кириллица в скрипте превращается в мусор — PowerShell тогда падает на
/// случайной кавычке посреди строки. Base64 переживает любую кодировку, а файл
/// пишем UTF-8 с BOM, чтобы PowerShell 5.1 читал его правильно.
/// Слушает ли кто-то порт на той стороне (именно LISTENING, не TIME_WAIT).
fn listening(host: &str, port: u16) -> Result<bool, String> {
    if remote_os(host)? == "windows" {
        let out = powershell(
            host,
            &format!(
                "if (Get-NetTCPConnection -LocalPort {port} -State Listen -ErrorAction SilentlyContinue) {{ 'LISTENING' }} else {{ 'free' }}"
            ),
        )?;
        Ok(out.contains("LISTENING"))
    } else {
        let out = ssh(
            host,
            &format!(
                "sh -c 'ss -ltn 2>/dev/null | grep -q \":{port} \" && echo LISTENING || echo free'"
            ),
        )?;
        Ok(out.contains("LISTENING"))
    }
}

fn put_file(host: &str, name: &str, content: &str) -> Result<(), String> {
    // Через `cmd /c echo ... >> file` содержимое молча теряется (проверено:
    // файл доезжал короче исходного на ~900 байт), а через `more` ломается
    // кодировка. scp копирует байт в байт, поэтому кладём файл им.
    //
    // BOM обязателен: PowerShell 5.1 без него читает .ps1 как ANSI, кириллица
    // превращается в мусор и скрипт падает на случайной кавычке.
    let mut body = vec![0xEF, 0xBB, 0xBF];
    body.extend_from_slice(content.as_bytes());
    let tmpdir = std::env::temp_dir();
    let local = tmpdir.join(format!("snens-upload-{}-{}", std::process::id(), name));
    std::fs::write(&local, &body).map_err(|e| format!("{}: {e}", local.display()))?;
    // `%USERNAME%` scp не раскрывает (в отличие от cmd): путь нужен настоящий.
    let home = ssh(host, "cmd /c echo %USERPROFILE%")
        .map(|s| s.trim().trim_end_matches('"').to_string())
        .unwrap_or_default();
    let target = if home.is_empty() {
        return Err(format!("не смог узнать домашний каталог на {host}"));
    } else {
        format!("{}/{name}", home.replace('\\', "/"))
    };
    let out = Command::new("scp")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-q")
        .arg(&local)
        .arg(format!("{host}:{target}"))
        .output()
        .map_err(|e| format!("scp: {e}"))?;
    let _ = std::fs::remove_file(&local);
    if !out.status.success() {
        return Err(format!(
            "scp {name} на {host} не прошёл: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn base64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Ждём, пока агент ответит на ping через туннель.
fn wait_agent(host: &str, port: u16, budget: Duration) -> Result<(), String> {
    let until = Instant::now() + budget;
    let mut last;
    loop {
        match ensure(host, port) {
            Ok(local) => {
                if ping(local).is_ok() {
                    return Ok(());
                }
                last = "порт открыт, но агент не отвечает на ping";
            }
            Err(e) => last = Box::leak(e.into_boxed_str()),
        }
        if Instant::now() >= until {
            let diag = ssh(host, "cmd /c type C:\\Users\\%USERNAME%\\snens-desk.out")
                .unwrap_or_else(|_| "нет snens-desk.out".into());
            return Err(format!(
                "агент на {host} не ответил за {} с: {last}\nвывод агента:\n{}",
                budget.as_secs(),
                diag.trim()
            ));
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

fn ping(local_port: u16) -> Result<(), String> {
    let addr = format!("127.0.0.1:{local_port}");
    let sa = addr
        .to_socket_addrs()
        .map_err(|e| format!("{addr}: {e}"))?
        .next()
        .ok_or_else(|| format!("{addr}: не разрешился"))?;
    let mut s = TcpStream::connect_timeout(&sa, Duration::from_secs(3))
        .map_err(|e| format!("{addr}: {e}"))?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    s.write_all(b"{\"cmd\":\"ping\"}\n")
        .map_err(|e| format!("{e}"))?;
    let mut buf = Vec::new();
    let mut one = [0u8; 1];
    loop {
        match std::io::Read::read(&mut s, &mut one) {
            Ok(0) => break,
            Ok(_) => {
                // Агент отвечает строкой с CRLF (PowerShell WriteLine) — ждать
                // только `\n` можно, но собирать надо без `\r`, иначе сравнение
                // с "\"ok\":true" промахивается.
                if one[0] == b'\n' {
                    break;
                }
                buf.push(one[0]);
                if buf.len() > 4096 {
                    break;
                }
            }
            Err(e) => return Err(format!("{e}")),
        }
    }
    let text = String::from_utf8_lossy(&buf).replace('\r', "");
    if text.contains("\"ok\":true") {
        Ok(())
    } else {
        Err(format!("ответ: {}", text.trim()))
    }
}

pub fn stop_agent(host: &str) -> Result<(), String> {
    powershell(
        host,
        "Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'powershell.exe' -and $_.CommandLine -like '*desk-agent*' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }; 'stopped'",
    )?;
    Ok(())
}
