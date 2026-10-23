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

/// Запустить powershell-скрипт на той стороне.
fn powershell(host: &str, script: &str) -> Result<String, String> {
    ssh(
        host,
        format!(
            "powershell -NoProfile -EncodedCommand {}",
            ps_encoded(script)
        ),
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
        Err(e)
            if e.contains("Permission denied")
                || e.contains("Connection refused")
                || e.contains("timed out") =>
        {
            return Err(format!("ssh до {host} не работает: {e}"));
        }
        Err(_) => {}
    }
    if ssh(host, "uname -s")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        return Ok("unix");
    }
    Err(format!(
        "не понял, что за ОС на {host}: ни `cmd /c echo`, ни `uname` не ответили"
    ))
}

/// Наш ли Chrome слушает порт: смотрим командную строку владельца и ищем в ней
/// каталог профиля агента. Нужно, чтобы `launch` переиспользовал свой браузер, а
/// не прилипал к чужому и не плодил профили.
pub fn ours_on_port(host: &str, port: u16) -> Result<bool, String> {
    if remote_os(host)? == "windows" {
        let out = powershell(
            host,
            &format!(
                "$p = (Get-NetTCPConnection -LocalPort {port} -State Listen -ErrorAction SilentlyContinue | Select-Object -First 1).OwningProcess; \
                 if (-not $p) {{ 'no' }} else {{ $cl = (Get-CimInstance Win32_Process -Filter \"ProcessId=$p\").CommandLine; \
                 if ($cl -like '*snens-chrome*') {{ 'ours' }} else {{ 'foreign' }} }}"
            ),
        )?;
        Ok(out.contains("ours"))
    } else {
        let out = ssh(
            host,
            format!(
                "sh -c 'for p in $(pgrep -f \"remote-debugging-port={port}\"); do tr \"\\0\" \" \" < /proc/$p/cmdline | grep -q snens-chrome && echo ours && exit; done; echo no'"
            ),
        )?;
        Ok(out.contains("ours"))
    }
}

/// Свободен ли порт на той стороне. Спрашиваем до запуска: если порт занят,
/// Chrome молча прилипнет к уже запущенному процессу с ЧУЖИМ профилем (проверено:
/// так и вышло на 9222 с профилем человека), и отладка будет не туда.
pub fn port_free(host: &str, port: u16) -> Result<bool, String> {
    if remote_os(host)? == "windows" {
        let out = powershell(
            host,
            &format!(
                "if (Get-NetTCPConnection -LocalPort {port} -State Listen -ErrorAction SilentlyContinue) {{ 'busy' }} else {{ 'free' }}"
            ),
        )?;
        Ok(out.contains("free"))
    } else {
        let out = ssh(
            host,
            format!("sh -c 'command -v ss >/dev/null && ss -ltn | grep -q \":{port} \" && echo busy || echo free'"),
        )?;
        Ok(out.contains("free"))
    }
}

/// Первый свободный порт начиная с `from` (не больше десяти попыток).
pub fn pick_port(host: &str, from: u16) -> Result<u16, String> {
    for p in from..from.saturating_add(10) {
        if port_free(host, p)? {
            return Ok(p);
        }
    }
    Err(format!(
        "на {host} заняты порты {from}…{}: освободи один или укажи --port",
        from.saturating_add(9)
    ))
}

