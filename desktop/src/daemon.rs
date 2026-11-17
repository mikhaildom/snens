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

/// Приватный каталог для ready-файла.
///
/// Путь вида `/tmp/snens-desktop-ready-<pid>.json` предсказуем, а `/tmp` общий
/// для всех пользователей: чужой файл по этому пути подсовывал бы свой `url`
/// (окно подключилось бы к чужому серверу и отдало туда токен инстанса) и свой
/// `token_file` (адрес любого файла, содержимое которого ушло бы в окно как
/// токен). Каталог создаём с 0700 и с непредсказуемым именем; `create_dir`
/// атомарен, так что предсозданный чужой каталог не подхватится.
fn ready_dir() -> Result<PathBuf, String> {
    let base = std::env::temp_dir();
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut last = String::new();
    for attempt in 0..8u32 {
        let dir = base.join(format!(
            "snens-desktop-{}-{seed:x}{attempt}",
            std::process::id()
        ));
        #[cfg(unix)]
        let made = {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(false)
                .mode(0o700)
                .create(&dir)
        };
        #[cfg(not(unix))]
        let made = std::fs::create_dir(&dir);
        match made {
            Ok(()) => return Ok(dir),
            // Имя занято — это либо чужой каталог, либо прошлый прогон: берём другое.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!(
        "не удалось создать каталог для ready-файла в {}: {last}",
        base.display()
    ))
}

/// Файл, куда пишется stderr ядра. Без него текст «смотри лог» отправлял в
/// пустоту: stderr шёл в `Stdio::null()`, и диагностировать старт было нечем.
fn core_log(app: &tauri::AppHandle) -> PathBuf {
    let dir = app
        .path()
        .app_log_dir()
        .unwrap_or_else(|_| std::env::temp_dir());
    let _ = std::fs::create_dir_all(&dir);
    dir.join("core.log")
}

/// Ошибочная ветка запуска: гасим уже поднятый процесс и убираем ready-каталог.
///
/// Лишний `kill` тут не забота, а необходимость: живое ядро держит
/// `surface-api/server.lock`, и следующий запуск молча не поднимется — ровно
/// та проблема, ради которой сделан job object.
fn abandon(app: &tauri::AppHandle, ready: &Path, log: &Path, what: String) -> Box<dyn std::error::Error> {
    app.state::<Daemon>().stop();
    let _ = std::fs::remove_dir_all(ready);
    format!("{what} — смотри {}", log.display()).into()
}

