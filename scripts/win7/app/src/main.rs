//! snens-app — приложение snens для Windows 7.
//!
//! Настоящее окно snens (desktop/, Tauri) держится на движке WebView2, а на
//! Windows 7 он кончился на версии 109 и с текущей сборкой окна не заводится.
//! Тот же движок Chromium 109 есть в Chrome / Supermium / Edge для Win7: им и
//! рисуем. Браузер запускается в режиме приложения (`--app`): отдельное окно
//! без адресной строки и вкладок, со своим профилем, — для человека это
//! программа snens, а не сайт.
//!
//! Что делает запуск:
//! 1. первый раз переносит настройки из `settings\` рядом с exe в %APPDATA%\snens;
//! 2. поднимает ядро `snens serve` без консольного окна, если оно ещё не
//!    работает (журнал — %APPDATA%\snens\serve.log);
//! 3. берёт у ядра код подключения и открывает окно уже с ним (`#pair=…`),
//!    так что вводить ничего не надо;
//! 4. ждёт, пока окно закроют, и гасит ядро, если сама его поднимала: на
//!    одном каталоге данных может работать только одно ядро, и брошенное
//!    ядро не дало бы запустить настоящее окно snens (snens-desktop.exe).
//!    Если окно уже было открыто (браузер передал вызов ему и сразу вышел,
//!    а профиль окна занят), ядро не трогаем — им пользуется то окно.
//!
//! Ошибки — окном сообщения: консоли у этой программы нет.
#![windows_subsystem = "windows"]

use std::{
    env,
    ffi::{c_void, OsStr},
    fs,
    net::{SocketAddr, TcpStream},
    os::windows::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const PORT: u16 = 7420;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CORE_START_TIMEOUT: Duration = Duration::from_secs(45);

fn main() {
    if let Err(e) = run() {
        message_box(&e);
    }
}

fn run() -> Result<(), String> {
    let here = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .ok_or("не удалось узнать, где лежит программа")?;
    let core = here.join("snens.exe");
    if !core.is_file() {
        return Err(format!("Нет ядра: {}\nПапку snens нужно держать целиком.", core.display()));
    }
    let data = config_dir()?;
    first_run_settings(&here, &data)?;
    if !data.join("config.toml").is_file() {
        return Err(format!(
            "Нет настроек: {}\\config.toml\nСмотри README.txt, раздел «Первый запуск».",
            data.display()
        ));
    }

    let addr = SocketAddr::from(([127, 0, 0, 1], PORT));
    let own_core = if listening(addr) { None } else { Some(start_core(&core, &data, addr)?) };

    let base = format!("http://127.0.0.1:{PORT}/");
    let url = match pair_code(&core) {
        Some(code) => format!("{base}#pair={code}"),
        None => base,
    };
    let profile = window_profile()?;
    let mut window = match open_window(&url, &profile) {
        Ok(w) => w,
        Err(e) => {
            // Окна нет — своё ядро гасим, иначе оно осталось бы невидимым
            // (без консоли) и держало бы порт и каталог данных.
            stop(own_core);
            return Err(e);
        }
    };
    let _ = window.wait();
    // Браузер вышел. Если профиль окна всё ещё занят — это другой экземпляр,
    // которому наш браузер отдал окно: ядро нужно ему, не гасим.
    if !profile_in_use(&profile) {
        stop(own_core);
    }
    Ok(())
}

fn stop(core: Option<Child>) {
    if let Some(mut core) = core {
        let _ = core.kill();
        let _ = core.wait();
    }
}

/// Свой профиль окна: подключение к ядру и настройки окна живут отдельно от
/// обычного браузера, а окно не смешивается с его вкладками.
fn window_profile() -> Result<PathBuf, String> {
    let profile = env::var_os("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("snens").join("window"))
        .ok_or("не задана переменная LOCALAPPDATA")?;
    let _ = fs::create_dir_all(&profile);
    Ok(profile)
}

/// Профилем пользуется работающий браузер: Chromium держит `lockfile` в
/// каталоге профиля открытым без общего доступа, пока жив.
fn profile_in_use(profile: &Path) -> bool {
    let lock = profile.join("lockfile");
    lock.exists() && fs::OpenOptions::new().write(true).open(&lock).is_err()
}

/// Каталог настроек ядра — тот же, что ищет `snens`: SNENS_CONFIG_DIR или %APPDATA%\snens.
fn config_dir() -> Result<PathBuf, String> {
    if let Some(d) = env::var_os("SNENS_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(d));
    }
    env::var_os("APPDATA")
        .map(|a| PathBuf::from(a).join("snens"))
        .ok_or_else(|| "не задана переменная APPDATA".into())
}

/// Первый запуск: config.toml и .env (ключ) лежат в settings\ рядом с exe.
/// Существующие настройки не трогаем.
fn first_run_settings(here: &Path, data: &Path) -> Result<(), String> {
    let src = here.join("settings");
    if data.join("config.toml").is_file() || !src.join("config.toml").is_file() {
        return Ok(());
    }
    fs::create_dir_all(data).map_err(|e| format!("не создать {}: {e}", data.display()))?;
    for name in ["config.toml", ".env"] {
        let from = src.join(name);
        if from.is_file() {
            fs::copy(&from, data.join(name))
                .map_err(|e| format!("не скопировать {}: {e}", from.display()))?;
        }
    }
    Ok(())
}

fn listening(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Ядро без консольного окна; рабочий каталог — домашняя папка (там агент и
/// работает с файлами, а не в C:\Windows\system32 или в папке программы).
fn start_core(core: &Path, data: &Path, addr: SocketAddr) -> Result<Child, String> {
    let log_path = data.join("serve.log");
    let log = fs::File::create(&log_path).map_err(|e| format!("не создать {}: {e}", log_path.display()))?;
    let log2 = log.try_clone().map_err(|e| e.to_string())?;
    let home = env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| data.to_path_buf());
    let mut child = Command::new(core)
        .args(["serve", "--listen"])
        .arg(addr.to_string())
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log2)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("не удалось запустить ядро {}: {e}", core.display()))?;

    let start = Instant::now();
    while start.elapsed() < CORE_START_TIMEOUT {
        if listening(addr) {
            return Ok(child);
        }
        if let Ok(Some(st)) = child.try_wait() {
            return Err(format!(
                "Ядро snens завершилось при запуске ({st}).\n\n{}\n\nЖурнал: {}",
                log_tail(&log_path),
                log_path.display()
            ));
        }
        thread::sleep(Duration::from_millis(250));
    }
    let _ = child.kill();
    Err(format!(
        "Ядро snens не открыло порт {} за {} с.\n\n{}\n\nЖурнал: {}",
        addr.port(),
        CORE_START_TIMEOUT.as_secs(),
        log_tail(&log_path),
        log_path.display()
    ))
}

fn log_tail(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(8)..].join("\n")
}

