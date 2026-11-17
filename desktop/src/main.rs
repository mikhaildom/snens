//! Окно snens: поднимает своё ядро (или подключается к чужому) и живёт с ним
//! одной жизнью.
//!
//! Почему ядро — дочерний процесс, а не Windows-служба: служба требует
//! администратора и работает в сессии 0, где нет ни профиля пользователя, ни
//! браузера, ни окон. То есть половина инструментов агента (веб, desk, файлы в
//! профиле) там просто не работает. Поэтому ядро запускается из окна и умирает
//! вместе с ним.
//!
//! Файлы разложены по слоям, а не в одном main.rs:
//! - `daemon.rs` — запуск и остановка ядра, ожидание его готовности;
//! - `connection.rs` — что окно знает о ядре: адрес, токен, свой или чужой;
//! - `tray.rs` — иконка в системном трее и меню;
//! - `single.rs` — один экземпляр на пользователя: второй запуск поднимает
//!   окно первого и выходит;
//! - `job.rs` — жизнь ядра привязана к жизни окна (Windows: job object).

// Консольного окна у GUI-сборки нет вовсе. Это атрибут самого exe: без него
// Windows показывает чёрное пустое окно терминала рядом с интерфейсом, и его
// закрытие убивает приложение (владелец видел ровно это).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod connection;
mod daemon;
mod job;
mod single;
mod tray;

use tauri::{Manager, RunEvent, WindowEvent};

use daemon::{Daemon, Session};

fn main() {
    // Второй экземпляр не должен поднимать второе ядро и воровать окно: он
    // поднимает окно первого и уходит. Без этого запуск по шорткату молча
    // ничего не делал (паника в setup у GUI-сборки не видна).
    if single::already_running() {
        return;
    }
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(Daemon::new())
        .invoke_handler(tauri::generate_handler![
            connection::connection,
            connection::set_remote,
            connection::clear_remote,
            connection::reopen,
        ])
        .setup(|app| {
            // Источник соединения — с диска: у человека, который уже подключил
            // окно к чужому ядру, свой процесс не поднимается вовсе. Иначе два
            // ядра на одной машине дрались бы за один каталог данных.
            let stored = connection::load_stored(app.handle());
            let remote = stored.is_some();
            let conn = match stored {
                Some(c) => c,
                None => daemon::start(app.handle())?,
            };
            {
                let state = app.state::<Daemon>();
                // Замок отравлен — это паника в другом потоке, а `unwrap` здесь
                // уронил бы setup: в GUI-сборке паника не видна (нет ни консоли,
                // ни трея, ни окна). Отдаём ошибку наружу.
                *state
                    .session
                    .lock()
                    .map_err(|_| "внутренняя ошибка окна — перезапусти приложение")? =
                    Some(Session {
                        connection: conn.clone(),
                        remote,
                    });
            }
            // Порядок важен: СНАЧАЛА окно, потом трей. Трей поднимает своё
            // скрытое окно-посредник, и если оно создано раньше главного,
            // WebView2 получает "Invalid window handle" (HRESULT 0x80070578) и
            // приложение падает в setup — проверено на живой Windows.
            open_window(app.handle(), &conn, remote)?;
            tray::install(app.handle())?;
            // Слушатель просьб «покажи окно» от второго экземпляра: только
            // после создания окна — до этого показывать нечего.
            single::serve_show_requests(app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            // Крестик окна прячет окно, а не убивает приложение: ядро держит
            // долгие ходы и подагентов, и потерять их случайным кликом по
            // крестику — потеря работы. Выход — из меню трея.
            //
            // `is_visible` перед `hide` — не суеверие. Раньше окно показывал
            // второй экземпляр через Win32 `ShowWindow`, и Tauri продолжал
            // считать его спрятанным: следующий крестик звал `hide`, тот
            // ничего не делал («уже спрятано»), и окно снаружи «не
            // закрывалось». Показ теперь идёт только через Tauri, но проверка
            // остаётся: любое рассинхронизированное состояние лечится здесь,
            // а не выясняется на живой машине.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                if window.is_visible().unwrap_or(false) {
                    let _ = window.hide();
                }
            }
        })
        .build(tauri::generate_context!())
        .unwrap_or_else(|e| {
            // Панику в GUI-сборке некому показать (нет консоли), поэтому хотя бы
            // сохранём текст в файл рядом с логом ядра и выйдем с кодом 1.
            let text = format!("snens-desktop: не запустилось: {e}");
            eprintln!("{text}");
            if let Ok(dir) = app_handle_log_dir() {
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(dir.join("desktop.log"), &text);
            }
            std::process::exit(1);
        });

    app.run(|handle, event| {
        // Гасим ядро только когда приложение действительно завершается.
        // `ExitRequested` отменяемо (`api.prevent_exit()` — например, плагином
        // автообновления), и убитое там ядро оставило бы живое окно без
        // соединения и без способа поднять его заново.
        if let RunEvent::Exit = event {
            // При удалённом подключении своего процесса нет — гасить нечего.
            handle.state::<Daemon>().stop();
        }
    });
}

