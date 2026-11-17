//! Соединение окна с ядром: адрес, токен и откуда они взялись.
//!
//! Два источника, и это осознанное разделение:
//! - **своё ядро** — оболочка сама запустила sidecar и знает адрес из его
//!   ready-файла. Такое соединение не сохраняется на диск: при следующем
//!   запуске будет новый порт, и запомненный адрес только врал бы.
//! - **чужое ядро** (пейринг) — человек подключил окно к серверу по адресу и
//!   коду. Адрес и токен живут в файле конфигурации приложения на диске
//!   (`$APPDATA/ch.svuvi.snens/connection.json`), потому что после перезапуска
//!   оболочка должна проснуться уже подключённой и **не поднимать своё ядро**.

use std::path::PathBuf;

use tauri::Manager;

/// Что нужно окну, чтобы говорить с ядром. `serde` в обе стороны: это ответ
/// команды `connection()` для клиента и то, что лежит в файле пейринга.
#[derive(Clone, serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
pub struct Connection {
    pub base_url: String,
    pub token: String,
}

/// Файл запомненного пейринга. В `app_config_dir` (у Tauri это `%APPDATA%\
/// ch.svuvi.snens` на Windows, `~/.config/ch.svuvi.snens` на Linux) — рядом с
/// конфигом приложения, а не в каталоге данных ядра: это настройка оболочки,
/// ядру о ней знать не надо.
fn stored_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|d| d.join("connection.json"))
}

/// Прочитать запомненное удалённое соединение. Битый файл — «не подключено»:
/// окно лучше спросит заново, чем откроется на мёртвый адрес.
pub fn load_stored(app: &tauri::AppHandle) -> Option<Connection> {
    let path = stored_path(app)?;
    let text = std::fs::read_to_string(path).ok()?;
    let conn: Connection = serde_json::from_str(&text).ok()?;
    // Тот же минимум, что проверяет `set_remote`: без него файл не соединение.
    (conn.base_url.starts_with("http") && conn.token.trim().len() >= 32).then_some(conn)
}

/// Запомнить (или забыть, если `None`) удалённое соединение. Ошибка записи
/// возвращается наружу: молча «сохранённое» соединение теряется при следующем
/// запуске, и человек видит окно на своём ядре без объяснения.
pub fn save_stored(app: &tauri::AppHandle, conn: &Connection) -> Result<(), String> {
    let Some(path) = stored_path(app) else {
        return Err("не нашёл каталог конфигурации приложения".into());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("не удалось создать {}: {e}", dir.display()))?;
    }
    let body = serde_json::to_string_pretty(conn)
        .map_err(|e| format!("не удалось сериализовать соединение: {e}"))?;
    // Пишем через временный файл: оборванная запись (диск, антивирус) не должна
    // оставить окно без адреса, по которому оно уже работало.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body).map_err(|e| format!("не удалось записать {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("не удалось сохранить {}: {e}", path.display())
    })
}

pub fn forget_stored(app: &tauri::AppHandle) {
    if let Some(path) = stored_path(app) {
        let _ = std::fs::remove_file(path);
    }
}

/// Пересоздать окно на текущем соединении.
///
/// Зачем не `location.reload()`: адрес и токен лежат в `window.__SNENS__`,
/// который оболочка кладёт в окно при его СОЗДАНИИ. Перезагрузка страницы
/// взяла бы старые значения из того же окна, и после пейринга клиент продолжил
/// бы ходить на прежнее ядро (проверено). Поэтому окно уничтожается и
/// создаётся заново — с новым инжектом.
#[tauri::command]
pub fn reopen(app: tauri::AppHandle, state: tauri::State<'_, crate::Daemon>) -> Result<(), String> {
    let conn = state
        .connection
        .lock()
        .map_err(|_| "внутренняя ошибка окна — перезапусти приложение")?
        .clone()
        .ok_or("нет соединения")?;
    let remote = state.remote.lock().map(|g| *g).unwrap_or(false);
    // Старое окно сносим ДО создания нового (метка `main` должна освободиться),
    // но если новое не собралось — поднимаем его обратно на том же соединении:
    // иначе остаётся приложение без единого окна (трей показывает то же окно).
    let previous = app.get_webview_window("main");
    if let Some(w) = previous {
        let _ = w.destroy();
    }
    if let Err(e) = crate::open_window(&app, &conn, remote) {
        let restored = crate::open_window(&app, &conn, remote);
        return match restored {
            Ok(()) => Err(format!("окно не пересоздалось: {e}")),
            Err(_) => Err(format!("окно потеряно: {e}")),
        };
    }
    Ok(())
}

/// Ответ команды: адрес и токен текущего соединения.
#[tauri::command]
pub fn connection(state: tauri::State<'_, crate::Daemon>) -> Option<Connection> {
    state.connection.lock().ok().and_then(|g| g.clone())
}

/// Подключиться к чужому ядру (экран пейринга). Обмен кода на токен делает
/// клиент — ему проще показать ошибку ядра как есть. Здесь только проверка
/// ввода и сохранение: следующий запуск окна поднимется уже на этом адресе.
#[tauri::command]
pub fn set_remote(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::Daemon>,
    base_url: String,
    token: String,
) -> Result<(), String> {
    let base = base_url.trim().trim_end_matches('/').to_string();
    if !base.starts_with("http://") && !base.starts_with("https://") {
        return Err("адрес должен начинаться с http:// или https://".into());
    }
    if token.trim().len() < 32 {
        return Err("токен подозрительно короткий".into());
    }
    let conn = Connection {
        base_url: base,
        token: token.trim().to_string(),
    };
    save_stored(&app, &conn)?;
    *state
        .connection
        .lock()
        .map_err(|_| "внутренняя ошибка окна — перезапусти приложение")? = Some(conn);
    *state.remote.lock().map_err(|_| "внутренняя ошибка окна — перезапусти приложение")? = true;
    Ok(())
}

/// Отключиться от чужого ядра: забыть адрес и вернуться к своему. Ядро
/// поднимется при следующем запуске — менять процесс на ходу нельзя, клиент
/// держит уже открытые подписки на старый адрес.
#[tauri::command]
pub fn clear_remote(app: tauri::AppHandle, state: tauri::State<'_, crate::Daemon>) -> Result<(), String> {
    forget_stored(&app);
    *state.connection.lock().map_err(|_| "внутренняя ошибка окна — перезапусти приложение")? = None;
    *state.remote.lock().map_err(|_| "внутренняя ошибка окна — перезапусти приложение")? = false;
    Ok(())
}
