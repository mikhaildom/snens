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

use daemon::Daemon;

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
                *state.connection.lock().unwrap() = Some(conn.clone());
                *state.remote.lock().unwrap() = remote;
            }
            // Порядок важен: СНАЧАЛА окно, потом трей. Трей поднимает своё
            // скрытое окно-посредник, и если оно создано раньше главного,
            // WebView2 получает "Invalid window handle" (HRESULT 0x80070578) и
            // приложение падает в setup — проверено на живой Windows.
            open_window(app.handle(), &conn, remote)?;
            tray::install(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // Крестик окна прячет окно, а не убивает приложение: ядро держит
            // долгие ходы и подагентов, и потерять их случайным кликом по
            // крестику — потеря работы. Выход — из меню трея.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("приложение не собралось");

    app.run(|handle, event| {
        if let RunEvent::ExitRequested { .. } = event {
            // Гасим ядро до выхода: иначе оно переживёт окно и следующая
            // установка не сможет перезаписать его файлы. При удалённом
            // подключении своего процесса нет — гасить нечего.
            handle.state::<Daemon>().stop();
        }
    });
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
    tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::App("index.html".into()))
        .title("snens")
        .inner_size(1280.0, 860.0)
        .min_inner_size(900.0, 600.0)
        .initialization_script(&init)
        .build()?;
    Ok(())
}