/// Запустить своё ядро и дождаться готовности. Возвращает адрес и токен.
pub fn start(app: &tauri::AppHandle) -> Result<Connection, Box<dyn std::error::Error>> {
    let bin = side("snens");
    if !bin.is_file() {
        return Err(format!(
            "ядро не найдено: {}. Переустанови приложение или укажи адрес в настройках",
            bin.display()
        )
        .into());
    }
    // ready-файл уникален на процесс окна: два окна (или окно и TUI) не должны
    // читать адрес друг друга. Каталог приватный — см. `ready_dir`.
    let ready_dir = match ready_dir() {
        Ok(d) => d,
        Err(e) => return Err(e.into()),
    };
    let ready_path = ready_dir.join("ready.json");
    let log_path = core_log(app);
    let log = std::fs::File::create(&log_path).ok();

    let mut cmd = Command::new(&bin);
    cmd.arg("serve")
        .arg("--listen")
        .arg("127.0.0.1:0")
        .env("SNENS_READY_FILE", &ready_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log.map(Stdio::from).unwrap_or_else(Stdio::null));
    // Окно без консоли. `CREATE_NO_WINDOW` обязателен даже при
    // `windows_subsystem = "windows"` у оболочки: атрибут окна гасит консоль
    // ТОЛЬКО у нашего exe, а дочерний консольный процесс получил бы свою —
    // чёрное пустое окно поверх интерфейса (владелец видел именно это).
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&ready_dir);
            return Err(format!("не удалось запустить {}: {e}", bin.display()).into());
        }
    };
    // Привязка к job — ДО того, как что-либо может упасть ниже: если setup
    // упадёт или окно выгрузят, ядро умрёт вместе с процессом и не оставит
    // за собой лок `surface-api/server.lock` (из-за него следующий запуск
    // молча не поднимался).
    let job = crate::job::attach(&child);
    {
        let state = app.state::<Daemon>();
        let mut child = child;
        // Отравленный замок — не повод оставить процесс жить: убиваем и уходим.
        let mut slot = match state.child.lock() {
            Ok(g) => g,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_dir_all(&ready_dir);
                return Err("внутренняя ошибка окна — перезапусти приложение".into());
            }
        };
        *slot = Some(child);
        drop(slot);
        match state.job.lock() {
            Ok(mut j) => *j = job,
            // Замок job не нужен для жизни процесса: `stop()` забирает child.
            Err(_) => {
                if let Some(j) = job {
                    crate::job::close(j);
                }
            }
        }
    }

    let ready = match wait_ready_file(&ready_path, READY_BUDGET, app) {
        Ok(v) => v,
        Err(e) => return Err(abandon(app, &ready_dir, &log_path, e)),
    };
    let base = match ready.get("url").and_then(|v| v.as_str()) {
        Some(u) => u.to_string(),
        None => return Err(abandon(app, &ready_dir, &log_path, "ядро не сообщило url".into())),
    };
    let token_file = match ready.get("token_file").and_then(|v| v.as_str()) {
        Some(p) => PathBuf::from(p),
        None => {
            return Err(abandon(
                app,
                &ready_dir,
                &log_path,
                "ядро не сообщило token_file".into(),
            ))
        }
    };
    // Только обычный файл: через ready-файл можно было подсунуть путь к любому
    // файлу, и его содержимое ушло бы в окно как токен.
    let regular = std::fs::symlink_metadata(&token_file)
        .map(|m| m.file_type().is_file())
        .unwrap_or(false);
    if !regular {
        return Err(abandon(
            app,
            &ready_dir,
            &log_path,
            format!("токен ядра не обычный файл: {}", token_file.display()),
        ));
    }
    let token = match read_token(&token_file) {
        Some(t) => t,
        None => return Err(abandon(app, &ready_dir, &log_path, "нет api.token".into())),
    };
    let _ = std::fs::remove_dir_all(&ready_dir);

    // Адрес получен, но HTTP-сервер мог ещё не подняться: окно на пустоту
    // открывать незачем.
    if !wait_ready(&base, &token, PING_BUDGET) {
        return Err(abandon(
            app,
            &ready_dir,
            &log_path,
            "ядро не отвечает на /v1/model".into(),
        ));
    }
    Ok(Connection {
        base_url: base,
        token,
    })
}

/// Дождаться ready-файла от ядра и разобрать его.
///
/// Заодно следим за самим процессом: если ядро умерло на старте (нет прав,
/// плохой аргумент), ответ известен сразу, и ждать 30 с незачем — человек
/// получает код возврата, а не «не запустилось за 30 с».
fn wait_ready_file(
    path: &Path,
    budget: Duration,
    app: &tauri::AppHandle,
) -> Result<serde_json::Value, String> {
    let until = Instant::now() + budget;
    while Instant::now() < until {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                return Ok(v);
            }
        }
        if let Some(how) = child_exit(app) {
            return Err(format!("ядро завершилось на старте ({how})"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("ядро не сообщило адрес за {} с", budget.as_secs()))
}

/// Как завершился дочерний процесс, если уже завершился.
fn child_exit(app: &tauri::AppHandle) -> Option<String> {
    let state = app.state::<Daemon>();
    let mut guard = state.child.lock().ok()?;
    let child = guard.as_mut()?;
    match child.try_wait() {
        Ok(Some(status)) => Some(match status.code() {
            Some(c) => format!("код {c}"),
            None => "убит сигналом".to_string(),
        }),
        _ => None,
    }
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
