// Окно snens: поднимает своё ядро и живёт с ним одной жизнью.
//
// Почему ядро — дочерний процесс, а не Windows-служба: служба требует
// администратора и работает в сессии 0, где нет ни профиля пользователя, ни
// браузера, ни окон. То есть половина инструментов агента (веб, desk, файлы в
// профиле) там просто не работает. Поэтому ядро запускается из окна и умирает
// вместе с ним (job object с KILL_ON_JOB_CLOSE).

use std::{
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

use tauri::{Manager, RunEvent, State};

/// Что нужно окну, чтобы говорить с ядром.
#[derive(Clone, serde::Serialize)]
pub struct Connection {
    base_url: String,
    token: String,
}

/// Запущенное ядро: процесс и то, что окну нужно знать о нём.
struct Daemon {
    child: Mutex<Option<Child>>,
    connection: Mutex<Option<Connection>>,
}

/// Свободный порт на loopback: два экземпляра приложения не должны драться за
/// один адрес. `bind(0)` отдаёт выбранный порт, и мы тут же отпускаем его —
/// гонка теоретически возможна, но на loopback в пределах одного пользователя
/// это цена, которую проще принять, чем держать порт занятым до старта ядра.
fn free_port() -> std::io::Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

/// Где лежат бинарники: рядом с exe (так их кладёт установщик).
fn side(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    let dir = exe.parent().unwrap_or(Path::new("."));
    dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
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

/// Прочитать токен инстанса; если его нет — ядро создаст при старте, поэтому
/// сначала запускаем ядро, потом читаем.
fn read_token(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[tauri::command]
fn connection(state: State<'_, Daemon>) -> Option<Connection> {
    state.connection.lock().ok().and_then(|g| g.clone())
}

/// Удалённое подключение (пейринг): окно запоминает адрес и токен и больше не
/// запускает своё ядро. Здесь только сохранение — обмен кода на токен делает
/// клиент (ему так проще показать ошибку ядра как есть).
#[tauri::command]
fn set_remote(state: State<'_, Daemon>, base_url: String, token: String) -> Result<(), String> {
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Err("адрес должен начинаться с http:// или https://".into());
    }
    if token.trim().len() < 32 {
        return Err("токен подозрительно короткий".into());
    }
    *state
        .connection
        .lock()
        .map_err(|_| "состояние отравлено")? = Some(Connection {
        base_url: base_url.trim_end_matches('/').to_string(),
        token: token.trim().to_string(),
    });
    Ok(())
}

fn main() {
    let daemon = Daemon {
        child: Mutex::new(None),
        connection: Mutex::new(None),
    };

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(daemon)
        .invoke_handler(tauri::generate_handler![connection, set_remote])
        .setup(|app| {
            let state = app.state::<Daemon>();
            // Порт выбираем сами и передаём ядру явно: окну нужно знать адрес до
            // того, как ядро напечатает свой.
            let port = free_port().map_err(|e| format!("свободный порт: {e}"))?;
            let base = format!("http://127.0.0.1:{port}");
            let bin = side("snens");

            let mut cmd = Command::new(&bin);
            cmd.arg("serve")
                .arg("--listen")
                .arg(format!("127.0.0.1:{port}"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // Окно без консоли: без этого флага консольный процесс получит своё
            // окно терминала и повиснет поверх интерфейса.
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            let child = cmd
                .spawn()
                .map_err(|e| format!("не удалось запустить {}: {e}", bin.display()))?;
            *state.child.lock().unwrap() = Some(child);

            // Токен ядро создаёт при первом старте; читаем после запуска.
            let data = snens_data_dir();
            let token_path = data.join("api.token");
            let mut token = None;
            for _ in 0..50 {
                token = read_token(&token_path);
                if token.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let token = token.ok_or("ядро не создало api.token")?;

            if !wait_ready(&base, &token, Duration::from_secs(20)) {
                return Err("ядро не ответило на /v1/model за 20 с".into());
            }
            *state.connection.lock().unwrap() = Some(Connection {
                base_url: base.clone(),
                token: token.clone(),
            });

            // Клиент живёт на том же адресе: ядро отдаёт `web/` само.
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.eval(&format!(
                    "window.location.replace({base:?})"
                ));
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("приложение не собралось");

    app.run(|handle, event| {
        if let RunEvent::ExitRequested { .. } = event {
            // Гасим ядро до выхода приложения: иначе оно переживёт окно и
            // следующая установка не сможет перезаписать его файлы.
            let state = handle.state::<Daemon>();
            if let Ok(mut guard) = state.child.lock() {
                if let Some(mut child) = guard.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
    });
}

/// Каталог данных snens: та же раскладка, что у ядра (`cfg::data_dir`), но
/// без его кода — оболочка не линкуется со ядром.
fn snens_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SNENS_DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    if cfg!(windows) {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata).join("snens");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/share/snens")
}
