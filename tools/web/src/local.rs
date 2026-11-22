//! `web launch` на этой же машине: snens и Chrome на одном компьютере.
//!
//! Остальной `launch` рассчитан на ssh до машины пользователя. Когда агент живёт
//! там же, где браузер (ноутбук с Windows 7, обычный десктоп), ssh не нужен:
//! запускаем отдельный профиль Chrome с `--remote-debugging-port` прямо отсюда.
//! Отдельный `--user-data-dir` обязателен: Chrome 136+ игнорирует порт на
//! профиле человека, а старые (109 на Win7) пускают и так — работает везде.
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Имя хоста в `launch.json` для локального запуска.
pub const HOST: &str = "local";

/// Где искать браузер: `SNENS_CHROME`, потом стандартные места.
/// Chrome, Supermium (Chromium для Win7), Chromium, в конце Edge.
pub fn find_browser() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("SNENS_CHROME").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(p);
        return if p.is_file() {
            Ok(p)
        } else {
            Err(format!("web: SNENS_CHROME={} — файла нет", p.display()))
        };
    }
    candidates()
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| {
            "web: не нашла Chrome / Supermium / Chromium / Edge в стандартных местах — \
             укажи путь в SNENS_CHROME"
                .to_string()
        })
}

#[cfg(windows)]
fn candidates() -> Vec<PathBuf> {
    let roots: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .iter()
        .filter_map(|k| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from))
        .collect();
    let rels = [
        r"Google\Chrome\Application\chrome.exe",
        r"Supermium\chrome.exe",
        r"Chromium\Application\chrome.exe",
        r"Microsoft\Edge\Application\msedge.exe",
    ];
    let mut out = Vec::new();
    for rel in rels {
        for r in &roots {
            out.push(r.join(rel));
        }
    }
    out
}

#[cfg(not(windows))]
fn candidates() -> Vec<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let names = [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "microsoft-edge",
    ];
    let mut out = Vec::new();
    for n in names {
        for d in std::env::split_paths(&path) {
            out.push(d.join(n));
        }
    }
    out.push(PathBuf::from(
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ));
    out
}

/// Профиль агента по умолчанию — отдельный от профиля человека.
pub fn default_profile() -> PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .or_else(|| std::env::var_os("APPDATA"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("snens").join("chrome-profile")
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".local/share/snens/web-profile")
    }
}

pub fn port_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Первый свободный порт с `from` (по два: соседний порт Chrome не трогаем).
pub fn pick_port(from: u16) -> Result<u16, String> {
    (0..20u16)
        .map(|i| from.saturating_add(i * 2))
        .find(|p| port_free(*p))
        .ok_or_else(|| format!("web: нет свободного порта с {from}"))
}

/// Отвечает ли на порту CDP браузера.
pub fn alive(port: u16) -> bool {
    crate::Endpoint::direct(port)
        .get("/json/version")
        .map(|v| v.contains("Browser"))
        .unwrap_or(false)
}

/// Порт, который уже открытый браузер с этим профилем записал в
/// `DevToolsActivePort` (первая строка). Нужен, когда запись о запуске
/// потерялась: второй запуск с тем же профилем молча отдаёт окно первому
/// процессу, и новый порт так и не поднимается.
pub fn running_port(profile: &std::path::Path) -> Option<u16> {
    let text = std::fs::read_to_string(profile.join("DevToolsActivePort")).ok()?;
    parse_active_port(&text).filter(|p| alive(*p))
}

fn parse_active_port(text: &str) -> Option<u16> {
    text.lines().next()?.trim().parse().ok().filter(|p| *p != 0)
}

/// Запустить браузер с отладкой и дождаться, пока CDP ответит.
pub fn start(exe: &std::path::Path, port: u16, profile: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(profile)
        .map_err(|e| format!("web: профиль {}: {e}", profile.display()))?;
    let mut cmd = Command::new(exe);
    cmd.arg(format!("--user-data-dir={}", profile.display()))
        .arg(format!("--remote-debugging-port={port}"))
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--window-size=1280,800",
            "about:blank",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut cmd);
    cmd.spawn()
        .map_err(|e| format!("web: не запустить {}: {e}", exe.display()))?;
    let until = Instant::now() + Duration::from_secs(30);
    while Instant::now() < until {
        if alive(port) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    Err(format!(
        "web: {} не поднял отладку на порту {port} за 30 с. Если этот профиль уже открыт \
         другим окном без отладки — закрой его и повтори",
        exe.display()
    ))
}

/// Браузер не должен умереть вместе с тулом: отдельная группа процессов.
#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

#[cfg(not(any(windows, unix)))]
fn detach(_cmd: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_a_free_port_and_skips_a_busy_one() {
        let busy = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let p = busy.local_addr().unwrap().port();
        assert!(!port_free(p));
        let got = pick_port(p).unwrap();
        assert_ne!(got, p);
        assert!(port_free(got));
    }

    #[test]
    fn active_port_file() {
        assert_eq!(parse_active_port("9222
/devtools/browser/abc
"), Some(9222));
        assert_eq!(parse_active_port("0
"), None);
        assert_eq!(parse_active_port("мусор"), None);
        assert_eq!(parse_active_port(""), None);
    }

    #[test]
    fn default_profile_is_not_the_users_profile() {
        let p = default_profile();
        let s = p.to_string_lossy().to_lowercase();
        assert!(s.contains("snens"), "{s}");
        assert!(!s.contains("user data"), "{s}");
    }
}
