//! Запуск своего ядра и его остановка.
//!
//! Ядро — sidecar: бинарник `snens` лежит рядом с окном (так его кладёт
//! установщик). Порт и путь к токену не угадываются: ядро само выбирает
//! свободный порт (`--listen 127.0.0.1:0`) и пишет фактический адрес в
//! ready-файл, который оболочка передала через `SNENS_READY_FILE`.

use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

use tauri::Manager;

use crate::connection::Connection;

/// Сколько ждём адрес от ядра и его ответ на `/v1/model`. Первый запуск на
/// холодную машину с антивирусом бывает медленным, но 30 с — уже «что-то не
/// так», и человеку честнее показать ошибку, чем вечный спиннер.
const READY_BUDGET: Duration = Duration::from_secs(30);
const PING_BUDGET: Duration = Duration::from_secs(20);

/// Запущенное ядро и то, что окно о нём знает.
pub struct Daemon {
    child: Mutex<Option<Child>>,
    /// Привязка жизни ядра к жизни окна (Windows: job object). Из `Daemon`
    /// никогда не достаётся — держится только ради ручки.
    job: Mutex<Option<crate::job::Job>>,
    pub connection: Mutex<Option<Connection>>,
    /// Окно работает с чужим ядром: своего процесса нет, `child` пуст.
    pub remote: Mutex<bool>,
}

impl Daemon {
    pub fn new() -> Self {
        Self {
            child: Mutex::new(None),
            job: Mutex::new(None),
            connection: Mutex::new(None),
            remote: Mutex::new(false),
        }
    }

    /// Погасить своё ядро. Зовётся перед выходом приложения: иначе процесс
    /// переживёт окно и следующая установка не сможет перезаписать файлы.
    ///
    /// Отдельная переменная, а не `if let Ok(...)`: временный `Result` от
    /// `lock()` живёт до конца выражения и держит заимствование `state`, а
    /// `state` умирает раньше (E0597 — ловится только на Windows-сборке).
    pub fn stop(&self) {
        let stopped = self.child.lock().map(|mut guard| guard.take()).unwrap_or(None);
        if let Some(mut child) = stopped {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Ручку job закрываем ПОСЛЕ kill+wait: закрытие само по себе убивает
        // содержимое, но порядок делает намерение явным.
        if let Some(job) = self.job.lock().map(|mut g| g.take()).unwrap_or(None) {
            crate::job::close(job);
        }
    }
}

/// Где лежит sidecar: рядом с exe (так его кладёт установщик).
fn side(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    let dir = exe.parent().unwrap_or(Path::new("."));
    dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

/// Запустить своё ядро и дождаться готовности. Возвращает адрес и токен.
pub fn start(app: &tauri::AppHandle) -> Result<Connection, Box<dyn std::error::Error>> {
    let bin = side("snens");
    if !bin.is_file() {
        return Err(format!("нет ядра рядом с окном: {}", bin.display()).into());
    }
    // ready-файл уникален на процесс окна: два окна (или окно и TUI) не должны
    // читать адрес друг друга.
    let ready_path = std::env::temp_dir().join(format!(
        "snens-desktop-ready-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&ready_path);

    let mut cmd = Command::new(&bin);
    cmd.arg("serve")
        .arg("--listen")
        .arg("127.0.0.1:0")
        .env("SNENS_READY_FILE", &ready_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Окно без консоли. `CREATE_NO_WINDOW` обязателен даже при
    // `windows_subsystem = "windows"` у оболочки: атрибут окна гасит консоль
    // ТОЛЬКО у нашего exe, а дочерний консольный процесс получил бы свою —
    // чёрное пустое окно поверх интерфейса (владелец видел именно это).
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let child = cmd.spawn()?;
    // Привязка к job — ДО того, как что-либо может упасть ниже: если setup
    // упадёт или окно выгрузят, ядро умрёт вместе с процессом и не оставит
    // за собой лок `surface-api/server.lock` (из-за него следующий запуск
    // молча не поднимался).
    let job = crate::job::attach(&child);
    {
        let state = app.state::<Daemon>();
        *state.child.lock().map_err(|_| "состояние отравлено")? = Some(child);
        *state.job.lock().map_err(|_| "состояние отравлено")? = job;
    }

    let ready = wait_ready_file(&ready_path, READY_BUDGET)
        .ok_or("ядро не сообщило адрес за 30 с")?;
    let _ = std::fs::remove_file(&ready_path);
    let base = ready
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or("ядро не сообщило url")?
        .to_string();
    let token_file = ready
        .get("token_file")
        .and_then(|v| v.as_str())
        .ok_or("ядро не сообщило token_file")?;
    let token = read_token(Path::new(token_file)).ok_or("нет api.token")?;

    // Адрес получен, но HTTP-сервер мог ещё не подняться: окно на пустоту
    // открывать незачем.
    if !wait_ready(&base, &token, PING_BUDGET) {
        return Err("ядро не ответило на /v1/model за 20 с".into());
    }
    Ok(Connection {
        base_url: base,
        token,
    })
}

/// Дождаться ready-файла от ядра и разобрать его.
fn wait_ready_file(path: &Path, budget: Duration) -> Option<serde_json::Value> {
    let until = Instant::now() + budget;
    while Instant::now() < until {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                return Some(v);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// Прочитать токен инстанса; если его нет — ядро создаст при старте, поэтому
/// сначала запускаем ядро, потом читаем.
fn read_token(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Ждём, пока ядро ответит на /v1/model: окно не должно открываться на пустоту.
fn wait_ready(base: &str, token: &str, budget: Duration) -> bool {
    let until = Instant::now() + budget;
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    while Instant::now() < until {
        let ok = client
            .get(format!("{base}/v1/model"))
            .bearer_auth(token)
            .send()
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if ok {
            return true;
        }
        std::thread::sleep(Duration::from_millis(120));
    }
    false
}