/// Запуск в интерактивной сессии пользователя, чтобы окно было видно на его
/// экране. Из ssh процесс попадает в сессию 0 (служебную, без экрана) — там у
/// Chrome нет ни окна, ни GPU-контекста. `schtasks /IT` запускает задачу в
/// сессии вошедшего пользователя, и окно появляется на его мониторе.
pub fn start_visible(host: &str, port: u16, profile: Option<&str>) -> Result<(), String> {
    if remote_os(host)? != "windows" {
        return Err(
            "видимое окно сейчас поддержано на Windows (через schtasks /IT). На unix запускай              Chrome внутри своей X/Wayland-сессии — там окно видно по определению"
                .into(),
        );
    }
    let prof = match profile {
        Some(p) => p.to_string(),
        None => r"C:\Users\%USERNAME%\.snens-chrome".to_string(),
    };
    let prof = prof.replace("%USERNAME%", "$env:USERNAME");
    // Обёртка отдельным файлом: путь к chrome.exe с пробелами не проходит в /TR.
    let wrapper = format!(
        r#"$prof = "{prof}"
if ($prof -like '*$env:USERNAME*') {{ $prof = $prof.Replace('$env:USERNAME', $env:USERNAME) }}
$exe = @('C:\Program Files\Google\Chrome\Application\chrome.exe','C:\Program Files (x86)\Google\Chrome\Application\chrome.exe') | Where-Object {{ Test-Path $_ }} | Select-Object -First 1
if (-not $exe) {{ exit 2 }}
$log = Join-Path $env:USERPROFILE 'snens-chrome-visible.out'
"=== " + (Get-Date -Format o) | Set-Content -Encoding ASCII $log
"profile=$prof port={port}" | Add-Content -Encoding ASCII $log
"existing: " + ((Get-CimInstance Win32_Process -Filter "Name='chrome.exe'" | Where-Object {{ $_.CommandLine -match 'snens-chrome' -and $_.CommandLine -notmatch '--type=' }} | Measure-Object).Count) | Add-Content -Encoding ASCII $log
& $exe "--user-data-dir=$prof" --remote-debugging-port={port} --no-first-run --no-default-browser-check --window-size=1440,900 about:blank
"exit=$LASTEXITCODE" | Add-Content -Encoding ASCII $log
"#
    );
    write_remote_file(host, "snens-chrome-visible.ps1", &wrapper)?;
    let task = "snens-chrome-visible";
    let tr = r"powershell -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File C:\Users\%USERNAME%\snens-chrome-visible.ps1";
    let _ = ssh(host, format!("schtasks /Delete /TN {task} /F"));
    ssh(
        host,
        format!(r#"schtasks /Create /TN {task} /TR "{tr}" /SC ONCE /ST 00:00 /IT /F"#),
    )?;
    ssh(host, format!("schtasks /Run /TN {task}"))?;
    wait_port(host, port, Duration::from_secs(25))?;
    Ok(())
}

/// Положить файл на ту сторону через stdin ssh: кириллица и кавычки в аргументах
/// ломаются, а через stdin доезжает как есть.
fn write_remote_file(host: &str, name: &str, content: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg(host)
        .arg(format!(r"cmd /c more > C:\Users\%USERNAME%\{}", name))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ssh {host}: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // Windows-переводы строк: powershell их ждёт.
        let crlf = content.replace("\r\n", "\n").replace('\n', "\r\n");
        stdin
            .write_all(crlf.as_bytes())
            .map_err(|e| format!("stdin ssh: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("ssh {host}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "не смог записать {name} на {host}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Поднять окно агентского Chrome на экран пользователя (в его сессии).
pub fn show_window(host: &str) -> Result<(), String> {
    if remote_os(host)? != "windows" {
        return Err("показ окна сейчас поддержан на Windows".into());
    }
    let script = r#"Add-Type -Namespace W -Name U -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
[DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
[DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
[DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, System.Text.StringBuilder s, int n);
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
public delegate bool EnumProc(IntPtr h, IntPtr p);
'@
$found = $false
$cb = [W.U+EnumProc]{
  param($h, $p)
  $sb = New-Object System.Text.StringBuilder 512
  [void][W.U]::GetWindowText($h, $sb, 512)
  if ($sb.ToString() -like '*Google Chrome*') {
    [void][W.U]::ShowWindow($h, 9)
    [void][W.U]::BringWindowToTop($h)
    [void][W.U]::SetForegroundWindow($h)
    $script:found = $true
  }
  return $true
}
[void][W.U]::EnumWindows($cb, [IntPtr]::Zero)
if ($found) { 'shown' } else { 'no-window' }
"#;
    write_remote_file(host, "snens-chrome-show.ps1", script)?;
    let task = "snens-chrome-show";
    let tr = r"powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\%USERNAME%\snens-chrome-show.ps1";
    let _ = ssh(host, format!("schtasks /Delete /TN {task} /F"));
    ssh(
        host,
        format!(r#"schtasks /Create /TN {task} /TR "{tr}" /SC ONCE /ST 00:00 /IT /F"#),
    )?;
    ssh(host, format!("schtasks /Run /TN {task}"))?;
    Ok(())
}

/// Запустить отдельный профиль Chrome на той стороне и дождаться порта CDP.
/// `profile` — каталог профиля (на Windows путь вида `C:\Users\x\.snens-chrome`).
pub fn start_agent_chrome(host: &str, port: u16, profile: Option<&str>) -> Result<String, String> {
    let os = remote_os(host)?;
    match os {
        "windows" => {
            // Профиль: свой каталог на машину пользователя. Путь берём из
            // $env:USERPROFILE на той стороне — `%USERNAME%` Chrome не раскрывает
            // (проверено: создался каталог с буквальным именем).
            //
            // Склейка через конкатенацию, без интерполяции в строке: PowerShell
            // не подставляет переменные внутрь одинарных кавычек, и `$prof`
            // уезжал в командную строку Chrome буквально.
            let prof_expr = match profile {
                Some(p) => format!("'{}'", p),
                None => "(Join-Path $env:USERPROFILE '.snens-chrome')".to_string(),
            };
            let script = format!(
                r#"$exe = @('C:\Program Files\Google\Chrome\Application\chrome.exe','C:\Program Files (x86)\Google\Chrome\Application\chrome.exe') | Where-Object {{ Test-Path $_ }} | Select-Object -First 1; if (-not $exe) {{ Write-Error 'chrome.exe not found'; exit 2 }}; $prof = {prof_expr}; $cmd = '"' + $exe + '" --user-data-dir="' + $prof + '" --remote-debugging-port={port} --no-first-run --no-default-browser-check --window-size=1440,900 about:blank'; $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{{ CommandLine = $cmd }}; if ($r.ReturnValue -ne 0) {{ Write-Error "Create returned $($r.ReturnValue)"; exit 3 }}; 'pid=' + $r.ProcessId"#
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
            let diag = ssh(
                host,
                "cmd /c type C:\\Users\\%USERNAME%\\snens-chrome-visible.out 2>nul",
            )
            .unwrap_or_default();
            return Err(format!(
                "Chrome на {host} не поднял отладку на порту {port} за {} с.{}\n\
                 Частые причины: порт занят другим Chrome (тогда профиль агента не свой), \
                 chrome.exe не найден в стандартном месте, на unix не установлен google-chrome.\n\
                 Уже запущенные агентские Chrome: `web launch` без --port переиспользует свой.",
                budget.as_secs(),
                if diag.trim().is_empty() {
                    String::new()
                } else {
                    format!("\nвывод запуска:\n{}", diag.trim())
                }
            ));
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}