/// Каталог логов приложения — без `AppHandle` (он недоступен, когда `build`
/// уже упал). Правило то же, что у Tauri: `%LOCALAPPDATA%`/`XDG_STATE_HOME`.
fn app_handle_log_dir() -> Result<std::path::PathBuf, ()> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Library/Logs"))
    } else {
        std::env::var_os("XDG_STATE_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
            })
    };
    base.map(|b| b.join("ch.svuvi.snens").join("logs"))
        .ok_or(())
}

/// Открыть окно на ядре `conn`. Окно всегда грузит **свой** бандл клиента
/// (`resources/web`), а адрес ядра приходит инжектом.
///
/// Это важно для пейринга: при `WebviewUrl::External` окно грузило бы клиент
/// ЧУЖОГО ядра — и версия интерфейса ехала бы с сервера, а экран подключения
/// (кнопка «вернуться к своему ядру») был бы недоступен вовсе, потому что
/// собран он в нашем клиенте. Своя статика + абсолютный адрес API дают одно
/// поведение в обоих режимах; CORS на ядре открыт.
///
/// Создаётся здесь, а не из конфига: `initialization_script` выполняется до кода
/// страницы, а клиент читает `window.__SNENS__` при инициализации модуля —
/// `eval` после загрузки окна опаздывал бы (проверено: модель в шапке стояла
/// «—», запросы падали 404).
pub fn open_window(
    app: &tauri::AppHandle,
    conn: &connection::Connection,
    remote: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // `remote` нужен клиенту, чтобы показать в настройках «вернуться к своему
    // ядру» вместо формы подключения.
    let init = format!(
        "window.__SNENS__ = {{ baseUrl: {base}, token: {token}, shell: true, remote: {remote} }};",
        base = serde_json::to_string(&conn.base_url)?,
        token = serde_json::to_string(&conn.token)?,
    );
    // `initialization_script` выполняется на КАЖДОЙ навигации верхнего уровня,
    // а не только на первой загрузке своего бандла. Без фильтра любой переход
    // на чужой origin (редирект, ссылка из ленты) получил бы токен инстанса в
    // `window.__SNENS__`. Поэтому разрешаем остаться только внутри своего
    // бандла или на самом адресе ядра.
    //
    // Схема своего бандла зависит от платформы: на Windows это
    // `http://tauri.localhost`, на macOS/Linux — `tauri://localhost`.
    // Проверка только по `https://tauri.localhost` (было в 0.3.0) блокировала
    // ПЕРВУЮ же навигацию на свой `index.html`: окно оставалось about:blank,
    // то есть пустой экран у каждого, кто поставил установщик.
    let own = conn.base_url.clone();
    tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::App("index.html".into()))
        .title("snens")
        .inner_size(1280.0, 860.0)
        .min_inner_size(900.0, 600.0)
        .on_navigation(move |url| {
            let s = url.as_str();
            let scheme = url.scheme();
            let host = url.host_str().unwrap_or("");
            // Свой бандл: любая локальная схема tauri.
            if scheme == "tauri" || host == "tauri.localhost" {
                return true;
            }
            s.starts_with(&own)
        })
        .initialization_script(&init)
        .build()?;
    Ok(())
}
