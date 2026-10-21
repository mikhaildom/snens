// Запуск отдельного профиля Chrome для агента. Смысл — автономия: с собственным
// `--user-data-dir` Chrome разрешает `--remote-debugging-port` без всякого
// подтверждения человеком (проверено на Chrome 156 под Windows), а профиль
// пользователя с тумблером в chrome://inspect требует Allow на каждую сессию —
// для ночной работы это неприемлемо.
//
// Процесс обязан пережить ssh-сессию: на Windows обычный `Start-Process` из ssh
// умирает вместе с сессией, поэтому там `Win32_Process.Create`; на unix —
// `setsid`.
use std::process::Command;
use std::time::{Duration, Instant};

/// PowerShell принимает скрипт в `-EncodedCommand` как base64 от UTF-16LE. Это
/// единственный способ передать сложный скрипт через ssh → cmd → powershell без
/// боя с экранированием кавычек: раньше `{$_}` и `'"'` разваливались в
/// CommandNotFoundException.
fn ps_encoded(script: &str) -> String {
    let mut bytes = Vec::with_capacity(script.len() * 2);
    for u in script.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    base64(&bytes)
}

fn base64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Запустить powershell-скрипт на той стороне.
fn powershell(host: &str, script: &str) -> Result<String, String> {
    ssh(
        host,
        format!("powershell -NoProfile -EncodedCommand {}", ps_encoded(script)),
    )
}

fn ssh<A: AsRef<str>>(host: &str, cmd: A) -> Result<String, String> {
    let out = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg(host)
        .arg(cmd.as_ref())
        .output()
        .map_err(|e| format!("ssh {host}: {e}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "на {host} команда не прошла: {}",
            err.trim().lines().last().unwrap_or("причина не сказана")
        ));
    }
    text = text.replace('\r', "");
    Ok(text)
}

/// Какая ОС на той стороне: `windows` или `unix`. Определяем по факту, а не по
/// догадке: пробуем cmd, которого на unix нет.
pub fn remote_os(host: &str) -> Result<&'static str, String> {
    // Порядок важен: на Windows `uname` может существовать (git-bash/WSL), а
    // `cmd /c echo` есть только там. Ошибку ssh тоже нужно отличать от «не та ОС»:
    // если ssh вообще не работает, сказать надо про ssh, а не про ОС.
    // Маркер без кавычек в ответе: Windows-OpenSSH возвращает `w"` (cmd добавляет
    // кавычку), поэтому сравниваем не «равно», а «содержит слово».
    match ssh(host, "cmd /c echo SNENS_WINDOWS") {
        Ok(s) if s.contains("SNENS_WINDOWS") => return Ok("windows"),
        Ok(_) => {}
        Err(e) if e.contains("Permission denied") || e.contains("Connection refused") || e.contains("timed out") => {
            return Err(format!("ssh до {host} не работает: {e}"));
        }
        Err(_) => {}
    }
    if ssh(host, "uname -s").map(|s| !s.trim().is_empty()).unwrap_or(false) {
        return Ok("unix");
    }
    Err(format!(
        "не понял, что за ОС на {host}: ни `cmd /c echo`, ни `uname` не ответили"
    ))
}

/// Запустить отдельный профиль Chrome на той стороне и дождаться порта CDP.
/// `profile` — каталог профиля (на Windows путь вида `C:\Users\x\.snens-chrome`).
pub fn start_agent_chrome(host: &str, port: u16, profile: Option<&str>) -> Result<String, String> {
    let os = remote_os(host)?;
    match os {
        "windows" => {
            // Путь с обратными слэшами — в raw-строке, иначе Rust съедает \P.
            let prof = profile.unwrap_or(r"C:\Users\%USERNAME%\.snens-chrome");
            let script = format!(
                r#"$exe = @('C:\Program Files\Google\Chrome\Application\chrome.exe','C:\Program Files (x86)\Google\Chrome\Application\chrome.exe') | Where-Object {{ Test-Path $_ }} | Select-Object -First 1; if (-not $exe) {{ Write-Error 'chrome.exe не найден'; exit 2 }}; $cmd = '"' + $exe + '" --user-data-dir="{prof}" --remote-debugging-port={port} --no-first-run --no-default-browser-check --window-size=1440,900 about:blank'; $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{{ CommandLine = $cmd }}; if ($r.ReturnValue -ne 0) {{ Write-Error "Create вернул $($r.ReturnValue)"; exit 3 }}; "pid=$($r.ProcessId)""#
            );
            powershell(host, &script)?;
        }
        _ => {
            let prof = profile.unwrap_or("$HOME/.snens-chrome");
            let cmd = format!(
                "sh -c 'nohup setsid \"$(command -v google-chrome || command -v google-chrome-stable || command -v chromium || command -v chromium-browser)\" \
                 --user-data-dir={prof} --remote-debugging-port={port} --no-first-run \
                 --no-default-browser-check --window-size=1440,900 about:blank >/tmp/snens-chrome.log 2>&1 &'"
            );
            ssh(host, cmd)?;
        }
    }
    // Порт поднимается не мгновенно; и на той стороне он может быть занят другим
    // Chrome — тогда наш запуск молча прилипнет к чужому профилю, поэтому ждём
    // именно /json/version с нашим портом.
    wait_port(host, port, Duration::from_secs(20))?;
    Ok(os.to_string())
}

/// Ждём, пока CDP на той стороне ответит (через ssh, без туннеля).
fn wait_port(host: &str, port: u16, budget: Duration) -> Result<(), String> {
    let os = remote_os(host)?;
    let until = Instant::now() + budget;
    loop {
        let probe = match os {
            "windows" => powershell(
                host,
                &format!(
                    "try {{ (Invoke-WebRequest -UseBasicParsing http://127.0.0.1:{port}/json/version -TimeoutSec 3).StatusCode }} catch {{ 0 }}"
                ),
            )
            .unwrap_or_default(),
            _ => ssh(
                host,
                format!("curl -s -o /dev/null -w '%{{http_code}}' --max-time 3 http://127.0.0.1:{port}/json/version"),
            )
            .unwrap_or_default(),
        };
        if probe.trim().trim_matches('"') == "200" {
            return Ok(());
        }
        if Instant::now() >= until {
            return Err(format!(
                "Chrome на {host} не поднял отладку на порту {port} за {} с.\n\
                 Частые причины: порт занят другим Chrome (тогда профиль агента не свой), \
                 chrome.exe не найден в стандартном месте, на unix не установлен google-chrome.",
                budget.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}
