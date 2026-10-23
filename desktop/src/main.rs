// Окно snens: поднимает своё ядро и живёт с ним одной жизнью.
//
// Почему ядро — дочерний процесс, а не Windows-служба: служба требует
// администратора и работает в сессии 0, где нет ни профиля пользователя, ни
// браузера, ни окон. То есть половина инструментов агента (веб, desk, файлы в
// профиле) там просто не работает. Поэтому ядро запускается из окна и умирает
// вместе с ним (job object с KILL_ON_JOB_CLOSE).

use std::{
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
            let bin = side("snens");

            // Порт и путь к токену не угадываем: ядро само выберет свободный
            // (`--listen 127.0.0.1:0`) и запишет ФАКТИЧЕСКИЕ значения в ready-файл.
            // Иначе оболочка держала бы вторую копию правил «где данные, какой
            // порт» — и расходилась бы с ядром (проверено: расходилась).
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

            let ready = wait_ready_file(&ready_path, Duration::from_secs(30))
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

            if !wait_ready(&base, &token, Duration::from_secs(20)) {
                return Err("ядро не ответило на /v1/model за 20 с".into());
            }
            *state.connection.lock().unwrap() = Some(Connection {
                base_url: base.clone(),
                token,
            });

            // Клиенту нужно знать адрес ядра и токен: в упакованном приложении
            // vite-прокси нет, и относительный `/api` ведёт в никуда (проверено:
            // окно рисовалось, модель в шапке стояла «—», запросы падали 404).
            //
            // Окно создаётся ЗДЕСЬ, а не из конфига: `initialization_script`
            // выполняется до кода страницы, а клиент читает `window.__SNENS__`
            // при инициализации модуля. `eval` после загрузки окна опаздывал бы.
            let token = state
                .connection
                .lock()
                .map(|g| g.as_ref().map(|c| c.token.clone()).unwrap_or_default())
                .unwrap_or_default();
            let init = format!(
                "window.__SNENS__ = {{ baseUrl: {base_json}, token: {token_json} }};",
                base_json = serde_json::to_string(&base)?,
                token_json = serde_json::to_string(&token)?,
            );
            tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::External(
                base.parse().map_err(|e| format!("адрес ядра: {e}"))?,
            ))
            .title("snens")
            .inner_size(1280.0, 860.0)
            .min_inner_size(900.0, 600.0)
            .initialization_script(&init)
            .build()?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("приложение не собралось");

    app.run(|handle, event| {
        if let RunEvent::ExitRequested { .. } = event {
            // Гасим ядро до выхода приложения: иначе оно переживёт окно и
            // следующая установка не сможет перезаписать его файлы.
            let state = handle.state::<Daemon>();
            // Отдельная переменная, а не `if let Ok(...)`: временный `Result` от
            // `lock()` живёт до конца выражения и держит заимствование `state`,
            // а `state` умирает раньше (E0597 — ловится только на Windows-сборке).
            let stopped = state
                .child
                .lock()
                .map(|mut guard| guard.take())
                .unwrap_or(None);
            if let Some(mut child) = stopped {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    });
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