/// Код подключения окна к ядру (`snens pair`): ищем в выводе «ddd-ddd».
/// Не вышло — окно откроется без кода и само спросит.
fn pair_code(core: &Path) -> Option<String> {
    let out = Command::new(core)
        .args(["pair", "--endpoint", &format!("http://127.0.0.1:{PORT}")])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    find_code(&String::from_utf8_lossy(&out.stdout))
}

fn find_code(text: &str) -> Option<String> {
    let b = text.as_bytes();
    (0..b.len().saturating_sub(6)).find_map(|i| {
        let w = &b[i..i + 7];
        let digits = |r: &[u8]| r.iter().all(u8::is_ascii_digit);
        let edge_ok = |j: Option<&u8>| j.map_or(true, |c| !c.is_ascii_digit());
        (digits(&w[..3]) && w[3] == b'-' && digits(&w[4..])
            && edge_ok(i.checked_sub(1).and_then(|j| b.get(j)))
            && edge_ok(b.get(i + 7)))
        .then(|| String::from_utf8_lossy(w).into_owned())
    })
}

/// Браузеры на движке Chromium, у которых есть режим приложения.
fn find_browser() -> Option<PathBuf> {
    let roots: Vec<PathBuf> = ["LOCALAPPDATA", "ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"]
        .iter()
        .filter_map(|v| env::var_os(v).map(PathBuf::from))
        .collect();
    let rel = [
        r"Supermium\chrome.exe",
        r"Supermium\Application\chrome.exe",
        r"Supermium\supermium.exe",
        r"Google\Chrome\Application\chrome.exe",
        r"Microsoft\Edge\Application\msedge.exe",
    ];
    rel.iter()
        .flat_map(|r| roots.iter().map(move |root| root.join(r)))
        .find(|p| p.is_file())
}

fn open_window(url: &str, profile: &Path) -> Result<Child, String> {
    let Some(browser) = find_browser() else {
        return Err(
            "Не найден браузер для окна snens: нужен Chrome 109, Supermium или Edge 109.\n\
             Поставьте один из них и запустите snens ещё раз."
                .into(),
        );
    };
    Command::new(&browser)
        .arg(format!("--app={url}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .args(["--no-first-run", "--no-default-browser-check", "--start-maximized"])
        .spawn()
        .map_err(|e| format!("не удалось открыть окно через {}: {e}", browser.display()))
}

fn message_box(text: &str) {
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }
    let wide = |s: &str| OsStr::new(s).encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let (t, c) = (wide(text), wide("snens"));
    const MB_ICONERROR: u32 = 0x10;
    // SAFETY: обе строки — живые нуль-терминированные UTF-16 буферы до конца вызова.
    unsafe {
        MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), MB_ICONERROR);
    }
}

#[cfg(test)]
mod tests {
    use super::find_code;

    #[test]
    fn code_from_pair_output() {
        let out = "Код пейринга: 049-770\nкод действует 5:00\nАдрес: http://127.0.0.1:7420\n";
        assert_eq!(find_code(out).as_deref(), Some("049-770"));
        assert_eq!(find_code("Адрес: http://127.0.0.1:7420"), None);
        assert_eq!(find_code("1234-5678"), None);
    }
}
